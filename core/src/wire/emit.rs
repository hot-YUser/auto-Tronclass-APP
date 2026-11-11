//! The one place an event leaves the core. Everything passes through `send`, which redacts before
//! it serializes, so "can this leak a secret?" has a single answer to audit.
//!
//! Redaction is for this display path only. The vault and config writers never go through it: they
//! must store the real value.

use super::Event;
use serde::Serialize;
use serde_json::Value;

/// The host's event sink. The bytes are UTF-8 JSON, valid only for the duration of the call.
pub type EventCb = extern "C" fn(*const u8, usize);

/// Keys whose values are secrets, compared case-insensitively. The list is deliberately tight:
/// `key` and `session` are absent so ordinary id fields survive.
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "master_password",
    "cookies",
    "cookies_json",
    "api_key",
    "llm_key",
    "qr_remote_key",
    "device_key_b64",
    "authorization",
    "secret",
];

const REDACTED: &str = "[redacted]";

/// An unsolicited event.
pub fn emit<'a>(cb: EventCb, event: impl Into<Event<'a>>) {
    send(cb, None, &event.into());
}

/// The event that completes command `id`.
pub fn respond<'a>(cb: EventCb, id: u64, event: impl Into<Event<'a>>) {
    send(cb, Some(id), &event.into());
}

#[derive(Serialize)]
struct Envelope<'a> {
    id: Option<u64>,
    #[serde(flatten)]
    event: &'a Event<'a>,
}

fn send(cb: EventCb, id: Option<u64>, event: &Event<'_>) {
    // Events are plain data: serializing one cannot fail.
    let Ok(mut value) = serde_json::to_value(Envelope { id, event }) else {
        return;
    };
    redact(&mut value);
    let json = value.to_string();
    cb(json.as_ptr(), json.len());
}

/// Replace every value under a sensitive key with `[redacted]`, at any depth.
fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if SENSITIVE_KEYS
                    .iter()
                    .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                {
                    *value = Value::String(REDACTED.to_string());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Problem;
    use serde_json::json;
    use std::sync::Mutex;

    static SINK: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    /// The sink is shared, so the tests that read it take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    extern "C" fn sink(ptr: *const u8, len: usize) {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        SINK.lock()
            .unwrap()
            .push(serde_json::from_slice(bytes).unwrap());
    }

    #[test]
    fn events_carry_their_tag_and_a_null_or_command_id() {
        let _serial = SERIAL.lock().unwrap();
        SINK.lock().unwrap().clear();
        emit(sink, Problem::error("sign_failed", "no").activity("token"));
        respond(
            sink,
            7,
            Event::Reply {
                ok: true,
                error: None,
                data: Some(json!({ "account_id": "a" })),
            },
        );
        respond(sink, 8, Event::reply(Err("bad".into())));
        assert_eq!(
            *SINK.lock().unwrap(),
            [
                json!({ "id": null, "event": "Error", "severity": "error", "code": "sign_failed",
                        "message": "no", "activity_token": "token" }),
                json!({ "id": 7, "event": "Reply", "ok": true, "error": null,
                        "data": { "account_id": "a" } }),
                json!({ "id": 8, "event": "Reply", "ok": false, "error": "bad" }),
            ]
        );
    }

    #[test]
    fn reply_data_is_redacted_on_the_way_out() {
        let _serial = SERIAL.lock().unwrap();
        SINK.lock().unwrap().clear();
        respond(
            sink,
            1,
            Event::Reply {
                ok: true,
                error: None,
                data: Some(json!({ "nested": [{ "Password": "hunter2" }] })),
            },
        );
        assert_eq!(
            SINK.lock().unwrap()[0]["data"]["nested"][0]["Password"],
            REDACTED
        );
    }

    #[test]
    fn redaction_reaches_nested_objects_and_arrays_and_spares_ids() {
        let mut value = json!({
            "password": "hunter2",
            "cookies": "session=abc",
            "nested": { "api_key": "sk-abc123", "note": "ok" },
            "list": [{ "master_password": "root-pw" }, { "authorization": "Bearer xyz" }],
            "QR_REMOTE_KEY": "qr-secret",
            "account_id": "keep-me"
        });
        redact(&mut value);
        let text = value.to_string();
        for secret in [
            "hunter2",
            "session=abc",
            "sk-abc123",
            "root-pw",
            "Bearer xyz",
            "qr-secret",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(value["account_id"], "keep-me");
        assert_eq!(value["nested"]["note"], "ok");
    }
}
