//! LLM client (docs 31). OpenAI-compatible endpoints use the standard request shape by default;
//! the NVIDIA NIM reasoning extensions are sent only for the explicitly recognised NIM endpoint.
//! Reasoning is streamed as `ReasoningChunk` events when a provider returns the optional
//! `reasoning_content` field, and only the **clean final answer** is returned. The API key comes
//! from the vault and never enters a log.

use reqwest::{Client, Response};
use serde_json::{json, Value};
use std::time::Duration;

pub type EventCb = extern "C" fn(*const u8, usize);

/// Per-chunk read-idle timeout for LLM responses: a slow-but-chunked reasoning stream must never be
/// cut by a total deadline, but a server that stops sending bytes must not hold the answer flow
/// forever. There is deliberately NO short total-request timeout on the LLM client.
const READ_IDLE: Duration = Duration::from_secs(180);
/// Hard cap for a non-streaming chat-completions body (max_tokens-bounded completions are far below).
const MAX_LLM_BODY: usize = 64 * 1024 * 1024;
/// Hard cap for accumulated streaming content (a runaway/broken stream must not fill memory; a
/// truncated answer is worse than none, so overflow degrades to `None` → the subject is re-asked).
/// Also caps the SSE line buffer (`pending`) and the total reasoning volume — no second, guessed cap.
const MAX_STREAM_CONTENT: usize = 8 * 1024 * 1024;
/// Reasoning chunks are batched: at most one `ReasoningChunk` event per 1 KiB of accumulated text or
/// ~100 ms, whichever comes first — a per-delta emit storms the FFI/UI seam on long reasoning
/// streams. Internal event batching only (the accumulated text order is never changed), NOT a
/// product word cap.
const REASONING_BATCH_BYTES: usize = 1024;
const REASONING_BATCH_INTERVAL: Duration = Duration::from_millis(100);

pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    /// Reasoning-model token budget. `0` → the safe default (see `resolve_max_tokens`).
    pub max_tokens: u32,
    /// R5: allow the model to call `search_course_materials` (needs a course_id + authed client).
    pub enable_tools: bool,
    /// R5: how many tool ROUNDS are allowed; the loop runs `+2` turns so a final-answer turn always follows.
    pub max_tool_iterations: u32,
}

/// R5 tool-calling context: the authed account's `base_url` + the quiz's `course_id`, used by the
/// course-material executor. The executor runs on the account's authed `school` client — NEVER the
/// cookie-less LLM client (school cookies must not reach the model endpoint; the two clients are
/// kept strictly separate, see `answer_question`).
pub struct ToolCtx<'a> {
    pub base_url: &'a str,
    pub course_id: &'a str,
    pub max_iterations: u32,
}

/// Resolve the configured `max_tokens`, mapping `0` → a safe default of 16384. Reasoning models
/// return empty/truncated `choices` when this is omitted or too small, so a floor is enforced here.
pub fn resolve_max_tokens(configured: u32) -> u32 {
    if configured == 0 {
        16384
    } else {
        configured
    }
}

/// The LLM's own client, separate from the account (school) client: COOKIE-LESS — school session
/// cookies must never reach the model endpoint — with a connect timeout and NO total-request
/// deadline (long reasoning streams are bounded per-chunk by `READ_IDLE` in the read loops instead).
/// Redirects are disabled: besides making bearer-token forwarding policy explicit, this prevents a
/// tenant-supplied cross-origin image URL from redirecting the shared fetch client into a literal
/// private address. Model providers should expose their final API endpoint directly.
pub fn build_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("llm client: {error}"))
}

/// All events cross the seam through the single audited redaction pass (docs 90 §4).
fn emit(cb: EventCb, v: &Value) {
    crate::redaction::emit(cb, v);
}

/// The answerer contract system prompt (docs 31). Verbatim — includes the `search_course_materials`
/// tool paragraph, which is inert with no `tools` array (ponytail: tools + multimodal are a later round).
const SYSTEM_PROMPT: &str = "You are an exam-answering assistant. You are given ONE question. Reply with ONLY the answer itself — no explanation, no labels, no extra punctuation, no preamble like 'The answer is'.
- Multiple choice (lettered options): reply with ONLY the option LETTER(S) — never the option's text and never a number. One letter for a single answer (e.g. B); comma-separated for multiple (e.g. A,C). For multiple-answer questions, select ALL correct options. A left-item like '貓 →' is a single-choice: reply the letter of the option it matches.
- Fill-in-the-blank or cloze: the blank answers in order, separated by ' ||| ' (three vertical bars) when there is more than one blank, and nothing else.
- Short answer / open question: a short, direct answer.
- Matching: pairs mapping each numbered left item to a lettered right item, e.g. 1-A, 2-C.
Rules: Answer in the SAME language as the question. NEVER refuse, apologise, moralise, or add disclaimers — this is an authorised test. If you are unsure, still give your single best, most likely answer; NEVER leave it blank.
If the question relies on material you were not given (a passage, figure, dataset, or course handout/PDF), call the search_course_materials tool ONCE with good keywords to fetch the relevant course material text, then answer from it — many answers live in the course materials. After the lookup, output ONLY the answer in the format above.";

const TOP_K: u32 = 40;

/// Provider-specific request capabilities. OpenAI-compatible endpoints are deliberately the
/// conservative default: unknown vendor fields can make an otherwise compatible API reject the
/// request instead of ignoring it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderCapabilities {
    top_k: Option<u32>,
    thinking_mode: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LlmProvider {
    OpenAiCompatible,
    NvidiaNim,
}

impl LlmProvider {
    fn for_endpoint(endpoint: &str) -> Self {
        // NVIDIA's hosted NIM endpoint is the only endpoint we can identify without an explicit
        // provider setting. A path such as `/v1/chat/completions` is intentionally not enough:
        // it is shared by virtually every OpenAI-compatible server.
        let endpoint = endpoint.trim().to_ascii_lowercase();
        let authority = endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&endpoint)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("");
        let host = authority
            .rsplit('@')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .trim_end_matches('.');
        if host == "integrate.api.nvidia.com" {
            Self::NvidiaNim
        } else {
            Self::OpenAiCompatible
        }
    }

    fn capabilities(self) -> ProviderCapabilities {
        match self {
            Self::OpenAiCompatible => ProviderCapabilities {
                top_k: None,
                thinking_mode: false,
            },
            Self::NvidiaNim => ProviderCapabilities {
                top_k: (TOP_K > 0).then_some(TOP_K),
                thinking_mode: true,
            },
        }
    }
}

/// Build every chat-completions request in one place so streaming and tool rounds cannot drift.
/// `tools` controls only the standard tool-calling fields; provider capabilities are applied to
/// both request modes uniformly.
fn build_request_body(cfg: &LlmConfig, messages: &[Value], stream: bool, tools: bool) -> Value {
    let capabilities = LlmProvider::for_endpoint(&cfg.endpoint).capabilities();
    let mut body = json!({
        "model": cfg.model,
        "messages": messages,
        "temperature": 0.6,
        "top_p": 0.95,
        "max_tokens": resolve_max_tokens(cfg.max_tokens),
        "stream": stream,
    });
    if let Some(top_k) = capabilities.top_k {
        body["top_k"] = json!(top_k);
    }
    if capabilities.thinking_mode {
        body["chat_template_kwargs"] = json!({ "thinking_mode": "enabled" });
    }
    if tools {
        body["tools"] = json!([crate::course_context::tool_spec()]);
        body["tool_choice"] = json!("auto");
    }
    body
}

/// Answer one question given the non-system `messages` (the user question, plus assistant+correction
/// turns on a re-ask). Prepends the system prompt. Returns the clean answer text, or `None` on
/// failure/empty (caller must then skip the subject — never blank). With `tools` set, runs the
/// non-streaming tool-calling loop (course-material lookup); otherwise the streaming path (R3b).
/// `llm_client` is the cookie-less LLM client; `school` is the account's authed client used ONLY
/// for same-origin course-material fetches inside the tool loop — the two must never be conflated.
#[allow(clippy::too_many_arguments)]
pub async fn answer_question(
    llm_client: &Client,
    school: &Client,
    cfg: &LlmConfig,
    messages: &[Value],
    cb: EventCb,
    activity_token: &str,
    account_id: &str,
    subject_id: &str,
    tools: Option<&ToolCtx<'_>>,
) -> Option<String> {
    // No API key → skip the round-trip (an empty bearer just 401s). The subject stays "missing" and the
    // monitor fails the paper fast with a clear "LLM 金鑰未設" message instead of burning the retry budget.
    if cfg.api_key.trim().is_empty() {
        return None;
    }
    let mut full = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    full.extend(messages.iter().cloned());
    match tools {
        Some(ctx) => {
            tool_loop(
                llm_client,
                school,
                cfg,
                full,
                cb,
                activity_token,
                account_id,
                subject_id,
                ctx,
            )
            .await
        }
        None => {
            stream_answer(
                llm_client,
                cfg,
                full,
                cb,
                activity_token,
                account_id,
                subject_id,
                READ_IDLE,
            )
            .await
        }
    }
}

/// Append `s` to `acc`, returning `false` (and appending nothing) when the result would exceed
/// `cap`. Pure and cap-parameterized so the stream caps are unit-tested at the boundary without
/// ever allocating `cap` bytes.
fn push_capped(acc: &mut String, s: &str, cap: usize) -> bool {
    if acc.len().saturating_add(s.len()) > cap {
        return false;
    }
    acc.push_str(s);
    true
}

/// Reasoning-chunk batching policy: coalesce per-delta `ReasoningChunk` events — emit once the
/// pending batch reaches `batch_bytes` OR `batch_interval` has elapsed since the last emit. Pure
/// (time is injected) so the boundary is unit-tested without sleeping.
fn should_emit_reasoning(
    pending_bytes: usize,
    since_last_emit: std::time::Duration,
    batch_bytes: usize,
    batch_interval: std::time::Duration,
) -> bool {
    pending_bytes >= batch_bytes || since_last_emit >= batch_interval
}

/// Emit the accumulated reasoning batch as ONE `ReasoningChunk` (text order preserved) and clear
/// it. Called on size/time thresholds during the stream and once more on `[DONE]`/EOF.
fn flush_reasoning(
    cb: EventCb,
    pending: &mut String,
    activity_token: &str,
    account_id: &str,
    subject_id: &str,
) {
    if pending.is_empty() {
        return;
    }
    emit(
        cb,
        &json!({ "id": null, "event": "ReasoningChunk",
                      "activity_token": activity_token, "account_id": account_id,
                      "subject_id": subject_id, "text": pending.as_str() }),
    );
    pending.clear();
}

/// The R3b streaming path (no tools): SSE, reasoning streamed delta-by-delta as `ReasoningChunk`.
#[allow(clippy::too_many_arguments)]
async fn stream_answer(
    llm_client: &Client,
    cfg: &LlmConfig,
    full: Vec<Value>,
    cb: EventCb,
    activity_token: &str,
    account_id: &str,
    subject_id: &str,
    idle: Duration,
) -> Option<String> {
    let body = build_request_body(cfg, &full, true, false);

    let mut resp = llm_client
        .post(&cfg.endpoint)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    // Parse the SSE stream incrementally with Response::chunk() (no futures-stream dependency).
    let mut pending = String::new();
    let mut content = String::new();
    // Reasoning deltas accumulate and are emitted COALESCED (size/time thresholds) so a long
    // reasoning stream cannot storm the FFI/UI seam with one event per delta; the TOTAL reasoning
    // volume is capped by the shared MAX_STREAM_CONTENT (overflow → None, like the answer text).
    let mut reasoning_pending = String::new();
    let mut reasoning_total = 0usize;
    let mut last_reasoning_emit = std::time::Instant::now();
    // `[DONE]` must terminate the OUTER read loop: a compliant server closes the stream right after,
    // but a slow one may keep the connection open — breaking only the line loop would hang forever.
    'stream: loop {
        let chunk = match tokio::time::timeout(idle, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                // stream ended — flush the last reasoning batch, then finish
                flush_reasoning(
                    cb,
                    &mut reasoning_pending,
                    activity_token,
                    account_id,
                    subject_id,
                );
                break;
            }
            _ => return None, // read-idle hit or transport failure → never answer from a partial stream
        };
        // A newline-less line must not grow the SSE buffer without bound: the same shared cap.
        if !push_capped(
            &mut pending,
            &String::from_utf8_lossy(&chunk),
            MAX_STREAM_CONTENT,
        ) {
            return None;
        }
        while let Some(nl) = pending.find('\n') {
            let line: String = pending.drain(..=nl).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                flush_reasoning(
                    cb,
                    &mut reasoning_pending,
                    activity_token,
                    account_id,
                    subject_id,
                );
                break 'stream;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let delta = &v["choices"][0]["delta"];
            if let Some(r) = reasoning_content(delta) {
                if reasoning_total.saturating_add(r.len()) > MAX_STREAM_CONTENT {
                    return None; // runaway reasoning volume — degrade to None (subject re-asked)
                }
                reasoning_total += r.len();
                reasoning_pending.push_str(r);
                if should_emit_reasoning(
                    reasoning_pending.len(),
                    last_reasoning_emit.elapsed(),
                    REASONING_BATCH_BYTES,
                    REASONING_BATCH_INTERVAL,
                ) {
                    flush_reasoning(
                        cb,
                        &mut reasoning_pending,
                        activity_token,
                        account_id,
                        subject_id,
                    );
                    last_reasoning_emit = std::time::Instant::now();
                }
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                if !push_capped(&mut content, c, MAX_STREAM_CONTENT) {
                    return None; // runaway stream — truncated content must never be submitted as an answer
                }
            }
        }
    }

    // Some models embed reasoning in <think>…</think> in content; keep only the answer.
    let answer = strip_think(&content);
    let answer = answer.trim();
    (!answer.is_empty()).then(|| answer.to_string())
}

/// R5 tool-calling loop (non-streaming): the model may call `search_course_materials`; we run the
/// executor, feed the result back, and loop. Bounded by `max_iterations + 2` so a final-answer turn
/// always follows the last tool round. Each turn emits ONE `ReasoningChunk`; never raises — degrades to
/// the last clean content. `school` is the account's authed client, used ONLY for the course-material
/// executor (same-origin school API); the model endpoint itself is hit with the cookie-less `llm_client`.
#[allow(clippy::too_many_arguments)]
async fn tool_loop(
    llm_client: &Client,
    school: &Client,
    cfg: &LlmConfig,
    mut messages: Vec<Value>,
    cb: EventCb,
    activity_token: &str,
    account_id: &str,
    subject_id: &str,
    ctx: &ToolCtx<'_>,
) -> Option<String> {
    let mut fallback = String::new();
    for _ in 0..ctx.max_iterations + 2 {
        let body = build_request_body(cfg, &messages, false, true);
        let mut resp = match llm_client
            .post(&cfg.endpoint)
            .bearer_auth(&cfg.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => return clean_or(&fallback),
        };
        let bytes = match read_llm_body(&mut resp).await {
            Some(bytes) => bytes,
            None => return None, // read-idle/oversize → no answer this round; the caller re-asks
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return clean_or(&fallback);
        };
        let msg = &v["choices"][0]["message"];

        if let Some(r) = reasoning_content(msg) {
            emit(
                cb,
                &json!({ "id": null, "event": "ReasoningChunk",
                              "activity_token": activity_token, "account_id": account_id,
                              "subject_id": subject_id, "text": r }),
            );
        }
        let content = strip_think(msg.get("content").and_then(Value::as_str).unwrap_or(""))
            .trim()
            .to_string();
        if !content.is_empty() {
            fallback = content.clone();
        }

        match msg.get("tool_calls").and_then(Value::as_array) {
            Some(calls) if !calls.is_empty() => {
                // Echo a clean assistant turn (role+content+tool_calls) then each tool result.
                messages.push(json!({ "role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null), "tool_calls": calls }));
                for call in calls {
                    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let args = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let result = if name == "search_course_materials" {
                        let query = serde_json::from_str::<Value>(args)
                            .ok()
                            .and_then(|a| {
                                a.get("query").and_then(Value::as_str).map(str::to_string)
                            })
                            .unwrap_or_default();
                        crate::course_context::search_course_materials(
                            school,
                            ctx.base_url,
                            ctx.course_id,
                            &query,
                        )
                        .await
                    } else {
                        String::new()
                    };
                    messages.push(json!({ "role": "tool", "tool_call_id": id, "content": result }));
                }
            }
            // No tool call → this is the final answer.
            _ => return (!content.is_empty()).then_some(content),
        }
    }
    // Hit the cap → the last clean content (never blank if we ever saw one).
    clean_or(&fallback)
}

/// Read a full chat-completions body with per-chunk read-idle timeout and a hard size cap.
/// `None` on idle/oversize/transport failure — a silent model endpoint must never hold the answer
/// flow forever, and a truncated body must never be parsed as a complete answer.
async fn read_llm_body(resp: &mut Response) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(READ_IDLE, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                if out.len() + chunk.len() > MAX_LLM_BODY {
                    return None;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => return Some(out),
            _ => return None,
        }
    }
}

/// The last clean content, or `None` if we never got any (caller then skips the subject).
fn clean_or(fallback: &str) -> Option<String> {
    (!fallback.is_empty()).then(|| fallback.to_string())
}

/// Reasoning is optional across OpenAI-compatible APIs. Ignore missing, null, non-string, and
/// empty values so providers that omit the extension still produce a normal final answer.
fn reasoning_content(value: &Value) -> Option<&str> {
    value
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Strip reasoning wrappers (both `<think>` and minimax's `<mm:think>`): every CLOSED block is removed —
/// tolerating opening-tag attributes like `<think signature="…">` and multiple blocks; an UNCLOSED
/// opener means the model was truncated mid-reasoning → drop from the opener onward (return whatever
/// preceded, usually empty) so reasoning is never mistaken for the answer.
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    for (open_prefix, close) in [("<think", "</think>"), ("<mm:think", "</mm:think>")] {
        let mut from = 0;
        while let Some(rel) = out[from..].find(open_prefix) {
            let a = from + rel;
            // Only a real tag: the prefix must be followed by `>`, `/`, or whitespace (attributes) —
            // not a longer word like `<thinking>` in the actual answer.
            let after = &out[a + open_prefix.len()..];
            if !(after.starts_with('>')
                || after.starts_with('/')
                || after.starts_with(char::is_whitespace))
            {
                from = a + open_prefix.len();
                continue;
            }
            let Some(gt) = out[a..].find('>') else {
                out.truncate(a); // opener without a closing `>` → truncated mid-tag
                break;
            };
            let tag_end = a + gt + 1;
            match out[tag_end..].find(close) {
                Some(crel) => {
                    let close_end = tag_end + crel + close.len();
                    out.replace_range(a..close_end, "");
                    from = a; // keep scanning from where the block was removed (more blocks may follow)
                }
                None => {
                    out.truncate(a); // unclosed → truncated mid-reasoning
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        answer_question, build_client, build_request_body, push_capped, should_emit_reasoning,
        stream_answer, strip_think, LlmConfig,
    };
    use serde_json::{json, Value};

    fn config(endpoint: &str) -> LlmConfig {
        LlmConfig {
            endpoint: endpoint.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            max_tokens: 100,
            enable_tools: false,
            max_tool_iterations: 0,
        }
    }

    #[test]
    fn generic_openai_body_omits_nim_fields() {
        let cfg = config("https://api.openai.com/v1/chat/completions");
        let body = build_request_body(
            &cfg,
            &[json!({"role": "user", "content": "hello"})],
            true,
            false,
        );
        assert_eq!(body["stream"], true);
        assert!(body.get("top_k").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert!(body.get("tools").is_none());

        let tool_body = build_request_body(&cfg, &[], false, true);
        assert!(tool_body.get("top_k").is_none());
        assert!(tool_body.get("chat_template_kwargs").is_none());
        assert!(tool_body["tools"].is_array());
    }

    #[test]
    fn nvidia_nim_body_adds_vendor_fields() {
        let body = build_request_body(
            &config("https://integrate.api.nvidia.com/v1/chat/completions"),
            &[],
            false,
            true,
        );
        assert_eq!(body["stream"], false);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["chat_template_kwargs"]["thinking_mode"], "enabled");
        assert!(body["tools"].is_array());
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn request_builder_keeps_stream_and_tools_consistent() {
        let cfg = config("https://integrate.api.nvidia.com/v1/chat/completions");
        let stream = build_request_body(&cfg, &[], true, false);
        let tools = build_request_body(&cfg, &[], false, true);
        assert_eq!(stream["stream"], true);
        assert_eq!(tools["stream"], false);
        assert_eq!(stream["top_k"], tools["top_k"]);
        assert_eq!(
            stream["chat_template_kwargs"],
            tools["chat_template_kwargs"]
        );
        assert!(stream.get("tools").is_none());
        assert!(tools["tools"].is_array());
        assert_eq!(tools["tool_choice"], "auto");
    }

    extern "C" fn noop_cb(_: *const u8, _: usize) {}

    /// A one-shot HTTP server that writes `body` after reading the request. `keep_open` keeps the
    /// connection alive after the body (a slow server) instead of closing it.
    async fn raw_server(body: Vec<u8>, keep_open: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            if keep_open {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn sse_done_terminates_the_outer_read_loop() {
        // A slow-but-compliant server sends the final `[DONE]` then KEEPS the connection open.
        // Breaking only the line loop would hang on the next chunk(); `[DONE]` must end the whole
        // read so the answer is returned without waiting for the server to close.
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"B\"}}]}\n\ndata: [DONE]\n";
        let base = raw_server(stream.as_bytes().to_vec(), true).await;
        let cfg = config(&format!("{base}/v1/chat/completions"));
        let llm_client = build_client().unwrap();
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            answer_question(
                &llm_client,
                &reqwest::Client::new(),
                &cfg,
                &[json!({"role": "user", "content": "q"})],
                noop_cb,
                "t",
                "a",
                "s",
                None,
            ),
        )
        .await
        .expect("[DONE] must end the read without waiting for the connection to close")
        .expect("a complete stream yields an answer");
        assert_eq!(answer, "B");
    }

    #[tokio::test]
    async fn stream_read_idle_failure_returns_none() {
        // One chunk, then silence: the read-idle timeout must end the read with `None` (the subject
        // is re-asked) — never hang forever, and never submit a partial stream as the answer.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(10)).await; // then silence
        });
        let client = build_client().unwrap();
        let cfg = config(&format!("http://{address}/v1/chat/completions"));
        let full = vec![
            json!({"role": "system", "content": crate::llm::SYSTEM_PROMPT}),
            json!({"role": "user", "content": "q"}),
        ];
        let answer = stream_answer(
            &client,
            &cfg,
            full,
            noop_cb,
            "t",
            "a",
            "s",
            std::time::Duration::from_millis(150),
        )
        .await;
        assert_eq!(
            answer, None,
            "read-idle failure must return None, not partial content"
        );
    }

    #[tokio::test]
    async fn llm_client_never_sends_school_cookies() {
        // Same hostname, different port: the school client's jar holds a host-only session cookie
        // (set by the school server). The cookie-less LLM client must not carry it to the model
        // endpoint — the two clients share nothing.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let school_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let school_addr = school_listener.local_addr().unwrap();
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = school_listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = "HTTP/1.1 200 OK\r\nSet-Cookie: session=secret; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        tokio::spawn(async move {
            let (mut stream, _) = llm_listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let request_text = String::from_utf8_lossy(&request);
            let _ = tx.send(request_text.to_ascii_lowercase().contains("cookie:"));
            let sse_body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"B\"}}]}\n\ndata: [DONE]\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let school = reqwest::Client::builder()
            .cookie_provider(std::sync::Arc::new(
                reqwest_cookie_store::CookieStoreMutex::new(cookie_store::CookieStore::default()),
            ))
            .build()
            .unwrap();
        school
            .get(format!("http://{school_addr}/"))
            .send()
            .await
            .unwrap(); // jar now holds the cookie

        let llm_client = build_client().unwrap();
        let cfg = config(&format!("http://{llm_addr}/v1/chat/completions"));
        let answer = answer_question(
            &llm_client,
            &school,
            &cfg,
            &[json!({"role": "user", "content": "q"})],
            noop_cb,
            "t",
            "a",
            "s",
            None,
        )
        .await;
        assert_eq!(answer.as_deref(), Some("B"));
        assert!(
            !rx.await.unwrap(),
            "the cookie-less LLM client must not carry school cookies"
        );
    }

    #[test]
    fn strip_think_closed_unclosed_and_mm() {
        assert_eq!(strip_think("<think>reasoning</think>B"), "B");
        assert_eq!(strip_think("<mm:think>r</mm:think>A,C"), "A,C");
        assert_eq!(strip_think("prefix <mm:think>truncated forever"), "prefix "); // unclosed → dropped
        assert_eq!(strip_think("<think>only reasoning, cut off"), ""); // unclosed, nothing before
        assert_eq!(strip_think("plain answer"), "plain answer");
        // attributed opening tag (minimax emits a signature attr) → still stripped.
        assert_eq!(strip_think("<think signature=\"abc\">r</think>B"), "B");
        // multiple closed blocks → all removed.
        assert_eq!(strip_think("<think>a</think>X<think>b</think>Y"), "XY");
        // `<think`-prefixed word that isn't a tag is left alone.
        assert_eq!(
            strip_think("the answer is <thinking>"),
            "the answer is <thinking>"
        );
    }

    #[test]
    fn push_capped_enforces_the_boundary_without_allocating_the_cap() {
        // The stream caps are tested through this pure helper at a tiny cap — no 8 MiB allocation.
        let mut acc = String::from("abc");
        assert!(push_capped(&mut acc, "def", 6)); // exactly at the cap → accepted
        assert_eq!(acc, "abcdef");
        assert!(!push_capped(&mut acc, "x", 6)); // one over → rejected, nothing appended
        assert_eq!(acc, "abcdef");
        assert!(push_capped(&mut acc, "", 6)); // an empty append never fails
        assert_eq!(acc, "abcdef");
        let mut tiny = String::new();
        assert!(!push_capped(&mut tiny, "hello", 4));
        assert_eq!(tiny, "");
    }

    #[test]
    fn reasoning_emit_policy_batches_on_size_or_interval() {
        use std::time::Duration;
        let interval = Duration::from_millis(100);
        // Size threshold reached → emit even before the interval elapses.
        assert!(should_emit_reasoning(1024, Duration::ZERO, 1024, interval));
        assert!(should_emit_reasoning(1500, Duration::ZERO, 1024, interval));
        // Interval elapsed → emit even for a small pending batch.
        assert!(should_emit_reasoning(10, interval, 1024, interval));
        // Neither → hold the batch.
        assert!(!should_emit_reasoning(1023, Duration::ZERO, 1024, interval));
        assert!(!should_emit_reasoning(
            10,
            Duration::from_millis(99),
            1024,
            interval
        ));
    }

    #[tokio::test]
    async fn sse_lines_spanning_chunks_are_assembled_before_parsing() {
        // A server may split one SSE line across chunks with no newline: the buffer must assemble
        // the line across chunk boundaries (its cap is the pure `push_capped` boundary above).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            // One logical event split into three newline-less chunks.
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"B")
                .await
                .unwrap();
            stream.write_all(b"\"}}]}\n").await.unwrap();
            stream.write_all(b"\ndata: [DONE]\n").await.unwrap();
        });
        let client = build_client().unwrap();
        let cfg = config(&format!("http://{address}/v1/chat/completions"));
        let full = vec![
            json!({"role": "system", "content": crate::llm::SYSTEM_PROMPT}),
            json!({"role": "user", "content": "q"}),
        ];
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream_answer(
                &client,
                &cfg,
                full,
                noop_cb,
                "t",
                "a",
                "s",
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("the stream must complete")
        .expect("the assembled line must parse and yield an answer");
        assert_eq!(answer, "B");
    }

    // Collects `ReasoningChunk` texts in emit order. `thread_local!` — `tokio::test` runs on a
    // current-thread runtime, so the stream's emits and the test's assertions share this thread;
    // separate locals keep the two tests fully isolated with zero locking.
    thread_local! {
        static RA_FLUSH: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        static RA_VOLUME: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    extern "C" fn collect_flush(ptr: *const u8, len: usize) {
        let s = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if v["event"] == "ReasoningChunk" {
                if let Some(t) = v["text"].as_str() {
                    RA_FLUSH.with(|c| c.borrow_mut().push(t.to_string()));
                }
            }
        }
    }
    extern "C" fn collect_volume(ptr: *const u8, len: usize) {
        let s = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if v["event"] == "ReasoningChunk" {
                if let Some(t) = v["text"].as_str() {
                    RA_VOLUME.with(|c| c.borrow_mut().push(t.to_string()));
                }
            }
        }
    }

    #[tokio::test]
    async fn reasoning_deltas_are_batched_and_flushed_in_order() {
        // 2 × 200-byte reasoning deltas stay below the 1 KiB emit threshold: nothing is emitted
        // during the stream, and the [DONE] flush emits ONE event with both texts in stream order.
        RA_FLUSH.with(|c| c.borrow_mut().clear());
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let r1 = "x".repeat(200);
        let r2 = "y".repeat(200);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let payload = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{r1}\"}}}}]}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{r2}\"}}}}]}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"content\":\"B\"}}}}]}}\n\
             \ndata: [DONE]\n"
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(payload.as_bytes()).await.unwrap();
        });
        let client = build_client().unwrap();
        let cfg = config(&format!("http://{address}/v1/chat/completions"));
        let full = vec![
            json!({"role": "system", "content": crate::llm::SYSTEM_PROMPT}),
            json!({"role": "user", "content": "q"}),
        ];
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream_answer(
                &client,
                &cfg,
                full,
                collect_flush,
                "t",
                "a",
                "s",
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("stream must complete")
        .expect("answer must parse");
        assert_eq!(answer, "B");
        let emits = RA_FLUSH.with(|c| c.borrow().clone());
        // Either ONE [DONE]-flush batch (fast machine) or at most TWO if the 100 ms interval split
        // the deltas (slow machine) — the hard contract is stream order and the final flush.
        assert!(
            !emits.is_empty() && emits.len() <= 2,
            "deltas must be coalesced, got {emits:?}"
        );
        assert_eq!(
            emits.concat(),
            format!("{r1}{r2}"),
            "reasoning text order must be preserved"
        );
        assert!(
            emits.last().unwrap().ends_with(&r2),
            "the final flush must carry the last reasoning text"
        );
    }

    #[tokio::test]
    async fn reasoning_volume_over_the_batch_size_emits_mid_stream_in_order() {
        // 3 × 500-byte deltas cross the 1 KiB threshold mid-stream: batches may split, but the
        // concatenated emit order must equal the stream order (never reordered or dropped).
        RA_VOLUME.with(|c| c.borrow_mut().clear());
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let parts = ["a".repeat(500), "b".repeat(500), "c".repeat(500)];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let payload = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{}\"}}}}]}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{}\"}}}}]}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{}\"}}}}]}}\n\
             data: {{\"choices\":[{{\"delta\":{{\"content\":\"A\"}}}}]}}\n\
             \ndata: [DONE]\n",
            parts[0], parts[1], parts[2]
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(payload.as_bytes()).await.unwrap();
        });
        let client = build_client().unwrap();
        let cfg = config(&format!("http://{address}/v1/chat/completions"));
        let full = vec![
            json!({"role": "system", "content": crate::llm::SYSTEM_PROMPT}),
            json!({"role": "user", "content": "q"}),
        ];
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream_answer(
                &client,
                &cfg,
                full,
                collect_volume,
                "t",
                "a",
                "s",
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("stream must complete")
        .expect("answer must parse");
        assert_eq!(answer, "A");
        let emits = RA_VOLUME.with(|c| c.borrow().clone());
        assert!(
            !emits.is_empty(),
            "the batch must be flushed before the answer"
        );
        assert_eq!(
            emits.concat(),
            format!("{}{}{}", parts[0], parts[1], parts[2]),
            "reasoning must be emitted in stream order, never dropped"
        );
    }
}
