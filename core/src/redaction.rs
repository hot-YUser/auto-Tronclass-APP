//! The single audited event chokepoint (docs 10 `redaction`; docs 90 §4). EVERY event crosses the
//! FFI seam through `emit` here, which first runs one recursive redaction traversal so a secret can
//! never reach a log / event / export. "Will it leak a secret?" is auditable in exactly one place.
//!
//! Redaction runs only on the **display/serialization** path — NEVER on the vault/config file-write
//! path (that would corrupt the user's real key into `[redacted]`; docs 90 §4). Also holds the global
//! logging level so `debug` lines are dropped unless the user opted into debug logging.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU8, Ordering};

pub type EventCb = extern "C" fn(*const u8, usize);

/// Object keys whose values are secrets and must never be serialized. Case-insensitive. Kept tight
/// on purpose — `key`/`session` are deliberately excluded so legitimate id fields are not clobbered;
/// free-text leak vectors (error messages) are guarded at-source, not here.
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "master_password",
    "cookies",
    "cookies_json",
    "api_key",
    "llm_key",
    "authorization",
    "secret",
];

const REDACTED: &str = "[redacted]";

// 0 = normal, 1 = debug.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(0);

/// Set the global log level from Settings (`normal` | `debug`). Called on Init and UpdateConfig.
pub fn set_level(level: &str) {
    LOG_LEVEL.store(u8::from(level.eq_ignore_ascii_case("debug")), Ordering::Relaxed);
}

fn is_debug() -> bool {
    LOG_LEVEL.load(Ordering::Relaxed) == 1
}

/// Recursively replace any value under a sensitive key with `[redacted]`. Descends objects & arrays.
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if SENSITIVE_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                    *val = Value::String(REDACTED.to_string());
                } else {
                    redact(val);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact),
        _ => {}
    }
}

/// The one place the raw event callback is invoked: clone → redact → serialize → cross the seam.
pub fn emit(cb: EventCb, v: &Value) {
    let mut owned = v.clone();
    redact(&mut owned);
    let s = owned.to_string();
    (cb)(s.as_ptr(), s.len());
}

/// Leveled log line (docs 20 `LogLine` = already-redacted). `debug` lines are dropped unless the
/// current level is debug; everything goes through `emit` so it is redacted like any other event.
pub fn log_line(cb: EventCb, level: &str, text: &str) {
    if level.eq_ignore_ascii_case("debug") && !is_debug() {
        return;
    }
    emit(cb, &json!({ "id": null, "event": "LogLine", "level": level, "text": text }));
}
