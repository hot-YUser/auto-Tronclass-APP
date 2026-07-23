//! Live real-server integration tests — the ultimate ground truth (docs 90; the fake is offline & self-authored).
//! Every test is `#[ignore]`: they hit a REAL TronClass tenant + real LLM and only run when explicitly asked
//! with credentials in the environment. NEVER hardcode secrets here — the v1 runner script
//! (`scripts/_v2_live.py`) reads them from `config.conf` and exports them, so this file is safe in the repo.
//!
//!   cargo test --lib live_ -- --ignored --nocapture --test-threads=1
//!
//! Env: TRON_BASE_URL, TRON_USER, TRON_PASS, TRON_TEACHER_USER, TRON_TEACHER_PASS,
//!      TRON_LLM_KEY, TRON_LLM_ENDPOINT, TRON_LLM_MODEL, TRON_COURSE(=55379),
//!      and per-phase: TRON_EXAM_ID, TRON_ROLLCALL_ID.

use crate::answer::{self, Source};
use crate::llm::{self, EventCb, LlmConfig};
use crate::login::{self, LoginOutcome};
use crate::providers::Endpoints;
use crate::rollcall::{self, NumberCfg};
use std::collections::HashMap;
use cookie_store::CookieStore;
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use std::sync::Arc;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("env {k} not set — run via scripts/_v2_live.py"))
}

extern "C" fn noop_cb(_: *const u8, _: usize) {}

/// A fresh cookie-jar client, exactly like `engine::build_client` (cookie-only auth, no header injection).
fn new_client() -> Client {
    let jar = Arc::new(CookieStoreMutex::new(CookieStore::default()));
    Client::builder().cookie_provider(jar).build().expect("reqwest client")
}

/// Log in with the given creds → (authed client, endpoints, user_no). Panics on captcha/SSO/failure.
async fn authed(base: &str, user: &str, pass: &str) -> (Client, Endpoints, String) {
    let client = new_client();
    let ep = Endpoints::derive(base);
    match login::login(&client, &ep, user, pass).await {
        LoginOutcome::Ok => {}
        LoginOutcome::Failed(e) => panic!("login failed: {e}"),
        LoginOutcome::NeedCaptcha { .. } => panic!("login needs captcha — cannot run headless"),
    }
    assert!(login::verify_session(&client, &ep).await, "verify_session=false after login");
    (client, ep, login::user_no_from_username(user))
}

// ---- Phase 1: login parity (zero side effects) ----

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_login_student() {
    let (_, _, user_no) = authed(&env("TRON_BASE_URL"), &env("TRON_USER"), &env("TRON_PASS")).await;
    println!("student user_no = {user_no:?}");
    assert!(!user_no.is_empty(), "fetch_user_no empty");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_login_teacher() {
    let (_, _, user_no) =
        authed(&env("TRON_BASE_URL"), &env("TRON_TEACHER_USER"), &env("TRON_TEACHER_PASS")).await;
    println!("teacher user_no = {user_no:?}");
    assert!(!user_no.is_empty(), "fetch_user_no empty");
}

// ---- Phase 2: LLM connectivity (minimax reasoning; guards the empty-choices/max_tokens gotcha) ----

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_llm() {
    let cfg = LlmConfig {
        endpoint: env("TRON_LLM_ENDPOINT"),
        model: env("TRON_LLM_MODEL"),
        api_key: env("TRON_LLM_KEY"),
        max_tokens: 0, // → resolve_max_tokens floor (16384)
        enable_tools: false,
        max_tool_iterations: 0,
    };
    let client = new_client();
    let messages =
        vec![serde_json::json!({"role":"user","content":"單選題：2+2=? A) 3  B) 4  C) 5  D) 6"})];
    let ans = llm::answer_question(&client, &cfg, &messages, noop_cb as EventCb, "live", "q1", None).await;
    println!("llm answer = {ans:?}");
    let ans = ans.expect("llm returned None — empty choices? check max_tokens/thinking_mode");
    assert!(ans.to_uppercase().contains('B'), "expected the letter B, got {ans:?}");
}

// ---- School seed: probe every seeded base_url's /login and classify it with v2's detector ----
// No creds needed: `cargo test --lib live_school_probe -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_school_probe() {
    use crate::login::{detect_login_kind, LoginKind};
    let reg = crate::providers::Registry::factory();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .unwrap();

    let mut set = tokio::task::JoinSet::new();
    for s in reg.schools.clone() {
        let client = client.clone();
        set.spawn(async move {
            let url = format!("{}/login", s.base_url.trim_end_matches('/'));
            let (label, known) = match client.get(&url).send().await {
                Ok(r) => {
                    let status = r.status();
                    let final_url = r.url().to_string();
                    let html = r.text().await.unwrap_or_default();
                    let kind = match detect_login_kind(&html, &final_url) {
                        LoginKind::PasswordForm(_) => "PasswordForm",
                        LoginKind::PublicCloudEmail(_) => "PublicCloudEmail",
                        LoginKind::Captcha { .. } => "Captcha",
                        LoginKind::SsoRedirect => "SsoRedirect",
                        LoginKind::EmailSpa => "EmailSpa",
                        LoginKind::Unknown => "Unknown",
                    };
                    (format!("HTTP {} → {kind}", status.as_u16()), status.is_success() && kind != "Unknown")
                }
                Err(e) => (format!("ERR {e}"), false),
            };
            (s.key, s.base_url, label, known)
        });
    }
    let mut results = Vec::new();
    while let Some(Ok(r)) = set.join_next().await {
        results.push(r);
    }
    results.sort();
    let total = results.len();
    let known = results.iter().filter(|r| r.3).count();
    for (key, base, label, _) in &results {
        println!("{key:>10}  {base:<40}  {label}");
    }
    println!("\n{known}/{total} schools reachable + classified to a known login kind");
    assert!(total >= 30, "seed too small: {total}");
    assert!(known * 100 / total >= 60, "too many schools unreachable/unclassified: {known}/{total}");
}

// ---- Phase 6: full monitor pipeline via the engine FFI (poll → detect → gate → sign → SignedIn) ----
// Drives the REAL engine (Init/AddAccount/StartMonitoring) against a teacher-seeded RADAR rollcall
// (radar signs with an empty body — no brute force). Seed + cleanup: scripts/_v2_rollcall_live.py.

static MON_EVENTS: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
fn mon_events() -> &'static std::sync::Mutex<Vec<String>> {
    MON_EVENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}
extern "C" fn mon_collect(ptr: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(ptr, len) };
    mon_events().lock().unwrap().push(String::from_utf8_lossy(b).into_owned());
}
fn mon_send(h: *mut std::ffi::c_void, cmd: &serde_json::Value) {
    let s = cmd.to_string();
    unsafe { crate::core_send(h, s.as_ptr(), s.len()) };
}
fn mon_wait<F: Fn(&serde_json::Value) -> bool>(pred: F, secs: u64) -> Option<serde_json::Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        let hit = mon_events()
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .find(&pred);
        if hit.is_some() {
            return hit;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

#[test] // plain #[test]: the engine FFI owns its OWN runtime — a #[tokio::test] nests runtimes → abort.
#[ignore]
fn live_monitor_pipeline() {
    let base = env("TRON_BASE_URL");
    let user = env("TRON_USER");
    let pass = env("TRON_PASS");
    mon_events().lock().unwrap().clear();
    let h = crate::core_init(mon_collect);
    let dir = std::env::temp_dir()
        .join(format!("tron-live-{}", crate::config::new_id()))
        .to_string_lossy()
        .replace('\\', "/");

    mon_send(h, &serde_json::json!({"id":1,"cmd":"Init","data_dir":dir}));
    mon_wait(|v| v["event"] == "Reply" && v["id"] == 1, 10);
    // 1-student course → 0% present before signing, so drop the 15% gate; short countdown.
    mon_send(h, &serde_json::json!({"id":2,"cmd":"UpdateConfig","patch":{"attendance_gate_percent":0.0,"countdown_secs":2}}));
    mon_wait(|v| v["event"] == "Reply" && v["id"] == 2, 5);
    mon_send(h, &serde_json::json!({"id":3,"cmd":"CreateVault","master_password":"pw"}));
    mon_wait(|v| v["event"] == "Reply" && v["id"] == 3, 5);
    mon_send(h, &serde_json::json!({"id":4,"cmd":"AddAccount","label":"stu","school":base,"username":user,"password":pass}));
    mon_wait(|v| v["event"] == "Reply" && v["id"] == 4, 5);
    mon_send(h, &serde_json::json!({"id":5,"cmd":"StartMonitoring"}));
    mon_wait(|v| v["event"] == "Reply" && v["id"] == 5, 15);

    let online = mon_wait(|v| v["event"] == "AccountStatus" && v["state"] == "online", 25);
    println!("account online: {}", online.is_some());
    let signed = mon_wait(|v| v["event"] == "SignedIn", 45);
    println!("SignedIn event: {signed:?}");
    unsafe { crate::core_free(h) };
    assert!(signed.is_some(), "monitor did not detect+sign the seeded rollcall end-to-end");
}

// ---- Phase 4: auto-answer parity (needs a teacher-seeded live exam on 55379) ----
// Seed + cleanup are driven by scripts/_v2_exam_live.py (teacher creates a 6-question known-answer exam).

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_exam_answer() {
    let base = env("TRON_BASE_URL");
    let exam = env("TRON_EXAM_ID");
    let course = env("TRON_COURSE");
    let (client, ep, _) = authed(&base, &env("TRON_USER"), &env("TRON_PASS")).await;

    let paper = answer::fetch_paper(&client, &ep, Source::Exam, &exam, "").await.expect("fetch_paper");
    println!("exam subjects={} instance_id={:?}", paper.subjects.len(), paper.instance_id);
    assert!(!paper.subjects.is_empty(), "no subjects — exam not open / not distributed?");

    let cfg = LlmConfig {
        endpoint: env("TRON_LLM_ENDPOINT"),
        model: env("TRON_LLM_MODEL"),
        api_key: env("TRON_LLM_KEY"),
        max_tokens: 0,
        enable_tools: false,
        max_tool_iterations: 0,
    };
    let prior = HashMap::new();
    let answers =
        answer::shared_answers(&client, &cfg, noop_cb as EventCb, &exam, &course, &base, &paper.subjects, 4, &prior).await;
    let missing = answer::missing_subjects(&paper.subjects, &answers);
    println!("answered {}/{} subjects; missing={missing:?}", answers.len(), paper.subjects.len());
    assert!(missing.is_empty(), "LLM left subjects unanswered: {missing:?}");

    let (sid, retake) =
        answer::submit_exam(&client, &ep, &exam, &paper.instance_id, &answers, &paper.subjects).await.expect("submit_exam");
    println!("submitted sid={sid:?} retake={retake}");
    assert!(!sid.is_empty(), "empty submission_id");

    // Score check: the review GET carries the graded score — proves the submit BODIES scored, not just 2xx'd.
    if let Ok(r) = client.get(ep.exam_submission_review(&exam, &sid)).send().await {
        if let Ok(v) = r.json::<serde_json::Value>().await {
            let score = ["score", "exam_score", "objective_score", "total_score", "final_score"]
                .iter()
                .find_map(|k| v.get(*k).cloned());
            println!("review score fields: {score:?}");
        }
    }
}

// ---- Phase 5: rollcall parity (needs a teacher-seeded live rollcall on 55379) ----
// Seed + cleanup are driven by scripts/_v2_rollcall_live.py (teacher account); this test only signs.

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_number_rollcall() {
    let base = env("TRON_BASE_URL");
    let id = env("TRON_ROLLCALL_ID");
    let code = env("TRON_NUMBER_CODE");
    let (client, ep, user_no) = authed(&base, &env("TRON_USER"), &env("TRON_PASS")).await;

    // The IDOR read of the shared code from the roster (v2's primary, non-brute path).
    let read = rollcall::read_number_code(&client, &ep, &id).await;
    println!("read_number_code (IDOR) = {read:?}   (teacher set {code:?})");

    // Sign with the known code (avoid brute-forcing the real server). cfg is unused on this path.
    let cfg = NumberCfg { concurrency: 8, min_concurrency: 2, cooldown_ms: 200, max_cooldowns: 2 };
    let signed = rollcall::sign_number(&client, &ep, &id, "v2-live-test", Some(&code), cfg).await;
    println!("sign_number = {signed:?}");

    // The real success criterion is the roster: am I actually marked present?
    let present = rollcall::recheck_on_call_fine(&client, &ep, &id, &user_no).await;
    println!("recheck_on_call_fine = {present}");
    assert!(present, "student not on_call_fine after number sign (sign result: {signed:?})");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_radar_rollcall() {
    let base = env("TRON_BASE_URL");
    let id = env("TRON_ROLLCALL_ID");
    let (client, ep, user_no) = authed(&base, &env("TRON_USER"), &env("TRON_PASS")).await;

    let strategies = vec!["empty_answer".to_string(), "global_wgs84".to_string()];
    let signed = rollcall::sign_radar(&client, &ep, &id, &strategies, &user_no, "v2-live-test").await;
    println!("sign_radar = {signed:?}");
    let present = rollcall::recheck_on_call_fine(&client, &ep, &id, &user_no).await;
    println!("recheck_on_call_fine = {present}");
    assert!(present, "student not on_call_fine after radar sign (sign result: {signed:?})");
}

/// LLM-free proof of the exam SUBMIT body contract: numeric ids (string ids → HTTP 400, confirmed live).
/// Answers are arbitrary first-option guesses — we only assert the server ACCEPTS the submission body.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_exam_submit_contract() {
    let base = env("TRON_BASE_URL");
    let exam = env("TRON_EXAM_ID");
    let (client, ep, _) = authed(&base, &env("TRON_USER"), &env("TRON_PASS")).await;
    let paper = answer::fetch_paper(&client, &ep, Source::Exam, &exam, "").await.expect("fetch_paper");
    assert!(!paper.instance_id.is_empty(), "instance_id empty (int extraction regressed)");
    assert!(!paper.subjects.is_empty(), "no subjects — exam not open?");

    let mut answers = HashMap::new();
    for s in &paper.subjects {
        if let Some(first) = s.get("options").and_then(|v| v.as_array()).and_then(|o| o.first()) {
            let oid = first
                .get("id")
                .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
                .unwrap_or_default();
            if !oid.is_empty() {
                answers.insert(crate::quiz::subject_id(s), crate::quiz::Answer::Options(vec![oid]));
            }
        }
    }
    println!("guess answers for {}/{} subjects; instance_id={}", answers.len(), paper.subjects.len(), paper.instance_id);
    let (sid, retake) = answer::submit_exam(&client, &ep, &exam, &paper.instance_id, &answers, &paper.subjects)
        .await
        .expect("submit_exam rejected — string ids (id-type) regressed?");
    println!("guess submit OK sid={sid:?} retake={retake}");
    assert!(!sid.is_empty(), "empty submission_id");
    assert!(retake, "exam should allow retake");

    // Replay the review's leaked correct answers (announce_answer=immediate) → full marks. Validates the
    // resubmit_correct contract: read int `answer_option_ids` from the review, overlay, resubmit numerically.
    answer::resubmit_correct(&client, &ep, &exam, &sid, &answers, &paper.subjects).await.expect("resubmit_correct");
    // Poll the best score (grading can lag the resubmit by a beat).
    let mut score = None;
    for _ in 0..6 {
        let list: serde_json::Value =
            client.get(ep.exam_submissions(&exam)).send().await.unwrap().json().await.unwrap();
        score = list
            .get("exam_score")
            .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
        if score == Some(60.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    println!("exam_score after resubmit_correct = {score:?}  (max = 60.0)");
    assert_eq!(score, Some(60.0), "resubmit_correct did not reach full marks — leak-replay id-type?");
}

// ---- Phase 3: course_context (real course materials + PDF text) ----

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_course_context() {
    let base = env("TRON_BASE_URL");
    let (client, _, _) = authed(&base, &env("TRON_USER"), &env("TRON_PASS")).await;
    let course = env("TRON_COURSE");
    let text =
        crate::course_context::search_course_materials(&client, &base, &course, "講義 material pdf").await;
    let head: String = text.chars().take(600).collect();
    println!("course_materials ({} chars):\n{head}", text.len());
    // No non-empty assertion: a course may legitimately have no text material. This proves the
    // activities→material→upload_references→pdf chain runs against the real server without erroring.
}
