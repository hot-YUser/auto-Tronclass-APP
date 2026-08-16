use crate::supervisor::{format_rfc3339_utc, now_epoch_seconds};
use serde_json::{json, Value};
use std::ffi::c_void;

pub fn latest_monitoring_snapshot(events: &[Value]) -> Option<&Value> {
    events
        .iter()
        .rev()
        .find(|event| event["event"] == "MonitoringSnapshot")
        .map(|event| &event["snapshot"])
}

pub fn account_id(events: &[Value], label: &str) -> Option<String> {
    latest_monitoring_snapshot(events)?["accounts"]
        .as_array()?
        .iter()
        .find(|account| account["label"] == label)?["account_id"]
        .as_str()
        .map(str::to_string)
}

pub fn event_has_account(event: &Value, label: &str) -> bool {
    event["event"] == "MonitoringSnapshot"
        && event["snapshot"]["accounts"]
            .as_array()
            .is_some_and(|accounts| accounts.iter().any(|account| account["label"] == label))
}

pub fn event_account_login_state(event: &Value, account_id: &str, state: &str) -> bool {
    event["event"] == "MonitoringSnapshot"
        && event["snapshot"]["accounts"]
            .as_array()
            .and_then(|accounts| {
                accounts
                    .iter()
                    .find(|account| account["account_id"] == account_id)
            })
            .is_some_and(|account| account["login_state"] == state)
}

pub fn apply_clock_command(id: u64, snapshot: &Value) -> String {
    let targets = snapshot["targets"]
        .as_array()
        .expect("snapshot targets")
        .iter()
        .map(|target| {
            json!({
                "target": target["target"].clone(),
                "is_open": false,
                "window_key": null,
                "current_window_start_utc": null,
                "next_boundary_utc": null,
                "next_is_open": null,
                "clock_error": null,
            })
        })
        .collect::<Vec<_>>();
    let clock_revision = snapshot["clock_revision"].as_u64().unwrap_or(0) + 1;
    json!({
        "id": id,
        "cmd": "ApplyScheduleClock",
        "clock_revision": clock_revision,
        "config_revision": snapshot["config_revision"],
        "schedule_revision": snapshot["schedule_revision"],
        "evaluated_at_utc": format_rfc3339_utc(now_epoch_seconds()),
        "targets": targets,
    })
    .to_string()
}

pub fn start_account_command(id: u64, account_id: &str) -> String {
    json!({
        "id": id,
        "cmd": "StartTarget",
        "target": { "kind": "account", "account_id": account_id },
    })
    .to_string()
}

pub fn activate_account(
    handle: *mut c_void,
    clock_id: u64,
    start_id: u64,
    events: &[Value],
    account_id: &str,
) {
    let snapshot = latest_monitoring_snapshot(events)
        .expect("MonitoringSnapshot before target activation")
        .clone();
    send_raw(handle, &apply_clock_command(clock_id, &snapshot));
    send_raw(handle, &start_account_command(start_id, account_id));
}

fn send_raw(handle: *mut c_void, command: &str) {
    unsafe { crate::core_send(handle, command.as_ptr(), command.len()) };
}

pub fn stop_all_command(id: u64) -> String {
    json!({ "id": id, "cmd": "StopAllMonitoring" }).to_string()
}
