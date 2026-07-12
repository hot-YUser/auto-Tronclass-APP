//! R2 radar e2e (headless, over the FFI against the real-contract fake): the solver reverse-locates a
//! hidden target from anchor distances (lite carries no coordinate) → hits → recheck → signs; and a
//! beacon rollcall's coordinate answers carry a radarSignal. Serialized via `SEQ`. The solver
//! convergence + the independent known-value haversine assertions live in `radar.rs`'s own tests.

use crate::config::new_id;
use crate::fake;
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
fn http(base_url: &str, req: &str) -> String {
    let mut s = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}
fn post(base: &str, path: &str, body: &str) {
    http(base, &format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body));
}
fn get(base: &str, path: &str) -> String {
    http(base, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"))
}
struct Harness {
    h: *mut std::ffi::c_void,
    id: u64,
}
impl Harness {
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
/// Boot one account monitoring against a fresh fake; returns (harness, base, account_id).
fn boot(tag: &str) -> (Harness, String, String) {
    events().lock().unwrap().clear();
    let base = start_fake();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r2-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/");
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
fn radar_solver_locates_and_signs() {
    let _g = SEQ.lock().unwrap();
    let (hz, base, dave) = boot("solve");
    // Hidden target far from the old hardcoded (25, 121.5); requires_coords → the empty {} path fails,
    // forcing the global solver, which must reverse-locate from anchor distances (lite has no coords).
    post(&base, "/_test/open_rollcall", r#"{"id":"RAD","kind":"radar","lat":35.6812,"lng":139.7671,"requires_coords":true,"scope_radius":100}"#);
    assert!(wait_for(signed("RAD", &dave), 30).is_some(), "solver reverse-locates the hidden target and signs");
    drop(hz);
}

#[test]
fn radar_beacon_attaches_signal() {
    let _g = SEQ.lock().unwrap();
    let (hz, base, dave) = boot("beacon");
    post(&base, "/_test/open_rollcall", r#"{"id":"RADB","kind":"radar","lat":-33.8568,"lng":151.2153,"requires_coords":true,"scope_radius":100,"use_beacon":true,"beacon_nonce":"nonce-xyz"}"#);
    assert!(wait_for(signed("RADB", &dave), 30).is_some(), "beacon radar signs");
    let saw: Value = serde_json::from_str(&get(&base, "/_test/saw_radar_signal")).unwrap_or(Value::Null);
    assert_eq!(saw["saw"], true, "coordinate answers carried a radarSignal when use_beacon");
    drop(hz);
}
