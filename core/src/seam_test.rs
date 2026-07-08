//! End-to-end seam test over the real FFI entry points: the slice-1 login flow (Init → CreateVault
//! → AddAccount → Login) against the in-repo fake TronClass. Still proves the three skeleton risks
//! (async over FFI, unsolicited reverse-channel events, heartbeat) and adds:
//!   - a successful real-form login (good creds) → `LoginResult{ok:true}`
//!   - a false-positive guard: bad creds land the fake's 200-with-login-page, and the content-based
//!     `verify_session` rejects it → `LoginResult{ok:false}` (never a silent/false success).

use crate::config::new_id;
use crate::fake;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// The callback can't capture, so events land in a global Vec. Only this test uses the callback.
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

fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id
}

fn start_fake() -> u16 {
    let (ptx, prx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (port, listener) = fake::bind_ephemeral().await;
            ptx.send(port).unwrap();
            fake::serve(listener).await;
        });
    });
    prx.recv().unwrap()
}

fn send(handle: *mut std::ffi::c_void, json: &str) {
    unsafe { crate::core_send(handle, json.as_ptr(), json.len()) };
}

fn account_id_by_label(label: &str) -> Option<String> {
    let accounts = snapshot();
    // Scan Accounts events newest-last for the account with this label.
    for ev in accounts.iter().rev() {
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

#[test]
fn seam_login_flow_end_to_end() {
    let port = start_fake();
    let base = format!("http://127.0.0.1:{port}");
    let data_dir = std::env::temp_dir().join(format!("tron-seam-{}", new_id()));
    let data_dir = data_dir.to_string_lossy().replace('\\', "/"); // JSON-safe path

    let handle = crate::core_init(collect);

    send(handle,&format!(r#"{{"id":1,"cmd":"Init","data_dir":"{data_dir}"}}"#));
    assert!(wait_for(reply_ok(1), 10).is_some(), "Init should reply");

    send(handle,r#"{"id":2,"cmd":"CreateVault","master_password":"pw"}"#);
    assert!(wait_for(reply_ok(2), 10).unwrap()["ok"] == true, "CreateVault ok");

    // Good account → successful real-form login.
    send(
        handle,
        &format!(r#"{{"id":3,"cmd":"AddAccount","label":"good","school":"{base}","username":"test","password":"secret"}}"#),
    );
    assert!(wait_for(reply_ok(3), 10).unwrap()["ok"] == true, "AddAccount good ok");
    let good_id = wait_for(|_| account_id_by_label("good").is_some(), 5)
        .and_then(|_| account_id_by_label("good"))
        .expect("good account id");
    send(handle,&format!(r#"{{"id":4,"cmd":"Login","account_id":"{good_id}"}}"#));
    let good_login = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 4, 15).expect("good LoginResult");
    assert_eq!(good_login["ok"], true, "good creds must log in via the real form");

    // Bad account → the fake serves 200+login-page; content-based verify_session must reject it.
    send(
        handle,
        &format!(r#"{{"id":5,"cmd":"AddAccount","label":"bad","school":"{base}","username":"test","password":"WRONG"}}"#),
    );
    wait_for(reply_ok(5), 10);
    let bad_id = account_id_by_label("bad").expect("bad account id");
    send(handle,&format!(r#"{{"id":6,"cmd":"Login","account_id":"{bad_id}"}}"#));
    let bad_login = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 6, 15).expect("bad LoginResult");
    assert_eq!(bad_login["ok"], false, "bad creds must fail loudly (200-with-login-page is not success)");

    // Skeleton risks still hold.
    assert!(wait_for(|v| v["event"] == "Tick", 3).is_some(), "heartbeat ticks");
    assert!(snapshot().iter().any(|v| v["event"] == "StateChanged"), "unsolicited StateChanged");

    unsafe { crate::core_free(handle) };
}
