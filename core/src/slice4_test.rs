//! Slice-4 tests (headless). Pure gates (scheduling / max_tokens / redaction / vault key-unlock) plus
//! e2e over the FFI: settings persist, captcha login (challenge → submit → success), SSO→cookie
//! fallback routing, an operating-hours-closed schedule suppressing detection, and platform-key
//! unlock. The e2e tests share one global event sink, so they serialize via `SEQ`.

use crate::config::{new_id, Operating, Settings};
use crate::fake;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ===================== pure unit tests (no FFI, run in parallel) =====================

#[test]
fn operating_gate_open_and_closed() {
    // Empty schedule → always open.
    assert!(Operating::default().is_open(0, 0));

    // epoch 0 = 1970-01-01 00:00 UTC = Thursday (weekday 3 with Mon=0), minute 0.
    let sched: Operating = serde_json::from_value(json!({
        "days": [{ "weekday": 3, "enabled": true, "windows": [{ "start": "00:00", "end": "01:00" }] }]
    }))
    .unwrap();
    assert!(sched.is_open(0, 0), "00:00 Thu is inside 00:00–01:00");
    assert!(sched.is_open(59 * 60, 0), "00:59 still inside");
    assert!(!sched.is_open(60 * 60, 0), "01:00 is the exclusive end → closed");
    assert!(!sched.is_open(2 * 3600, 0), "02:00 outside the window");

    // A different weekday (Friday = epoch + 1 day) is not listed → inherits always-on.
    assert!(sched.is_open(86400 + 2 * 3600, 0), "Friday not listed → open");

    // Listed but disabled → closed even inside a would-be window.
    let disabled: Operating = serde_json::from_value(json!({
        "days": [{ "weekday": 3, "enabled": false, "windows": [{ "start": "00:00", "end": "23:59" }] }]
    }))
    .unwrap();
    assert!(!disabled.is_open(0, 0), "disabled Thursday → closed");

    // tz offset shifts the local weekday/time: +480 min pushes epoch 0 into Thursday 08:00.
    let morning: Operating = serde_json::from_value(json!({
        "days": [{ "weekday": 3, "enabled": true, "windows": [{ "start": "07:00", "end": "09:00" }] }]
    }))
    .unwrap();
    assert!(morning.is_open(0, 480), "UTC 00:00 + 8h = 08:00 local, inside 07:00–09:00");
    assert!(!morning.is_open(0, 0), "without the offset it is 00:00, outside");

    // A window that wraps past midnight.
    let overnight: Operating = serde_json::from_value(json!({
        "days": [{ "weekday": 3, "enabled": true, "windows": [{ "start": "22:00", "end": "02:00" }] }]
    }))
    .unwrap();
    assert!(overnight.is_open(30 * 60, 0), "00:30 is inside a 22:00→02:00 wrap");
}

#[test]
fn max_tokens_default_and_zero_resolve_to_16384() {
    assert_eq!(Settings::default().llm_max_tokens, 16384, "fresh default is 16384");
    assert_eq!(crate::llm::resolve_max_tokens(0), 16384, "0 → safe default");
    assert_eq!(crate::llm::resolve_max_tokens(32000), 32000, "explicit value preserved");
}

#[test]
fn radar_default_chain_is_empty_then_wgs84() {
    assert_eq!(Settings::default().radar_strategy, vec!["empty_answer".to_string(), "global_wgs84".to_string()]);
}

#[test]
fn redaction_hides_secrets_everywhere() {
    // Secrets under sensitive keys — nested in objects and arrays — must all become [redacted].
    let mut v = json!({
        "event": "LoginResult",
        "password": "hunter2",
        "cookies": "session=abc",
        "nested": { "api_key": "sk-abc123", "note": "ok" },
        "list": [ { "master_password": "root-pw" }, { "authorization": "Bearer xyz" } ],
        "account_id": "keep-me"
    });
    crate::redaction::redact(&mut v);
    let s = v.to_string();

    for secret in ["hunter2", "session=abc", "sk-abc123", "root-pw", "Bearer xyz"] {
        assert!(!s.contains(secret), "secret {secret} leaked through redaction: {s}");
    }
    assert!(s.contains("[redacted]"), "redaction marker present");
    assert_eq!(v["account_id"], "keep-me", "non-secret ids are preserved");
    assert_eq!(v["nested"]["note"], "ok", "non-secret siblings preserved");
}

// A dedicated sink for the leveled-logging test (log_line needs a C callback).
static RBUF: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn rbuf() -> &'static Mutex<Vec<String>> {
    RBUF.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn rcollect(ptr: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(ptr, len) };
    rbuf().lock().unwrap().push(String::from_utf8_lossy(b).into_owned());
}

#[test]
fn leveled_logging_drops_debug_at_normal() {
    let _g = SEQ.lock().unwrap(); // serialize: LOG_LEVEL is a process global
    rbuf().lock().unwrap().clear();

    crate::redaction::set_level("normal");
    crate::redaction::log_line(rcollect, "debug", "should be dropped");
    crate::redaction::log_line(rcollect, "info", "always shown");
    assert_eq!(rbuf().lock().unwrap().len(), 1, "debug dropped at normal, info kept");

    crate::redaction::set_level("debug");
    crate::redaction::log_line(rcollect, "debug", "now shown");
    assert_eq!(rbuf().lock().unwrap().len(), 2, "debug emitted at debug level");
    crate::redaction::set_level("normal"); // restore
}

#[test]
fn vault_unlock_with_platform_key_roundtrip() {
    use crate::secrets::{AccountSecret, VaultFile};
    let path = std::env::temp_dir().join(format!("tron-slice4-vault-{}", new_id()));

    let mut v = VaultFile::create(&path, "master-pw").unwrap();
    v.set("acc1", AccountSecret { password: "s3cret".into(), cookies: String::new() }).unwrap();
    let key = v.key_bytes().expect("key while unlocked");
    drop(v);

    // Unlock with the stored key (no password) → data intact.
    let v2 = VaultFile::unlock_with_key(&path, key).expect("key unlock");
    assert_eq!(v2.get("acc1").unwrap().password, "s3cret");

    // A wrong key fails AEAD authentication cleanly.
    let mut bad = key;
    bad[0] ^= 0xff;
    assert!(VaultFile::unlock_with_key(&path, bad).is_err(), "wrong key rejected");

    let _ = std::fs::remove_file(&path);
}

// ===================== e2e over the FFI (serialized via SEQ) =====================

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
fn post(base_url: &str, path: &str, body: &str) -> String {
    let mut s = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
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
    std::env::temp_dir().join(format!("tron-slice4-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/")
}

#[test]
fn settings_persist_over_the_seam() {
    let _g = SEQ.lock().unwrap();
    let mut hz = Harness::new();
    let dir = data_dir("settings");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    assert!(wait_for(reply_ok(i), 10).is_some());

    let i = hz.next();
    let patch = r#"{"llm_max_tokens":32000,"radar_strategy":["empty_answer","global_wgs84"],
        "number_concurrency":4,"number_cooldown_ms":500,"poll_idle_secs":9,"quiz_detect_secs":30,
        "log_level":"debug","max_answer_reask":7,"tz_offset_minutes":540,
        "operating":{"days":[{"weekday":2,"enabled":true,"windows":[{"start":"08:30","end":"17:00"}]}]}}"#;
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{patch}}}"#));
    assert!(wait_for(reply_ok(i), 5).unwrap()["ok"] == true);

    // Read the persisted config.json back and confirm every knob round-tripped.
    let cfg = crate::config::Config::load(&PathBuf::from(&dir).join("config.json"));
    let s = &cfg.settings;
    assert_eq!(s.llm_max_tokens, 32000);
    assert_eq!(s.number_concurrency, 4);
    assert_eq!(s.number_cooldown_ms, 500);
    assert_eq!(s.poll_idle_secs, 9);
    assert_eq!(s.quiz_detect_secs, 30);
    assert_eq!(s.log_level, "debug");
    assert_eq!(s.max_answer_reask, 7);
    assert_eq!(s.tz_offset_minutes, 540);
    assert_eq!(s.operating.days.len(), 1);
    assert_eq!(s.operating.days[0].windows[0].start, "08:30");
    crate::redaction::set_level("normal"); // Init/UpdateConfig flipped the global level
}

#[test]
fn captcha_login_challenge_and_submit() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("captcha");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    assert!(wait_for(reply_ok(i), 10).is_some());
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);

    // Turn on the fake's captcha login page.
    post(&base, "/_test/captcha", r#"{"required":true,"expected":"A1B2"}"#);

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let dave = account_id("dave").unwrap();

    let login_id = hz.next();
    send(hz.h, &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{dave}"}}"#));

    // The core grabs the captcha image and asks us to solve it.
    let challenge = wait_for(|v| v["event"] == "CaptchaChallenge" && v["account_id"].as_str() == Some(&dave), 10)
        .expect("CaptchaChallenge");
    let expected_b64 = crate::login::encode_base64(fake::CAPTCHA_IMAGE.as_bytes());
    assert_eq!(challenge["image_b64"].as_str().unwrap(), expected_b64, "image bytes shipped as base64");

    // No login result yet — it is blocked awaiting the answer.
    assert!(none_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 2), "login waits for captcha");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"SubmitCaptcha","account_id":"{dave}","text":"A1B2"}}"#));
    wait_for(reply_ok(i), 5);

    let result = wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], true, "captcha answered → login succeeds");
}

#[test]
fn captcha_wrong_answer_fails() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("captcha-bad");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    post(&base, "/_test/captcha", r#"{"required":true,"expected":"A1B2"}"#);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"e","school":"{base}","username":"e","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let eid = account_id("e").unwrap();
    let login_id = hz.next();
    send(hz.h, &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{eid}"}}"#));
    wait_for(|v| v["event"] == "CaptchaChallenge", 10).expect("challenge");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"SubmitCaptcha","account_id":"{eid}","text":"WRONG"}}"#));
    wait_for(reply_ok(i), 5);
    let result = wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], false, "wrong captcha → login fails");
}

#[test]
fn sso_login_routes_to_cookie_fallback() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("sso");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    post(&base, "/_test/sso", r#"{"enabled":true}"#);

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"carol","school":"{base}","username":"carol","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let carol = account_id("carol").unwrap();

    let login_id = hz.next();
    send(hz.h, &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{carol}"}}"#));
    let result = wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], false, "SSO page cannot password-login");
    assert!(result["reason"].as_str().unwrap_or("").contains("cookie"), "routed to the cookie fallback");

    // The ImportCookies fallback command is reachable and runs end-to-end (bogus cookies → login_failed).
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"ImportCookies","account_id":"{carol}","cookies_json":"[]"}}"#));
    wait_for(reply_ok(i), 5);
    assert!(
        wait_for(|v| v["event"] == "AccountStatus" && v["account_id"].as_str() == Some(&carol), 5).is_some(),
        "ImportCookies reports an AccountStatus"
    );
}

#[test]
fn schedule_closed_suppresses_monitoring() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("sched");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);

    // All seven weekdays enabled but with no windows → closed all the time, whatever today is.
    let days: Vec<String> = (0..7).map(|w| format!(r#"{{"weekday":{w},"enabled":true,"windows":[]}}"#)).collect();
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"operating":{{"days":[{}]}}}}}}"#, days.join(",")));
    wait_for(reply_ok(i), 5);

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"frank","school":"{base}","username":"frank","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    wait_for(reply_ok(i), 15);
    wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10);

    // Open a rollcall — but the poller is gated closed, so it must never be detected.
    post(&base, "/_test/open_rollcall", r#"{"id":"SCHED1","kind":"self_registration","attendance_rate":100}"#);
    assert!(
        none_for(|v| v["event"] == "RollcallDetected" && v["rollcall_id"] == "SCHED1", 5),
        "closed schedule → no detection"
    );

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"StopMonitoring"}}"#));
    wait_for(reply_ok(i), 5);
}

#[test]
fn keystore_unlock_flow() {
    let _g = SEQ.lock().unwrap();
    let mut hz = Harness::new();
    let dir = data_dir("keystore");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    // Create the vault (auto-stores the key in the keystore) and add a secret.
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"g","school":"http://x","username":"g","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);

    // Lock the vault, then unlock it with the stored platform key (no password).
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"LockVault"}}"#));
    wait_for(reply_ok(i), 5);
    assert!(
        wait_for(|v| v["event"] == "VaultState" && v["unlocked"] == false, 3).is_some(),
        "vault reports locked"
    );

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UnlockWithKeystore"}}"#));
    let r = wait_for(reply_ok(i), 5).expect("reply");
    assert_eq!(r["ok"], true, "keystore unlock succeeds (AEAD confirms the key)");
    assert!(
        wait_for(|v| v["event"] == "VaultState" && v["unlocked"] == true, 3).is_some(),
        "vault reports unlocked again"
    );
}

#[test]
fn keystore_unlock_without_stored_key_errors() {
    let _g = SEQ.lock().unwrap();
    let mut hz = Harness::new(); // fresh Core → fresh empty MemKeyStore
    let dir = data_dir("keystore-empty");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UnlockWithKeystore"}}"#));
    let r = wait_for(reply_ok(i), 5).expect("reply");
    assert_eq!(r["ok"], false, "no stored key → error");
}
