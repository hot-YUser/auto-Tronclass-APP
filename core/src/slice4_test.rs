//! Slice-4 tests (headless). Pure gates plus e2e over the FFI: settings persistence, captcha
//! login, SSO→cookie fallback routing, and platform-key unlock.

use crate::config::{new_id, Settings};
use crate::fake;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ===================== pure unit tests (no FFI, run in parallel) =====================

#[test]
fn max_tokens_default_and_zero_resolve_to_16384() {
    assert_eq!(
        Settings::default().llm_max_tokens,
        16384,
        "fresh default is 16384"
    );
    assert_eq!(crate::llm::resolve_max_tokens(0), 16384, "0 → safe default");
    assert_eq!(
        crate::llm::resolve_max_tokens(32000),
        32000,
        "explicit value preserved"
    );
}

#[test]
fn radar_default_chain_is_empty_then_wgs84() {
    assert_eq!(
        Settings::default().radar_strategy,
        vec!["empty_answer".to_string(), "global_wgs84".to_string()]
    );
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

    for secret in [
        "hunter2",
        "session=abc",
        "sk-abc123",
        "root-pw",
        "Bearer xyz",
    ] {
        assert!(
            !s.contains(secret),
            "secret {secret} leaked through redaction: {s}"
        );
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
    rbuf()
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(b).into_owned());
}

#[test]
fn leveled_logging_drops_debug_at_normal() {
    let _g = SEQ.lock().unwrap(); // serialize: LOG_LEVEL is a process global
    rbuf().lock().unwrap().clear();

    crate::redaction::set_level("normal");
    crate::redaction::log_line(rcollect, "debug", "should be dropped");
    crate::redaction::log_line(rcollect, "info", "always shown");
    assert_eq!(
        rbuf().lock().unwrap().len(),
        1,
        "debug dropped at normal, info kept"
    );

    crate::redaction::set_level("debug");
    crate::redaction::log_line(rcollect, "debug", "now shown");
    assert_eq!(
        rbuf().lock().unwrap().len(),
        2,
        "debug emitted at debug level"
    );
    crate::redaction::set_level("normal"); // restore
}

// ===================== e2e over the FFI (serialized via SEQ) =====================

static SEQ: Mutex<()> = Mutex::new(());
static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn collect(ptr: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(ptr, len) };
    events()
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(b).into_owned());
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
// boot 專用：回覆必須是 ok=true，失敗時由 .expect 以階段訊息立即停止。
fn ok_reply(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id && v["ok"] == true
}
fn send(h: *mut std::ffi::c_void, json: &str) {
    unsafe { crate::core_send(h, json.as_ptr(), json.len()) };
}
fn account_id(label: &str) -> Option<String> {
    crate::test_support::account_id(&snapshot(), label)
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
        Harness {
            h: crate::core_init(Some(collect)),
            id: 0,
        }
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
    std::env::temp_dir()
        .join(format!("tron-slice4-{tag}-{}", new_id()))
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn settings_persist_over_the_seam() {
    let _g = SEQ.lock().unwrap();
    let mut hz = Harness::new();
    let dir = data_dir("settings");

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");

    let i = hz.next();
    let patch = r#"{"llm_max_tokens":32000,"radar_strategy":["empty_answer","global_wgs84"],
        "number_concurrency":4,"number_min_concurrency":4,"number_cooldown_ms":500,
        "poll_idle_secs":9,"quiz_detect_secs":30,"log_level":"debug","max_answer_reask":7}"#;
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{patch}}}"#),
    );
    wait_for(ok_reply(i), 5).expect("UpdateConfig 未回覆 ok");

    // Read the persisted config.json back and confirm every knob round-tripped.
    let cfg = crate::config::Config::load(&PathBuf::from(&dir).join("config.json")).unwrap();
    let s = &cfg.settings;
    assert_eq!(s.llm_max_tokens, 32000);
    assert_eq!(s.number_concurrency, 4);
    assert_eq!(s.number_min_concurrency, 4);
    assert_eq!(s.number_cooldown_ms, 500);
    assert_eq!(s.poll_idle_secs, 9);
    assert_eq!(s.quiz_detect_secs, 30);
    assert_eq!(s.log_level, "debug");
    assert_eq!(s.max_answer_reask, 7);
    crate::redaction::set_level("normal"); // Init/UpdateConfig flipped the global level
}

#[test]
fn captcha_login_challenge_and_submit() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("captcha");

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault"}}"#));
    wait_for(ok_reply(i), 5).expect("CreateVault 未回覆 ok");

    // Turn on the fake's captcha login page.
    post(
        &base,
        "/_test/captcha",
        r#"{"required":true,"expected":"A1B2"}"#,
    );

    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
    let dave = account_id("dave").unwrap();

    let login_id = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{dave}"}}"#),
    );

    // The core grabs the captcha image and asks us to solve it.
    let challenge = wait_for(
        |v| v["event"] == "CaptchaChallenge" && v["account_id"].as_str() == Some(&dave),
        10,
    )
    .expect("CaptchaChallenge");
    let expected_b64 = crate::login::encode_base64(fake::CAPTCHA_IMAGE.as_bytes());
    assert_eq!(
        challenge["image_b64"].as_str().unwrap(),
        expected_b64,
        "image bytes shipped as base64"
    );

    // No login result yet — it is blocked awaiting the answer.
    assert!(
        none_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 2),
        "login waits for captcha"
    );

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"SubmitCaptcha","account_id":"{dave}","text":"A1B2"}}"#),
    );
    wait_for(reply_ok(i), 5);

    let result =
        wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], true, "captcha answered → login succeeds");
}

#[test]
fn captcha_wrong_answer_fails() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("captcha-bad");

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault"}}"#));
    wait_for(ok_reply(i), 5).expect("CreateVault 未回覆 ok");
    post(
        &base,
        "/_test/captcha",
        r#"{"required":true,"expected":"A1B2"}"#,
    );
    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"e","school":"{base}","username":"e","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
    let eid = account_id("e").unwrap();
    let login_id = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{eid}"}}"#),
    );
    wait_for(|v| v["event"] == "CaptchaChallenge", 10).expect("challenge");
    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"SubmitCaptcha","account_id":"{eid}","text":"WRONG"}}"#),
    );
    wait_for(reply_ok(i), 5);
    let result =
        wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], false, "wrong captcha → login fails");
}

#[test]
fn sso_login_routes_to_cookie_fallback() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("sso");

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault"}}"#));
    wait_for(ok_reply(i), 5).expect("CreateVault 未回覆 ok");
    post(&base, "/_test/sso", r#"{"enabled":true}"#);

    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"carol","school":"{base}","username":"carol","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
    let carol = account_id("carol").unwrap();

    let login_id = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{carol}"}}"#),
    );
    let result =
        wait_for(|v| v["event"] == "LoginResult" && v["id"] == login_id, 10).expect("LoginResult");
    assert_eq!(result["ok"], false, "SSO page cannot password-login");
    assert!(
        result["reason"].as_str().unwrap_or("").contains("cookie"),
        "routed to the cookie fallback"
    );

    // The ImportCookies fallback command is reachable and runs end-to-end (bogus cookies → login_failed).
    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"ImportCookies","account_id":"{carol}","cookies_json":"[]"}}"#
        ),
    );
    wait_for(reply_ok(i), 5);
    assert!(
        wait_for(
            |v| crate::test_support::event_account_login_state(v, &carol, "error"),
            5
        )
        .is_some(),
        "ImportCookies 以完整 MonitoringSnapshot 回報錯誤"
    );
}

#[test]
fn schedule_closed_suppresses_monitoring() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir("sched");

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");

    let i = hz.next();
    // poll_idle_secs=1：absence 窗口 5s ≥ 3× poll cadence。
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"poll_idle_secs":1}}}}"#),
    );
    wait_for(ok_reply(i), 5).expect("UpdateConfig 未回覆 ok");

    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault"}}"#));
    wait_for(ok_reply(i), 5).expect("CreateVault 未回覆 ok");
    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"frank","school":"{base}","username":"frank","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
    let frank = account_id("frank").unwrap();
    let clock_id = hz.next();
    let clock = crate::test_support::apply_clock_command(
        clock_id,
        crate::test_support::latest_monitoring_snapshot(&snapshot()).unwrap(),
    );
    send(hz.h, &clock);
    wait_for(ok_reply(clock_id), 5).expect("ApplyScheduleClock 未回覆 ok");
    let login_id = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{login_id},"cmd":"Login","account_id":"{frank}"}}"#),
    );
    wait_for(
        |v| crate::test_support::event_account_login_state(v, &frank, "online"),
        10,
    )
    .expect("帳號未上線");

    // Open a rollcall — but the poller is gated closed, so it must never be detected.
    post(
        &base,
        "/_test/open_rollcall",
        r#"{"id":"SCHED1","kind":"self_registration","attendance_rate":100}"#,
    );
    assert!(
        none_for(
            |v| v["event"] == "RollcallDetected" && v["rollcall_id"] == "SCHED1",
            5
        ),
        "closed schedule → no detection"
    );

    let i = hz.next();
    send(hz.h, &crate::test_support::stop_all_command(i));
    wait_for(reply_ok(i), 5);
}

#[test]
fn vault_auto_unlocks_without_a_password() {
    let _g = SEQ.lock().unwrap();
    let mut hz = Harness::new();
    let dir = data_dir("autounlock");

    // No CreateVault, no master password: Init auto-unlocks the vault with the persistent device key.
    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");
    assert!(
        wait_for(|v| v["event"] == "VaultState" && v["unlocked"] == true, 3).is_some(),
        "vault auto-unlocks at Init — no password step"
    );

    // The vault is already open, so a secret can be stored straight away.
    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"g","school":"http://x","username":"g","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
}

/// Boot one account against a fresh fake and start monitoring with the anti-fake gate at `gate` %.
fn boot_monitoring(tag: &str, gate: f64) -> (Harness, String) {
    let base = start_fake();
    let mut hz = Harness::new();
    let dir = data_dir(tag);
    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#),
    );
    wait_for(ok_reply(i), 10).expect("Init 未回覆 ok");
    let i = hz.next();
    // poll_idle_secs=1：config_update 的 absence 窗口 3s ≥ 3× poll cadence；預設 5s 會小於窗口。
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":1,"poll_idle_secs":1,"attendance_gate_percent":{gate}}}}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("UpdateConfig 未回覆 ok");
    let i = hz.next();
    send(
        hz.h,
        &format!(
            r#"{{"id":{i},"cmd":"AddAccount","label":"eve","school":"{base}","username":"eve","password":"secret"}}"#
        ),
    );
    wait_for(ok_reply(i), 5).expect("AddAccount 未回覆 ok");
    let eve = account_id("eve").unwrap();
    let clock_id = hz.next();
    let start_id = hz.next();
    crate::test_support::activate_account(hz.h, clock_id, start_id, &snapshot(), &eve);
    wait_for(ok_reply(clock_id), 5).expect("ApplyScheduleClock 未回覆 ok");
    wait_for(ok_reply(start_id), 15).expect("StartTarget 未回覆 ok");
    wait_for(
        |v| crate::test_support::event_account_login_state(v, &eve, "online"),
        10,
    )
    .expect("帳號未上線");
    (hz, base)
}

/// A settings change must reach the long-lived actor and its pollers without target restart.
#[test]
fn config_update_applies_to_a_running_monitor() {
    let _g = SEQ.lock().unwrap();
    let (mut hz, base) = boot_monitoring("livecfg", 100.0);

    post(
        &base,
        "/_test/open_rollcall",
        r#"{"id":"RC50","kind":"self_registration","attendance_rate":50}"#,
    );
    let signed = |v: &Value| v["event"] == "SignedIn" && v["rollcall_id"] == "RC50";
    assert!(
        none_for(signed, 3),
        "a 50%-attendance rollcall is held by the 100% gate"
    );

    // Lower the gate WHILE monitoring — no stop/start cycle.
    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"attendance_gate_percent":10}}}}"#),
    );
    assert_eq!(
        wait_for(reply_ok(i), 5).unwrap()["ok"],
        true,
        "UpdateConfig accepted"
    );
    assert!(
        wait_for(signed, 15).is_some(),
        "the running monitor must adopt the new gate without a restart"
    );
}

/// While a rollcall is held below the gate there is no countdown, so the UI needs the live class rate
/// (and the threshold) to render that wait instead of an empty box — and a flip to `holding:false` when
/// the gate is finally met, so it can swap back to the countdown.
#[test]
fn below_gate_emits_live_attendance_for_the_ui() {
    let _g = SEQ.lock().unwrap();
    let (mut hz, base) = boot_monitoring("gateui", 80.0);

    post(
        &base,
        "/_test/open_rollcall",
        r#"{"id":"RC40","kind":"self_registration","attendance_rate":40}"#,
    );
    let held = wait_for(
        |v| v["event"] == "RollcallGate" && v["rollcall_id"] == "RC40" && v["holding"] == true,
        15,
    )
    .expect("a held rollcall emits RollcallGate{holding:true}");
    let rate = held["rate"]
        .as_f64()
        .expect("carries the live class attendance rate");
    assert!(
        (30.0..50.0).contains(&rate),
        "rate ≈ the fake's 40% roster, got {rate}"
    );
    assert_eq!(
        held["gate_percent"].as_f64(),
        Some(80.0),
        "carries the configured threshold"
    );

    let i = hz.next();
    send(
        hz.h,
        &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"attendance_gate_percent":10}}}}"#),
    );
    wait_for(reply_ok(i), 5);
    assert!(
        wait_for(
            |v| v["event"] == "RollcallGate" && v["rollcall_id"] == "RC40" && v["holding"] == false,
            15
        )
        .is_some(),
        "clearing the gate emits holding:false so the UI restores the countdown"
    );
}
