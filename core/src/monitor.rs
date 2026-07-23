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
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub type EventCb = extern "C" fn(*const u8, usize);

/// All events cross the seam through the single audited redaction pass (docs 90 §4).
fn emit(cb: EventCb, v: &Value) {
    crate::redaction::emit(cb, v);
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

pub enum MonitorMsg {
    Detected(Detected),
    GateResult { key: ActivityKey, rate: Option<f64> },
    CodeRead { key: ActivityKey, code: Option<String> },
    SignResult { key: ActivityKey, account_id: String, result: Result<SignOutcome, String> },
    SignNow { rollcall_id: String },
    Defer { rollcall_id: String },
    // --- quiz (slice 3) ---
    QuizDetected { account_id: String, base_url: String, source: String, course: String, course_id: String, activity_id: String, stem: String },
    QuizPrepared { key: ActivityKey, instance_id: String, subjects: Vec<Value>, shared: Map<String, Answer>, existing: Map<String, Map<String, Answer>> },
    /// R3c all-or-nothing gate: prepare could NOT fully answer the paper (or a re-fetch failed / found
    /// the activity gone). `gone` → the activity closed (silent done); else re-prepare with `partial`
    /// carried, until `missing` clears or the retry budget deadline is hit.
    QuizPrepareRetry { key: ActivityKey, partial: Map<String, Answer>, missing: Vec<String>, gone: bool },
    QuizSubmitResult { key: ActivityKey, account_id: String, result: Result<String, String> },
    QuizSubmitNow { quiz_id: String },
    QuizHold { quiz_id: String },
    QuizDiscard { quiz_id: String },
    QuizSetAnswer { quiz_id: String, account_id: String, subject_id: String, answer: Value },
    // --- session expiry / re-login (R4-D) ---
    AuthLost { account_id: String },
    AuthRestored { account_id: String, ok: bool },
    Stop,
}

struct Activity {
    kind: RollcallKind,
    course: String,
    participants: HashSet<String>,
    attendance_rate: Option<f64>,
    number_code: Option<String>,
    code_requested: bool,
    gate_pending: bool,
    countdown_deadline: Option<Instant>,
    acted: bool,
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

    let tune = PollTuning {
        idle: Duration::from_secs(cfg.poll_idle_secs.max(1)),
        quiz_detect: Duration::from_secs(cfg.quiz_detect_secs.max(1)),
        operating: cfg.operating.clone(),
        tz_offset_minutes: cfg.tz_offset_minutes,
        wanted_types: cfg.autoanswer_types.clone(),
    };

    let mut tasks = Vec::new();
    for acc in map.values() {
        tasks.push(tokio::spawn(poller(acc.clone(), tx.clone(), cb, tune.clone())));
    }
    tasks.push(tokio::spawn(actor(cb, map, rx, tx.clone(), cfg)));

    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "monitoring" }));
    MonitorHandle { tx, tasks }
}

/// Poll one account's rollcalls; report each newly-seen rollcall once (the actor fetches fresh
/// attendance itself). Adaptive cadence: faster when something is active. Outside the operating-hours
/// schedule the poller neither polls nor detects (docs 20) — the actor stays alive but idle.
async fn poller(acc: Arc<Account>, tx: UnboundedSender<MonitorMsg>, cb: EventCb, tune: PollTuning) {
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
    cfg: MonitorConfig,
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
                    MonitorMsg::CodeRead { key, code } => { if let Some(a) = activities.get_mut(&key) { a.number_code = code; a.code_requested = true; } }
                    MonitorMsg::SignResult { key, account_id, result } => on_sign_result(&mut activities, &self_tx, cb, key, account_id, result),
                    MonitorMsg::SignNow { rollcall_id } => { if let Some(key) = find_key(&activities, &rollcall_id) { dispatch_signs(&mut activities, &accounts, &self_tx, &cfg, cb, &key); } }
                    MonitorMsg::Defer { rollcall_id } => on_defer(&mut activities, cb, &rollcall_id),
                    MonitorMsg::QuizDetected { account_id, base_url, source, course, course_id, activity_id, stem } =>
                        on_quiz_detected(&mut quizzes, &accounts, &self_tx, &cfg, cb, base_url, source, course, course_id, activity_id, account_id, stem),
                    MonitorMsg::QuizPrepared { key, instance_id, subjects, shared, existing } =>
                        on_quiz_prepared(&mut quizzes, &cfg, cb, key, instance_id, subjects, shared, existing),
                    MonitorMsg::QuizPrepareRetry { key, partial, missing, gone } =>
                        on_quiz_prepare_retry(&mut quizzes, &cfg, cb, key, partial, missing, gone),
                    MonitorMsg::QuizSetAnswer { quiz_id, account_id, subject_id, answer } =>
                        on_quiz_set_answer(&mut quizzes, &cfg, cb, &quiz_id, &account_id, &subject_id, answer),
                    MonitorMsg::QuizSubmitNow { quiz_id } => { if let Some(key) = find_quiz_key(&quizzes, &quiz_id) { dispatch_quiz_submits(&mut quizzes, &accounts, &self_tx, &cfg, &key); } }
                    MonitorMsg::QuizHold { quiz_id } => { if let Some(q) = find_quiz_mut(&mut quizzes, &quiz_id) { q.countdown_deadline = None; q.held = true; } }
                    MonitorMsg::QuizDiscard { quiz_id } => { if let Some(q) = find_quiz_mut(&mut quizzes, &quiz_id) { q.countdown_deadline = None; q.discarded = true; emit(cb, &json!({"id":null,"event":"LogLine","level":"info","text":format!("quiz {quiz_id} discarded")})); } }
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
        kind: d.kind,
        course: d.course.clone(),
        participants: HashSet::new(),
        attendance_rate: None,
        number_code: None,
        code_requested: false,
        gate_pending: true,
        countdown_deadline: None,
        acted: false,
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
    if rate + f64::EPSILON < cfg.gate_percent {
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
                              "id_": key.2, "remaining_secs": remaining }));
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
                                     "code": "qr_needs_teacher", "message": format!("rollcall {rollcall_id}: qr needs a teacher account") })),
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
                                  "code": "sign_failed", "message": format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} re-logins)") }));
            } else {
                a.needs_resign.insert(account_id.clone());
                emit(cb, &json!({ "id": null, "event": "LogLine", "level": "warn",
                                  "text": format!("rollcall {}: {account_id} session lost mid-sign, re-logging in", key.2) }));
                tx.send(MonitorMsg::AuthLost { account_id }).ok();
            }
        }
        Err(e) => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                    "code": "sign_failed", "message": format!("{account_id}: {e}") })),
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

fn on_defer(activities: &mut HashMap<ActivityKey, Activity>, cb: EventCb, rollcall_id: &str) {
    if let Some(key) = find_key(activities, rollcall_id) {
        if let Some(a) = activities.get_mut(&key) {
            a.countdown_deadline = None;
            a.gate_pending = false;
            emit(cb, &json!({ "id": null, "event": "PendingSignIn", "rollcall_id": rollcall_id }));
        }
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

fn find_key(activities: &HashMap<ActivityKey, Activity>, rollcall_id: &str) -> Option<ActivityKey> {
    activities.keys().find(|k| k.2 == rollcall_id).cloned()
}

fn emit_rollcall_detected(cb: EventCb, rollcall_id: &str, base_url: &str, a: &Activity) {
    let accounts: Vec<&String> = a.participants.iter().collect();
    emit(cb, &json!({ "id": null, "event": "RollcallDetected", "rollcall_id": rollcall_id,
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
    source: Source,
    course: String,
    course_id: String, // R5: course the tool executor searches for materials
    activity_id: String,
    stem: String, // homework question stem, from the detection payload
    participants: HashSet<String>,
    detect_at: Option<Instant>,
    prepare_started: bool,
    prepare_deadline: Option<Instant>, // R3c: give-up deadline for the re-prepare retry budget
    instance_id: String,
    subjects: Vec<Value>,
    shared: Map<String, Answer>,                 // subject_id -> shared LLM/replay answer
    overrides: Map<String, Map<String, Answer>>, // account -> subject -> answer
    conflicts: Map<String, HashSet<String>>,     // account -> unresolved conflict subjects
    countdown_deadline: Option<Instant>,
    held: bool,
    discarded: bool,
    acted: bool,
    submitted: HashSet<String>,
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
        source: Source::parse(&source),
        course,
        course_id,
        activity_id: activity_id.clone(),
        stem,
        participants: HashSet::new(),
        detect_at: None,
        prepare_started: false,
        prepare_deadline: None,
        instance_id: String::new(),
        subjects: Vec::new(),
        shared: Map::new(),
        overrides: Map::new(),
        conflicts: Map::new(),
        countdown_deadline: None,
        held: false,
        discarded: false,
        acted: false,
        submitted: HashSet::new(),
    });
    q.participants.insert(account_id);
    // Prepare is kicked off from the tick after a short grace window, so all enrolled accounts that
    // detect the activity in the same poll cycle are gathered first (per-account conflicts need them).
    if q.detect_at.is_none() {
        q.detect_at = Some(Instant::now());
    }
    let _ = (accounts, tx, cfg, cb);
}

#[allow(clippy::too_many_arguments)]
fn on_quiz_prepared(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    instance_id: String,
    subjects: Vec<Value>,
    shared: Map<String, Answer>,
    existing: Map<String, Map<String, Answer>>,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    q.instance_id = instance_id;
    q.subjects = subjects;
    q.shared = shared;
    q.conflicts.clear();
    for (acc, ex) in existing {
        let mut cset = HashSet::new();
        for (sid, exa) in &ex {
            if q.shared.get(sid).is_some_and(|sha| sha != exa) {
                cset.insert(sid.clone()); // existing answer differs → conflict, keep existing (no overwrite)
            }
        }
        if !cset.is_empty() {
            q.conflicts.insert(acc.clone(), cset);
        }
        q.overrides.entry(acc).or_default().extend(ex);
    }
    emit_quiz_prepared(cb, q);
    let conflicts: usize = q.conflicts.values().map(|s| s.len()).sum();
    // Same `held` gate as on_quiz_set_answer: a re-prepare (R3c retry) of a quiz the user already held
    // must not re-arm the auto-submit countdown behind their back.
    if conflicts == 0 && !q.held {
        q.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    }
}

/// R3c all-or-nothing retry: prepare could not fully answer the paper (or a re-fetch failed/was gone).
/// `gone` → the activity closed → silent done. Otherwise carry the partial answers and re-arm prepare
/// after a backoff, until `missing` clears or the minutes-scale budget deadline is hit (then one Error).
fn on_quiz_prepare_retry(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    partial: Map<String, Answer>,
    missing: Vec<String>,
    gone: bool,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    if q.acted || q.discarded {
        return;
    }
    if gone {
        q.discarded = true; // activity closed → silent done (never a half-submit, no Error)
        q.countdown_deadline = None;
        return;
    }
    q.shared = partial; // carry answers resolved so far into the next prepare (leaked answers still win)
    let now = Instant::now();
    let deadline = *q
        .prepare_deadline
        .get_or_insert_with(|| now + Duration::from_secs(cfg.prepare_retry_budget_secs));
    if now >= deadline {
        // Budget exhausted → give up on THIS paper (never a half-submit); name the stuck subjects.
        q.discarded = true;
        q.countdown_deadline = None;
        let detail = if missing.is_empty() {
            "could not fetch the paper".to_string()
        } else {
            format!("unanswerable subjects: {}", missing.join(", "))
        };
        emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                          "code": "quiz_unanswerable", "message": format!("{}: {detail}", q.activity_id) }));
        return;
    }
    // Re-arm prepare after a ~poll-idle backoff; the tick's grace-gate re-spawns with q.shared as prior.
    q.prepare_started = false;
    q.detect_at = Some(now + Duration::from_secs(cfg.poll_idle_secs.max(1)));
}

fn on_quiz_set_answer(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    quiz_id: &str,
    account_id: &str,
    subject_id: &str,
    answer: Value,
) {
    let Some(q) = find_quiz_mut(quizzes, quiz_id) else { return };
    q.overrides.entry(account_id.to_string()).or_default().insert(subject_id.to_string(), answer_from_value(&answer));
    if let Some(cset) = q.conflicts.get_mut(account_id) {
        cset.remove(subject_id);
        if cset.is_empty() {
            q.conflicts.remove(account_id);
        }
    }
    emit(cb, &json!({ "id": null, "event": "AnswerUpdated", "quiz_id": quiz_id,
                      "account_id": account_id, "subject_id": subject_id, "source": "user", "conflict": false }));
    let conflicts: usize = q.conflicts.values().map(|s| s.len()).sum();
    // `held` gates the re-arm: once the user held this quiz, resolving a conflict must NOT restart the
    // auto-submit countdown (only an explicit SubmitNow may). Without this, hold-then-resolve silently
    // re-armed and auto-submitted — overriding the user's decision on the one path that acts for them.
    if conflicts == 0 && q.countdown_deadline.is_none() && !q.held && !q.acted && !q.discarded {
        q.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    }
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

        // Start prepare once the grace window has gathered all same-cycle participants.
        if !q.prepare_started {
            if let Some(t) = q.detect_at {
                if now.saturating_duration_since(t) >= Duration::from_millis(1200) {
                    q.prepare_started = true;
                    let participants: Vec<Arc<Account>> = q.participants.iter().filter_map(|id| accounts.get(id).cloned()).collect();
                    spawn_quiz_prepare(participants, q.source, q.activity_id.clone(), q.course_id.clone(), q.stem.clone(), cfg.llm(), cfg.max_answer_reask, q.shared.clone(), tx.clone(), key.clone(), cb);
                }
            }
            continue;
        }

        let Some(deadline) = q.countdown_deadline else { continue };
        if q.acted {
            continue;
        }
        let remaining = deadline.saturating_duration_since(now).as_secs();
        emit(cb, &json!({ "id": null, "event": "Countdown", "scope": "quiz",
                          "id_": q.activity_id, "remaining_secs": remaining }));
        if now >= deadline {
            dispatch_quiz_submits(quizzes, accounts, tx, cfg, &key);
        }
    }
}

fn dispatch_quiz_submits(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    cfg: &MonitorConfig,
    key: &ActivityKey,
) {
    let Some(q) = quizzes.get_mut(key) else { return };
    if q.acted || q.discarded {
        return;
    }
    q.acted = true;
    q.countdown_deadline = None;
    let (source, instance_id, subjects) = (q.source, q.instance_id.clone(), q.subjects.clone());
    let resubmit = cfg.resubmit_for_correct;
    let activity_id = q.activity_id.clone();
    for acc_id in q.participants.iter().cloned().collect::<Vec<_>>() {
        let Some(acc) = accounts.get(&acc_id).cloned() else { continue };
        let mut answers = q.shared.clone();
        if let Some(ov) = q.overrides.get(&acc_id) {
            for (k, v) in ov {
                answers.insert(k.clone(), v.clone());
            }
        }
        spawn_quiz_submit(acc, source, activity_id.clone(), instance_id.clone(), subjects.clone(), answers, resubmit, tx.clone(), key.clone());
    }
}

fn on_quiz_submit_result(quizzes: &mut HashMap<ActivityKey, QuizActivity>, cb: EventCb, key: ActivityKey, account_id: String, result: Result<String, String>) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    match result {
        Ok(detail) => {
            q.submitted.insert(account_id.clone());
            emit(cb, &json!({ "id": null, "event": "QuizSubmitted", "quiz_id": q.activity_id, "account_id": account_id, "result": detail }));
        }
        Err(e) => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error", "code": "quiz_submit_failed", "message": format!("{account_id}: {e}") })),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_quiz_prepare(participants: Vec<Arc<Account>>, source: Source, activity_id: String, course_id: String, stem: String, llm: LlmConfig, max_reask: u32, prior: Map<String, Answer>, tx: UnboundedSender<MonitorMsg>, key: ActivityKey, cb: EventCb) {
    tokio::spawn(async move {
        let Some(lead) = participants.first().cloned() else { return };
        let ep = Endpoints::derive(&lead.base_url);
        let paper = match answer::fetch_paper(&lead.client, &ep, source, &activity_id, &stem).await {
            Ok(p) => p,
            // R3c: a re-fetch failure is ambiguous (often a transient 404) → not-ready, retry; the actor
            // Errors only at the budget deadline. Carry the prior partial so answered subjects survive.
            Err(_) => {
                tx.send(MonitorMsg::QuizPrepareRetry { key, partial: prior, missing: Vec::new(), gone: false }).ok();
                return;
            }
        };
        // An empty paper = the activity closed / dropped out → silent done (v1-style), never an Error.
        if paper.subjects.is_empty() {
            tx.send(MonitorMsg::QuizPrepareRetry { key, partial: prior, missing: Vec::new(), gone: true }).ok();
            return;
        }
        let shared = answer::shared_answers(&lead.client, &llm, cb, &activity_id, &course_id, &lead.base_url, &paper.subjects, max_reask, &prior).await;
        let missing = answer::missing_subjects(&paper.subjects, &shared);
        if !missing.is_empty() {
            // No LLM key → the missing subjects can never be answered by retrying; fail fast with a clear
            // message instead of spinning the retry budget for minutes. (Leak-answered subjects already
            // fill `shared` WITHOUT the LLM, so a fully-leaked paper never reaches this branch.)
            if llm.api_key.trim().is_empty() {
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error", "code": "llm_key_missing",
                    "message": format!("{activity_id}：尚未設定 LLM 金鑰，無法自動作答（請至 設定 → 儲存金鑰）") }));
                tx.send(MonitorMsg::QuizPrepareRetry { key, partial: shared, missing, gone: true }).ok();
                return;
            }
            // All-or-nothing: never submit a half-paper — carry the partial answers and retry.
            tx.send(MonitorMsg::QuizPrepareRetry { key, partial: shared, missing, gone: false }).ok();
            return;
        }
        // Fully answered → gather each account's existing answers (conflict detection) and prepare.
        let mut existing: Map<String, Map<String, Answer>> = Map::new();
        for acc in &participants {
            let epa = Endpoints::derive(&acc.base_url);
            if let Ok(p) = answer::fetch_paper(&acc.client, &epa, source, &activity_id, &stem).await {
                let mut m = Map::new();
                for s in &p.subjects {
                    if let Some(a) = answer::existing_answer(s) {
                        m.insert(crate::quiz::subject_id(s), a);
                    }
                }
                if !m.is_empty() {
                    existing.insert(acc.id.clone(), m);
                }
            }
        }
        tx.send(MonitorMsg::QuizPrepared {
            key,
            instance_id: paper.instance_id,
            subjects: paper.subjects,
            shared,
            existing,
        })
        .ok();
    });
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
                        let _ = answer::resubmit_correct(&acc.client, &ep, &activity_id, &sid, &answers, &subjects).await;
                    }
                    Ok(format!("submitted {sid}"))
                }
                Err(e) => Err(e),
            },
            Source::ClassroomExam => {
                // per-subject POST with the full exam wrapper (flat body → 400). ponytail: v1 also gates
                // on the server's started_subjects_count≥1 (R2.5); here each answered subject is posted.
                for s in &subjects {
                    let sid = crate::quiz::subject_id(s);
                    if let Some(a) = answers.get(&sid) {
                        let body = answer::classroom_body(&instance_id, s, a);
                        let _ = acc.client.post(ep.classroom_submit(&activity_id, &sid)).json(&body).send().await;
                    }
                }
                Ok("submitted (classroom)".into())
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
        .participants
        .iter()
        .map(|acc| {
            let questions: Vec<Value> = q
                .subjects
                .iter()
                .map(|s| {
                    let sid = crate::quiz::subject_id(s);
                    let conflict = q.conflicts.get(acc).map(|c| c.contains(&sid)).unwrap_or(false);
                    json!({ "subject_id": sid, "conflict": conflict })
                })
                .collect();
            json!({ "account_id": acc, "questions": questions })
        })
        .collect();
    let conflict_count: usize = q.conflicts.values().map(|s| s.len()).sum();
    emit(cb, &json!({ "id": null, "event": "QuizPrepared", "quiz_id": q.activity_id, "course": q.course,
                      "per_account": per_account, "conflict_count": conflict_count }));
}

fn find_quiz_key(quizzes: &HashMap<ActivityKey, QuizActivity>, quiz_id: &str) -> Option<ActivityKey> {
    quizzes.iter().find(|(_, q)| q.activity_id == quiz_id).map(|(k, _)| k.clone())
}
fn find_quiz_mut<'a>(quizzes: &'a mut HashMap<ActivityKey, QuizActivity>, quiz_id: &str) -> Option<&'a mut QuizActivity> {
    quizzes.values_mut().find(|q| q.activity_id == quiz_id)
}

async fn get_json(client: &Client, url: &str) -> Result<Value, String> {
    client.get(url).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())
}
async fn post_json(client: &Client, url: &str, body: &Value) -> Result<(), String> {
    client.post(url).json(body).send().await.map_err(|e| e.to_string()).map(|_| ())
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

fn answer_from_value(v: &Value) -> Answer {
    if let Some(o) = v.get("options").and_then(Value::as_array) {
        Answer::Options(o.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
    } else if let Some(b) = v.get("blanks").and_then(Value::as_array) {
        Answer::Blanks(b.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
    } else if let Some(t) = v.get("text").and_then(Value::as_str) {
        Answer::Text(t.to_string())
    } else if let Some(vv) = v.get("vote").and_then(Value::as_array) {
        Answer::Vote(vv.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
    } else {
        Answer::Text(v.as_str().unwrap_or("").to_string())
    }
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
        let mut conflicts: Map<String, HashSet<String>> = Map::new();
        conflicts.insert("acc1".to_string(), HashSet::from(["subj1".to_string()]));
        let q = QuizActivity {
            source: Source::Exam,
            course: String::new(),
            course_id: String::new(),
            activity_id: "act1".to_string(),
            stem: String::new(),
            participants: HashSet::from(["acc1".to_string()]),
            detect_at: None,
            prepare_started: true,
            prepare_deadline: None,
            instance_id: String::new(),
            subjects: vec![],
            shared: Map::new(),
            overrides: Map::new(),
            conflicts,
            countdown_deadline: None,
            held: false,
            discarded: false,
            acted: false,
            submitted: HashSet::new(),
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
        on_quiz_set_answer(&mut quizzes, &cfg, noop_cb, "act1", "acc1", "subj1", json!("x"));
        let q = quizzes.get(&key).unwrap();
        assert!(q.conflicts.is_empty(), "the conflict is resolved");
        assert!(q.countdown_deadline.is_none(), "a HELD quiz must not re-arm auto-submit — only SubmitNow may");
    }

    #[test]
    fn unheld_quiz_rearms_countdown_when_conflict_resolves() {
        let (mut quizzes, key) = quiz_with_conflict(); // held = false
        let cfg = cfg_countdown(15);
        on_quiz_set_answer(&mut quizzes, &cfg, noop_cb, "act1", "acc1", "subj1", json!("x"));
        let q = quizzes.get(&key).unwrap();
        assert!(q.countdown_deadline.is_some(), "an un-held quiz re-arms once its last conflict clears");
    }
}
