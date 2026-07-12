//! R1 tests: the number-answer response classifier (pure) and, over the FFI against the real-contract
//! fake, the brute-force success-flag path, the fatal-abort path, and the my_present recheck (confirms
//! the caller when the class is NOT full — proving recheck matches the caller's own `user_no`, not any
//! entry / whole class). The e2e tests share one event sink → serialized via `SEQ`.

use crate::config::new_id;
use crate::fake;
use crate::rollcall::{classify_response, CodeResult};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ===================== pure classifier =====================

#[test]
fn classify_response_table() {
    use CodeResult::*;
    // Fatal: auth lost / redirect / a 200 that is really the login page.
    assert_eq!(classify_response(401, ""), Fatal);
    assert_eq!(classify_response(403, ""), Fatal);
    assert_eq!(classify_response(302, ""), Fatal);
    assert_eq!(classify_response(200, "<html><form>login</form></html>"), Fatal);
    // Transient: throttled / server hiccup.
    assert_eq!(classify_response(429, ""), Transient);
    assert_eq!(classify_response(408, ""), Transient);
    assert_eq!(classify_response(500, ""), Transient);
    assert_eq!(classify_response(503, ""), Transient);
    // Wrong: this code rejected.
    assert_eq!(classify_response(400, r#"{"success":false}"#), Wrong);
    assert_eq!(classify_response(409, ""), Wrong);
    assert_eq!(classify_response(422, ""), Wrong);
    assert_eq!(classify_response(200, r#"{"success":false}"#), Wrong);
    assert_eq!(classify_response(200, r#"{"message":"wrong number code"}"#), Wrong);
    // Success: a 2xx with a success flag — OR any 2xx without a wrong/auth marker (v1 contract).
    assert_eq!(classify_response(200, r#"{"success":true}"#), Success);
    assert_eq!(classify_response(200, r#"{"is_success":true}"#), Success);
    assert_eq!(classify_response(200, r#"{"status":"success"}"#), Success);
    // The REAL live accept body (2026-07): `{"id":…,"status":"on_call"}` — no success bool. v1 defaults a
    // bare 2xx to Success; the old v2 default of Wrong silently rejected every real number sign.
    assert_eq!(classify_response(200, r#"{"id":925957,"status":"on_call"}"#), Success);
    assert_eq!(classify_response(200, "just text no flag"), Success);
}

// ===================== e2e (serialized via SEQ) =====================

static SEQ: Mutex<()> = Mutex::new(());
static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn collect(ptr: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(ptr, len) };
    events().lock().unwrap().push(String::from_utf8_lossy(b).into_owned());
}
fn snapshot() -> Vec<Value> {
    events().lock().unwrap().iter().filter_map(|s| serde_json::from_str(s).ok()).collect()
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
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if snapshot().iter().any(&pred) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}
fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id
}
fn signed(rollcall_id: &str, account: &str) -> impl Fn(&Value) -> bool {
    let (r, a) = (rollcall_id.to_string(), account.to_string());
    move |v| v["event"] == "SignedIn" && v["rollcall_id"] == r && v["account_id"].as_str() == Some(&a)
}
fn send(h: *mut std::ffi::c_void, json: &str) {
    unsafe { crate::core_send(h, json.as_ptr(), json.len()) };
}
fn account_id(label: &str) -> Option<String> {
    for ev in snapshot().iter().rev() {
        if ev["event"] == "Accounts" {
            if let Some(a) = ev["accounts"].as_array()?.iter().find(|a| a["label"] == label) {
                return a["id"].as_str().map(str::to_string);
            }
        }
    }
    None
}
fn start_fake() -> String {
    let (ptx, prx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (port, listener) = fake::bind_ephemeral().await;
            ptx.send(port).unwrap();
            fake::serve(listener).await;
        });
    });
    format!("http://127.0.0.1:{}", prx.recv().unwrap())
}
fn post(base_url: &str, path: &str, body: &str) {
    let mut s = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
}
struct Harness {
    h: *mut std::ffi::c_void,
    id: u64,
}
impl Harness {
    fn new() -> Harness {
        events().lock().unwrap().clear();
        Harness { h: crate::core_init(collect), id: 0 }
    }
    fn next(&mut self) -> u64 {
        self.id += 1;
        self.id
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        unsafe { crate::core_free(self.h) };
    }
}
fn data_dir(tag: &str) -> String {
    std::env::temp_dir().join(format!("tron-r1-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/")
}

/// Boot one account, monitoring, online. Returns (harness, base, account_id).
fn boot(tag: &str) -> (Harness, String, String) {
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir(tag);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2}}}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let dave = account_id("dave").unwrap();
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    wait_for(reply_ok(i), 15);
    wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10);
    (hz, base, dave)
}

#[test]
fn number_brute_finds_code_via_success_flag() {
    let _g = SEQ.lock().unwrap();
    let (hz, base, dave) = boot("brute");
    // hide_code → the shared code is unreadable → brute-force; code 0007 lands in the first batch of 100.
    post(&base, "/_test/open_rollcall", r#"{"id":"BRUTE","kind":"number","number_code":"0007","attendance_rate":100,"hide_code":true}"#);
    assert!(wait_for(signed("BRUTE", &dave), 20).is_some(), "brute finds the code via the success flag");
    drop(hz);
}

#[test]
fn number_fatal_aborts_the_round() {
    let _g = SEQ.lock().unwrap();
    let (hz, base, dave) = boot("fatal");
    // A 403 on the answer endpoint (session invalid) triggers re-login + re-sign (R4.1 #2); a PERMANENT
    // 403 re-logins fine yet keeps failing → after MAX_RESIGN bounded retries it surfaces a sign_failed.
    post(&base, "/_test/open_rollcall", r#"{"id":"FATAL","kind":"number","number_code":"1234","attendance_rate":100,"hide_code":true,"number_fatal":true}"#);
    assert!(
        wait_for(|v| v["event"] == "Error" && v["code"] == "sign_failed", 15).is_some(),
        "fatal response surfaces a sign_failed Error after bounded re-login retries"
    );
    assert!(none_for(signed("FATAL", &dave), 2), "fatal → never reported as signed");
    drop(hz);
}

#[test]
fn my_present_confirms_when_class_not_full() {
    let _g = SEQ.lock().unwrap();
    let (hz, base, dave) = boot("myp");
    // attendance 40% → class NOT full (present≠total) and top-level status not fine → the ONLY way
    // recheck can confirm is the caller's own user_no entry (my_present). SignedIn proves it works.
    post(&base, "/_test/open_rollcall", r#"{"id":"MYP","kind":"self_registration","attendance_rate":40}"#);
    assert!(wait_for(signed("MYP", &dave), 15).is_some(), "recheck confirms the caller via my_present, not whole-class");
    drop(hz);
}
