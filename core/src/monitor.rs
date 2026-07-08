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

/// Per-account runtime context (session already authenticated by the engine).
pub struct Account {
    pub id: String,
    pub device_id: String,
    pub is_teacher: bool,
    pub course_id: Option<String>,
    pub base_url: String,
    pub client: Client,
}

type ActivityKey = (String, String, String); // (base_url, kind_str, rollcall_id)

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
    QuizDetected { account_id: String, base_url: String, source: String, course: String, activity_id: String },
    QuizPrepared { key: ActivityKey, instance_id: String, subjects: Vec<Value>, shared: Map<String, Answer>, existing: Map<String, Map<String, Answer>>, allow_retake: bool, reveal: bool },
    QuizSubmitResult { key: ActivityKey, account_id: String, result: Result<String, String> },
    QuizSubmitNow { quiz_id: String },
    QuizHold { quiz_id: String },
    QuizDiscard { quiz_id: String },
    QuizSetAnswer { quiz_id: String, account_id: String, subject_id: String, answer: Value },
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
    pub resubmit_for_correct: bool,
    pub radar_strategy: Vec<String>,
    pub number_concurrency: u32,
    pub number_cooldown_ms: u64,
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
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    let mut courses: Vec<String> = Vec::new(); // cached; the timetable rarely changes
    let mut seen_quiz: HashSet<String> = HashSet::new();
    let mut last_quiz: Option<Instant> = None; // None → detect on the very first open iteration
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

        let interval = match acc.client.get(ep.rollcalls()).send().await {
            Ok(resp) if resp.status().is_success() => {
                let v = resp.json::<Value>().await.unwrap_or(Value::Null);
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
            _ => {
                emit(cb, &json!({ "id": null, "event": "AccountStatus",
                                  "account_id": acc.id, "state": "offline" }));
                tune.idle
            }
        };
        // Quiz detection on its own (slower) cadence, decoupled from the rollcall poll (docs 31).
        if last_quiz.is_none_or(|t| t.elapsed() >= tune.quiz_detect) {
            detect_quizzes(&acc, &ep, &tx, &mut courses, &mut seen_quiz).await;
            last_quiz = Some(Instant::now());
        }

        // Stop cleanly when the actor (and its receiver) is gone.
        if tx.is_closed() {
            break;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Fetch the account's courses once (cached), then per course look for in-progress answerable
/// activities and report each new one. `/distribute` is never used for detection (it churns).
async fn detect_quizzes(
    acc: &Arc<Account>,
    ep: &Endpoints,
    tx: &UnboundedSender<MonitorMsg>,
    courses: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if courses.is_empty() {
        if let Ok(v) = get_json(&acc.client, &ep.my_courses()).await {
            *courses = extract_array(&v, "courses").iter().filter_map(id_of).collect();
        }
    }
    for cid in courses.iter() {
        for url in [ep.course_activities(cid), ep.course_exams(cid), ep.course_homework(cid)] {
            let Ok(v) = get_json(&acc.client, &url).await else { continue };
            for a in extract_array(&v, "activities") {
                // "currently answerable": is_in_progress==false excludes; missing → keep (fall back).
                if a.get("is_in_progress").and_then(Value::as_bool) == Some(false) {
                    continue;
                }
                let Some(aid) = id_of(&a) else { continue };
                if !seen.insert(format!("{cid}/{aid}")) {
                    continue;
                }
                tx.send(MonitorMsg::QuizDetected {
                    account_id: acc.id.clone(),
                    base_url: acc.base_url.clone(),
                    source: a.get("type").and_then(Value::as_str).unwrap_or("exam").to_string(),
                    course: a.get("course_name").and_then(Value::as_str).unwrap_or("").to_string(),
                    activity_id: aid,
                })
                .ok();
            }
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
                    MonitorMsg::SignResult { key, account_id, result } => on_sign_result(&mut activities, cb, key, account_id, result),
                    MonitorMsg::SignNow { rollcall_id } => { if let Some(key) = find_key(&activities, &rollcall_id) { dispatch_signs(&mut activities, &accounts, &self_tx, &cfg, cb, &key); } }
                    MonitorMsg::Defer { rollcall_id } => on_defer(&mut activities, cb, &rollcall_id),
                    MonitorMsg::QuizDetected { account_id, base_url, source, course, activity_id } =>
                        on_quiz_detected(&mut quizzes, &accounts, &self_tx, &cfg, cb, base_url, source, course, activity_id, account_id),
                    MonitorMsg::QuizPrepared { key, instance_id, subjects, shared, existing, allow_retake, reveal } =>
                        on_quiz_prepared(&mut quizzes, &cfg, cb, key, instance_id, subjects, shared, existing, allow_retake, reveal),
                    MonitorMsg::QuizSetAnswer { quiz_id, account_id, subject_id, answer } =>
                        on_quiz_set_answer(&mut quizzes, &cfg, cb, &quiz_id, &account_id, &subject_id, answer),
                    MonitorMsg::QuizSubmitNow { quiz_id } => { if let Some(key) = find_quiz_key(&quizzes, &quiz_id) { dispatch_quiz_submits(&mut quizzes, &accounts, &self_tx, &cfg, &key); } }
                    MonitorMsg::QuizHold { quiz_id } => { if let Some(q) = find_quiz_mut(&mut quizzes, &quiz_id) { q.countdown_deadline = None; q.held = true; } }
                    MonitorMsg::QuizDiscard { quiz_id } => { if let Some(q) = find_quiz_mut(&mut quizzes, &quiz_id) { q.countdown_deadline = None; q.discarded = true; emit(cb, &json!({"id":null,"event":"LogLine","level":"info","text":format!("quiz {quiz_id} discarded")})); } }
                    MonitorMsg::QuizSubmitResult { key, account_id, result } => on_quiz_submit_result(&mut quizzes, cb, key, account_id, result),
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
    let (num_conc, num_cooldown) = (cfg.number_concurrency, cfg.number_cooldown_ms);

    if kind == RollcallKind::Qr {
        // QR: needs a teacher account for this base_url; teacher sources data, students sign their own id.
        let teacher = accounts.values().find(|acc| acc.base_url == base_url && acc.is_teacher).cloned();
        match teacher {
            Some(t) if t.course_id.is_some() => {
                let students: Vec<Arc<Account>> =
                    participants.iter().filter_map(|id| accounts.get(id).cloned()).filter(|acc| !acc.is_teacher).collect();
                spawn_qr_teacher_assist(t, students, tx.clone(), key.clone());
            }
            _ => emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                                   "code": "qr_needs_teacher", "message": format!("rollcall {rollcall_id}: qr needs a teacher account") })),
        }
        return;
    }

    for acc_id in participants {
        let Some(acc) = accounts.get(&acc_id).cloned() else { continue };
        spawn_sign(acc, kind, code.clone(), rollcall_id.clone(), radar_strategy.clone(), num_conc, num_cooldown, tx.clone(), key.clone());
    }
}

fn on_sign_result(
    activities: &mut HashMap<ActivityKey, Activity>,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    result: Result<SignOutcome, String>,
) {
    let Some(a) = activities.get_mut(&key) else { return };
    match result {
        Ok(outcome) => {
            a.signed.insert(account_id.clone());
            if a.number_code.is_none() {
                a.number_code = outcome.discovered_code.clone(); // share a brute-forced code
            }
            emit(cb, &json!({ "id": null, "event": "SignedIn", "rollcall_id": key.2,
                              "account_id": account_id, "course": a.course, "method": outcome.method }));
        }
        Err(e) => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                    "code": "sign_failed", "message": format!("{account_id}: {e}") })),
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
fn spawn_sign(acc: Arc<Account>, kind: RollcallKind, code: Option<String>, rollcall_id: String, radar_strategy: Vec<String>, num_conc: u32, num_cooldown: u64, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result = match kind {
            RollcallKind::Number => rollcall::sign_number(&acc.client, &ep, &rollcall_id, &acc.device_id, code.as_deref(), num_conc, num_cooldown).await,
            RollcallKind::Radar => rollcall::sign_radar(&acc.client, &ep, &rollcall_id, &radar_strategy).await,
            RollcallKind::SelfRegistration => rollcall::sign_self_registration(&acc.client, &ep, &rollcall_id).await,
            RollcallKind::Qr | RollcallKind::Unknown => Err("unsupported here".into()),
        };
        tx.send(MonitorMsg::SignResult { key, account_id: acc.id.clone(), result }).ok();
    });
}

/// Teacher sources `data` from its own qr rollcall, then each student signs THEIR own rollcall_id
/// with that data (docs 32). One task signs all students, then messages a result per student.
fn spawn_qr_teacher_assist(teacher: Arc<Account>, students: Vec<Arc<Account>>, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    let student_rollcall_id = key.2.clone();
    tokio::spawn(async move {
        let ep = Endpoints::derive(&teacher.base_url);
        let course_id = teacher.course_id.clone().unwrap_or_default();

        // Teacher starts its OWN qr rollcall purely to source the rotating data.
        let teacher_rollcall_id = match teacher.client.post(ep.teacher_create_rollcall(&course_id)).json(&json!({ "type": "qr" })).send().await {
            Ok(r) => {
                let v = r.json::<Value>().await.unwrap_or(Value::Null);
                v.get("rollcall_id").or_else(|| v.get("id")).and_then(|x| x.as_str()).unwrap_or_default().to_string()
            }
            Err(_) => String::new(),
        };
        let _ = teacher.client.post(ep.teacher_start_rollcall(&teacher_rollcall_id)).send().await;

        let data = rollcall::teacher_source_qr_data(&teacher.client, &ep, &course_id, &teacher_rollcall_id).await;
        for s in &students {
            let result = match &data {
                Some(data) => rollcall::sign_qr_with_teacher_data(&s.client, &ep, &student_rollcall_id, &s.device_id, data).await,
                None => Err("qr: teacher could not source data".into()),
            };
            tx.send(MonitorMsg::SignResult { key: key.clone(), account_id: s.id.clone(), result }).ok();
        }
        let _ = teacher.client.put(ep.teacher_stop_qr(&teacher_rollcall_id)).send().await; // close teacher end
    });
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
    activity_id: String,
    participants: HashSet<String>,
    detect_at: Option<Instant>,
    prepare_started: bool,
    instance_id: String,
    subjects: Vec<Value>,
    shared: Map<String, Answer>,                 // subject_id -> shared LLM/replay answer
    overrides: Map<String, Map<String, Answer>>, // account -> subject -> answer
    conflicts: Map<String, HashSet<String>>,     // account -> unresolved conflict subjects
    allow_retake: bool,
    reveal: bool,
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
    activity_id: String,
    account_id: String,
) {
    let key = (base_url, format!("quiz:{source}"), activity_id.clone());
    let q = quizzes.entry(key.clone()).or_insert_with(|| QuizActivity {
        source: Source::parse(&source),
        course,
        activity_id: activity_id.clone(),
        participants: HashSet::new(),
        detect_at: None,
        prepare_started: false,
        instance_id: String::new(),
        subjects: Vec::new(),
        shared: Map::new(),
        overrides: Map::new(),
        conflicts: Map::new(),
        allow_retake: false,
        reveal: false,
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
    allow_retake: bool,
    reveal: bool,
) {
    let Some(q) = quizzes.get_mut(&key) else { return };
    q.instance_id = instance_id;
    q.subjects = subjects;
    q.shared = shared;
    q.allow_retake = allow_retake;
    q.reveal = reveal;
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
    if conflicts == 0 {
        q.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    }
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
    if conflicts == 0 && q.countdown_deadline.is_none() && !q.acted && !q.discarded {
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
                    spawn_quiz_prepare(participants, q.source, q.activity_id.clone(), cfg.llm(), cfg.max_answer_reask, tx.clone(), key.clone(), cb);
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
    let (allow_retake, reveal, resubmit) = (q.allow_retake, q.reveal, cfg.resubmit_for_correct);
    let activity_id = q.activity_id.clone();
    for acc_id in q.participants.iter().cloned().collect::<Vec<_>>() {
        let Some(acc) = accounts.get(&acc_id).cloned() else { continue };
        let mut answers = q.shared.clone();
        if let Some(ov) = q.overrides.get(&acc_id) {
            for (k, v) in ov {
                answers.insert(k.clone(), v.clone());
            }
        }
        spawn_quiz_submit(acc, source, activity_id.clone(), instance_id.clone(), subjects.clone(), answers, allow_retake, reveal, resubmit, tx.clone(), key.clone());
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
fn spawn_quiz_prepare(participants: Vec<Arc<Account>>, source: Source, activity_id: String, llm: LlmConfig, max_reask: u32, tx: UnboundedSender<MonitorMsg>, key: ActivityKey, cb: EventCb) {
    tokio::spawn(async move {
        let Some(lead) = participants.first().cloned() else { return };
        let ep = Endpoints::derive(&lead.base_url);
        let paper = match answer::fetch_paper(&lead.client, &ep, source, &activity_id).await {
            Ok(p) => p,
            Err(e) => {
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error", "code": "quiz_fetch", "message": e }));
                return;
            }
        };
        let shared = answer::shared_answers(&lead.client, &llm, cb, &activity_id, &paper.subjects, max_reask).await;
        // Per-account existing answers (each account's own view), for conflict detection.
        let mut existing: Map<String, Map<String, Answer>> = Map::new();
        for acc in &participants {
            let epa = Endpoints::derive(&acc.base_url);
            if let Ok(p) = answer::fetch_paper(&acc.client, &epa, source, &activity_id).await {
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
            allow_retake: paper.allow_retake,
            reveal: paper.reveal,
        })
        .ok();
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_quiz_submit(acc: Arc<Account>, source: Source, activity_id: String, instance_id: String, subjects: Vec<Value>, answers: Map<String, Answer>, allow_retake: bool, reveal: bool, resubmit: bool, tx: UnboundedSender<MonitorMsg>, key: ActivityKey) {
    tokio::spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result: Result<String, String> = match source {
            Source::Exam => match answer::submit_exam(&acc.client, &ep, &activity_id, &instance_id, &answers, &subjects).await {
                Ok(sid) => {
                    if resubmit && allow_retake && reveal {
                        let _ = answer::resubmit_correct(&acc.client, &ep, &activity_id, &instance_id, &sid).await;
                    }
                    Ok(format!("submitted {sid}"))
                }
                Err(e) => Err(e),
            },
            Source::ClassroomExam => {
                // per-subject POST with the full exam wrapper (flat body → 400).
                for s in &subjects {
                    let sid = crate::quiz::subject_id(s);
                    if let Some(a) = answers.get(&sid) {
                        let body = answer::classroom_body(&instance_id, &sid, a);
                        let _ = acc.client.post(ep.classroom_submit(&activity_id, &sid)).json(&body).send().await;
                    }
                }
                Ok("submitted (classroom)".into())
            }
            Source::Vote => {
                let letters: Vec<String> = answers.values().flat_map(vote_letters).collect();
                post_json(&acc.client, &ep.vote_cast(&activity_id), &answer::vote_body(&letters)).await.map(|_| "voted".into())
            }
            Source::CoursewareQuiz => {
                let items = source_items(&subjects, &answers);
                post_json(&acc.client, &ep.courseware_submissions(&activity_id), &answer::courseware_body(&items)).await.map(|_| "submitted (courseware)".into())
            }
            Source::Questionnaire => {
                let items = source_items(&subjects, &answers);
                post_json(&acc.client, &ep.questionnaire_submissions(&activity_id), &answer::questionnaire_body(&items)).await.map(|_| "submitted (questionnaire)".into())
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
fn source_items(subjects: &[Value], answers: &Map<String, Answer>) -> Vec<(String, Answer)> {
    subjects
        .iter()
        .filter_map(|s| {
            let sid = crate::quiz::subject_id(s);
            let qtype = s.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            answers.get(&sid).map(|a| (qtype, a.clone()))
        })
        .collect()
}
