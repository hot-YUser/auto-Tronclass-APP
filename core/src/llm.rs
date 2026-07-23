//! LLM client (docs 31). Default NVIDIA NIM + a `minimax` reasoning model. Reasoning is always on
//! (`chat_template_kwargs`), an explicit `max_tokens` is always sent (reasoning models return empty
//! `choices` without it), reasoning is streamed as `ReasoningChunk` events, and only the **clean
//! final answer** is returned. The API key comes from the vault and never enters a log.

use reqwest::Client;
use serde_json::{json, Value};

pub type EventCb = extern "C" fn(*const u8, usize);

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

/// R5 tool-calling context: the authed account's `base_url` + the quiz's `course_id`, so the executor
/// can fetch that course's materials on the SAME client used for the LLM call.
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

/// Answer one question given the non-system `messages` (the user question, plus assistant+correction
/// turns on a re-ask). Prepends the system prompt. Returns the clean answer text, or `None` on
/// failure/empty (caller must then skip the subject — never blank). With `tools` set, runs the
/// non-streaming tool-calling loop (course-material lookup); otherwise the streaming path (R3b).
pub async fn answer_question(
    client: &Client,
    cfg: &LlmConfig,
    messages: &[Value],
    cb: EventCb,
    quiz_id: &str,
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
        Some(ctx) => tool_loop(client, cfg, full, cb, quiz_id, subject_id, ctx).await,
        None => stream_answer(client, cfg, full, cb, quiz_id, subject_id).await,
    }
}

/// The R3b streaming path (no tools): SSE, reasoning streamed delta-by-delta as `ReasoningChunk`.
async fn stream_answer(
    client: &Client,
    cfg: &LlmConfig,
    full: Vec<Value>,
    cb: EventCb,
    quiz_id: &str,
    subject_id: &str,
) -> Option<String> {
    let mut body = json!({
        "model": cfg.model,
        "messages": full,
        "temperature": 0.6,
        "top_p": 0.95,
        "max_tokens": resolve_max_tokens(cfg.max_tokens), // else HTTP 200 with empty choices
        "stream": true,
        // reasoning on — the VALUE is the string "enabled" (a wrong key → HTTP 200 + empty choices).
        "chat_template_kwargs": { "thinking_mode": "enabled" }
    });
    if TOP_K > 0 {
        body["top_k"] = json!(TOP_K);
    }

    let mut resp = client
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
    while let Ok(Some(bytes)) = resp.chunk().await {
        pending.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(nl) = pending.find('\n') {
            let line: String = pending.drain(..=nl).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            let delta = &v["choices"][0]["delta"];
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !r.is_empty() {
                    emit(cb, &json!({ "id": null, "event": "ReasoningChunk",
                                      "quiz_id": quiz_id, "subject_id": subject_id, "text": r }));
                }
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                content.push_str(c);
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
/// the last clean content.
#[allow(clippy::too_many_arguments)]
async fn tool_loop(
    client: &Client,
    cfg: &LlmConfig,
    mut messages: Vec<Value>,
    cb: EventCb,
    quiz_id: &str,
    subject_id: &str,
    ctx: &ToolCtx<'_>,
) -> Option<String> {
    let mut fallback = String::new();
    for _ in 0..ctx.max_iterations + 2 {
        let mut body = json!({
            "model": cfg.model,
            "messages": messages,
            "temperature": 0.6,
            "top_p": 0.95,
            "max_tokens": resolve_max_tokens(cfg.max_tokens),
            "stream": false,
            "chat_template_kwargs": { "thinking_mode": "enabled" },
            "tools": [crate::course_context::tool_spec()],
            "tool_choice": "auto"
        });
        if TOP_K > 0 {
            body["top_k"] = json!(TOP_K);
        }
        let resp = match client.post(&cfg.endpoint).bearer_auth(&cfg.api_key).json(&body).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return clean_or(&fallback),
        };
        let Ok(v) = resp.json::<Value>().await else { return clean_or(&fallback) };
        let msg = &v["choices"][0]["message"];

        if let Some(r) = msg.get("reasoning_content").and_then(Value::as_str) {
            if !r.is_empty() {
                emit(cb, &json!({ "id": null, "event": "ReasoningChunk",
                                  "quiz_id": quiz_id, "subject_id": subject_id, "text": r }));
            }
        }
        let content = strip_think(msg.get("content").and_then(Value::as_str).unwrap_or("")).trim().to_string();
        if !content.is_empty() {
            fallback = content.clone();
        }

        match msg.get("tool_calls").and_then(Value::as_array) {
            Some(calls) if !calls.is_empty() => {
                // Echo a clean assistant turn (role+content+tool_calls) then each tool result.
                messages.push(json!({ "role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null), "tool_calls": calls }));
                for call in calls {
                    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
                    let args = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
                    let result = if name == "search_course_materials" {
                        let query = serde_json::from_str::<Value>(args).ok()
                            .and_then(|a| a.get("query").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_default();
                        crate::course_context::search_course_materials(client, ctx.base_url, ctx.course_id, &query).await
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

/// The last clean content, or `None` if we never got any (caller then skips the subject).
fn clean_or(fallback: &str) -> Option<String> {
    (!fallback.is_empty()).then(|| fallback.to_string())
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
            if !(after.starts_with('>') || after.starts_with('/') || after.starts_with(char::is_whitespace)) {
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
    use super::strip_think;

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
        assert_eq!(strip_think("the answer is <thinking>"), "the answer is <thinking>");
    }
}
