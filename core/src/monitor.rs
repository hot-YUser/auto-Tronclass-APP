//! Multi-account rollcall monitoring. Per-account **poller** tasks each poll their own rollcalls and
//! feed a single central **actor**. The actor merges detections into activities keyed by
//! `(base_url, kind, rollcall_id)`, runs the 15% gate + 15 s countdown, and dispatches a per-account
//! sign for every participant.
//!
//! DISCIPLINE: the actor loop does pure state/coordination and **never awaits network** — every HTTP
//! step (gate fetch, code read, radar solve, sign, on_call_fine recheck) is `tokio::spawn`ed and its
//! result comes back as a `MonitorMsg`. One slow account can never freeze the others' countdowns.

use crate::answer::{self, Source};
use crate::llm::LlmConfig;
use crate::login;
use crate::protocol::AnswerWire;
use crate::providers::Endpoints;
use crate::quiz::Answer;
use crate::rollcall::{self, RollcallKind, SignOutcome};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap as Map;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub type EventCb = extern "C" fn(*const u8, usize);

/// All events cross the seam through the single audited redaction pass (docs 90 §4).
fn emit(cb: EventCb, v: &Value) {
    crate::redaction::emit(cb, v);
}

fn command_reply(cb: EventCb, command_id: u64, result: Result<(), String>) {
    match result {
        Ok(()) => emit(
            cb,
            &json!({ "id": command_id, "event": "Reply", "ok": true, "error": null }),
        ),
        Err(error) => emit(
            cb,
            &json!({ "id": command_id, "event": "Reply", "ok": false, "error": error }),
        ),
    }
}

/// Per-account runtime context (session already authenticated by the engine). Carries the credentials
/// (vault-sourced) so a poller can re-login on session expiry without an engine round-trip. NB: NO
/// `#[derive(Debug)]` — the password (a `Secret`) must never be `{:?}`-logged (R4-D security).
pub struct Account {
    pub id: String,
    pub device_id: String,
    /// The account's own TronClass user id, captured at monitor start. Empty if capture failed —
    /// recheck then falls back to whole-class/top-level (never "any entry").
    pub user_no: String,
    pub is_teacher: bool,
    pub course_id: Option<String>,
    pub base_url: String,
    pub client: Client,
    pub username: String,
    pub password: crate::secrets::Secret,
}

type ActivityKey = (String, String, String); // (base_url, kind_str, rollcall_id)

/// R4.1 #2: bound sign re-login retries so a permanent 403 (not a real expiry) can't loop forever.
const MAX_RESIGN: u32 = 3;

pub struct Detected {
    account_id: String,
    base_url: String,
    rollcall_id: String,
    kind: RollcallKind,
    course: String,
}

pub(crate) enum MonitorMsg {
    Detected(Detected),
    GateResult { key: ActivityKey, rate: Option<f64> },
    CodeRead { key: ActivityKey, code: Option<String> },
    SignResult { key: ActivityKey, account_id: String, result: Result<SignOutcome, String> },
    SignNow { command_id: u64, activity_token: String },
    Defer { command_id: u64, activity_token: String },
    // --- quiz (slice 3) ---
    QuizDetected { account_id: String, base_url: String, source: String, course: String, course_id: String, activity_id: String, stem: String },
    QuizPrepared { key: ActivityKey, attempts: Vec<PreparedAttempt> },
    /// R3c all-or-nothing gate: prepare could NOT fully answer the paper (or a re-fetch failed / found
    /// the activity gone). `gone` → the activity closed (silent done); else re-prepare with `partial`
    /// carried, until `missing` clears or the retry budget deadline is hit.
    QuizPrepareRetry {
        key: ActivityKey,
        account_id: String,
        generation: u64,
        contract: Vec<Value>,
        partial: Map<String, Answer>,
        missing: Vec<String>,
    },
    QuizPrepareGone { key: ActivityKey, account_id: String, generation: u64 },
    QuizPrepareFailed {
        key: ActivityKey,
        account_id: String,
        generation: u64,
        code: String,
        message: String,
    },
    QuizSubmitResult { key: ActivityKey, account_id: String, result: Result<String, String> },
    QuizSubmitNow { command_id: u64, activity_token: String },
    QuizHold { command_id: u64, activity_token: String },
    QuizDiscard { command_id: u64, activity_token: String },
    QuizSetAnswer {
        command_id: u64,
        activity_token: String,
        account_id: String,
        subject_id: String,
        answer: AnswerWire,
    },
    // --- session expiry / re-login (R4-D) ---
    AuthLost { account_id: String },
    AuthRestored { account_id: String, ok: bool },
    /// Settings changed while monitoring: adopt them live (boxed — much larger than the other variants).
    ConfigUpdated(Box<MonitorConfig>),
    Stop,
}

struct Activity {
    activity_token: String,
    kind: RollcallKind,
    course: String,
    participants: HashSet<String>,
    attendance_rate: Option<f64>,
    number_code: Option<String>,
    code_requested: bool,
    gate_pending: bool,
    countdown_deadline: Option<Instant>,
    acted: bool,
    sign_pending: bool,                   // manual override waiting on a number code-read before it can sign
    signed: HashSet<String>,
    needs_resign: HashSet<String>,        // accounts whose sign hit a dead session → re-sign after re-login
    resign_attempts: HashMap<String, u32>, // per-account auth-lost re-sign count (bounds a permanent 403)
}

pub struct MonitorHandle {
    pub tx: UnboundedSender<MonitorMsg>,
    pub tasks: Vec<JoinHandle<()>>,
}

pub struct MonitorConfig {
    pub countdown_secs: u64,
    pub gate_percent: f64,
    pub llm_endpoint: String,
    pub llm_model: String,
    pub llm_key: Option<String>,
    pub llm_max_tokens: u32,
    pub max_answer_reask: u32,
    pub prepare_retry_budget_secs: u64,
    pub autoanswer_types: Vec<String>,
    pub enable_llm_tools: bool,
    pub max_tool_iterations: u32,
    pub resubmit_for_correct: bool,
    pub radar_strategy: Vec<String>,
    pub number_concurrency: u32,
    pub number_min_concurrency: u32,
    pub number_cooldown_ms: u64,
    pub number_max_cooldowns: u32,
    pub poll_idle_secs: u64,
    pub quiz_detect_secs: u64,
    pub operating: crate::config::Operating,
    pub tz_offset_minutes: i64,
}

impl MonitorConfig {
    fn llm(&self) -> LlmConfig {
        LlmConfig {
            endpoint: self.llm_endpoint.clone(),
            model: self.llm_model.clone(),
            api_key: self.llm_key.clone().unwrap_or_default(),
            max_tokens: self.llm_max_tokens,
            enable_tools: self.enable_llm_tools,
            max_tool_iterations: self.max_tool_iterations,
        }
    }

    /// The slice of the config each poller needs (cadences + schedule + family allowlist).
    fn tuning(&self) -> PollTuning {
        PollTuning {
            idle: Duration::from_secs(self.poll_idle_secs.max(1)),
            quiz_detect: Duration::from_secs(self.quiz_detect_secs.max(1)),
            operating: self.operating.clone(),
            tz_offset_minutes: self.tz_offset_minutes,
            wanted_types: self.autoanswer_types.clone(),
        }
    }
}

/// Per-poller tuning snapshot (schedule gate + cadences). Cloned into each poller at `start()`.
#[derive(Clone)]
struct PollTuning {
    idle: Duration,
    quiz_detect: Duration,
    operating: crate::config::Operating,
    tz_offset_minutes: i64,
    wanted_types: Vec<String>, // R4 auto-answer family allowlist (empty = all)
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Classified GET result for the poll canary: a genuine JSON body, an auth-lost signal, or a plain
/// transport/5xx failure.
enum Fetched {
    Ok(Value),
    AuthLost,
    Down,
}

/// GET `url` and classify the response for session-expiry (R4-D). Auth is lost on a `401`, a redirect
/// whose final path contains `login`, or a `2xx` whose body isn't JSON (a 200 login page — which the
/// old `json().unwrap_or(Null)` silently treated as healthy).
async fn fetch_classified(client: &Client, url: &str) -> Fetched {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Fetched::Down,
    };
    if resp.status().as_u16() == 401 {
        return Fetched::AuthLost;
    }
    if !resp.status().is_success() {
        return Fetched::Down;
    }
    if rollcall::response_url_is_login(resp.url()) {
        return Fetched::AuthLost; // redirected to a login URL (whole-url, lowercased — shared truth)
    }
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(body.trim()) {
        Ok(v) if v.is_object() || v.is_array() => Fetched::Ok(v),
        _ => Fetched::AuthLost, // 200 but not JSON → a login page was served
    }
}

/// Re-login on the account's OWN client (its cookie jar is shared, so a fresh session cookie overwrites
/// the stale one). A captcha school can't be driven unattended → `NeedCaptcha` counts as failure.
async fn relogin(acc: &Account) -> bool {
    let ep = Endpoints::derive(&acc.base_url);
    matches!(
        login::login(&acc.client, &ep, &acc.username, acc.password.expose()).await,
        login::LoginOutcome::Ok
    )
}

/// Spawn a re-login; emit the account's new online/offline status, then clear the actor's in-flight flag.
fn spawn_relogin(acc: Arc<Account>, tx: UnboundedSender<MonitorMsg>, cb: EventCb) {
    tokio::spawn(async move {
        let ok = relogin(&acc).await;
        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": acc.id,
                          "state": if ok { "online" } else { "offline" } }));
        tx.send(MonitorMsg::AuthRestored { account_id: acc.id.clone(), ok }).ok();
    });
}

/// Spawn the actor + one poller per account on the current tokio runtime.
pub fn start(cb: EventCb, accounts: Vec<Account>, cfg: MonitorConfig) -> MonitorHandle {
    let (tx, rx) = unbounded_channel();
    let map: HashMap<String, Arc<Account>> =
        accounts.into_iter().map(|a| (a.id.clone(), Arc::new(a))).collect();

    // Pollers read their tuning from a watch channel so a live `ConfigUpdated` reaches them too — the
    // actor owns the sender and republishes on every settings change (no stop/start needed).
    let (tune_tx, tune_rx) = watch::channel(cfg.tuning());

    let mut tasks = Vec::new();
    for acc in map.values() {
        tasks.push(tokio::spawn(poller(acc.clone(), tx.clone(), cb, tune_rx.clone())));
    }
    tasks.push(tokio::spawn(actor(cb, map, rx, tx.clone(), cfg, tune_tx)));

    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "monitoring" }));
    MonitorHandle { tx, tasks }
}

/// Poll one account's rollcalls; report each newly-seen rollcall once (the actor fetches fresh
/// attendance itself). Adaptive cadence: faster when something is active. Outside the operating-hours
/// schedule the poller neither polls nor detects (docs 20) — the actor stays alive but idle.
async fn poller(acc: Arc<Account>, tx: UnboundedSender<MonitorMsg>, cb: EventCb, mut tune_rx: watch::Receiver<PollTuning>) {
    let mut tune = tune_rx.borrow_and_update().clone();
    let ep = Endpoints::derive(&acc.base_url);
    let mut seen: HashSet<String> = HashSet::new();
    let mut courses: Vec<String> = Vec::new(); // refreshed every 300s (a new enrolment appears)
    let mut last_courses: Option<Instant> = None;
    let mut seen_quiz: HashSet<String> = HashSet::new();
    let mut voted_quiz: HashSet<String> = HashSet::new(); // interactions already voted (skip re-cast)
    let mut last_quiz: Option<Instant> = None; // None → detect on the very first open iteration
    let mut online = true; // the engine emitted the initial online; edge-trigger status changes only
    // ponytail: active=1s / idle=poll_idle_secs. The docs' 0.5s startup fast-window is a refinement —
    // the first poll is immediate anyway, so detection latency is already ~one interval.
    loop {
        if tx.is_closed() {
            break;
        }
        // Adopt a live settings change (cheap: only clones when the actor actually republished).
        if tune_rx.has_changed().unwrap_or(false) {
            tune = tune_rx.borrow_and_update().clone();
        }
        // Operating-hours gate: closed → skip polling + detection, re-check on a coarse cadence.
        if !tune.operating.is_open(now_epoch_secs(), tune.tz_offset_minutes) {
            crate::redaction::log_line(cb, "debug", &format!("schedule closed, {} idle", acc.id));
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }

        let interval = match fetch_classified(&acc.client, &ep.rollcalls()).await {
            Fetched::Ok(v) => {
                if !online {
                    // recovered from a transient blip → clear the stale offline badge (edge-triggered).
                    emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": acc.id, "state": "online" }));
                    online = true;
                }
                let list = extract_rollcalls(&v);
                let active = !list.is_empty();
                for rc in list {
                    let Some(id) = rollcall_id(&rc) else { continue };
                    if !seen.insert(id.clone()) {
                        continue; // already reported
                    }
                    tx.send(MonitorMsg::Detected(Detected {
                        account_id: acc.id.clone(),
                        base_url: acc.base_url.clone(),
                        rollcall_id: id,
                        kind: rollcall::classify(&rc),
                        course: course_name(&rc),
                    }))
                    .ok();
                }
                if active { Duration::from_secs(1) } else { tune.idle }
            }
            // The rollcall poll is the auth-lost canary (it runs every cycle): a 401 / redirect-to-login /
            // 200-login-page → ask the actor to re-login (it dedups). Covers a session lost mid-sign too.
            Fetched::AuthLost => {
                tx.send(MonitorMsg::AuthLost { account_id: acc.id.clone() }).ok();
                tune.idle
            }
            Fetched::Down => {
                if online {
                    emit(cb, &json!({ "id": null, "event": "AccountStatus",
                                      "account_id": acc.id, "state": "offline" }));
                    online = false;
                }
                tune.idle
            }
        };
        // Quiz detection on its own (slower) cadence, decoupled from the rollcall poll (docs 31).
        if last_quiz.is_none_or(|t| t.elapsed() >= tune.quiz_detect) {
            detect_quizzes(&acc, &ep, &tx, &mut courses, &mut last_courses, &mut seen_quiz, &mut voted_quiz, &tune.wanted_types).await;
            last_quiz = Some(Instant::now());
        }

        // Stop cleanly when the actor (and its receiver) is gone.
        if tx.is_closed() {
            break;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Refresh the account's courses (every 300s — a mid-semester enrolment is otherwise never seen), then
/// per course run ONE detector per enabled family, each with its real list endpoint + array key + gate
/// (v1 `_poll_course`). Emits each newly-answerable activity once with its family's canonical `source`.
#[allow(clippy::too_many_arguments)]
async fn detect_quizzes(
    acc: &Arc<Account>,
    ep: &Endpoints,
    tx: &UnboundedSender<MonitorMsg>,
    courses: &mut Vec<String>,
    last_courses: &mut Option<Instant>,
    seen: &mut HashSet<String>,
    voted: &mut HashSet<String>,
    wanted: &[String],
) {
    if last_courses.is_none_or(|t| t.elapsed() >= Duration::from_secs(300)) || courses.is_empty() {
        if let Ok(v) = get_json(&acc.client, &ep.my_courses()).await {
            let fresh: Vec<String> = first_array(&v, &["courses", "items", "data"]).iter().filter_map(course_id_of).collect();
            if !fresh.is_empty() {
                *courses = fresh;
            }
            *last_courses = Some(Instant::now());
        }
    }
    let want = |f: &str| wanted.is_empty() || wanted.iter().any(|w| w == f);
    let now = now_epoch_secs();
    for cid in courses.clone() {
        if want("exam") {
            for a in family_list(acc, &ep.course_exam_list(&cid), "exams").await {
                if exam_answerable(&a, now) {
                    emit_quiz(tx, acc, seen, "exam", &cid, &a, "");
                }
            }
        }
        if want("questionnaire") {
            for a in family_list(acc, &ep.course_questionnaire_list(&cid), "questionnaires").await {
                // v1: absent is_started → not started → skip.
                if field_or(&a, "is_started", false) && !field_or(&a, "is_closed", false) && !already_submitted(&a) {
                    emit_quiz(tx, acc, seen, "questionnaire", &cid, &a, "");
                }
            }
        }
        if want("homework") {
            for a in family_list(acc, &ep.course_homework(&cid), "homework_activities").await {
                if !field_or(&a, "is_closed", false) && !already_submitted(&a) {
                    let stem = a.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                    emit_quiz(tx, acc, seen, "homework", &cid, &a, &stem);
                }
            }
        }
        if want("vote") {
            detect_vote(acc, ep, tx, &cid, seen, voted).await;
        }
        if want("classroom") {
            for a in family_list(acc, &ep.course_classroom_list(&cid), "classrooms").await {
                // status stays "start" after 收答 closes but started_subjects_count drops to 0.
                if a.get("status").and_then(Value::as_str) == Some("start")
                    && a.get("started_subjects_count").and_then(Value::as_i64).unwrap_or(0) >= 1
                {
                    emit_quiz(tx, acc, seen, "classroom-exam", &cid, &a, "");
                }
            }
        }
        if want("courseware") {
            detect_courseware(acc, ep, tx, &cid, seen).await;
        }
    }
}

/// GET a family list endpoint and return its items (by `key`, bare-array fallback); [] on any error.
async fn family_list(acc: &Arc<Account>, url: &str, key: &str) -> Vec<Value> {
    match get_json(&acc.client, url).await {
        Ok(v) => extract_array(&v, key),
        Err(_) => Vec::new(),
    }
}

/// Dedup on `cid/aid`, then emit one QuizDetected with the family's canonical `source`.
fn emit_quiz(tx: &UnboundedSender<MonitorMsg>, acc: &Arc<Account>, seen: &mut HashSet<String>, source: &str, cid: &str, a: &Value, stem: &str) {
    let Some(aid) = id_of(a) else { return };
    if !seen.insert(format!("{cid}/{aid}")) {
        return;
    }
    tx.send(MonitorMsg::QuizDetected {
        account_id: acc.id.clone(),
        base_url: acc.base_url.clone(),
        source: source.to_string(),
        course: a.get("course_name").and_then(Value::as_str).unwrap_or("").to_string(),
        course_id: cid.to_string(),
        activity_id: aid,
        stem: stem.to_string(),
    })
    .ok();
}

/// First present array among `keys`, else a bare top-level array.
fn first_array(v: &Value, keys: &[&str]) -> Vec<Value> {
    for k in keys {
        if let Some(a) = v.get(*k).and_then(Value::as_array) {
            return a.clone();
        }
    }
    v.as_array().cloned().unwrap_or_default()
}

/// A course id from `id | course_id | courseId` (string or integer).
fn course_id_of(v: &Value) -> Option<String> {
    ["id", "course_id", "courseId"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string()))))
}

fn field_or(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}

/// Already-submitted across the family's variant field names (real tenants differ; §8 needs-real-account).
fn already_submitted(a: &Value) -> bool {
    ["has_submitted", "submitted", "is_submitted"].iter().any(|k| field_or(a, k, false))
}

/// Exam answerable gate (v1): started, not closed, not explicitly not-in-progress, window not past, not
/// already submitted, and attempts not exhausted.
fn exam_answerable(a: &Value, now: i64) -> bool {
    // v1: absent is_started means NOT started → skip (don't default-open).
    let started = field_or(a, "is_started", false);
    let closed = field_or(a, "is_closed", false);
    let in_progress = a.get("is_in_progress").and_then(Value::as_bool) != Some(false);
    let past = end_epoch(a).map(|e| e < now).unwrap_or(false);
    let times = a.get("submit_times").and_then(Value::as_i64).unwrap_or(0);
    let used = a.get("submission_count").and_then(Value::as_i64).unwrap_or(0);
    let exhausted = times > 0 && used >= times;
    started && !closed && in_progress && !past && !already_submitted(a) && !exhausted
}

/// `end_time` as a UTC epoch — a real tenant sends an ISO-8601 string (v1 `_iso_before_now`); tolerate a
/// bare integer epoch too. `None` (absent/unparseable) ⇒ the caller treats it as *not past* (never over-gate).
fn end_epoch(a: &Value) -> Option<i64> {
    let v = a.get("end_time")?;
    v.as_i64().or_else(|| v.as_str().and_then(iso8601_to_epoch))
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]` to a UTC epoch (civil-date math; no date crate across the
/// 4 ABIs). Lenient: missing tz → treated as UTC; anything unparseable → `None`.
fn iso8601_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let (y, m, day) = (d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?, d.next()?.parse::<i64>().ok()?);
    // Split the time from an optional trailing Z / ±HH:MM offset.
    let (time, off_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        (t, sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60))
    } else {
        (rest, 0)
    };
    let mut tp = time.split(':');
    let hh = tp.next()?.parse::<i64>().ok()?;
    let mm = tp.next()?.parse::<i64>().ok()?;
    let ss = tp.next().unwrap_or("0").split('.').next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // days_from_civil (Howard Hinnant): days since 1970-01-01.
    let yy = y - i64::from(m <= 2);
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss - off_secs)
}

/// vote: `interactions` → `type=="vote" && status=="start"`; then read the vote and skip if the caller
/// already voted (`user_no` ∈ `students[].user_no`), caching voted ids to avoid re-cast 400 churn.
async fn detect_vote(acc: &Arc<Account>, ep: &Endpoints, tx: &UnboundedSender<MonitorMsg>, cid: &str, seen: &mut HashSet<String>, voted: &mut HashSet<String>) {
    for a in family_list(acc, &ep.course_interactions(cid), "interactions").await {
        if a.get("type").and_then(Value::as_str) != Some("vote") || a.get("status").and_then(Value::as_str) != Some("start") {
            continue;
        }
        let Some(aid) = id_of(&a) else { continue };
        if voted.contains(&aid) || seen.contains(&format!("{cid}/{aid}")) {
            continue;
        }
        if let Ok(v) = get_json(&acc.client, &ep.votes_read(&aid)).await {
            let already = v.get("students").and_then(Value::as_array).map(|arr| {
                arr.iter().any(|s| s.get("user_no").and_then(Value::as_str).map(|u| u.eq_ignore_ascii_case(&acc.user_no)).unwrap_or(false))
            }).unwrap_or(false);
            if already {
                voted.insert(aid); // cache so we don't re-read/re-cast
                continue;
            }
        }
        emit_quiz(tx, acc, seen, "vote", cid, &a, "");
    }
}

/// courseware: generic activities filtered to `type=="material"`, then per material the quizzes chain;
/// each quiz gate `!is_closed && is_started!=false`, and skip when its `my-submission` is already truthy.
async fn detect_courseware(acc: &Arc<Account>, ep: &Endpoints, tx: &UnboundedSender<MonitorMsg>, cid: &str, seen: &mut HashSet<String>) {
    for m in family_list(acc, &ep.course_activities(cid), "activities").await {
        if m.get("type").and_then(Value::as_str) != Some("material") {
            continue;
        }
        let Some(mat_id) = id_of(&m) else { continue };
        for q in family_list(acc, &ep.courseware_quizzes(&mat_id), "quizzes").await {
            if field_or(&q, "is_closed", false) || !field_or(&q, "is_started", true) {
                continue;
            }
            let Some(qid) = id_of(&q) else { continue };
            if seen.contains(&format!("{cid}/{qid}")) {
                continue;
            }
            // Skip when already answered (a truthy my-submission object).
            let done = get_json(&acc.client, &ep.courseware_my_submission(&qid)).await
                .map(|v| v.is_object() && !v.as_object().map(|o| o.is_empty()).unwrap_or(true))
                .unwrap_or(false);
            if done {
                continue;
            }
            emit_quiz(tx, acc, seen, "courseware-quiz", cid, &q, "");
        }
    }
}

async fn actor(
    cb: EventCb,
    accounts: HashMap<String, Arc<Account>>,
    mut rx: UnboundedReceiver<MonitorMsg>,
    self_tx: UnboundedSender<MonitorMsg>,
    mut cfg: MonitorConfig,
    tune_tx: watch::Sender<PollTuning>,
) {
    let mut activities: HashMap<ActivityKey, Activity> = HashMap::new();
    let mut quizzes: HashMap<ActivityKey, QuizActivity> = HashMap::new();
    let mut reauth: HashSet<String> = HashSet::new(); // accounts with a re-login in flight (dedup)
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(msg) = maybe else { break };
                match msg {
                    MonitorMsg::Stop => break,
                    MonitorMsg::Detected(d) => on_detected(&mut activities, &accounts, &self_tx, cb, d),
                    MonitorMsg::GateResult { key, rate } => on_gate(&mut activities, &accounts, &self_tx, cb, &cfg, key, rate),
                    MonitorMsg::CodeRead { key, code } => {
                        // A manual override may be waiting on this code (below-gate number: gate held before
                        // the code-read step). Record it, then sign now if an override was pending.
                        let dispatch = match activities.get_mut(&key) {
                            Some(a) => { a.number_code = code; a.code_requested = true; std::mem::take(&mut a.sign_pending) }
                            None => false,
                        };
                        if dispatch { dispatch_signs(&mut activities, &accounts, &self_tx, &cfg, cb, &key); }
                    }
                    MonitorMsg::SignResult { key, account_id, result } => on_sign_result(&mut activities, &self_tx, cb, key, account_id, result),
                    MonitorMsg::SignNow { command_id, activity_token } => {
                        let result = find_activity_key(&activities, &activity_token)
                            .ok_or_else(|| "unknown rollcall activity_token".to_string())
                            .map(|key| on_sign_now(&mut activities, &accounts, &self_tx, &cfg, cb, &key));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::Defer { command_id, activity_token } => {
                        let result = find_activity_key(&activities, &activity_token)
                            .ok_or_else(|| "unknown rollcall activity_token".to_string())
                            .map(|key| on_defer(&mut activities, cb, &key));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizDetected { account_id, base_url, source, course, course_id, activity_id, stem } =>
                        on_quiz_detected(&mut quizzes, &accounts, &self_tx, &cfg, cb, base_url, source, course, course_id, activity_id, account_id, stem),
                    MonitorMsg::QuizPrepared { key, attempts } =>
                        on_quiz_prepared(&mut quizzes, &cfg, cb, key, attempts),
                    MonitorMsg::QuizPrepareRetry { key, account_id, generation, contract, partial, missing } =>
                        on_quiz_prepare_retry(&mut quizzes, &cfg, cb, key, account_id, generation, contract, partial, missing),
                    MonitorMsg::QuizPrepareGone { key, account_id, generation } =>
                        on_quiz_prepare_gone(&mut quizzes, &cfg, key, account_id, generation),
                    MonitorMsg::QuizPrepareFailed { key, account_id, generation, code, message } =>
                        on_quiz_prepare_failed(&mut quizzes, &cfg, cb, key, account_id, generation, code, message),
                    MonitorMsg::QuizSetAnswer { command_id, activity_token, account_id, subject_id, answer } => {
                        let result = on_quiz_set_answer(&mut quizzes, &cfg, cb, &activity_token, &account_id, &subject_id, answer);
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizSubmitNow { command_id, activity_token } => {
                        let result = find_quiz_key(&quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .and_then(|key| dispatch_quiz_submits(&mut quizzes, &accounts, &self_tx, &cfg, &key));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizHold { command_id, activity_token } => {
                        let result = find_quiz_mut(&mut quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .map(|q| { q.countdown_deadline = None; q.held = true; });
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizDiscard { command_id, activity_token } => {
                        let result = find_quiz_mut(&mut quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .map(|q| {
                                q.countdown_deadline = None;
                                q.discarded = true;
                                emit(cb, &json!({"id":null,"event":"LogLine","level":"info",
                                    "text":format!("quiz {} discarded", q.activity_id),
                                    "activity_token": q.activity_token}));
                            });
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizSubmitResult { key, account_id, result } => on_quiz_submit_result(&mut quizzes, cb, key, account_id, result),
                    MonitorMsg::AuthLost { account_id } => {
                        // Session expired mid-poll. Re-login once (dedup concurrent triggers); the poller
                        // keeps sending AuthLost each cycle until AuthRestored clears the in-flight flag.
                        if reauth.insert(account_id.clone()) {
                            match accounts.get(&account_id).cloned() {
                                Some(acc) => spawn_relogin(acc, self_tx.clone(), cb),
                                None => { reauth.remove(&account_id); }
                            }
                        }
                    }
                    // Settings changed mid-run: adopt them here AND republish the poller slice, so the
                    // change bites immediately (a held rollcall re-checks its gate on the next tick).
                    // Already-armed countdowns keep their deadline — only new ones use the new length.
                    MonitorMsg::ConfigUpdated(new) => {
                        cfg = *new;
                        let _ = tune_tx.send(cfg.tuning());
                    }
                    MonitorMsg::AuthRestored { account_id, ok } => {
                        reauth.remove(&account_id);
                        // Only on a SUCCESSFUL re-login do we re-sign the rollcalls this account lost.
                        if ok {
                            redispatch_signs(&mut activities, &accounts, &self_tx, &cfg, &account_id);
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                on_tick(&mut activities, &accounts, &self_tx, &cfg, cb);
                on_quiz_tick(&mut quizzes, &accounts, &self_tx, &cfg, cb);
            }
        }
    }
    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
}

fn on_detected(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cb: EventCb,
    d: Detected,
) {
    let key = (d.base_url.clone(), d.kind.as_str().to_string(), d.rollcall_id.clone());
    let entry = activities.entry(key.clone()).or_insert_with(|| Activity {
        activity_token: crate::config::new_id(),
        kind: d.kind,
        course: d.course.clone(),
        participants: HashSet::new(),
        attendance_rate: None,
        number_code: None,
        code_requested: false,
        gate_pending: true,
        countdown_deadline: None,
        acted: false,
        sign_pending: false,
        signed: HashSet::new(),
        needs_resign: HashSet::new(),
        resign_attempts: HashMap::new(),
    });
    let is_new_participant = entry.participants.insert(d.account_id.clone());
    if is_new_participant {
        emit_rollcall_detected(cb, &d.rollcall_id, &d.base_url, entry);
        // Kick a gate check the first time this activity is seen.
        if entry.gate_pending {
            spawn_gate_check(accounts, tx, &key, &d.account_id);
        }
    }
}

fn on_gate(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cb: EventCb,
    cfg: &MonitorConfig,
    key: ActivityKey,
    rate: Option<f64>,
) {
    let Some(a) = activities.get_mut(&key) else { return };
    a.attendance_rate = rate;
    let rate = rate.unwrap_or(0.0);
    if a.acted || a.countdown_deadline.is_some() {
        return;
    }
    // The UI renders this: while held there is no countdown, so it shows the LIVE class rate closing on
    // the threshold instead of an empty countdown slot, and swaps back on `holding:false`.
    let holding = rate + f64::EPSILON < cfg.gate_percent;
    emit(cb, &json!({ "id": null, "event": "RollcallGate", "rollcall_id": key.2,
                      "activity_token": a.activity_token,
                      "rate": a.attendance_rate, "gate_percent": cfg.gate_percent, "holding": holding }));
    if holding {
        // Below the anti-fake-rollcall gate → hold and re-check on the next detection window.
        a.gate_pending = true;
        emit(cb, &json!({ "id": null, "event": "LogLine", "level": "info",
                          "text": format!("rollcall {} below {:.0}% gate ({:.1}%), holding", key.2, cfg.gate_percent, rate) }));
        return;
    }
    a.gate_pending = false;
    a.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    // number: read the shared code once, now.
    if a.kind == RollcallKind::Number && !a.code_requested {
        a.code_requested = true;
        if let Some(acc_id) = a.participants.iter().next() {
            spawn_code_read(accounts, tx, &key, acc_id);
        }
    }
}

fn on_tick(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    cb: EventCb,
) {
    let now = Instant::now();
    let keys: Vec<ActivityKey> = activities.keys().cloned().collect();
    for key in keys {
        let Some(a) = activities.get_mut(&key) else { continue };
        if let Some(deadline) = a.countdown_deadline {
            if a.acted {
                continue;
            }
            let remaining = deadline.saturating_duration_since(now).as_secs();
            emit(cb, &json!({ "id": null, "event": "Countdown", "scope": "rollcall",
                              "activity_token": a.activity_token, "external_id": key.2,
                              "remaining_secs": remaining }));
            if now >= deadline {
                dispatch_signs(activities, accounts, tx, cfg, cb, &key);
            }
        } else if a.gate_pending && !a.acted {
            // Re-check the gate for activities still holding below threshold.
            if let Some(acc_id) = a.participants.iter().next().cloned() {
                spawn_gate_check(accounts, tx, &key, &acc_id);
            }
        }
    }
}

/// Manual override ("立即簽到"): sign the held rollcall NOW, bypassing the anti-fake gate. For a NUMBER
/// rollcall whose shared code hasn't been read yet (the gate held BEFORE the code-read step ran), read
/// the code first and sign the instant it lands — NEVER brute-force 0000–9999 against the real server
/// when the roster exposes the code. Fixes the reported「簽到率未達門檻時立即簽到沒反應」: a held number
/// rollcall silently brute-forced (thousands of PUTs, rate-limits, no timely sign) instead of signing.
fn on_sign_now(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
) {
    // Decide under a scoped borrow, then act once it ends (dispatch_signs re-borrows `activities`).
    enum Act {
        None,
        ReadCode(Option<String>),
        Dispatch,
    }
    let act = {
        let Some(a) = activities.get_mut(key) else { return };
        if a.acted {
            Act::None
        } else if a.kind == RollcallKind::Number && a.number_code.is_none() {
            // Held number without its code: read it, then sign on CodeRead (see the CodeRead arm).
            a.gate_pending = false;
            a.countdown_deadline = None;
            a.sign_pending = true;
            if a.code_requested {
                Act::ReadCode(None) // a read is already in flight; sign_pending fires when it lands
            } else {
                a.code_requested = true;
                Act::ReadCode(a.participants.iter().next().cloned())
            }
        } else {
            Act::Dispatch
        }
    };
    match act {
        Act::None | Act::ReadCode(None) => {}
        Act::ReadCode(Some(acc_id)) => spawn_code_read(accounts, tx, key, &acc_id),
        Act::Dispatch => dispatch_signs(activities, accounts, tx, cfg, cb, key),
    }
}

/// Dispatch a sign for every participant — each with its own session/device id. Marks the activity
/// acted so it fires once. QR routes through teacher-assist.
fn dispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
) {
    let Some(a) = activities.get_mut(key) else { return };
    if a.acted {
        return;
    }
    a.acted = true;
    a.countdown_deadline = None;
    let kind = a.kind;
    let code = a.number_code.clone();
    let participants: Vec<String> = a.participants.iter().cloned().collect();
    let rollcall_id = key.2.clone();
    let base_url = key.0.clone();
    let activity_token = a.activity_token.clone();
    let radar_strategy = cfg.radar_strategy.clone();
    let ncfg = rollcall::NumberCfg {
        concurrency: cfg.number_concurrency,
        min_concurrency: cfg.number_min_concurrency,
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };

    if kind == RollcallKind::Qr {
        // QR: needs a teacher account for this base_url; teacher sources data, students sign their own id.
        let teacher = accounts.values().find(|acc| acc.base_url == base_url && acc.is_teacher).cloned();
        match teacher {
            // course_id may be empty — the task falls back to the teacher's first my-course.
            Some(t) => {
                let students: Vec<Arc<Account>> =
                    participants.iter().filter_map(|id| accounts.get(id).cloned()).filter(|acc| !acc.is_teacher).collect();
                spawn_qr_teacher_assist(t, students, tx.clone(), key.clone());
            }
            None => emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                                     "code": "qr_needs_teacher",
                                     "activity_token": activity_token,
                                     "message": "偵測到 QR 點名,但此站台沒有教師帳號可輔助——請到「帳號」新增一個教師帳號並開啟 QR 輔助。" })),
        }
        return;
    }

    for acc_id in participants {
        let Some(acc) = accounts.get(&acc_id).cloned() else { continue };
        spawn_sign(acc, kind, code.clone(), rollcall_id.clone(), radar_strategy.clone(), ncfg, tx.clone(), key.clone());
    }
}

fn on_sign_result(
    activities: &mut HashMap<ActivityKey, Activity>,
    tx: &UnboundedSender<MonitorMsg>,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    result: Result<SignOutcome, String>,
) {
    let Some(a) = activities.get_mut(&key) else { return };
    match result {
        Ok(outcome) => {
            a.signed.insert(account_id.clone());
            a.needs_resign.remove(&account_id);
            a.resign_attempts.remove(&account_id);
            if a.number_code.is_none() {
                a.number_code = outcome.discovered_code.clone(); // share a brute-forced code
            }
            emit(cb, &json!({ "id": null, "event": "SignedIn", "rollcall_id": key.2,
                              "activity_token": a.activity_token,
                              "account_id": account_id, "course": a.course, "method": outcome.method }));
        }
        // Session died mid-sign (R4.1 #2): DON'T give up on the first hit — mark for re-sign, ask the
        // actor to re-login; `AuthRestored` re-dispatches this account (guarded by `signed`). BUT bound
        // it: a permanent 403 (not a real expiry) re-logins fine yet keeps failing → after MAX_RESIGN
        // give up with a hard sign_failed so it can't loop forever.
        Err(e) if rollcall::is_auth_lost(&e) => {
            let n = a.resign_attempts.entry(account_id.clone()).or_insert(0);
            *n += 1;
            if *n > MAX_RESIGN {
                a.needs_resign.remove(&account_id);
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                  "code": "sign_failed", "activity_token": a.activity_token,
                                  "message": format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} re-logins)") }));
            } else {
                a.needs_resign.insert(account_id.clone());
                emit(cb, &json!({ "id": null, "event": "LogLine", "level": "warn",
                                  "text": format!("rollcall {}: {account_id} session lost mid-sign, re-logging in", key.2) }));
                tx.send(MonitorMsg::AuthLost { account_id }).ok();
            }
        }
        Err(e) => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                    "code": "sign_failed", "activity_token": a.activity_token,
                                    "message": format!("{account_id}: {e}") })),
    }
}

/// After a re-login (R4.1 #2), re-dispatch a sign for ONLY the accounts that lost their session mid-sign
/// on each activity — guarded by `signed` so an already-signed account is never re-signed (no double-sign).
fn redispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    account_id: &str,
) {
    let Some(acc) = accounts.get(account_id).cloned() else { return };
    let ncfg = rollcall::NumberCfg {
        concurrency: cfg.number_concurrency,
        min_concurrency: cfg.number_min_concurrency,
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };
    for (key, a) in activities.iter_mut() {
        if a.needs_resign.remove(account_id) && !a.signed.contains(account_id) {
            spawn_sign(acc.clone(), a.kind, a.number_code.clone(), key.2.clone(), cfg.radar_strategy.clone(), ncfg, tx.clone(), key.clone());
        }
    }
}

fn on_defer(activities: &mut HashMap<ActivityKey, Activity>, cb: EventCb, key: &ActivityKey) {
    if let Some(a) = activities.get_mut(key) {
        a.countdown_deadline = None;
        a.gate_pending = false;
        emit(cb, &json!({ "id": null, "event": "PendingSignIn", "rollcall_id": key.2,
            "activity_token": a.activity_token }));
    }
}

// --- spawned network tasks (results return as messages; the actor never awaits these) ---

fn spawn_gate_check(accounts: &HashMap<String, Arc<Account>>, tx: &UnboundedSender<MonitorMsg>, key: &ActivityKey, acc_id: &str) {
    // Read the class attendance rate with a participant's authenticated session.
    let Some(acc) = accounts.get(acc_id).cloned() else { return };
    let (tx, key) = (tx.clone(), key.clone());
    let rollcall_id = key.2.clone();
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let rate = rollcall::attendance_rate(&acc.client, &ep, &rollcall_id).await;
        tx.send(MonitorMsg::GateResult { key, rate }).ok();
    });
}

fn spawn_code_read(accounts: &HashMap<String, Arc<Account>>, tx: &UnboundedSender<MonitorMsg>, key: &ActivityKey, acc_id: &str) {
    let Some(acc) = accounts.get(acc_id).cloned() else { return };
    let (tx, key) = (tx.clone(), key.clone());
    let rollcall_id = key.2.clone();
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let code = rollcall::read_number_code(&acc.client, &ep, &rollcall_id).await;
        tx.send(MonitorMsg::CodeRead { key, code }).ok();
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_sign(acc: Arc<Account>, kind: RollcallKind, code: Option<String>, rollcall_id: String, radar_strategy: Vec<String>, ncfg: rollcall::NumberCfg, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result = match kind {
            RollcallKind::Number => rollcall::sign_number(&acc.client, &ep, &rollcall_id, &acc.device_id, code.as_deref(), ncfg).await,
            RollcallKind::Radar => rollcall::sign_radar(&acc.client, &ep, &rollcall_id, &radar_strategy, &acc.user_no, &acc.device_id).await,
            RollcallKind::SelfRegistration => rollcall::sign_self_registration(&acc.client, &ep, &rollcall_id, &acc.user_no).await,
            RollcallKind::Qr | RollcallKind::Unknown => Err("unsupported here".into()),
        };
        tx.send(MonitorMsg::SignResult { key, account_id: acc.id.clone(), result }).ok();
    });
}

/// Teacher sources `data` from its own qr rollcall, then each student signs THEIR own rollcall_id
/// with that data (docs 32). Because the QR token is valid only ~1–4 s, this **re-sources and
/// re-sends** every ~1.5 s for up to ~12 s until each student confirms (one snapshot is not enough).
fn spawn_qr_teacher_assist(teacher: Arc<Account>, students: Vec<Arc<Account>>, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    let student_rollcall_id = key.2.clone();
    tokio::spawn(async move {
        let ep = Endpoints::derive(&teacher.base_url);
        // course_id: the teacher's, else fall back to its first my-course (don't just give up).
        let course_id = match teacher.course_id.clone() {
            Some(c) if !c.is_empty() => c,
            _ => first_course(&teacher.client, &ep).await.unwrap_or_default(),
        };

        // Teacher starts its OWN qr rollcall purely to source the rotating data (full create body).
        // ponytail: placeholder numeric/bool values; exact required fields need a real tenant to verify.
        let create_body = json!({
            "type": "qr_rollcall", "title": "auto", "status": "in_progress",
            "is_radar": false, "is_number": false, "number_code": null,
            "latitude": 0.0, "longitude": 0.0, "altitude": 0.0,
            "use_beacon": false, "duration": 3600, "student_rollcalls": []
        });
        let teacher_rollcall_id = match teacher.client.post(ep.teacher_create_rollcall(&course_id)).json(&create_body).send().await {
            Ok(r) => {
                let v = r.json::<Value>().await.unwrap_or(Value::Null);
                v.get("rollcall_id").or_else(|| v.get("id")).and_then(|x| x.as_str()).unwrap_or_default().to_string()
            }
            Err(_) => String::new(),
        };
        let _ = teacher.client.post(ep.teacher_start_rollcall(&teacher_rollcall_id)).send().await;

        let mut confirmed: HashSet<String> = HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(12);
        while confirmed.len() < students.len() && Instant::now() < deadline {
            if let Some(data) = rollcall::teacher_source_qr_data(&teacher.client, &ep, &course_id, &teacher_rollcall_id).await {
                for s in &students {
                    if confirmed.contains(&s.id) {
                        continue;
                    }
                    if let Ok(outcome) = rollcall::sign_qr_with_teacher_data(&s.client, &ep, &student_rollcall_id, &s.device_id, &data, &s.user_no).await {
                        confirmed.insert(s.id.clone());
                        tx.send(MonitorMsg::SignResult { key: key.clone(), account_id: s.id.clone(), result: Ok(outcome) }).ok();
                    }
                }
            }
            if confirmed.len() < students.len() {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
        for s in &students {
            if !confirmed.contains(&s.id) {
                tx.send(MonitorMsg::SignResult { key: key.clone(), account_id: s.id.clone(),
                    result: Err("qr: could not confirm within the token window".into()) }).ok();
            }
        }
        let _ = teacher.client.put(ep.teacher_stop_qr(&teacher_rollcall_id)).send().await; // close teacher end
    });
}

/// The teacher's first course id (my-courses) — the QR create fallback when no course_id is set.
async fn first_course(client: &Client, ep: &Endpoints) -> Option<String> {
    let v = get_json(client, &ep.my_courses()).await.ok()?;
    extract_array(&v, "courses").iter().find_map(id_of)
}

// --- small helpers ---

fn find_activity_key(activities: &HashMap<ActivityKey, Activity>, activity_token: &str) -> Option<ActivityKey> {
    activities
        .iter()
        .find(|(_, activity)| activity.activity_token == activity_token)
        .map(|(key, _)| key.clone())
}

fn emit_rollcall_detected(cb: EventCb, rollcall_id: &str, base_url: &str, a: &Activity) {
    let accounts: Vec<&String> = a.participants.iter().collect();
    emit(cb, &json!({ "id": null, "event": "RollcallDetected", "rollcall_id": rollcall_id,
                      "activity_token": a.activity_token,
                      "base_url": base_url, "kind": a.kind.as_str(), "course": a.course,
                      "attendance_rate": a.attendance_rate, "accounts": accounts }));
}

fn extract_rollcalls(v: &Value) -> Vec<Value> {
    v.get("rollcalls")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn rollcall_id(rc: &Value) -> Option<String> {
    rc.get("rollcall_id")
        .or_else(|| rc.get("id"))
        .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
}

fn course_name(rc: &Value) -> String {
    rc.get("course_name")
        .or_else(|| rc.get("course"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

// ================= quiz (slice 3) =================

struct QuizActivity {
    activity_token: String,
    source: Source,
    course: String,
    course_id: String, // R5: course the tool executor searches for materials
    activity_id: String,
    stem: String, // homework question stem, from the detection payload
    attempts: HashMap<String, PerAccountAttempt>,
    countdown_deadline: Option<Instant>,
    held: bool,
    discarded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptState {
    Waiting,
    Preparing,
    Ready,
    Submitting,
    Submitted,
    Gone,
    Failed,
}

struct PerAccountAttempt {
    state: AttemptState,
    prepare_generation: u64,
    prepare_at: Instant,
    prepare_deadline: Option<Instant>,
    answer_contract: Option<Vec<Value>>,
    instance_id: String,
    subjects: Vec<Value>,
    generated_answers: Map<String, Answer>,
    existing_answers: Map<String, Answer>,
    overrides: Map<String, Answer>,
    conflicts: HashSet<String>,
}

impl PerAccountAttempt {
    fn waiting(now: Instant) -> Self {
        Self {
            state: AttemptState::Waiting,
            prepare_generation: 0,
            prepare_at: now + Duration::from_millis(1200),
            prepare_deadline: None,
            answer_contract: None,
            instance_id: String::new(),
            subjects: Vec::new(),
            generated_answers: Map::new(),
            existing_answers: Map::new(),
            overrides: Map::new(),
            conflicts: HashSet::new(),
        }
    }
}

pub(crate) struct PreparedAttempt {
    account_id: String,
    generation: u64,
    instance_id: String,
    subjects: Vec<Value>,
    generated_answers: Map<String, Answer>,
    existing_answers: Map<String, Answer>,
}

#[derive(Clone)]
struct ReusableAnswers {
    contract: Vec<Value>,
    answers: Map<String, Answer>,
}

#[derive(Clone)]
struct PriorAnswers {
    contract: Vec<Value>,
    answers: Map<String, Answer>,
}

#[allow(clippy::too_many_arguments)]
fn on_quiz_detected(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    cb: EventCb,
    base_url: String,
    source: String,
    course: String,
    course_id: String,
    activity_id: String,
    account_id: String,
    stem: String,
) {
    let key = (base_url, format!("quiz:{source}"), activity_id.clone());
    let q = quizzes.entry(key.clone()).or_insert_with(|| QuizActivity {
        activity_token: crate::config::new_id(),
        source: Source::parse(&source),
        course,
        course_id,
        activity_id: activity_id.clone(),
        stem,
        attempts: HashMap::new(),
        countdown_deadline: None,
        held: false,
        discarded: false,
    });
    if !q.discarded && !q.attempts.contains_key(&account_id) {
        q.attempts.insert(account_id, PerAccountAttempt::waiting(Instant::now()));
        // A late participant invalidates a running countdown until its own paper and conflicts are known.
        q.countdown_deadline = None;
    }
    let _ = (accounts, tx, cfg, cb);
}

#[allow(clippy::too_many_arguments)]
fn on_quiz_prepared(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    prepared: Vec<PreparedAttempt>,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    if q.discarded {
        return;
    }
    for data in prepared {
        let Some(attempt) = q.attempts.get_mut(&data.account_id) else { continue };
        if attempt.state != AttemptState::Preparing
            || attempt.prepare_generation != data.generation
        {
            continue; // stale async completion after a terminal transition
        }
        attempt.answer_contract = Some(paper_contract(&data.subjects));
        attempt.instance_id = data.instance_id;
        attempt.subjects = data.subjects;
        attempt.generated_answers = data.generated_answers;
        attempt.existing_answers = data.existing_answers;
        attempt.overrides.clear();
        attempt.conflicts = attempt
            .existing_answers
            .iter()
            .filter(|(subject_id, existing)| {
                attempt
                    .generated_answers
                    .get(*subject_id)
                    .is_some_and(|generated| generated != *existing)
            })
            .map(|(subject_id, _)| subject_id.clone())
            .collect();
        attempt.state = AttemptState::Ready;
    }
    emit_quiz_prepared(cb, q);
    rearm_quiz_countdown(q, cfg);
}

/// R3c all-or-nothing retry: prepare could not fully answer the paper (or a re-fetch failed/was gone).
/// `gone` → the activity closed → silent done. Otherwise carry the partial answers and re-arm prepare
/// after a backoff, until `missing` clears or the minutes-scale budget deadline is hit (then one Error).
#[allow(clippy::too_many_arguments)]
fn on_quiz_prepare_retry(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    generation: u64,
    contract: Vec<Value>,
    partial: Map<String, Answer>,
    missing: Vec<String>,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else { return };
    if attempt.state != AttemptState::Preparing || attempt.prepare_generation != generation {
        return;
    }
    attempt.answer_contract = Some(contract);
    attempt.generated_answers = partial;
    let now = Instant::now();
    let deadline = *attempt
        .prepare_deadline
        .get_or_insert_with(|| now + Duration::from_secs(cfg.prepare_retry_budget_secs));
    if now >= deadline {
        attempt.state = AttemptState::Failed;
        let detail = if missing.is_empty() {
            "could not fetch the paper".to_string()
        } else {
            format!("unanswerable subjects: {}", missing.join(", "))
        };
        emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                          "code": "quiz_unanswerable", "activity_token": q.activity_token,
                          "account_id": account_id,
                          "message": format!("{}: {detail}", q.activity_id) }));
        rearm_quiz_countdown(q, cfg);
        return;
    }
    attempt.state = AttemptState::Waiting;
    attempt.prepare_at = now + Duration::from_secs(cfg.poll_idle_secs.max(1));
    q.countdown_deadline = None;
}

fn on_quiz_prepare_gone(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    key: ActivityKey,
    account_id: String,
    generation: u64,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else { return };
    if attempt.state == AttemptState::Preparing && attempt.prepare_generation == generation {
        attempt.state = AttemptState::Gone;
        rearm_quiz_countdown(q, cfg);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_quiz_prepare_failed(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    generation: u64,
    code: String,
    message: String,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else { return };
    if attempt.state != AttemptState::Preparing || attempt.prepare_generation != generation {
        return;
    }
    attempt.state = AttemptState::Failed;
    emit(cb, &json!({ "id": null, "event": "Error", "severity": "error", "code": code,
        "activity_token": q.activity_token, "account_id": account_id, "message": message }));
    rearm_quiz_countdown(q, cfg);
}

fn rearm_quiz_countdown(q: &mut QuizActivity, cfg: &MonitorConfig) {
    let preparation_pending = q
        .attempts
        .values()
        .any(|attempt| matches!(attempt.state, AttemptState::Waiting | AttemptState::Preparing));
    let unresolved_conflict = q.attempts.values().any(|attempt| !attempt.conflicts.is_empty());
    let has_ready = q.attempts.values().any(|attempt| attempt.state == AttemptState::Ready);
    if q.held || q.discarded || preparation_pending || unresolved_conflict || !has_ready {
        q.countdown_deadline = None;
    } else if q.countdown_deadline.is_none() {
        q.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    }
}

fn on_quiz_set_answer(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    activity_token: &str,
    account_id: &str,
    subject_id: &str,
    answer: AnswerWire,
) -> Result<(), String> {
    let q = find_quiz_mut(quizzes, activity_token)
        .ok_or_else(|| "unknown quiz activity_token".to_string())?;
    let attempt = q
        .attempts
        .get_mut(account_id)
        .ok_or_else(|| "account is not a participant in this quiz".to_string())?;
    if attempt.state != AttemptState::Ready {
        return Err("account paper is not ready for answer changes".to_string());
    }
    let subject = attempt
        .subjects
        .iter()
        .find(|subject| crate::quiz::subject_id(subject) == subject_id)
        .ok_or_else(|| "unknown subject for this quiz".to_string())?;
    let answer = answer.into_answer()?;
    crate::quiz::validate_answer(subject, &answer, q.source == Source::Vote)?;
    attempt.overrides.insert(subject_id.to_string(), answer.clone());
    attempt.conflicts.remove(subject_id);
    let answer_wire = AnswerWire::from_answer(&answer);
    let display_answer = answer_wire.display();
    emit(cb, &json!({ "id": null, "event": "AnswerUpdated", "quiz_id": q.activity_id,
                      "activity_token": q.activity_token, "account_id": account_id,
                      "subject_id": subject_id, "answer": answer_wire,
                      "display_answer": display_answer,
                      "source": "user", "conflict": false }));
    rearm_quiz_countdown(q, cfg);
    Ok(())
}

fn on_quiz_tick(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    cb: EventCb,
) {
    let now = Instant::now();
    let keys: Vec<ActivityKey> = quizzes.keys().cloned().collect();
    for key in keys {
        let Some(q) = quizzes.get_mut(&key) else { continue };
        let mut due_ids: Vec<String> = q
            .attempts
            .iter()
            .filter(|(_, attempt)| attempt.state == AttemptState::Waiting && now >= attempt.prepare_at)
            .map(|(account_id, _)| account_id.clone())
            .collect();
        due_ids.sort();
        if !due_ids.is_empty() {
            for account_id in due_ids
                .iter()
                .filter(|account_id| !accounts.contains_key(*account_id))
            {
                if let Some(attempt) = q.attempts.get_mut(account_id) {
                    attempt.state = AttemptState::Failed;
                }
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                    "code": "quiz_account_unavailable", "activity_token": q.activity_token,
                    "account_id": account_id, "message": "測驗帳號已不在監控工作階段中" }));
            }
            due_ids.retain(|account_id| accounts.contains_key(account_id));
            if due_ids.is_empty() {
                rearm_quiz_countdown(q, cfg);
                continue;
            }
            let participants: Vec<Arc<Account>> = due_ids
                .iter()
                .map(|account_id| accounts.get(account_id).expect("filtered account exists").clone())
                .collect();
            let priors: HashMap<String, PriorAnswers> = due_ids
                .iter()
                .filter_map(|account_id| {
                    let attempt = q.attempts.get(account_id)?;
                    Some((
                        account_id.clone(),
                        PriorAnswers {
                            contract: attempt.answer_contract.clone()?,
                            answers: attempt.generated_answers.clone(),
                        },
                    ))
                })
                .collect();
            let reusable: Vec<ReusableAnswers> = if cfg.enable_llm_tools {
                Vec::new()
            } else {
                q.attempts
                    .values()
                    .filter(|attempt| {
                        matches!(
                            attempt.state,
                            AttemptState::Ready | AttemptState::Submitting | AttemptState::Submitted
                        )
                    })
                    .map(|attempt| ReusableAnswers {
                        contract: paper_contract(&attempt.subjects),
                        answers: attempt.generated_answers.clone(),
                    })
                    .collect()
            };
            let mut generations = HashMap::new();
            for account_id in &due_ids {
                if let Some(attempt) = q.attempts.get_mut(account_id) {
                    attempt.prepare_generation = attempt.prepare_generation.wrapping_add(1).max(1);
                    attempt.state = AttemptState::Preparing;
                    generations.insert(account_id.clone(), attempt.prepare_generation);
                }
            }
            q.countdown_deadline = None;
            spawn_quiz_prepare(
                participants,
                q.source,
                q.activity_id.clone(),
                q.activity_token.clone(),
                q.course_id.clone(),
                q.stem.clone(),
                cfg.llm(),
                cfg.max_answer_reask,
                priors,
                generations,
                reusable,
                tx.clone(),
                key.clone(),
                cb,
            );
            continue;
        }

        let Some(deadline) = q.countdown_deadline else { continue };
        let remaining = deadline.saturating_duration_since(now).as_secs();
        emit(cb, &json!({ "id": null, "event": "Countdown", "scope": "quiz",
                          "activity_token": q.activity_token, "external_id": q.activity_id,
                          "remaining_secs": remaining }));
        if now >= deadline {
            let _ = dispatch_quiz_submits(quizzes, accounts, tx, cfg, &key);
        }
    }
}

fn dispatch_quiz_submits(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    key: &ActivityKey,
) -> Result<(), String> {
    let Some(q) = quizzes.get_mut(key) else { return Err("unknown quiz activity".to_string()) };
    if q.discarded {
        return Err("quiz was discarded".to_string());
    }
    if q
        .attempts
        .values()
        .any(|attempt| matches!(attempt.state, AttemptState::Waiting | AttemptState::Preparing))
    {
        return Err("quiz attempts are still preparing".to_string());
    }
    if q.attempts.values().any(|attempt| !attempt.conflicts.is_empty()) {
        return Err("quiz has unresolved answer conflicts".to_string());
    }
    q.countdown_deadline = None;
    let source = q.source;
    let resubmit = cfg.resubmit_for_correct;
    let activity_id = q.activity_id.clone();
    let ready_ids: Vec<String> = q
        .attempts
        .iter()
        .filter(|(_, attempt)| attempt.state == AttemptState::Ready)
        .map(|(account_id, _)| account_id.clone())
        .collect();
    if ready_ids.is_empty() {
        return Err("quiz has no ready attempts to submit".to_string());
    }
    let mut jobs = Vec::with_capacity(ready_ids.len());
    for account_id in &ready_ids {
        let account = accounts
            .get(account_id)
            .cloned()
            .ok_or_else(|| format!("quiz account {account_id} is unavailable"))?;
        let attempt = q.attempts.get(account_id).expect("ready attempt exists");
        let mut answers = attempt.generated_answers.clone();
        for (subject_id, answer) in &attempt.overrides {
            answers.insert(subject_id.clone(), answer.clone());
        }
        jobs.push((
            account,
            attempt.instance_id.clone(),
            attempt.subjects.clone(),
            answers,
        ));
    }
    for account_id in ready_ids {
        q.attempts.get_mut(&account_id).expect("ready attempt exists").state = AttemptState::Submitting;
    }
    for (account, instance_id, subjects, answers) in jobs {
        spawn_quiz_submit(
            account,
            source,
            activity_id.clone(),
            instance_id,
            subjects,
            answers,
            resubmit,
            tx.clone(),
            key.clone(),
        );
    }
    Ok(())
}

fn on_quiz_submit_result(quizzes: &mut HashMap<ActivityKey, QuizActivity>, cb: EventCb, key: ActivityKey, account_id: String, result: Result<String, String>) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    let Some(attempt) = q.attempts.get_mut(&account_id) else { return };
    match result {
        Ok(detail) => {
            attempt.state = AttemptState::Submitted;
            emit(cb, &json!({ "id": null, "event": "QuizSubmitted", "quiz_id": q.activity_id,
                "activity_token": q.activity_token, "account_id": account_id, "result": detail }));
        }
        Err(e) => {
            attempt.state = AttemptState::Ready;
            emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                "code": "quiz_submit_failed", "activity_token": q.activity_token,
                "account_id": account_id, "message": format!("{account_id}: {e}") }));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_quiz_prepare(
    participants: Vec<Arc<Account>>,
    source: Source,
    activity_id: String,
    activity_token: String,
    course_id: String,
    stem: String,
    llm: LlmConfig,
    max_reask: u32,
    priors: HashMap<String, PriorAnswers>,
    generations: HashMap<String, u64>,
    mut reusable: Vec<ReusableAnswers>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    cb: EventCb,
) {
    tokio::spawn(async move {
        let mut prepared = Vec::new();
        for account in participants {
            let account_id = account.id.clone();
            let generation = generations.get(&account_id).copied().unwrap_or_default();
            let prior_snapshot = priors.get(&account_id).cloned();
            let endpoints = Endpoints::derive(&account.base_url);
            let paper = match answer::fetch_paper(
                &account.client,
                &endpoints,
                source,
                &activity_id,
                &stem,
            )
            .await
            {
                Ok(paper) => paper,
                Err(_) => {
                    tx.send(MonitorMsg::QuizPrepareRetry {
                        key: key.clone(),
                        account_id,
                        generation,
                        contract: prior_snapshot
                            .as_ref()
                            .map(|prior| prior.contract.clone())
                            .unwrap_or_default(),
                        partial: prior_snapshot
                            .as_ref()
                            .map(|prior| prior.answers.clone())
                            .unwrap_or_default(),
                        missing: Vec::new(),
                    })
                    .ok();
                    continue;
                }
            };
            if paper.subjects.is_empty() {
                tx.send(MonitorMsg::QuizPrepareGone {
                    key: key.clone(),
                    account_id,
                    generation,
                })
                .ok();
                continue;
            }

            let contract = paper_contract(&paper.subjects);
            let prior = compatible_prior(prior_snapshot.as_ref(), &contract);
            let generated_answers = if prior.is_empty() && !llm.enable_tools {
                reusable
                    .iter()
                    .find(|entry| entry.contract == contract)
                    .map(|entry| entry.answers.clone())
            } else {
                None
            };
            let generated_answers = match generated_answers {
                Some(answers) => answers,
                None => {
                    answer::shared_answers(
                        &account.client,
                        &llm,
                        cb,
                        &activity_token,
                        &course_id,
                        &account.base_url,
                        &paper.subjects,
                        max_reask,
                        &prior,
                    )
                    .await
                }
            };
            let missing = answer::missing_subjects(&paper.subjects, &generated_answers);
            if !missing.is_empty() {
                if llm.api_key.trim().is_empty() {
                    tx.send(MonitorMsg::QuizPrepareFailed {
                        key: key.clone(),
                        account_id,
                        generation,
                        code: "llm_key_missing".to_string(),
                        message: format!(
                            "{activity_id}：尚未設定 LLM 金鑰，無法自動作答（請至 設定 → 儲存金鑰）"
                        ),
                    })
                    .ok();
                } else {
                    tx.send(MonitorMsg::QuizPrepareRetry {
                        key: key.clone(),
                        account_id,
                        generation,
                        contract,
                        partial: generated_answers,
                        missing,
                    })
                    .ok();
                }
                continue;
            }

            if !llm.enable_tools && !reusable.iter().any(|entry| entry.contract == contract) {
                reusable.push(ReusableAnswers {
                    contract,
                    answers: generated_answers.clone(),
                });
            }
            let existing_answers = paper
                .subjects
                .iter()
                .filter_map(|subject| {
                    answer::existing_answer(subject)
                        .map(|value| (crate::quiz::subject_id(subject), value))
                })
                .collect();
            prepared.push(PreparedAttempt {
                account_id,
                generation,
                instance_id: paper.instance_id,
                subjects: paper.subjects,
                generated_answers,
                existing_answers,
            });
        }
        if !prepared.is_empty() {
            tx.send(MonitorMsg::QuizPrepared {
                key,
                attempts: prepared,
            })
            .ok();
        }
    });
}

/// Remove only per-user response fields before comparing paper contracts. Correct-answer leaks remain:
/// they affect the generated answer and therefore must be identical before reuse is safe.
fn paper_contract(subjects: &[Value]) -> Vec<Value> {
    subjects
        .iter()
        .cloned()
        .map(|mut subject| {
            if let Some(object) = subject.as_object_mut() {
                for key in [
                    "student_answer_option_ids",
                    "student_answer",
                    "student_answers",
                    "my_answer",
                ] {
                    object.remove(key);
                }
            }
            subject
        })
        .collect()
}

fn compatible_prior(prior: Option<&PriorAnswers>, contract: &[Value]) -> Map<String, Answer> {
    prior
        .filter(|prior| prior.contract == contract)
        .map(|prior| prior.answers.clone())
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn spawn_quiz_submit(acc: Arc<Account>, source: Source, activity_id: String, instance_id: String, subjects: Vec<Value>, answers: Map<String, Answer>, resubmit: bool, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result: Result<String, String> = match source {
            Source::Exam => match answer::submit_exam(&acc.client, &ep, &activity_id, &instance_id, &answers, &subjects).await {
                Ok((sid, retake)) => {
                    // resubmit gate: EXAM + pref + the SUBMIT RESPONSE's allow_retake_exam + a submission
                    // id (v1 answer_flow.py:456) — a single-attempt exam must not burn its one graded attempt.
                    if resubmit && retake && !sid.is_empty() {
                        match answer::resubmit_correct(&acc.client, &ep, &activity_id, &sid, &answers, &subjects).await {
                            Ok(()) => Ok(format!("submitted {sid}; corrected resubmit completed")),
                            Err(error) => Err(format!("initial submit {sid} succeeded; correction pass failed: {error}")),
                        }
                    } else {
                        Ok(format!("submitted {sid}"))
                    }
                }
                Err(e) => Err(e),
            },
            Source::ClassroomExam => {
                // per-subject POST with the full exam wrapper (flat body → 400). ponytail: v1 also gates
                // on the server's started_subjects_count≥1 (R2.5); here each answered subject is posted.
                submit_classroom(&acc, &ep, &activity_id, &instance_id, &subjects, &answers)
                    .await
                    .map(|_| "submitted (classroom)".into())
            }
            Source::Questionnaire => {
                // exam wrapper (NOT courseware), to the questionnaire endpoint.
                let entries: Vec<Value> = subjects
                    .iter()
                    .filter_map(|s| answers.get(&crate::quiz::subject_id(s)).map(|a| answer::exam_subject_entry(s, a)))
                    .collect();
                post_json(&acc.client, &ep.questionnaire_submissions(&activity_id), &answer::questionnaire_body(&instance_id, &entries)).await.map(|_| "submitted (questionnaire)".into())
            }
            Source::Vote => {
                let letters: Vec<String> = answers.values().flat_map(vote_letters).collect();
                post_json(&acc.client, &ep.vote_cast(&activity_id), &answer::vote_body(&letters)).await.map(|_| "voted".into())
            }
            Source::CoursewareQuiz => {
                let items = source_items(&subjects, &answers);
                post_json(&acc.client, &ep.courseware_submissions(&activity_id), &answer::courseware_body(&items)).await.map(|_| "submitted (courseware)".into())
            }
            Source::Homework => {
                let text = answers.values().filter_map(answer_text).collect::<Vec<_>>().join("\n");
                post_json(&acc.client, &ep.homework_submissions(&activity_id), &answer::homework_body(&text)).await.map(|_| "submitted (homework)".into())
            }
        };
        tx.send(MonitorMsg::QuizSubmitResult { key, account_id: acc.id.clone(), result }).ok();
    });
}

fn emit_quiz_prepared(cb: EventCb, q: &QuizActivity) {
    let per_account: Vec<Value> = q
        .attempts
        .iter()
        .filter(|(_, attempt)| {
            matches!(
                attempt.state,
                AttemptState::Ready | AttemptState::Submitting | AttemptState::Submitted
            )
        })
        .map(|(account_id, attempt)| {
            let questions: Vec<Value> = attempt
                .subjects
                .iter()
                .map(|s| {
                    let sid = crate::quiz::subject_id(s);
                    let conflict = attempt.conflicts.contains(&sid);
                    let existing_answer = attempt.existing_answers.get(&sid);
                    let answer = attempt.generated_answers.get(&sid);
                    let answer_wire = answer.map(AnswerWire::from_answer);
                    let existing_wire = existing_answer.map(AnswerWire::from_answer);
                    let display_answer = answer_wire
                        .as_ref()
                        .map(AnswerWire::display)
                        .unwrap_or_default();
                    let options: Vec<Value> = s
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .map(|option| {
                                    json!({
                                        "id": id_of(option).unwrap_or_default(),
                                        "text": option.get("content")
                                            .or_else(|| option.get("description"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "subject_id": sid,
                        "parent_id": value_id(s.get("parent_id")),
                        "type": s.get("type").and_then(Value::as_str).unwrap_or(""),
                        "answer_type": s.get("answer_type").and_then(Value::as_str)
                            .or_else(|| s.get("type").and_then(Value::as_str)).unwrap_or(""),
                        "stem": subject_stem(s),
                        "options": options,
                        "answer": answer_wire,
                        "existing_answer": existing_wire,
                        "display_answer": display_answer,
                        "source": "llm",
                        "conflict": conflict
                    })
                })
                .collect();
            json!({ "account_id": account_id, "instance_id": attempt.instance_id, "questions": questions })
        })
        .collect();
    let conflict_count: usize = q.attempts.values().map(|attempt| attempt.conflicts.len()).sum();
    emit(cb, &json!({ "id": null, "event": "QuizPrepared", "schema_version": 1,
        "activity_token": q.activity_token, "quiz_id": q.activity_id,
        "activity": { "external_id": q.activity_id, "source": q.source.as_str(),
            "course_id": q.course_id, "course": q.course },
        "course": q.course, "per_account": per_account, "conflict_count": conflict_count }));
}

fn subject_stem(subject: &Value) -> &str {
    ["description", "content", "stem"]
        .iter()
        .find_map(|key| subject.get(*key).and_then(Value::as_str).filter(|text| !text.is_empty()))
        .unwrap_or("")
}

fn value_id(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value
            .as_str()
            .map(str::to_string)
            .or_else(|| value.as_i64().map(|id| id.to_string()))
            .or_else(|| value.as_u64().map(|id| id.to_string()))
    })
}

fn find_quiz_key(quizzes: &HashMap<ActivityKey, QuizActivity>, activity_token: &str) -> Option<ActivityKey> {
    quizzes
        .iter()
        .find(|(_, quiz)| quiz.activity_token == activity_token)
        .map(|(key, _)| key.clone())
}
fn find_quiz_mut<'a>(
    quizzes: &'a mut HashMap<ActivityKey, QuizActivity>,
    activity_token: &str,
) -> Option<&'a mut QuizActivity> {
    quizzes
        .values_mut()
        .find(|quiz| quiz.activity_token == activity_token)
}

async fn get_json(client: &Client, url: &str) -> Result<Value, String> {
    crate::http::json_checked(client.get(url), "fetch activity").await
}
async fn post_json(client: &Client, url: &str, body: &Value) -> Result<(), String> {
    crate::http::send_checked(client.post(url).json(body), "submit activity")
        .await
        .map(|_| ())
}

async fn submit_classroom(
    account: &Account,
    endpoints: &Endpoints,
    activity_id: &str,
    instance_id: &str,
    subjects: &[Value],
    answers: &Map<String, Answer>,
) -> Result<(), String> {
    let mut submitted = 0_usize;
    for subject in subjects {
        let subject_id = crate::quiz::subject_id(subject);
        let Some(answer) = answers.get(&subject_id) else {
            continue;
        };
        let body = answer::classroom_body(instance_id, subject, answer);
        let operation = format!("submit classroom subject {subject_id}");
        crate::http::send_checked(
            account
                .client
                .post(endpoints.classroom_submit(activity_id, &subject_id))
                .json(&body),
            &operation,
        )
        .await?;
        submitted += 1;
    }
    if submitted == 0 {
        return Err("submit classroom: no answered subjects".to_string());
    }
    Ok(())
}
fn extract_array(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).and_then(Value::as_array).or_else(|| v.as_array()).cloned().unwrap_or_default()
}
fn id_of(v: &Value) -> Option<String> {
    v.get("id")
        .or_else(|| v.get("activity_id"))
        .or_else(|| v.get("course_id"))
        .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
}

fn vote_letters(a: &Answer) -> Vec<String> {
    match a {
        Answer::Vote(l) => l.clone(),
        Answer::Options(o) => o.clone(),
        _ => vec![],
    }
}
fn answer_text(a: &Answer) -> Option<String> {
    match a {
        Answer::Text(t) => Some(t.clone()),
        Answer::Blanks(b) => Some(b.join(" ")),
        _ => None,
    }
}
/// (subject_id, answer_type, answer) for each answered subject — courseware's `subjects_answers` needs
/// all three (the answer_type falls back to the subject `type`).
fn source_items(subjects: &[Value], answers: &Map<String, Answer>) -> Vec<(String, String, Answer)> {
    subjects
        .iter()
        .filter_map(|s| {
            let sid = crate::quiz::subject_id(s);
            let atype = s
                .get("answer_type")
                .and_then(Value::as_str)
                .or_else(|| s.get("type").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            answers.get(&sid).map(|a| (sid, atype, a.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch_parses_z_offset_and_int() {
        // 2021-01-01T00:00:00Z = 1609459200.
        assert_eq!(iso8601_to_epoch("2021-01-01T00:00:00Z"), Some(1_609_459_200));
        // same instant expressed as +08:00 local (08:00 local == 00:00 UTC).
        assert_eq!(iso8601_to_epoch("2021-01-01T08:00:00+08:00"), Some(1_609_459_200));
        // fractional seconds + space separator tolerated.
        assert_eq!(iso8601_to_epoch("2021-01-01 00:00:00.500Z"), Some(1_609_459_200));
        assert_eq!(iso8601_to_epoch("not-a-date"), None);
        // end_epoch also accepts a bare integer epoch.
        assert_eq!(end_epoch(&json!({"end_time": 1_609_459_200_i64})), Some(1_609_459_200));
        assert_eq!(end_epoch(&json!({"end_time": "2021-01-01T00:00:00Z"})), Some(1_609_459_200));
        assert_eq!(end_epoch(&json!({})), None);
    }

    #[test]
    fn exam_answerable_gates_iso_expiry_and_absent_started() {
        let now = 1_700_000_000;
        // started, open, future end → answerable.
        assert!(exam_answerable(&json!({"is_started": true, "end_time": "2099-01-01T00:00:00Z"}), now));
        // a PAST ISO end_time → not answerable even though is_closed is false (the bug this fixes).
        assert!(!exam_answerable(&json!({"is_started": true, "is_closed": false, "end_time": "2000-01-01T00:00:00Z"}), now));
        // absent is_started → v1 treats as not-started → skip.
        assert!(!exam_answerable(&json!({"end_time": "2099-01-01T00:00:00Z"}), now));
        // absent end_time → not past → answerable.
        assert!(exam_answerable(&json!({"is_started": true}), now));
    }

    extern "C" fn noop_cb(_: *const u8, _: usize) {}

    fn cfg_countdown(secs: u64) -> MonitorConfig {
        MonitorConfig {
            countdown_secs: secs,
            gate_percent: 15.0,
            llm_endpoint: String::new(),
            llm_model: String::new(),
            llm_key: None,
            llm_max_tokens: 0,
            max_answer_reask: 0,
            prepare_retry_budget_secs: 0,
            autoanswer_types: vec![],
            enable_llm_tools: false,
            max_tool_iterations: 0,
            resubmit_for_correct: false,
            radar_strategy: vec![],
            number_concurrency: 1,
            number_min_concurrency: 1,
            number_cooldown_ms: 0,
            number_max_cooldowns: 0,
            poll_idle_secs: 5,
            quiz_detect_secs: 45,
            operating: crate::config::Operating::default(),
            tz_offset_minutes: 0,
        }
    }

    /// A single-account quiz with one unresolved conflict and no live countdown — the exact state
    /// after a user holds a paper that still has a conflict.
    fn quiz_with_conflict() -> (HashMap<ActivityKey, QuizActivity>, ActivityKey) {
        let key = ("http://x".to_string(), "quiz:exam".to_string(), "act1".to_string());
        let attempt = PerAccountAttempt {
            state: AttemptState::Ready,
            prepare_generation: 1,
            prepare_at: Instant::now(),
            prepare_deadline: None,
            answer_contract: Some(vec![json!({ "id": "subj1", "type": "short_answer", "content": "Question" })]),
            instance_id: "instance-acc1".to_string(),
            subjects: vec![json!({ "id": "subj1", "type": "short_answer", "content": "Question" })],
            generated_answers: Map::from([("subj1".to_string(), Answer::Text("llm".to_string()))]),
            existing_answers: Map::from([("subj1".to_string(), Answer::Text("old".to_string()))]),
            overrides: Map::new(),
            conflicts: HashSet::from(["subj1".to_string()]),
        };
        let q = QuizActivity {
            activity_token: "token1".to_string(),
            source: Source::Exam,
            course: String::new(),
            course_id: String::new(),
            activity_id: "act1".to_string(),
            stem: String::new(),
            attempts: HashMap::from([("acc1".to_string(), attempt)]),
            countdown_deadline: None,
            held: false,
            discarded: false,
        };
        let mut quizzes = HashMap::new();
        quizzes.insert(key.clone(), q);
        (quizzes, key)
    }

    #[test]
    fn held_quiz_does_not_rearm_countdown_when_conflict_resolves() {
        let (mut quizzes, key) = quiz_with_conflict();
        quizzes.get_mut(&key).unwrap().held = true; // user held while a conflict was still open
        let cfg = cfg_countdown(15);
        on_quiz_set_answer(
            &mut quizzes,
            &cfg,
            noop_cb,
            "token1",
            "acc1",
            "subj1",
            AnswerWire::Text { value: "x".into() },
        )
        .unwrap();
        let q = quizzes.get(&key).unwrap();
        assert!(q.attempts["acc1"].conflicts.is_empty(), "the conflict is resolved");
        assert!(q.countdown_deadline.is_none(), "a HELD quiz must not re-arm auto-submit — only SubmitNow may");
    }

    #[test]
    fn unheld_quiz_rearms_countdown_when_conflict_resolves() {
        let (mut quizzes, key) = quiz_with_conflict(); // held = false
        let cfg = cfg_countdown(15);
        on_quiz_set_answer(
            &mut quizzes,
            &cfg,
            noop_cb,
            "token1",
            "acc1",
            "subj1",
            AnswerWire::Text { value: "x".into() },
        )
        .unwrap();
        let q = quizzes.get(&key).unwrap();
        assert!(q.countdown_deadline.is_some(), "an un-held quiz re-arms once its last conflict clears");
    }

    #[test]
    fn invalid_typed_answer_does_not_resolve_conflict() {
        let (mut quizzes, key) = quiz_with_conflict();
        let cfg = cfg_countdown(15);

        let result = on_quiz_set_answer(
            &mut quizzes,
            &cfg,
            noop_cb,
            "token1",
            "acc1",
            "subj1",
            AnswerWire::Options {
                option_ids: vec!["not-for-text".into()],
            },
        );

        assert!(result.is_err());
        assert!(quizzes[&key].attempts["acc1"].conflicts.contains("subj1"));
        assert!(quizzes[&key].attempts["acc1"].overrides.is_empty());
    }

    #[test]
    fn late_participant_gets_own_waiting_attempt_and_cancels_countdown() {
        let (mut quizzes, key) = quiz_with_conflict();
        quizzes.get_mut(&key).unwrap().attempts.get_mut("acc1").unwrap().conflicts.clear();
        quizzes.get_mut(&key).unwrap().countdown_deadline = Some(Instant::now());
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_countdown(15);

        on_quiz_detected(
            &mut quizzes,
            &accounts,
            &tx,
            &cfg,
            noop_cb,
            "http://x".to_string(),
            "exam".to_string(),
            String::new(),
            String::new(),
            "act1".to_string(),
            "acc2".to_string(),
            String::new(),
        );

        let quiz = &quizzes[&key];
        assert_eq!(quiz.attempts["acc1"].instance_id, "instance-acc1");
        assert_eq!(quiz.attempts["acc2"].state, AttemptState::Waiting);
        assert!(quiz.countdown_deadline.is_none());
    }

    #[test]
    fn stale_prepare_generation_cannot_overwrite_new_attempt() {
        let (mut quizzes, key) = quiz_with_conflict();
        let attempt = quizzes.get_mut(&key).unwrap().attempts.get_mut("acc1").unwrap();
        attempt.state = AttemptState::Preparing;
        attempt.prepare_generation = 2;
        let cfg = cfg_countdown(15);

        on_quiz_prepared(
            &mut quizzes,
            &cfg,
            noop_cb,
            key.clone(),
            vec![PreparedAttempt {
                account_id: "acc1".to_string(),
                generation: 1,
                instance_id: "stale-instance".to_string(),
                subjects: vec![json!({ "id": "stale", "type": "short_answer" })],
                generated_answers: Map::from([(
                    "stale".to_string(),
                    Answer::Text("stale".to_string()),
                )]),
                existing_answers: Map::new(),
            }],
        );

        let attempt = &quizzes[&key].attempts["acc1"];
        assert_eq!(attempt.prepare_generation, 2);
        assert_eq!(attempt.instance_id, "instance-acc1");
        assert_eq!(attempt.state, AttemptState::Preparing);
    }

    #[test]
    fn paper_contract_ignores_existing_answer_but_not_option_identity() {
        let alice = vec![json!({
            "id": "s1",
            "type": "single_selection",
            "options": [{ "id": "alice-o1", "content": "A" }]
        })];
        let mut same_contract = alice.clone();
        same_contract[0]["student_answer_option_ids"] = json!(["alice-o1"]);
        let different_options = vec![json!({
            "id": "s1",
            "type": "single_selection",
            "options": [{ "id": "bob-o1", "content": "A" }]
        })];

        assert_eq!(paper_contract(&alice), paper_contract(&same_contract));
        assert_ne!(paper_contract(&alice), paper_contract(&different_options));
    }

    #[test]
    fn changed_paper_contract_drops_partial_answers() {
        let prior = PriorAnswers {
            contract: vec![json!({ "id": "s1", "options": [{ "id": "old" }] })],
            answers: Map::from([(
                "s1".to_string(),
                Answer::Options(vec!["old".to_string()]),
            )]),
        };
        let changed = vec![json!({ "id": "s1", "options": [{ "id": "new" }] })];

        assert!(compatible_prior(Some(&prior), &changed).is_empty());
        assert_eq!(compatible_prior(Some(&prior), &prior.contract), prior.answers);
    }

    #[test]
    fn gone_attempt_does_not_block_another_ready_account() {
        let (mut quizzes, key) = quiz_with_conflict();
        let quiz = quizzes.get_mut(&key).unwrap();
        quiz.attempts.get_mut("acc1").unwrap().conflicts.clear();
        quiz.attempts.insert("acc2".to_string(), PerAccountAttempt {
            state: AttemptState::Preparing,
            prepare_generation: 3,
            prepare_at: Instant::now(),
            prepare_deadline: None,
            answer_contract: None,
            instance_id: String::new(),
            subjects: Vec::new(),
            generated_answers: Map::new(),
            existing_answers: Map::new(),
            overrides: Map::new(),
            conflicts: HashSet::new(),
        });

        on_quiz_prepare_gone(&mut quizzes, &cfg_countdown(15), key.clone(), "acc2".to_string(), 3);

        assert_eq!(quizzes[&key].attempts["acc2"].state, AttemptState::Gone);
        assert!(quizzes[&key].countdown_deadline.is_some());
    }
}
