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

/// Answer one question. Streams reasoning as `ReasoningChunk`; returns the clean answer text, or
/// `None` on failure/empty (caller must then skip the subject — never submit blank, docs 31).
pub async fn answer_question(
    client: &Client,
    cfg: &LlmConfig,
    prompt: &str,
    cb: EventCb,
    quiz_id: &str,
    subject_id: &str,
) -> Option<String> {
    let body = json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.6,
        "max_tokens": resolve_max_tokens(cfg.max_tokens), // else HTTP 200 with empty choices
        "stream": true,
        "chat_template_kwargs": { "thinking": true }      // reasoning always on
    });

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

fn strip_think(s: &str) -> String {
    if let (Some(a), Some(b)) = (s.find("<think>"), s.find("</think>")) {
        if b > a {
            let mut out = String::new();
            out.push_str(&s[..a]);
            out.push_str(&s[b + "</think>".len()..]);
            return out;
        }
    }
    s.to_string()
}
