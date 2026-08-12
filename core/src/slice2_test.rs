//! Slice-2 end-to-end (headless): multi-account concurrent monitoring, the merged activity model,
//! the four rollcall types + `on_call_fine`, the 15% gate, and the countdown/defer flow — all
//! driven over the real FFI against the stateful fake TronClass.

use crate::config::new_id;
use crate::fake;
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn collect(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    events()
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(bytes).into_owned());
}
fn snapshot() -> Vec<Value> {
    events()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}
fn wait_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(v) = snapshot().into_iter().find(&pred) {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
fn none_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> bool {
    std::thread::sleep(Duration::from_secs(secs));
    !snapshot().iter().any(pred)
}
fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id
}
fn send(handle: *mut std::ffi::c_void, json: &str) {
    unsafe { crate::core_send(handle, json.as_ptr(), json.len()) };
}
fn account_id(label: &str) -> Option<String> {
    for ev in snapshot().iter().rev() {
        if ev["event"] == "Accounts" {
            if let Some(list) = ev["accounts"].as_array() {
                if let Some(a) = list.iter().find(|a| a["label"] == label) {
                    return a["id"].as_str().map(str::to_string);
                }
            }
        }
    }
    None
}

fn start_fake() -> String {
    let (ptx, prx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let (port, listener) = fake::bind_ephemeral().await;
            ptx.send(port).unwrap();
            fake::serve(listener).await;
        });
    });
    format!("http://127.0.0.1:{}", prx.recv().unwrap())
}

/// Open a rollcall on a fake server via its dev `_test` control endpoint (raw HTTP, no reqwest).
fn open_rollcall(base_url: &str, body: &str) {
    let addr = base_url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let req = format!(
        "POST /_test/open_rollcall HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
}

/// Read the fake's count of number-code PUTs for a rollcall (dev `_test` endpoint).
fn number_attempts(base_url: &str, id: &str) -> u32 {
    let addr = base_url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let req =
        format!("GET /_test/number_attempts/{id} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    let body = buf.rsplit("\r\n\r\n").next().unwrap_or("");
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["attempts"].as_u64())
        .unwrap_or(0) as u32
}

fn signed(rollcall_id: &str, account: &str) -> impl Fn(&Value) -> bool {
    let account = account.to_string();
    let rollcall_id = rollcall_id.to_string();
    move |v| {
        v["event"] == "SignedIn"
            && v["rollcall_id"] == rollcall_id
            && v["account_id"].as_str() == Some(&account)
    }
}

#[test]
fn slice2_multi_account_monitoring_and_four_types() {
    let base_a = start_fake();
    let base_b = start_fake();
    let data_dir = std::env::temp_dir().join(format!("tron-slice2-{}", new_id()));
    let data_dir = data_dir.to_string_lossy().replace('\\', "/");

    let h = crate::core_init(Some(collect));
    let mut id = 0u64;
    let mut next = || {
        id += 1;
        id
    };

    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{data_dir}"}}"#),
    );
    assert!(wait_for(reply_ok(i), 10).is_some(), "Init");
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2}}}}"#),
    );
    wait_for(reply_ok(i), 5);
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#),
    );
    assert!(
        wait_for(reply_ok(i), 5).unwrap()["ok"] == true,
        "CreateVault"
    );

    // Four accounts: alice/bob on A (students), a teacher on A (for QR), carol on B.
    for (label, base, extra) in [
        ("alice", &base_a, ""),
        ("bob", &base_a, ""),
        ("teacher", &base_a, r#","is_teacher":true,"course_id":"C1""#),
        ("carol", &base_b, ""),
    ] {
        let i = next();
        send(
            h,
            &format!(
                r#"{{"id":{i},"cmd":"AddAccount","label":"{label}","school":"{base}","username":"{label}","password":"secret"{extra}}}"#
            ),
        );
        assert!(
            wait_for(reply_ok(i), 5).unwrap()["ok"] == true,
            "AddAccount {label}"
        );
    }
    let alice = account_id("alice").unwrap();
    let bob = account_id("bob").unwrap();
    let carol = account_id("carol").unwrap();

    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    assert!(wait_for(reply_ok(i), 15).is_some(), "StartMonitoring");
    // all four should authenticate
    assert!(wait_for(
        |v| v["event"] == "AccountStatus" && v["state"] == "online",
        10
    )
    .is_some());

    // --- number rollcall on A, visible to alice+bob → both sign, merged into one activity ---
    open_rollcall(
        &base_a,
        r#"{"id":"RC1","kind":"number","number_code":"4242","attendance_rate":100,"course":"Math"}"#,
    );
    assert!(
        wait_for(signed("RC1", &alice), 15).is_some(),
        "alice signs RC1 (number)"
    );
    assert!(
        wait_for(signed("RC1", &bob), 15).is_some(),
        "bob signs RC1 (number)"
    );
    // A RollcallDetected for RC1@A eventually lists BOTH participants — one merged activity.
    let merged = |v: &Value| {
        v["event"] == "RollcallDetected"
            && v["rollcall_id"] == "RC1"
            && v["base_url"] == base_a.as_str()
            && v["accounts"].as_array().is_some_and(|a| {
                a.iter().any(|x| x.as_str() == Some(alice.as_str()))
                    && a.iter().any(|x| x.as_str() == Some(bob.as_str()))
            })
    };
    let rc1_a = wait_for(merged, 10).expect("RC1 merged alice+bob into one activity");
    let rc1_a_token = rc1_a["activity_token"]
        .as_str()
        .expect("RC1@A activity token")
        .to_string();

    // --- same rollcall id on B → a DISTINCT activity (different base_url never merges) ---
    open_rollcall(
        &base_b,
        r#"{"id":"RC1","kind":"self_registration","attendance_rate":100}"#,
    );
    assert!(
        wait_for(signed("RC1", &carol), 15).is_some(),
        "carol signs RC1 on B"
    );
    let rc1_b = wait_for(
        |v| {
            v["event"] == "RollcallDetected"
                && v["rollcall_id"] == "RC1"
                && v["base_url"] == base_b.as_str()
        },
        5,
    )
    .expect("B's RC1 is its own activity, not merged with A's");
    let rc1_b_token = rc1_b["activity_token"]
        .as_str()
        .expect("RC1@B activity token");
    assert_ne!(
        rc1_a_token, rc1_b_token,
        "same external id on different sites must have distinct tokens"
    );

    // --- self_registration, radar (empty→pass), qr (teacher-assist) on A ---
    open_rollcall(
        &base_a,
        r#"{"id":"RC2","kind":"self_registration","attendance_rate":100}"#,
    );
    assert!(
        wait_for(signed("RC2", &alice), 15).is_some(),
        "self_registration signs"
    );
    open_rollcall(
        &base_a,
        r#"{"id":"RC3","kind":"radar","attendance_rate":100}"#,
    );
    assert!(
        wait_for(signed("RC3", &alice), 15).is_some(),
        "radar (empty) signs"
    );
    open_rollcall(
        &base_a,
        r#"{"id":"RC4","kind":"qrcode","attendance_rate":100}"#,
    );
    let qr = wait_for(signed("RC4", &alice), 20).expect("qr teacher-assist signs alice");
    assert!(
        qr["method"].as_str().unwrap_or("").contains("qr"),
        "signed via qr teacher-assist"
    );

    // --- 15% gate blocks a near-empty rollcall ---
    open_rollcall(
        &base_a,
        r#"{"id":"RC9","kind":"self_registration","attendance_rate":5}"#,
    );
    let rc9 = wait_for(
        |v| v["event"] == "RollcallDetected" && v["rollcall_id"] == "RC9",
        10,
    )
    .expect("RC9 detected");
    let rc9_token = rc9["activity_token"].as_str().expect("RC9 activity token");
    assert!(
        none_for(|v| v["event"] == "SignedIn" && v["rollcall_id"] == "RC9", 4),
        "below-15% rollcall must NOT be signed"
    );

    // --- below-gate held → SignNow override signs anyway (the reported「簽到率未達門檻時立即簽到沒反應」路徑）---
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"SignNow","activity_token":"{rc9_token}"}}"#),
    );
    assert_eq!(wait_for(reply_ok(i), 5).expect("SignNow reply")["ok"], true);
    assert!(
        wait_for(signed("RC9", &alice), 10).is_some(),
        "SignNow must override the gate and sign the held RC9"
    );

    // --- below-gate NUMBER: manual override must READ the roster code, NOT brute-force 0000–9999 ---
    // Root cause of「立即簽到沒反應」: the code-read only ran on the gate-PASS path, so a held number
    // rollcall had number_code=None → SignNow brute-forced (thousands of PUTs, rate-limits, no timely sign).
    open_rollcall(
        &base_a,
        r#"{"id":"RC8","kind":"number","number_code":"4242","attendance_rate":5}"#,
    );
    let rc8 = wait_for(
        |v| v["event"] == "RollcallDetected" && v["rollcall_id"] == "RC8",
        10,
    )
    .expect("RC8 detected");
    let rc8_token = rc8["activity_token"].as_str().expect("RC8 activity token");
    assert!(
        none_for(signed("RC8", &alice), 3),
        "below-gate number must not auto-sign"
    );
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"SignNow","activity_token":"{rc8_token}"}}"#),
    );
    assert_eq!(wait_for(reply_ok(i), 5).expect("SignNow reply")["ok"], true);
    assert!(
        wait_for(signed("RC8", &alice), 10).is_some(),
        "SignNow on the held number signs"
    );
    // Read-not-brute: one PUT per participant (alice/bob/teacher on base_a share the read code) ≈ a few,
    // NEVER the thousands a 0000–9999 brute-force would make against the real server.
    let attempts = number_attempts(&base_a, "RC8");
    assert!(
        attempts <= 5,
        "manual override READ the code ({attempts} PUTs ≈ participants), not brute-forced"
    );

    // --- defer → PendingSignIn → no auto-sign → SignNow → signs ---
    open_rollcall(
        &base_a,
        r#"{"id":"RC5","kind":"self_registration","attendance_rate":100}"#,
    );
    let rc5 = wait_for(
        |v| v["event"] == "RollcallDetected" && v["rollcall_id"] == "RC5",
        10,
    )
    .expect("RC5 detected");
    let rc5_token = rc5["activity_token"].as_str().expect("RC5 activity token");
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"DeferSignIn","activity_token":"{rc5_token}"}}"#),
    );
    assert_eq!(
        wait_for(reply_ok(i), 5).expect("DeferSignIn reply")["ok"],
        true
    );
    assert!(
        wait_for(
            |v| v["event"] == "PendingSignIn" && v["rollcall_id"] == "RC5",
            5
        )
        .is_some(),
        "defer → PendingSignIn"
    );
    assert!(
        none_for(signed("RC5", &alice), 3),
        "deferred RC5 must not auto-sign"
    );
    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"SignNow","activity_token":"{rc5_token}"}}"#),
    );
    assert_eq!(wait_for(reply_ok(i), 5).expect("SignNow reply")["ok"], true);
    assert!(
        wait_for(signed("RC5", &alice), 10).is_some(),
        "SignNow completes the deferred sign"
    );

    let i = next();
    send(
        h,
        &format!(r#"{{"id":{i},"cmd":"SignNow","activity_token":"missing"}}"#),
    );
    let missing = wait_for(reply_ok(i), 5).expect("unknown token reply");
    assert_eq!(missing["ok"], false);

    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"StopMonitoring"}}"#));
    wait_for(reply_ok(i), 5);
    unsafe { crate::core_free(h) };
}
