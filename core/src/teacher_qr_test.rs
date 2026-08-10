//! QR teacher-assist across two INDEPENDENT fake TronClass endpoints (teacher on one host, student on
//! another). Locks in the fixes ported from PR #1:
//!   * the student signs on ITS OWN endpoint (not the teacher's), and a cross-tenant teacher is a valid
//!     data source (the token is portable), so a teacher on `base_a` assists a rollcall on `base_b`;
//!   * a session lost mid-flight is recovered once on BOTH sides (teacher source + student sign);
//!   * an HTTP-OK submission still requires roster confirmation before `SignedIn`.

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
    events().lock().unwrap().push(String::from_utf8_lossy(bytes).into_owned());
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
fn signed(rollcall_id: &str, account: &str) -> impl Fn(&Value) -> bool {
    let (account, rollcall_id) = (account.to_string(), rollcall_id.to_string());
    move |v| v["event"] == "SignedIn" && v["rollcall_id"] == rollcall_id && v["account_id"].as_str() == Some(&account)
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

/// Fire-and-forget POST to a fake's dev `_test` control endpoint (raw HTTP, no reqwest).
fn post_test(base_url: &str, path: &str, body: &str) {
    let addr = base_url.trim_start_matches("http://");
    let Ok(mut s) = std::net::TcpStream::connect(addr) else { return };
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(req.as_bytes());
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
}

fn open_qr(base_url: &str, id: &str) {
    post_test(base_url, "/_test/open_rollcall", &format!(r#"{{"id":"{id}","kind":"qrcode","attendance_rate":100}}"#));
}

#[test]
fn teacher_qr_cross_endpoint_sign_and_session_recovery() {
    events().lock().unwrap().clear();
    let teacher_base = start_fake();
    let student_base = start_fake();
    let data_dir = std::env::temp_dir().join(format!("tron-teacherqr-{}", new_id()));
    let data_dir = data_dir.to_string_lossy().replace('\\', "/");

    let h = crate::core_init(collect);
    let mut id = 0u64;
    let mut cmd = |body: String| {
        id += 1;
        let this = id;
        send(h, &format!(r#"{{"id":{this},{body}}}"#));
        assert!(
            wait_for(move |v| v["event"] == "Reply" && v["id"] == this && v["ok"] == true, 15).is_some(),
            "command {this} ok; events={:?}",
            snapshot()
        );
    };

    cmd(format!(r#""cmd":"Init","data_dir":"{data_dir}""#));
    cmd(r#""cmd":"UpdateConfig","patch":{"countdown_secs":1,"poll_idle_secs":1}"#.to_string());
    cmd(r#""cmd":"CreateVault","master_password":"pw""#.to_string());
    // Teacher lives on teacher_base; the monitored student lives on a DIFFERENT host.
    cmd(format!(
        r#""cmd":"AddAccount","label":"teacher","school":"{teacher_base}","username":"teacher","password":"secret","is_teacher":true,"course_id":"C1""#
    ));
    cmd(format!(
        r#""cmd":"AddAccount","label":"student","school":"{student_base}","username":"student","password":"secret""#
    ));
    let student = account_id("student").expect("student account id");

    cmd(r#""cmd":"StartMonitoring""#.to_string());
    assert!(wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10).is_some());

    // --- Phase 1: happy cross-endpoint path ---
    // The teacher (base_a) sources the token; the student (base_b) signs on its OWN endpoint. A latent
    // bug would derive the student's endpoint from the teacher's base_url and this would never confirm.
    open_qr(&student_base, "QR-A");
    let ev = wait_for(signed("QR-A", &student), 25).expect("student signs QR-A across endpoints");
    assert!(ev["method"].as_str().unwrap_or("").contains("qr"), "signed via qr teacher-assist");

    // --- Phase 2: a session lost mid-flight is recovered on BOTH sides ---
    // teacher_base: the whole session expires (its create/fetch serve a login page until re-login).
    // student_base: only the student's sign PUT expires (its detect poll stays healthy).
    post_test(&teacher_base, "/_test/expire", r#"{"expired":true,"mode":"login_page"}"#);
    post_test(&student_base, "/_test/expire_signs", r#"{"enabled":true,"user":"student"}"#);
    open_qr(&student_base, "QR-B");
    assert!(
        wait_for(signed("QR-B", &student), 25).is_some(),
        "student recovers its own session AND the teacher recovers its source, then signs QR-B"
    );

    // No secret ever crosses the seam (logs or events).
    let serialized = events().lock().unwrap().join("\n");
    assert!(!serialized.contains("QRDATA-XYZ"), "QR data never appears in events/logs");
    assert!(!serialized.contains("session=sk-"), "session cookies never appear in events/logs");

    unsafe { crate::core_free(h) };
}
