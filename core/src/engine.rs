//! The core handle: owns the tokio runtime, the event callback, and a long-lived
//! heartbeat task (a stand-in for the real monitor loop). Commands spawn onto the
//! runtime and never block the caller; every result comes back through the callback.

use crate::protocol::Command;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::runtime::Runtime;

/// The event callback into managed code. Receives UTF-8 JSON bytes valid only for
/// the duration of the call — the C# side copies synchronously and returns.
pub type EventCb = extern "C" fn(*const u8, usize);

pub struct Core {
    rt: Runtime,
    cb: EventCb,
}

/// Push one event through the callback. `s` outlives the synchronous call, then drops.
fn emit(cb: EventCb, v: &Value) {
    let s = v.to_string();
    (cb)(s.as_ptr(), s.len());
}

pub fn init(cb: EventCb) -> Box<Core> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // Heartbeat = the reverse channel + process-alive proof. A truly unsolicited event,
    // pushed forever from a runtime worker thread, independent of any command.
    rt.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut n: u64 = 0;
        loop {
            ticker.tick().await;
            n += 1;
            emit(cb, &json!({ "id": null, "event": "Tick", "n": n }));
        }
    });

    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
    Box::new(Core { rt, cb })
}

pub fn send(core: &Core, json_bytes: &[u8]) {
    let cb = core.cb;
    let cmd: Command = match serde_json::from_slice(json_bytes) {
        Ok(c) => c,
        Err(e) => {
            // Errors never silent (docs 90 §5): a bad command is reported, not swallowed.
            emit(
                cb,
                &json!({ "id": null, "event": "Error", "severity": "error",
                         "code": "bad_command", "message": e.to_string() }),
            );
            return;
        }
    };

    match cmd {
        Command::Login { id, base_url, username, password } => {
            core.rt.spawn(async move {
                emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "logging_in" }));
                // base_url is safe to log; credentials never enter an event (docs 90 §4).
                emit(cb, &json!({ "id": null, "event": "LogLine", "level": "info",
                                  "text": format!("login → {base_url}") }));

                match crate::login::login(&base_url, &username, &password).await {
                    Ok(detail) => {
                        emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
                        emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": true, "detail": detail }));
                    }
                    Err(e) => {
                        emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "login_failed" }));
                        emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                         "code": "login_failed", "message": e.clone() }));
                        // LLM/login failure → report, never fake success (docs 90 §5).
                        emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": e }));
                    }
                }
            });
        }
    }
}
