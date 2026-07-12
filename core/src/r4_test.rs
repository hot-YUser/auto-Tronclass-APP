//! R4 + R3c e2e: per-family detection & gates, session-expiry re-login, SSO-redirect login urljoin,
//! and the R3c all-or-nothing prepare gate (retry-success / persistent-error / empty-closed). Drives
//! everything over the FFI against the real-contract fake. Serialized via `SEQ` (shared global events).

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
fn any(pred: impl Fn(&Value) -> bool) -> bool {
    snapshot().iter().any(pred)
}
fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id
}
fn submitted(quiz_id: &str) -> impl Fn(&Value) -> bool + '_ {
    move |v| v["event"] == "QuizSubmitted" && v["quiz_id"] == quiz_id
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
fn http(base: &str, req: &str) -> String {
    let mut s = std::net::TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}
fn post(base: &str, path: &str, body: &str) {
    http(base, &format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body));
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
/// Boot one account monitoring. `budget`/`reask` tune the R3c retry loop; poll cadences are fast.
fn boot(tag: &str, base: &str, budget: u64, reask: u32) -> (Harness, String) {
    events().lock().unwrap().clear();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r4-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2,"quiz_detect_secs":1,"poll_idle_secs":1,"prepare_retry_budget_secs":{budget},"max_answer_reask":{reask},"llm_endpoint":"{base}/v1/chat/completions"}}}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"SetLlmKey","key":"k"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let dave = account_id("dave").unwrap();
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    wait_for(reply_ok(i), 15);
    wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10);
    (hz, dave)
}

/// Per-family detection + gates: a control exam and classroom submit; every gated-out variant
/// (classroom count 0, already-voted, already-submitted / attempts-exhausted exam, answered courseware,
/// empty paper) is NEVER submitted. One leaked subject each so submits need no LLM.
#[test]
fn per_family_detection_and_gates() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("gates", &base, 30, 1);
    let leaked = r#""subjects":[{"id":"s1","type":"single_selection","options":[{"id":"o1","is_answer":true},{"id":"o2"}]}]"#;
    // controls — must submit:
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"EX_OK","course_id":"C1","source":"exam",{leaked}}}"#));
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"CL_OK","course_id":"C1","source":"classroom-exam","started_subjects_count":1,{leaked}}}"#));
    // courseware positive — proves the material→quizzes→my-submission chain detects a FRESH quiz.
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"CW_OK","course_id":"C1","source":"courseware-quiz","my_submission":false,{leaked}}}"#));
    // gated out — must NOT submit:
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"CL_ZERO","course_id":"C1","source":"classroom-exam","started_subjects_count":0,{leaked}}}"#));
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"EX_DONE","course_id":"C1","source":"exam","has_submitted":true,{leaked}}}"#));
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"EX_EXHAUST","course_id":"C1","source":"exam","submit_times":1,"submission_count":1,{leaked}}}"#));
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"CW_DONE","course_id":"C1","source":"courseware-quiz","my_submission":true,{leaked}}}"#));
    post(&base, "/_test/open_quiz", r#"{"activity_id":"VT_VOTED","course_id":"C1","source":"vote","vote_type":"single","vote_items":{"A":"a","B":"b"},"vote_students":[{"user_no":"dave"}]}"#);
    post(&base, "/_test/open_quiz", r#"{"activity_id":"EMPTY1","course_id":"C1","source":"exam","subjects":[]}"#);
    // ISO end_time in the past, not yet is_closed → v1 time-expiry gate → not answerable.
    post(&base, "/_test/open_quiz", &format!(r#"{{"activity_id":"EX_EXPIRED","course_id":"C1","source":"exam","is_closed":false,"end_time":"2000-01-01T00:00:00Z",{leaked}}}"#));

    assert!(wait_for(submitted("EX_OK"), 25).is_some(), "control exam submits");
    assert!(wait_for(submitted("CL_OK"), 25).is_some(), "control classroom submits");
    assert!(wait_for(submitted("CW_OK"), 25).is_some(), "control courseware (material→quizzes chain) submits");
    for gated in ["CL_ZERO", "EX_DONE", "EX_EXHAUST", "CW_DONE", "VT_VOTED", "EMPTY1", "EX_EXPIRED"] {
        assert!(!any(submitted(gated)), "{gated} must be gated out (never submitted)");
    }
    drop(hz);
}

/// SSO-redirect login (R4-C urljoin): `/login` 302s to `/sso/login-page` whose form action is RELATIVE
/// (`authenticate`). Only a correct urljoin posts to `/sso/authenticate` → session established → online.
/// A base-join would post to `/authenticate` (404) → login fails → never online.
#[test]
fn sso_redirect_login_resolves_relative_action() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    post(&base, "/_test/sso_redirect", r#"{"enabled":true}"#); // before StartMonitoring logs in
    let (hz, _dave) = boot("sso", &base, 30, 1);
    // boot() already waited for AccountStatus online; reaching here means the relative-action POST worked.
    assert!(any(|v| v["event"] == "AccountStatus" && v["state"] == "online"), "SSO relative-action login came online");
    drop(hz);
}

/// Session-expiry auto re-login (R4-D): after the session is invalidated mid-run, the poll canary sees
/// the 200-login-page, re-logins, and the account recovers to online (not stuck offline).
#[test]
fn session_expiry_recovers_to_online() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("expire", &base, 30, 1);
    events().lock().unwrap().clear(); // drop the startup 'online' so we assert the RECOVERY one
    post(&base, "/_test/expire", r#"{"expired":true}"#);
    assert!(wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 20).is_some(),
        "account re-logins and recovers to online after expiry");
    drop(hz);
}

/// R4.1 #5: the poll canary recovers from a 401 and from a redirect-to-login (not just the inline
/// 200-login-page mode already covered) — each re-logins back to online.
fn expiry_mode_recovers(tag: &str, mode: &str) {
    let base = start_fake();
    let (hz, _dave) = boot(tag, &base, 30, 1);
    events().lock().unwrap().clear(); // drop the startup online so we assert the RECOVERY one
    post(&base, "/_test/expire", &format!(r#"{{"expired":true,"mode":"{mode}"}}"#));
    assert!(wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 20).is_some(),
        "{mode} expiry → poll canary re-logins to online");
    drop(hz);
}
#[test]
fn expiry_401_recovers_to_online() {
    let _g = SEQ.lock().unwrap();
    expiry_mode_recovers("exp401", "401");
}
#[test]
fn expiry_redirect_recovers_to_online() {
    let _g = SEQ.lock().unwrap();
    expiry_mode_recovers("expredir", "redirect");
}

/// R4.1 stale-offline: a transient 503 blip flips the badge to offline; when polling succeeds again the
/// poller edge-triggers back to online (the old code emitted offline but never re-emitted online).
#[test]
fn transient_down_then_ok_clears_stale_offline() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("staleoff", &base, 30, 1);
    events().lock().unwrap().clear();
    post(&base, "/_test/down", r#"{"enabled":true}"#);
    assert!(wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "offline", 15).is_some(), "503 blip → offline");
    events().lock().unwrap().clear();
    post(&base, "/_test/down", r#"{"enabled":false}"#);
    assert!(wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 15).is_some(), "recovery → online (edge-triggered)");
    drop(hz);
}

/// R3c retry-success: the LLM returns empty on the first call then succeeds. With max_reask=1, prepare #1
/// leaves the subject unanswered → re-prepare → prepare #2 answers it → the WHOLE paper submits.
#[test]
fn r3c_retry_then_whole_paper_submits() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("retry", &base, 30, 1);
    post(&base, "/_test/llm_fail_times", r#"{"times":1}"#);
    post(&base, "/_test/open_quiz", r#"{"activity_id":"RT1","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"why"}]}"#);
    assert!(wait_for(submitted("RT1"), 30).is_some(), "re-prepare fills the subject → whole paper submits");
    drop(hz);
}

fn signed_in(rc: &str) -> impl Fn(&Value) -> bool + '_ {
    move |v| v["event"] == "SignedIn" && v["rollcall_id"] == rc
}

/// R4.1 #2: a session that dies EXACTLY during a sign (poll/detect stay healthy) must not silently lose
/// the rollcall — after re-login the same account is re-signed → `SignedIn`. Covers number, radar, and
/// self_registration (each sign kind now flags auth-lost via the shared `response_auth_lost`).
fn sign_time_expiry_recovers(tag: &str, open_json: &str, rc: &str) {
    let base = start_fake();
    let (hz, _dave) = boot(tag, &base, 30, 1);
    post(&base, "/_test/expire_signs", r#"{"enabled":true}"#);
    post(&base, "/_test/open_rollcall", open_json);
    assert!(wait_for(signed_in(rc), 30).is_some(), "{rc}: re-signs to SignedIn after a sign-time expiry");
    drop(hz);
}
#[test]
fn sign_time_expiry_number_recovers() {
    let _g = SEQ.lock().unwrap();
    sign_time_expiry_recovers("sx-num", r#"{"id":"RC_NUM","kind":"number","number_code":"1234","attendance_rate":100.0}"#, "RC_NUM");
}
#[test]
fn sign_time_expiry_radar_recovers() {
    let _g = SEQ.lock().unwrap();
    sign_time_expiry_recovers("sx-rad", r#"{"id":"RC_RAD","kind":"radar","requires_coords":false,"attendance_rate":100.0}"#, "RC_RAD");
}
#[test]
fn sign_time_expiry_self_reg_recovers() {
    let _g = SEQ.lock().unwrap();
    sign_time_expiry_recovers("sx-self", r#"{"id":"RC_SELF","kind":"self_registration","attendance_rate":100.0}"#, "RC_SELF");
}

/// R4.1 #2 double-sign guard: two accounts on one rollcall — A signs OK, B's session expires during its
/// sign. After B re-logins, ONLY B is re-signed (A stays signed exactly once, never double-signed).
#[test]
fn double_sign_guard_only_reauthed_account_resigns() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    events().lock().unwrap().clear();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r4-ds-{}", new_id())).to_string_lossy().replace('\\', "/");
    let step = |hz: &mut Harness, cmd: String| {
        let i = hz.next();
        send(hz.h, &cmd);
        wait_for(reply_ok(i), 10);
    };
    // inline boot of TWO accounts (usernames aa/bb → user_no aa/bb; the fake expires only bb's signs).
    step(&mut hz, format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#));
    step(&mut hz, r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2,"quiz_detect_secs":1,"poll_idle_secs":1,"prepare_retry_budget_secs":30}}"#.to_string());
    step(&mut hz, r#"{"id":3,"cmd":"CreateVault","master_password":"pw"}"#.to_string());
    step(&mut hz, format!(r#"{{"id":4,"cmd":"AddAccount","label":"acctA","school":"{base}","username":"aa","password":"secret"}}"#));
    step(&mut hz, format!(r#"{{"id":5,"cmd":"AddAccount","label":"acctB","school":"{base}","username":"bb","password":"secret"}}"#));
    let a = account_id("acctA").unwrap();
    let b = account_id("acctB").unwrap();
    post(&base, "/_test/expire_signs", r#"{"enabled":true,"user":"bb"}"#); // only B's signs expire
    step(&mut hz, r#"{"id":6,"cmd":"StartMonitoring"}"#.to_string());
    post(&base, "/_test/open_rollcall", r#"{"id":"RC_DS","kind":"self_registration","attendance_rate":100.0}"#);
    let sib = |acc: &str, ev: &Value| ev["event"] == "SignedIn" && ev["rollcall_id"] == "RC_DS" && ev["account_id"].as_str() == Some(acc);
    assert!(wait_for(|v| sib(&b, v), 30).is_some(), "B recovers + signs after its re-login");
    assert!(wait_for(|v| sib(&a, v), 30).is_some(), "A signs");
    let a_signins = snapshot().iter().filter(|v| sib(&a, v)).count();
    assert_eq!(a_signins, 1, "A must sign EXACTLY once — never re-signed by B's recovery");
    drop(hz);
}

/// R3c persistent-unanswerable: the LLM never answers. Within the (short) retry budget the paper is NEVER
/// submitted; at the deadline exactly one `quiz_unanswerable` Error is emitted.
#[test]
fn r3c_persistent_unanswerable_errors_no_submit() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("persist", &base, 2, 1); // 2s budget
    post(&base, "/_test/llm_fail_times", r#"{"times":999}"#);
    post(&base, "/_test/open_quiz", r#"{"activity_id":"PF1","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"why"}]}"#);
    assert!(wait_for(|v| v["event"] == "Error" && v["code"] == "quiz_unanswerable", 25).is_some(),
        "budget exhausted → one quiz_unanswerable error");
    assert!(!any(submitted("PF1")), "a persistently-unanswerable paper is never submitted (no half-paper)");
    drop(hz);
}
