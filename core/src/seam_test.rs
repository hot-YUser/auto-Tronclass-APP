//! Offline seam test (docs 90 §10): drive the real FFI entry points against the in-repo
//! fake TronClass and assert the three skeleton risks in one run, plus the negative case.
//!
//! - async over FFI: a good-cred command round-trips to a `LoginResult{ok:true}`
//! - reverse channel: an unsolicited `StateChanged` arrives, unrequested
//! - process stays alive: the heartbeat `Tick` fires from the long-lived runtime task
//! - bad creds fail loudly (`ok:false`), never silently
//!
//! One core, one callback, two correlated commands — so results can't clobber each other
//! and the tests need no serialization.

use crate::fake;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// The callback is a bare `extern "C" fn` and cannot capture, so events route through a
// global sender. Set once; the mpsc receiver lives on the test thread.
static TX: OnceLock<Mutex<Sender<String>>> = OnceLock::new();

extern "C" fn collect(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let s = String::from_utf8_lossy(bytes).into_owned();
    let _ = TX.get().unwrap().lock().unwrap().send(s);
}

fn start_fake() -> u16 {
    let (ptx, prx) = channel();
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
    prx.recv().unwrap()
}

fn send_login(handle: *mut std::ffi::c_void, id: u64, port: u16, user: &str, pass: &str) {
    let cmd = format!(
        r#"{{"id":{id},"cmd":"Login","base_url":"http://127.0.0.1:{port}","username":"{user}","password":"{pass}"}}"#
    );
    unsafe { crate::core_send(handle, cmd.as_ptr(), cmd.len()) };
}

#[test]
fn seam_proves_three_things() {
    let (tx, rx) = channel();
    TX.set(Mutex::new(tx)).ok();

    let port = start_fake();
    let handle = crate::core_init(collect);

    send_login(handle, 1, port, fake::GOOD_USER, fake::GOOD_PASS); // → ok
    send_login(handle, 2, port, fake::GOOD_USER, "wrong"); // → fail

    let mut result1: Option<bool> = None;
    let mut result2: Option<bool> = None;
    let mut saw_statechanged = false;
    let mut saw_tick = false;

    let deadline = Instant::now() + Duration::from_secs(20);
    while (result1.is_none() || result2.is_none() || !saw_tick) && Instant::now() < deadline {
        let Ok(ev) = rx.recv_timeout(Duration::from_secs(1)) else { continue };
        let v: serde_json::Value = serde_json::from_str(&ev).expect("event is valid JSON");
        match v["event"].as_str() {
            Some("StateChanged") => saw_statechanged = true,
            Some("Tick") => saw_tick = true,
            Some("LoginResult") => {
                let ok = v["ok"].as_bool().unwrap();
                match v["id"].as_u64() {
                    Some(1) => result1 = Some(ok),
                    Some(2) => result2 = Some(ok),
                    _ => panic!("LoginResult without a known id: {ev}"),
                }
            }
            _ => {}
        }
    }

    unsafe { crate::core_free(handle) };

    assert_eq!(result1, Some(true), "good creds must log in (async over FFI + reply routing)");
    assert_eq!(result2, Some(false), "bad creds must fail loudly, never silently");
    assert!(saw_statechanged, "core must push an unsolicited StateChanged (reverse channel)");
    assert!(saw_tick, "heartbeat must tick (runtime/process stays alive between commands)");
}
