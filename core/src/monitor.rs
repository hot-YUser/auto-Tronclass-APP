//! Multi-account rollcall monitoring. Per-account **poller** tasks each poll their own rollcalls and
//! feed a single central **actor**. The actor merges detections into activities keyed by
//! `(base_url, kind, rollcall_id)`, runs the 15% gate + 15 s countdown, and dispatches a per-account
//! sign for every participant.
//!
//! DISCIPLINE: the actor loop does pure state/coordination and **never awaits network** — every HTTP
//! step (gate fetch, code read, radar solve, sign, on_call_fine recheck) is spawned through the
//! session's `TaskGroup` and its result comes back as a `MonitorMsg`. One slow account can never
//! freeze the others' countdowns. Every spawned helper is tracked by the actor's `TaskGroup`; actor
//! shutdown cancels pending helpers while definition barriers preserve already-authorized mutation.
//! The bounded abort-safe teacher-QR cleanup is the sole detached cleanup. A helper panic becomes one
//! fixed `core_panicked` event plus one generation-bound watchdog ping.

use crate::answer::{self, Source};
use crate::config::TargetId;
use crate::llm::LlmConfig;
use crate::login;
use crate::protocol::AccountResultPhase;
use crate::protocol::AnswerWire;
use crate::providers::Endpoints;
use crate::quiz::Answer;
use crate::rollcall::{self, RollcallKind, SignOutcome};
use crate::teacher_qr::{self, FailureKind};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap as Map;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorPlan {
    pub generation: u64,
    pub routes: Vec<MonitorRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorRoute {
    pub source_targets: Vec<TargetId>,
    pub detector_account_id: String,
    pub participant_account_ids: Vec<String>,
    pub course_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    AccountResult {
        sources: Vec<TargetId>,
        account_id: String,
        phase: AccountResultPhase,
        activity_kind: String,
        course_name: String,
        error: Option<String>,
    },
    AccountLogin {
        account_id: String,
        online: bool,
        error: Option<String>,
    },
}

type ActivityKey = (String, String, String); // (base_url, kind_str, rollcall_id)

/// R4.1 #2: bound sign re-login retries so a permanent 403 (not a real expiry) can't loop forever.
/// Also bounds manual SignNow re-attempts per account on the same counter (total failed attempts).
const MAX_RESIGN: u32 = 3;

/// Cadence between gate re-checks while a rollcall is held below the attendance threshold.
const GATE_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Per-account re-login backoff: the delay doubles after every failed attempt; after this many
/// consecutive failures the account is marked `GivenUp` (one clear Error, no more login requests).
const RELOGIN_BASE_DELAY_SECS: u64 = 5;
const RELOGIN_MAX_ATTEMPTS: u32 = 6;
const RELOGIN_MAX_DELAY_SECS: u64 = 300;

/// One cancellation scope for the long-lived actor. Every spawned network helper — gate check, code
/// read, sign, re-login, QR teacher-assist, quiz prepare/submit — is registered here. Actor shutdown
/// aborts them; definition changes instead use source-aware permits/barriers.
///
/// Panic containment: each helper runs inside a single-task `JoinSet` wrapper. A helper panic is
/// observed exactly once — one fixed `core_panicked` Error event plus one `panic_tx` ping for the
/// engine watchdog. Aborting the wrapper (a stop, a handle drop, or the startup guard) drops the
/// JoinSet, which aborts the helper — normal cancellation is silent. `panic_tx` is cloned per
/// spawn; when the last sender drops (stop + handle drop) the channel closes and the engine treats
/// `None` as no-op. NOTE: stopping a live monitor is the CALLER's job (`MonitorHandle::stop` /
/// `MonitorHandle::drop` — the actor ↔ group cycle means the group's own `Drop` cannot run while
/// the actor lives); `TaskGroup::drop` is only the last-resort teardown once the cycle is broken.
pub struct TaskGroup {
    cancelled: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    cb: EventCb,
    panic_tx: UnboundedSender<()>,
}

impl TaskGroup {
    pub fn new(cb: EventCb, panic_tx: UnboundedSender<()>) -> Self {
        TaskGroup {
            cancelled: Arc::new(AtomicBool::new(false)),
            tasks: StdMutex::new(Vec::new()),
            cb,
            panic_tx,
        }
    }
    /// Register a spawned task. If the group is already cancelled (a spawn racing stop), the task is
    /// aborted immediately instead of being tracked. The cancelled-check and the registration share
    /// the tasks lock with `cancel`, so a spawn can never slip past an in-progress stop un-aborted.
    ///
    /// The tracked handle is a thin observer: the real helper runs inside a single-task `JoinSet`,
    /// so aborting the wrapper (group cancel / drop) aborts the helper at its next await point
    /// (dropping a `JoinSet` aborts its tasks), and a helper panic surfaces as ONE `core_panicked`
    /// event + ONE `panic_tx` ping. A wrapper cancellation is a stop, not a panic — it emits
    /// nothing.
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let cb = self.cb;
        let panic_tx = self.panic_tx.clone();
        let handle = tokio::spawn(async move {
            let mut set = JoinSet::new();
            set.spawn(future);
            match set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(error)) if error.is_panic() => {
                    // Fixed message on purpose: the JoinError payload must never cross the FFI
                    // seam (a panic payload may embed secrets); the code alone identifies the event.
                    let _ = error;
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                                      "code": "core_panicked",
                                      "message": "monitor task failed internally" }),
                    );
                    let _ = panic_tx.send(());
                }
                // The wrapper itself was cancelled (group cancel/drop) — normal cancellation, never
                // a panic report.
                Some(Err(_)) | None => {}
            }
        });
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Completed JoinHandles retain their output allocation until dropped. Opportunistically
        // prune them on every registration so long monitoring sessions stay bounded.
        tasks.retain(|task| !task.is_finished());
        if self.cancelled.load(Ordering::Acquire) {
            handle.abort();
        } else {
            tasks.push(handle);
        }
    }

    /// Cancel every tracked task and mark the group cancelled (later spawns abort immediately).
    pub fn cancel(&self) {
        // Poison-tolerant: a lock poisoned by a past panic must not permanently brick the monitor
        // (spawn/cancel never hold the lock across a panic, so the guard state is still consistent).
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.cancelled.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *tasks);
        for handle in pending {
            handle.abort();
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        // Last-resort teardown once the actor ↔ group cycle is broken (every task completed or was
        // aborted): abort anything still tracked so a bare JoinHandle drop can never leave a
        // detached task. Live-monitor stopping is the handle's job (MonitorHandle::stop/drop) —
        // this is NOT the guarantee for a normal stop. Deliberately does NOT call `cancel`: Drop
        // must not re-enter the tasks lock on an unwind path (a re-entrant lock would
        // deadlock/poison). With the last Arc gone no other thread can be inside spawn/cancel, so
        // the poisoned-lock recovery is purely defensive.
        self.cancelled.store(true, Ordering::Release);
        let pending = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for handle in pending {
            handle.abort();
        }
    }
}

pub struct Detected {
    generation: u64,
    account_id: String,
    base_url: String,
    rollcall_id: String,
    kind: RollcallKind,
    course: String,
    course_id: Option<String>,
}

pub(crate) enum MonitorMsg {
    Detected(Detected),
    GateResult {
        key: ActivityKey,
        rate: Option<f64>,
    },
    CodeRead {
        key: ActivityKey,
        code: Option<String>,
    },
    SignResult {
        key: ActivityKey,
        account_id: String,
        result: Result<SignOutcome, String>,
    },
    SignNow {
        command_id: u64,
        activity_token: String,
    },
    Defer {
        command_id: u64,
        activity_token: String,
    },
    // --- quiz (slice 3) ---
    QuizDetected {
        generation: u64,
        account_id: String,
        base_url: String,
        source: String,
        course: String,
        course_id: String,
        activity_id: String,
        stem: String,
    },
    QuizPrepared {
        key: ActivityKey,
        attempts: Vec<PreparedAttempt>,
    },
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
    QuizPrepareGone {
        key: ActivityKey,
        account_id: String,
        generation: u64,
    },
    QuizPrepareFailed {
        key: ActivityKey,
        account_id: String,
        generation: u64,
        code: String,
        message: String,
    },
    QuizSubmitResult {
        key: ActivityKey,
        account_id: String,
        result: Result<QuizSubmitReport, QuizSubmitFailure>,
    },
    QuizSubmitNow {
        command_id: u64,
        activity_token: String,
    },
    QuizHold {
        command_id: u64,
        activity_token: String,
    },
    QuizDiscard {
        command_id: u64,
        activity_token: String,
    },
    QuizSetAnswer {
        command_id: u64,
        activity_token: String,
        account_id: String,
        subject_id: String,
        answer: AnswerWire,
    },
    // --- session expiry / re-login (R4-D) ---
    AuthLost {
        account_id: String,
    },
    AuthRestored {
        account_id: String,
        ok: bool,
    },
    /// Settings changed while monitoring: adopt them live (boxed — much larger than the other variants).
    ConfigUpdated(Box<MonitorConfig>),
    ApplyPlan {
        plan: MonitorPlan,
        cancel_removed_pending: bool,
    },
    UpsertAccounts(Vec<Account>),
    PrepareDefinitionChange {
        affected_sources: HashSet<TargetId>,
        reply: std::sync::mpsc::SyncSender<()>,
    },
    CommitDefinitionChange {
        plan: MonitorPlan,
        reply: std::sync::mpsc::SyncSender<()>,
    },
    RollbackDefinitionChange {
        reply: std::sync::mpsc::SyncSender<()>,
    },
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
    gate_in_flight: bool, // one gate request per activity in flight at most
    gate_next_check: Option<Instant>, // held rollcall's next gate re-check deadline (cadence)
    countdown_deadline: Option<Instant>,
    acted: bool,
    sign_pending: bool, // manual override waiting on a number code-read before it can sign
    signed: HashSet<String>,
    sign_failed: HashSet<String>, // non-auth sign failures → manual SignNow retries these only
    needs_resign: HashSet<String>, // accounts whose sign hit a dead session → re-sign after re-login
    resign_attempts: HashMap<String, u32>, // per-account failed sign count (bounds retries, incl. manual)
    sources: HashSet<TargetId>,
    plan_generation: u64,
    mutation_blocked: bool,
}

pub struct MonitorHandle {
    pub tx: UnboundedSender<MonitorMsg>,
    group: Arc<TaskGroup>,
}

impl MonitorHandle {
    /// A handle with no tasks yet (the engine's lifecycle tests construct one directly).
    #[cfg(test)]
    pub(crate) fn new(tx: UnboundedSender<MonitorMsg>) -> Self {
        // Dummy panic channel: the receiver is intentionally dropped, so a (never-expected) panic
        // ping simply no-ops — the engine's real watchdog is only wired up through `start`.
        extern "C" fn noop(_: *const u8, _: usize) {}
        let (panic_tx, _panic_rx) = unbounded_channel();
        MonitorHandle {
            tx,
            group: Arc::new(TaskGroup::new(noop, panic_tx)),
        }
    }

    pub fn apply_plan(
        &self,
        plan: MonitorPlan,
        cancel_removed_pending: bool,
    ) -> Result<(), String> {
        self.tx
            .send(MonitorMsg::ApplyPlan {
                plan,
                cancel_removed_pending,
            })
            .map_err(|_| "monitor actor stopped".to_string())
    }

    pub fn upsert_accounts(&self, accounts: Vec<Account>) -> Result<(), String> {
        self.tx
            .send(MonitorMsg::UpsertAccounts(accounts))
            .map_err(|_| "monitor actor stopped".to_string())
    }

    pub fn prepare_definition_change(
        &self,
        affected_sources: HashSet<TargetId>,
    ) -> Result<(), String> {
        let (reply, answer) = std::sync::mpsc::sync_channel(0);
        self.tx
            .send(MonitorMsg::PrepareDefinitionChange {
                affected_sources,
                reply,
            })
            .map_err(|_| "monitor actor stopped".to_string())?;
        answer
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "monitor definition barrier timed out".to_string())
    }

    pub fn commit_definition_change(&self, plan: MonitorPlan) -> Result<(), String> {
        let (reply, answer) = std::sync::mpsc::sync_channel(0);
        self.tx
            .send(MonitorMsg::CommitDefinitionChange { plan, reply })
            .map_err(|_| "monitor actor stopped".to_string())?;
        answer
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "monitor definition commit timed out".to_string())
    }

    pub fn rollback_definition_change(&self) -> Result<(), String> {
        let (reply, answer) = std::sync::mpsc::sync_channel(0);
        self.tx
            .send(MonitorMsg::RollbackDefinitionChange { reply })
            .map_err(|_| "monitor actor stopped".to_string())?;
        answer
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "monitor definition rollback timed out".to_string())
    }
    /// Stop monitoring: ask the actor to break (it emits `idle`), then cancel the whole task group —
    /// pollers, actor, and every tracked network helper (a brute-force round, QR assist, quiz
    /// prepare/submit) stop at their next await point. A helper spawned by a racing actor message
    /// aborts immediately inside `TaskGroup::spawn`.
    pub fn stop(&self) {
        let _ = self.tx.send(MonitorMsg::Stop);
        self.group.cancel();
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        // A dropped handle must never leave the monitor running: the actor ↔ group cycle (the actor
        // future holds a group clone, and the group's tasks hold the actor wrapper) means the group's
        // own Drop cannot run while the actor lives — cancel is the only stop. This covers every
        // path that discards the handle without `stop()` (a re-Init replacing CoreState, teardown,
        // error unwinds). Calling `cancel` twice (stop() then drop) is idempotent: the second pass
        // finds the task list already taken. No callback/`idle` is emitted from Drop on purpose.
        self.group.cancel();
    }
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

    /// The slice of the config each poller needs (cadences + family allowlist).
    fn tuning(&self) -> PollTuning {
        PollTuning {
            idle: Duration::from_secs(self.poll_idle_secs.max(1)),
            quiz_detect: Duration::from_secs(self.quiz_detect_secs.max(1)),
            wanted_types: self.autoanswer_types.clone(),
        }
    }
}

/// Per-poller tuning snapshot. Target scheduling is owned by the supervisor, never by pollers.
#[derive(Clone)]
struct PollTuning {
    idle: Duration,
    quiz_detect: Duration,
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
    let body =
        match crate::http::read_bounded(resp, crate::http::MAX_API_JSON, "monitor poll").await {
            Ok(bytes) => bytes,
            Err(_) => return Fetched::Down,
        };
    match serde_json::from_slice::<Value>(&body) {
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

/// Spawn a tracked re-login, then report its result back to the actor. The engine projects the
/// actor result into the authoritative MonitoringSnapshot.
fn spawn_relogin(acc: Arc<Account>, tx: UnboundedSender<MonitorMsg>, group: &TaskGroup) {
    group.spawn(async move {
        let ok = relogin(&acc).await;
        tx.send(MonitorMsg::AuthRestored {
            account_id: acc.id.clone(),
            ok,
        })
        .ok();
    });
}

/// Stack guard closing the `start`-unwind hole: the actor future holds its own `group` clone (and
/// `group.tasks` holds the actor's wrapper), so while the actor is spawned the group's own `Drop`
/// CANNOT run — a panic between the actor spawn and the handle return would leave every helper
/// detached. This guard cancels the group unconditionally on unwind while armed, and is disarmed
/// only after the `MonitorHandle` is fully assembled — from then on `MonitorHandle::drop`'s cancel
/// owns the stop guarantee (with `TaskGroup::drop` as the last-resort teardown once the cycle is
/// broken).
struct StartupGuard(Arc<TaskGroup>, bool);

impl StartupGuard {
    fn new(group: Arc<TaskGroup>) -> Self {
        StartupGuard(group, true)
    }

    /// The monitor is fully assembled: the guard must no longer cancel on drop.
    fn disarm(&mut self) {
        self.1 = false;
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.1 {
            self.0.cancel();
        }
    }
}

/// Spawn the actor plus one dormant-or-active poller per student on the current Tokio runtime.
pub fn start(
    cb: EventCb,
    accounts: Vec<Account>,
    initial_plan: MonitorPlan,
    cfg: MonitorConfig,
    panic_tx: UnboundedSender<()>,
    runtime_tx: UnboundedSender<RuntimeEvent>,
) -> MonitorHandle {
    let (tx, rx) = unbounded_channel();
    let map: HashMap<String, Arc<Account>> = accounts
        .into_iter()
        .map(|account| (account.id.clone(), Arc::new(account)))
        .collect();
    let (tune_tx, tune_rx) = watch::channel(cfg.tuning());
    let (plan_tx, plan_rx) = watch::channel(initial_plan.clone());
    let group = Arc::new(TaskGroup::new(cb, panic_tx));
    let mut startup = StartupGuard::new(group.clone());
    for account in map.values().filter(|account| !account.is_teacher) {
        group.spawn(poller(
            account.clone(),
            tx.clone(),
            tune_rx.clone(),
            plan_rx.clone(),
        ));
    }
    group.spawn(actor(ActorInit {
        cb,
        accounts: map,
        rx,
        self_tx: tx.clone(),
        cfg,
        tune_tx,
        plan_tx,
        plan: initial_plan,
        runtime_tx,
        group: group.clone(),
    }));
    startup.disarm();
    MonitorHandle { tx, group }
}

/// Poll one active detector's rollcalls and report each newly-seen rollcall once. Target scheduling
/// is enforced before a detector route reaches this poller.
async fn poller(
    acc: Arc<Account>,
    tx: UnboundedSender<MonitorMsg>,
    mut tune_rx: watch::Receiver<PollTuning>,
    mut plan_rx: watch::Receiver<MonitorPlan>,
) {
    let mut tune = tune_rx.borrow_and_update().clone();
    let ep = Endpoints::derive(&acc.base_url);
    let mut seen: HashSet<String> = HashSet::new();
    let mut courses: Vec<String> = Vec::new();
    let mut last_courses: Option<Instant> = None;
    let mut seen_quiz: HashSet<String> = HashSet::new();
    let mut voted_quiz: HashSet<String> = HashSet::new();
    let mut last_quiz: Option<Instant> = None;
    let mut generation = u64::MAX;

    loop {
        if tx.is_closed() {
            break;
        }
        if tune_rx.has_changed().unwrap_or(false) {
            tune = tune_rx.borrow_and_update().clone();
        }
        let plan = plan_rx.borrow_and_update().clone();
        if plan.generation != generation {
            generation = plan.generation;
            seen.clear();
            seen_quiz.clear();
            voted_quiz.clear();
            last_quiz = None;
        }
        if !plan
            .routes
            .iter()
            .any(|route| route.detector_account_id == acc.id)
        {
            tokio::select! {
                changed = plan_rx.changed() => {
                    if changed.is_err() { break; }
                }
                changed = tune_rx.changed() => {
                    if changed.is_err() { break; }
                }
            }
            continue;
        }

        let interval = match fetch_classified(&acc.client, &ep.rollcalls()).await {
            Fetched::Ok(value) => {
                let list = extract_rollcalls(&value);
                let active = !list.is_empty();
                for rollcall in list {
                    let Some(id) = rollcall_id(&rollcall) else {
                        continue;
                    };
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    tx.send(MonitorMsg::Detected(Detected {
                        generation,
                        account_id: acc.id.clone(),
                        base_url: acc.base_url.clone(),
                        rollcall_id: id,
                        kind: rollcall::classify(&rollcall),
                        course: course_name(&rollcall),
                        course_id: rollcall_course_id(&rollcall),
                    }))
                    .ok();
                }
                if active {
                    Duration::from_secs(1)
                } else {
                    tune.idle
                }
            }
            Fetched::AuthLost => {
                tx.send(MonitorMsg::AuthLost {
                    account_id: acc.id.clone(),
                })
                .ok();
                tune.idle
            }
            Fetched::Down => tune.idle,
        };
        if last_quiz.is_none_or(|last| last.elapsed() >= tune.quiz_detect) {
            detect_quizzes(
                &acc,
                &ep,
                &tx,
                &mut courses,
                &mut last_courses,
                &mut seen_quiz,
                &mut voted_quiz,
                &tune.wanted_types,
                generation,
            )
            .await;
            last_quiz = Some(Instant::now());
        }
        if tx.is_closed() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            changed = plan_rx.changed() => {
                if changed.is_err() { break; }
            }
            changed = tune_rx.changed() => {
                if changed.is_err() { break; }
            }
        }
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
    generation: u64,
) {
    if last_courses.is_none_or(|t| t.elapsed() >= Duration::from_secs(300)) || courses.is_empty() {
        if let Ok(v) = get_json(&acc.client, &ep.my_courses()).await {
            let fresh: Vec<String> = first_array(&v, &["courses", "items", "data"])
                .iter()
                .filter_map(course_id_of)
                .collect();
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
                    emit_quiz(
                        tx,
                        acc,
                        seen,
                        QuizDetection {
                            source: "exam",
                            course_id: &cid,
                            activity: &a,
                            stem: "",
                            generation,
                        },
                    );
                }
            }
        }
        if want("questionnaire") {
            for a in family_list(acc, &ep.course_questionnaire_list(&cid), "questionnaires").await {
                // v1: absent is_started → not started → skip.
                if field_or(&a, "is_started", false)
                    && !field_or(&a, "is_closed", false)
                    && !already_submitted(&a)
                {
                    emit_quiz(
                        tx,
                        acc,
                        seen,
                        QuizDetection {
                            source: "questionnaire",
                            course_id: &cid,
                            activity: &a,
                            stem: "",
                            generation,
                        },
                    );
                }
            }
        }
        if want("homework") {
            for a in family_list(acc, &ep.course_homework(&cid), "homework_activities").await {
                if homework_answerable(&a, now) {
                    let stem = a
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    emit_quiz(
                        tx,
                        acc,
                        seen,
                        QuizDetection {
                            source: "homework",
                            course_id: &cid,
                            activity: &a,
                            stem: &stem,
                            generation,
                        },
                    );
                }
            }
        }
        if want("vote") {
            detect_vote(acc, ep, tx, &cid, seen, voted, generation).await;
        }
        if want("classroom") {
            for a in family_list(acc, &ep.course_classroom_list(&cid), "classrooms").await {
                // status stays "start" after 收答 closes but started_subjects_count drops to 0.
                if a.get("status").and_then(Value::as_str) == Some("start")
                    && a.get("started_subjects_count")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        >= 1
                {
                    emit_quiz(
                        tx,
                        acc,
                        seen,
                        QuizDetection {
                            source: "classroom-exam",
                            course_id: &cid,
                            activity: &a,
                            stem: "",
                            generation,
                        },
                    );
                }
            }
        }
        if want("courseware") {
            detect_courseware(acc, ep, tx, &cid, seen, generation).await;
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

/// The poller-side dedup key for a detected quiz: source + course + activity. The bare course/activity
/// id collides ACROSS families (an exam and a courseware quiz can legitimately share an id), so the
/// family is part of the key — vote/courseware prechecks must use the same helper.
fn quiz_seen_key(source: &str, cid: &str, aid: &str) -> String {
    format!("{source}/{cid}/{aid}")
}

/// Dedup on `source/cid/aid`, then emit one QuizDetected with the family's canonical `source`.
struct QuizDetection<'a> {
    source: &'a str,
    course_id: &'a str,
    activity: &'a Value,
    stem: &'a str,
    generation: u64,
}

fn emit_quiz(
    tx: &UnboundedSender<MonitorMsg>,
    acc: &Arc<Account>,
    seen: &mut HashSet<String>,
    detection: QuizDetection<'_>,
) {
    let QuizDetection {
        source,
        course_id: cid,
        activity: a,
        stem,
        generation,
    } = detection;
    let Some(aid) = id_of(a) else { return };
    if !seen.insert(quiz_seen_key(source, cid, &aid)) {
        return;
    }
    tx.send(MonitorMsg::QuizDetected {
        generation,
        account_id: acc.id.clone(),
        base_url: acc.base_url.clone(),
        source: source.to_string(),
        course: a
            .get("course_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
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
    ["id", "course_id", "courseId"].iter().find_map(|k| {
        v.get(*k).and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })
    })
}

fn field_or(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}

/// Already-submitted across the family's variant field names (real tenants differ; §8 needs-real-account).
fn already_submitted(a: &Value) -> bool {
    ["has_submitted", "submitted", "is_submitted"]
        .iter()
        .any(|k| field_or(a, k, false))
}

/// Exam answerable gate (v1): started, not closed, not explicitly not-in-progress, window not past, not
/// already submitted, and attempts not exhausted.
fn exam_answerable(a: &Value, now: i64) -> bool {
    // v1: absent is_started means NOT started → skip (don't default-open).
    let started = field_or(a, "is_started", false);
    let closed = field_or(a, "is_closed", false);
    let in_progress = a.get("is_in_progress").and_then(Value::as_bool) != Some(false);
    // An absent deadline preserves v1's open-ended semantics; a present but malformed deadline is
    // unsafe to interpret and therefore fails closed.
    let past = match end_time(a) {
        EndTime::Absent => false,
        EndTime::Valid(epoch) => epoch < now,
        EndTime::Invalid => return false,
    };
    let times = a.get("submit_times").and_then(Value::as_i64).unwrap_or(0);
    let used = a
        .get("submission_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let exhausted = times > 0 && used >= times;
    started && !closed && in_progress && !past && !already_submitted(a) && !exhausted
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndTime {
    Absent,
    Valid(i64),
    Invalid,
}

/// Classify an epoch field value so absence remains distinct from a malformed present value.
fn epoch_value(v: Option<&Value>) -> EndTime {
    let Some(v) = v else {
        return EndTime::Absent;
    };
    // Several tenants (and the protocol fake that mirrors them) serialize an omitted optional
    // deadline as null / an empty string. Those are absence, not malformed evidence. Any non-empty
    // value that cannot be parsed still fails closed below.
    if v.is_null() || v.as_str().is_some_and(|value| value.trim().is_empty()) {
        return EndTime::Absent;
    }
    v.as_i64()
        .map(EndTime::Valid)
        .or_else(|| v.as_str().and_then(iso8601_to_epoch).map(EndTime::Valid))
        .unwrap_or(EndTime::Invalid)
}

/// Classify `end_time` so absence remains distinct from a malformed present value.
fn end_time(a: &Value) -> EndTime {
    epoch_value(a.get("end_time"))
}

/// Homework window: skip when a present `start_time`/`end_time` epoch (int or ISO) proves the window
/// has not opened or has ended. Absent fields keep the open default (compat); a present but
/// malformed value fails closed, exactly like the exam deadline rule.
fn homework_window_open(a: &Value, now: i64) -> bool {
    let started = match epoch_value(a.get("start_time")) {
        EndTime::Absent => true,
        EndTime::Valid(epoch) => now >= epoch,
        EndTime::Invalid => false,
    };
    let not_ended = match epoch_value(a.get("end_time")) {
        EndTime::Absent => true,
        EndTime::Valid(epoch) => now < epoch,
        EndTime::Invalid => false,
    };
    started && not_ended
}

/// Homework answerability gate: skip ONLY on explicit signals — `is_started:false`, closed,
/// submitted, or a start/end window showing not-started/ended. Missing fields keep the open default
/// (never guess a closed window from absent data).
fn homework_answerable(a: &Value, now: i64) -> bool {
    field_or(a, "is_started", true)
        && !field_or(a, "is_closed", false)
        && !already_submitted(a)
        && homework_window_open(a, now)
}

/// `end_time` as a UTC epoch — a real tenant sends an ISO-8601 string (v1 `_iso_before_now`); tolerate a
/// bare integer epoch too. Invalid or absent values return `None` for callers that only need the epoch.
#[cfg(test)]
fn end_epoch(a: &Value) -> Option<i64> {
    match end_time(a) {
        EndTime::Valid(epoch) => Some(epoch),
        EndTime::Absent | EndTime::Invalid => None,
    }
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM|±HHMM]` to a UTC epoch (civil-date math; no date crate
/// across the 4 ABIs). A missing timezone is retained as UTC for compatibility with existing tenants.
fn iso8601_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', ' '])?;
    if date.len() != 10
        || date.as_bytes().get(4) != Some(&b'-')
        || date.as_bytes().get(7) != Some(&b'-')
    {
        return None;
    }
    if !date
        .as_bytes()
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }
    let y = date.get(0..4)?.parse::<i64>().ok()?;
    let m = date.get(5..7)?.parse::<i64>().ok()?;
    let day = date.get(8..10)?.parse::<i64>().ok()?;

    // Split the time from an optional trailing Z / ±HH:MM offset.
    let (time, off_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let offset = &off[1..];
        let (oh, om) = if let Some((oh, om)) = offset.split_once(':') {
            if oh.len() != 2 || om.len() != 2 {
                return None;
            }
            (oh, om)
        } else {
            if offset.len() != 4 {
                return None;
            }
            (offset.get(0..2)?, offset.get(2..4)?)
        };
        let oh = oh.parse::<i64>().ok()?;
        let om = om.parse::<i64>().ok()?;
        // ISO-8601's practical UTC-offset range is -14:00..+14:00; the boundary hour
        // may not carry minutes (e.g. +14:01 is not a real civil offset).
        if oh > 14 || om > 59 || (oh == 14 && om != 0) {
            return None;
        }
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (rest, 0)
    };

    let clock = match time.split_once('.') {
        Some((clock, fraction))
            if !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            clock
        }
        Some(_) => return None,
        None => time,
    };
    let clock = clock.as_bytes();
    if clock.len() != 8 || clock[2] != b':' || clock[5] != b':' {
        return None;
    }
    if !clock
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 2 || index == 5 || byte.is_ascii_digit())
    {
        return None;
    }
    let hh = std::str::from_utf8(&clock[0..2])
        .ok()?
        .parse::<i64>()
        .ok()?;
    let mm = std::str::from_utf8(&clock[3..5])
        .ok()?
        .parse::<i64>()
        .ok()?;
    let ss = std::str::from_utf8(&clock[6..8])
        .ok()?
        .parse::<i64>()
        .ok()?;
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if day < 1 || day > days_in_month {
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
async fn detect_vote(
    acc: &Arc<Account>,
    ep: &Endpoints,
    tx: &UnboundedSender<MonitorMsg>,
    cid: &str,
    seen: &mut HashSet<String>,
    voted: &mut HashSet<String>,
    generation: u64,
) {
    for a in family_list(acc, &ep.course_interactions(cid), "interactions").await {
        if a.get("type").and_then(Value::as_str) != Some("vote")
            || a.get("status").and_then(Value::as_str) != Some("start")
        {
            continue;
        }
        let Some(aid) = id_of(&a) else { continue };
        if voted.contains(&aid) || seen.contains(&quiz_seen_key("vote", cid, &aid)) {
            continue;
        }
        if let Ok(v) = get_json(&acc.client, &ep.votes_read(&aid)).await {
            let already = v
                .get("students")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter().any(|s| {
                        s.get("user_no")
                            .and_then(Value::as_str)
                            .map(|u| u.eq_ignore_ascii_case(&acc.user_no))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if already {
                voted.insert(aid); // cache so we don't re-read/re-cast
                continue;
            }
        }
        emit_quiz(
            tx,
            acc,
            seen,
            QuizDetection {
                source: "vote",
                course_id: cid,
                activity: &a,
                stem: "",
                generation,
            },
        );
    }
}

/// courseware: generic activities filtered to `type=="material"`, then per material the quizzes chain;
/// each quiz gate `!is_closed && is_started!=false`, and skip when its `my-submission` is already truthy.
async fn detect_courseware(
    acc: &Arc<Account>,
    ep: &Endpoints,
    tx: &UnboundedSender<MonitorMsg>,
    cid: &str,
    seen: &mut HashSet<String>,
    generation: u64,
) {
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
            if seen.contains(&quiz_seen_key("courseware-quiz", cid, &qid)) {
                continue;
            }
            // Skip when already answered (a truthy my-submission object).
            let done = get_json(&acc.client, &ep.courseware_my_submission(&qid))
                .await
                .map(|v| v.is_object() && !v.as_object().map(|o| o.is_empty()).unwrap_or(true))
                .unwrap_or(false);
            if done {
                continue;
            }
            emit_quiz(
                tx,
                acc,
                seen,
                QuizDetection {
                    source: "courseware-quiz",
                    course_id: cid,
                    activity: &q,
                    stem: "",
                    generation,
                },
            );
        }
    }
}

/// Per-account re-login pacing (bounded exponential backoff). `GivenUp` is terminal for the session:
/// a permanent credential failure (wrong password / SSO) must not be retried every poll cycle.
#[derive(Clone, Copy)]
enum ReloginState {
    /// `attempts` consecutive failures so far; the next attempt may start at `next_at`.
    Cooling { attempts: u32, next_at: Instant },
    /// Automatic re-login stopped; further `AuthLost` signals are ignored until a success resets it.
    GivenUp,
}

/// May a re-login for `account_id` start now? (In-flight dedup is the actor's `reauth` set.)
fn relogin_due(backoff: &HashMap<String, ReloginState>, account_id: &str, now: Instant) -> bool {
    match backoff.get(account_id) {
        None => true, // first session loss → attempt immediately
        Some(ReloginState::Cooling { next_at, .. }) => now >= *next_at,
        Some(ReloginState::GivenUp) => false,
    }
}

/// Record one failed re-login, advancing the backoff. Returns the new failure count; the caller emits
/// the single terminal Error exactly when it equals `RELOGIN_MAX_ATTEMPTS` (`u32::MAX` = already given
/// up — never double-report).
fn relogin_failed(
    backoff: &mut HashMap<String, ReloginState>,
    account_id: &str,
    now: Instant,
) -> u32 {
    let n = match backoff.get(account_id).copied() {
        Some(ReloginState::GivenUp) => return u32::MAX,
        Some(ReloginState::Cooling { attempts, .. }) => attempts + 1,
        None => 1,
    };
    let state = if n >= RELOGIN_MAX_ATTEMPTS {
        ReloginState::GivenUp
    } else {
        let delay = (RELOGIN_BASE_DELAY_SECS * 2_u64.pow(n - 1)).min(RELOGIN_MAX_DELAY_SECS);
        ReloginState::Cooling {
            attempts: n,
            next_at: now + Duration::from_secs(delay),
        }
    };
    backoff.insert(account_id.to_string(), state);
    n
}

/// Pure gate-scheduling decision: may a gate request start right now? Enforces one in-flight request
/// per activity and the bounded re-check cadence for held rollcalls.
fn gate_check_due(a: &Activity, now: Instant) -> bool {
    a.gate_pending
        && !a.acted
        && !a.gate_in_flight
        && a.countdown_deadline.is_none()
        && a.gate_next_check.is_none_or(|deadline| now >= deadline)
}

/// Accounts a manual SignNow may re-attempt: participants whose non-auth sign failed, are NOT signed,
/// and are still within the per-account retry bound. `signed` is never cleared → no double-sign.
fn retryable_accounts(a: &Activity) -> Vec<String> {
    a.participants
        .iter()
        .filter(|p| !a.signed.contains(*p) && a.sign_failed.contains(*p))
        .filter(|p| a.resign_attempts.get(*p).copied().unwrap_or(0) <= MAX_RESIGN)
        .cloned()
        .collect()
}

struct ActorInit {
    cb: EventCb,
    accounts: HashMap<String, Arc<Account>>,
    rx: UnboundedReceiver<MonitorMsg>,
    self_tx: UnboundedSender<MonitorMsg>,
    cfg: MonitorConfig,
    tune_tx: watch::Sender<PollTuning>,
    plan_tx: watch::Sender<MonitorPlan>,
    plan: MonitorPlan,
    runtime_tx: UnboundedSender<RuntimeEvent>,
    group: Arc<TaskGroup>,
}

async fn actor(init: ActorInit) {
    let ActorInit {
        cb,
        mut accounts,
        mut rx,
        self_tx,
        mut cfg,
        tune_tx,
        plan_tx,
        mut plan,
        runtime_tx,
        group,
    } = init;
    let mut activities: HashMap<ActivityKey, Activity> = HashMap::new();
    let mut quizzes: HashMap<ActivityKey, QuizActivity> = HashMap::new();
    let mut reauth: HashSet<String> = HashSet::new();
    let mut relogin_backoff: HashMap<String, ReloginState> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(msg) = maybe else { break };
                match msg {
                    MonitorMsg::Stop => break,
                    MonitorMsg::Detected(detection) => {
                        if detection.generation == plan.generation {
                            on_detected(
                                &mut activities,
                                DetectionContext {
                                    accounts: &accounts,
                                    tx: &self_tx,
                                    group: &group,
                                    cb,
                                    cfg: &cfg,
                                    plan: &plan,
                                },
                                detection,
                            );
                        }
                    }
                    MonitorMsg::GateResult { key, rate } =>
                        on_gate(&mut activities, &accounts, &self_tx, &group, cb, &cfg, key, rate),
                    MonitorMsg::CodeRead { key, code } => {
                        let dispatch = match activities.get_mut(&key) {
                            Some(activity) => {
                                activity.number_code = code;
                                activity.code_requested = true;
                                std::mem::take(&mut activity.sign_pending)
                            }
                            None => false,
                        };
                        if dispatch {
                            dispatch_signs(
                                &mut activities,
                                &accounts,
                                &self_tx,
                                &group,
                                &cfg,
                                cb,
                                &key,
                            );
                        }
                    }
                    MonitorMsg::SignResult { key, account_id, result } => {
                        publish_rollcall_result(&runtime_tx, &activities, &key, &account_id, &result);
                        on_sign_result(&mut activities, &self_tx, cb, key, account_id, result);
                    }
                    MonitorMsg::SignNow { command_id, activity_token } => {
                        let result = find_activity_key(&activities, &activity_token)
                            .ok_or_else(|| "unknown rollcall activity_token".to_string())
                            .and_then(|key| on_sign_now(
                                &mut activities, &accounts, &self_tx, &group, &cfg, cb, &key,
                            ));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::Defer { command_id, activity_token } => {
                        let result = find_activity_key(&activities, &activity_token)
                            .ok_or_else(|| "unknown rollcall activity_token".to_string())
                            .map(|key| on_defer(&mut activities, cb, &key));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizDetected {
                        generation, account_id, base_url, source, course, course_id, activity_id, stem
                    } => {
                        if generation == plan.generation {
                            on_quiz_detected(
                                &mut quizzes, &plan, generation, base_url, source, course,
                                course_id, activity_id, account_id, stem,
                            );
                        }
                    }
                    MonitorMsg::QuizPrepared { key, attempts } =>
                        on_quiz_prepared(&mut quizzes, &cfg, cb, key, attempts),
                    MonitorMsg::QuizPrepareGone { key, account_id, generation } =>
                        on_quiz_prepare_gone(&mut quizzes, &cfg, cb, key, account_id, generation),
                    MonitorMsg::QuizPrepareRetry {
                        key, account_id, generation, contract, partial, missing
                    } => on_quiz_prepare_retry(
                        &mut quizzes, &cfg, cb, key, account_id, generation, contract, partial, missing,
                    ),
                    MonitorMsg::QuizPrepareFailed {
                        key, account_id, generation, code, message
                    } => on_quiz_prepare_failed(
                        &mut quizzes, &cfg, cb, key, account_id, generation, code, message,
                    ),
                    MonitorMsg::QuizSetAnswer {
                        command_id, activity_token, account_id, subject_id, answer
                    } => {
                        let result = on_quiz_set_answer(
                            &mut quizzes, &cfg, cb, &activity_token, &account_id, &subject_id, answer,
                        );
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizSubmitNow { command_id, activity_token } => {
                        let result = find_quiz_key(&quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .and_then(|key| dispatch_quiz_submits(
                                &mut quizzes, &accounts, &self_tx, &group, &cfg, &key,
                            ));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizHold { command_id, activity_token } => {
                        let result = find_quiz_mut(&mut quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .and_then(on_quiz_hold);
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizDiscard { command_id, activity_token } => {
                        let result = find_quiz_mut(&mut quizzes, &activity_token)
                            .ok_or_else(|| "unknown quiz activity_token".to_string())
                            .and_then(|quiz| on_quiz_discard(quiz, cb));
                        command_reply(cb, command_id, result);
                    }
                    MonitorMsg::QuizSubmitResult { key, account_id, result } => {
                        publish_quiz_result(&runtime_tx, &quizzes, &key, &account_id, &result);
                        on_quiz_submit_result(&mut quizzes, cb, key, account_id, result);
                    }
                    MonitorMsg::AuthLost { account_id } => {
                        if !reauth.contains(&account_id)
                            && relogin_due(&relogin_backoff, &account_id, Instant::now())
                        {
                            if let Some(account) = accounts.get(&account_id).cloned() {
                                reauth.insert(account_id.clone());
                                spawn_relogin(account, self_tx.clone(), &group);
                            }
                        }
                    }
                    MonitorMsg::ConfigUpdated(new) => {
                        cfg = *new;
                        let _ = tune_tx.send(cfg.tuning());
                    }
                    MonitorMsg::ApplyPlan { plan: next, cancel_removed_pending } => {
                        if next.generation > plan.generation {
                            if cancel_removed_pending {
                                cancel_removed_sources(&mut activities, &mut quizzes, &next);
                            }
                            plan = next;
                            let _ = plan_tx.send(plan.clone());
                        }
                    }
                    MonitorMsg::PrepareDefinitionChange { affected_sources, reply } => {
                        for activity in activities.values_mut().filter(|activity| !activity.acted) {
                            activity.mutation_blocked = !activity.sources.is_empty()
                                && activity.sources.iter().all(|source| affected_sources.contains(source));
                        }
                        for quiz in quizzes.values_mut() {
                            let authorized = quiz.attempts.values().any(|attempt| {
                                matches!(attempt.state, AttemptState::Submitting | AttemptState::Submitted)
                            });
                            if !authorized {
                                quiz.mutation_blocked = !quiz.sources.is_empty()
                                    && quiz.sources.iter().all(|source| affected_sources.contains(source));
                            }
                        }
                        let _ = reply.send(());
                    }
                    MonitorMsg::CommitDefinitionChange { plan: next, reply } => {
                        cancel_removed_sources(&mut activities, &mut quizzes, &next);
                        for activity in activities.values_mut() {
                            activity.mutation_blocked = false;
                        }
                        for quiz in quizzes.values_mut() {
                            quiz.mutation_blocked = false;
                        }
                        if next.generation >= plan.generation {
                            plan = next;
                            let _ = plan_tx.send(plan.clone());
                        }
                        let _ = reply.send(());
                    }
                    MonitorMsg::RollbackDefinitionChange { reply } => {
                        for activity in activities.values_mut() {
                            activity.mutation_blocked = false;
                        }
                        for quiz in quizzes.values_mut() {
                            quiz.mutation_blocked = false;
                        }
                        let _ = reply.send(());
                    }
                    MonitorMsg::UpsertAccounts(new_accounts) => {
                        for account in new_accounts {
                            if accounts.contains_key(&account.id) {
                                continue;
                            }
                            let account = Arc::new(account);
                            if !account.is_teacher {
                                group.spawn(poller(
                                    account.clone(),
                                    self_tx.clone(),
                                    tune_tx.subscribe(),
                                    plan_tx.subscribe(),
                                ));
                            }
                            accounts.insert(account.id.clone(), account);
                        }
                    }
                    MonitorMsg::AuthRestored { account_id, ok } => {
                        reauth.remove(&account_id);
                        if ok {
                            relogin_backoff.remove(&account_id);
                            redispatch_signs(
                                &mut activities, &accounts, &self_tx, &group, &cfg, cb, &account_id,
                            );
                            let _ = runtime_tx.send(RuntimeEvent::AccountLogin {
                                account_id: account_id.clone(),
                                online: true,
                                error: None,
                            });
                        } else {
                            let attempts =
                                relogin_failed(&mut relogin_backoff, &account_id, Instant::now());
                            if attempts == RELOGIN_MAX_ATTEMPTS {
                                emit(cb, &json!({
                                    "id": null, "event": "Error", "severity": "error",
                                    "code": "relogin_failed", "account_id": account_id,
                                    "message": format!(
                                        "account {account_id}: re-login failed {attempts} times; automatic retries stopped"
                                    )
                                }));
                                let _ = runtime_tx.send(RuntimeEvent::AccountLogin {
                                    account_id: account_id.clone(),
                                    online: false,
                                    error: Some("登入狀態已無法恢復".to_string()),
                                });
                            }
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                on_tick(&mut activities, &accounts, &self_tx, &group, &cfg, cb, Instant::now());
                on_quiz_tick(&mut quizzes, &accounts, &self_tx, &group, &cfg, cb);
            }
        }
    }
}

fn publish_rollcall_result(
    runtime_tx: &UnboundedSender<RuntimeEvent>,
    activities: &HashMap<ActivityKey, Activity>,
    key: &ActivityKey,
    account_id: &str,
    result: &Result<SignOutcome, String>,
) {
    let Some(activity) = activities.get(key) else {
        return;
    };
    let (phase, error) = match result {
        Ok(_) => (AccountResultPhase::Succeeded, None),
        Err(message) => (AccountResultPhase::Failed, Some(message.clone())),
    };
    let _ = runtime_tx.send(RuntimeEvent::AccountResult {
        sources: activity.sources.iter().cloned().collect(),
        account_id: account_id.to_string(),
        phase,
        activity_kind: activity.kind.as_str().to_string(),
        course_name: activity.course.clone(),
        error,
    });
}

fn publish_quiz_result(
    runtime_tx: &UnboundedSender<RuntimeEvent>,
    quizzes: &HashMap<ActivityKey, QuizActivity>,
    key: &ActivityKey,
    account_id: &str,
    result: &Result<QuizSubmitReport, QuizSubmitFailure>,
) {
    let Some(quiz) = quizzes.get(key) else {
        return;
    };
    let (phase, error) = match result {
        Ok(_) => (AccountResultPhase::Succeeded, None),
        Err(failure) => (AccountResultPhase::Failed, Some(failure.error.clone())),
    };
    let _ = runtime_tx.send(RuntimeEvent::AccountResult {
        sources: quiz.sources.iter().cloned().collect(),
        account_id: account_id.to_string(),
        phase,
        activity_kind: quiz.source.as_str().to_string(),
        course_name: quiz.course.clone(),
        error,
    });
}

fn cancel_removed_sources(
    activities: &mut HashMap<ActivityKey, Activity>,
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    next: &MonitorPlan,
) {
    let active: HashSet<&TargetId> = next
        .routes
        .iter()
        .flat_map(|route| route.source_targets.iter())
        .collect();
    activities.retain(|_, activity| {
        activity.sources.retain(|source| active.contains(source));
        !activity.sources.is_empty() || activity.acted
    });
    quizzes.retain(|_, quiz| {
        quiz.sources.retain(|source| active.contains(source));
        let mutation_authorized = quiz.attempts.values().any(|attempt| {
            matches!(
                attempt.state,
                AttemptState::Submitting | AttemptState::Submitted
            )
        });
        !quiz.sources.is_empty() || mutation_authorized
    });
}

struct DetectionContext<'a> {
    accounts: &'a HashMap<String, Arc<Account>>,
    tx: &'a UnboundedSender<MonitorMsg>,
    group: &'a TaskGroup,
    cb: EventCb,
    cfg: &'a MonitorConfig,
    plan: &'a MonitorPlan,
}

fn on_detected(
    activities: &mut HashMap<ActivityKey, Activity>,
    context: DetectionContext<'_>,
    detection: Detected,
) {
    let DetectionContext {
        accounts,
        tx,
        group,
        cb,
        cfg,
        plan,
    } = context;
    let matching: Vec<&MonitorRoute> = plan
        .routes
        .iter()
        .filter(|route| {
            route.detector_account_id == detection.account_id
                && (route.course_ids.is_empty()
                    || detection
                        .course_id
                        .as_ref()
                        .is_some_and(|course_id| route.course_ids.contains(course_id)))
        })
        .collect();
    if matching.is_empty() {
        return;
    }
    let participants: HashSet<String> = matching
        .iter()
        .flat_map(|route| route.participant_account_ids.iter())
        .cloned()
        .collect();
    if participants.is_empty() {
        return;
    }
    let sources: HashSet<TargetId> = matching
        .iter()
        .flat_map(|route| route.source_targets.iter().cloned())
        .collect();
    let key = (
        detection.base_url.clone(),
        detection.kind.as_str().to_string(),
        detection.rollcall_id.clone(),
    );
    let mut newly_added = Vec::new();
    {
        let entry = activities.entry(key.clone()).or_insert_with(|| Activity {
            activity_token: crate::config::new_id(),
            kind: detection.kind,
            course: detection.course.clone(),
            participants: HashSet::new(),
            attendance_rate: None,
            number_code: None,
            code_requested: false,
            gate_pending: true,
            gate_in_flight: false,
            gate_next_check: None,
            countdown_deadline: None,
            acted: false,
            mutation_blocked: false,
            sign_pending: false,
            signed: HashSet::new(),
            sign_failed: HashSet::new(),
            needs_resign: HashSet::new(),
            resign_attempts: HashMap::new(),
            sources: HashSet::new(),
            plan_generation: detection.generation,
        });
        entry.sources.extend(sources);
        if !entry.acted {
            entry.plan_generation = detection.generation;
        }
        for participant in participants {
            if entry.participants.insert(participant.clone()) {
                newly_added.push(participant);
            }
        }
        if !newly_added.is_empty() {
            emit_rollcall_detected(cb, &detection.rollcall_id, &detection.base_url, entry);
        }
    }
    if newly_added.is_empty() {
        return;
    }
    let acted = activities.get(&key).is_some_and(|activity| activity.acted);
    if acted {
        let eligible: Vec<String> = newly_added
            .into_iter()
            .filter(|account_id| {
                activities
                    .get(&key)
                    .is_some_and(|activity| !activity.signed.contains(account_id))
            })
            .collect();
        let qr_without_teacher = activities
            .get(&key)
            .is_some_and(|activity| activity.kind == RollcallKind::Qr)
            && !accounts.values().any(|account| account.is_teacher);
        if !eligible.is_empty() && !qr_without_teacher {
            dispatch_signs_for(activities, accounts, tx, group, cfg, cb, &key, eligible);
        }
    } else if let Some(entry) = activities.get_mut(&key) {
        if gate_check_due(entry, Instant::now())
            && spawn_gate_check(accounts, tx, group, &key, &detection.account_id)
        {
            entry.gate_in_flight = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn on_gate(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cb: EventCb,
    cfg: &MonitorConfig,
    key: ActivityKey,
    rate: Option<f64>,
) {
    let Some(a) = activities.get_mut(&key) else {
        return;
    };
    // The in-flight request has landed — clear the flag FIRST so the next scheduled check may spawn.
    a.gate_in_flight = false;
    if a.acted || !a.gate_pending || a.countdown_deadline.is_some() {
        // Stale result: after Defer / SignNow / a dispatch the gate is no longer authoritative, and a
        // late response must never re-arm (or re-hold) the countdown.
        return;
    }
    a.attendance_rate = rate;
    let rate = rate.unwrap_or(0.0);
    // The UI renders this: while held there is no countdown, so it shows the LIVE class rate closing on
    // the threshold instead of an empty countdown slot, and swaps back on `holding:false`.
    let holding = rate + f64::EPSILON < cfg.gate_percent;
    emit(
        cb,
        &json!({ "id": null, "event": "RollcallGate", "rollcall_id": key.2,
                      "activity_token": a.activity_token,
                      "rate": a.attendance_rate, "gate_percent": cfg.gate_percent, "holding": holding }),
    );
    if holding {
        // Below the anti-fake-rollcall gate → hold and re-check only on the bounded cadence.
        a.gate_pending = true;
        a.gate_next_check = Some(Instant::now() + GATE_RECHECK_INTERVAL);
        emit(
            cb,
            &json!({ "id": null, "event": "LogLine", "level": "info",
                          "text": format!("rollcall {} below {:.0}% gate ({:.1}%), holding", key.2, cfg.gate_percent, rate) }),
        );
        return;
    }
    a.gate_pending = false;
    a.gate_next_check = None;
    a.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    // number: read the shared code once, now.
    if a.kind == RollcallKind::Number && !a.code_requested {
        a.code_requested = true;
        if let Some(acc_id) = a.participants.iter().next() {
            spawn_code_read(accounts, tx, group, &key, acc_id);
        }
    }
}

fn on_tick(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    now: Instant,
) {
    let keys: Vec<ActivityKey> = activities.keys().cloned().collect();
    for key in keys {
        let Some(a) = activities.get_mut(&key) else {
            continue;
        };
        if let Some(deadline) = a.countdown_deadline {
            if a.acted {
                continue;
            }
            let remaining = deadline.saturating_duration_since(now).as_secs();
            emit(
                cb,
                &json!({ "id": null, "event": "Countdown", "scope": "rollcall",
                              "activity_token": a.activity_token, "external_id": key.2,
                              "remaining_secs": remaining }),
            );
            if now >= deadline {
                dispatch_signs(activities, accounts, tx, group, cfg, cb, &key);
            }
        } else if gate_check_due(a, now) {
            // Re-check a held rollcall only on its scheduled deadline: one request in flight max,
            // bounded cadence, never a per-tick burst.
            if let Some(acc_id) = a.participants.iter().next().cloned() {
                a.gate_next_check = None;
                if spawn_gate_check(accounts, tx, group, &key, &acc_id) {
                    a.gate_in_flight = true;
                } else {
                    // No usable account right now — pace anyway so a missing account can't spin the tick.
                    a.gate_next_check = Some(now + GATE_RECHECK_INTERVAL);
                }
            }
        }
    }
}

/// Manual override ("立即簽到"): sign the held rollcall NOW, bypassing the anti-fake gate. For a NUMBER
/// rollcall whose shared code hasn't been read yet (the gate held BEFORE the code-read step ran), read
/// the code first and sign the instant it lands — NEVER brute-force 0000–9999 against the real server
/// when the roster exposes the code. Fixes the reported「簽到率未達門檻時立即簽到沒反應」: a held number
/// rollcall silently brute-forced (thousands of PUTs, rate-limits, no timely sign) instead of signing.
///
/// A second press after a dispatch re-attempts ONLY the accounts whose non-auth sign failed (bounded
/// per account, `signed` guard) — never a fake ok on a dead end, and never a full re-dispatch that
/// would double-sign.
fn on_sign_now(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
) -> Result<(), String> {
    // Decide under a scoped borrow, then act once it ends (dispatch_signs_for re-borrows `activities`).
    enum Act {
        ReadCode(Option<String>),
        Dispatch(Vec<String>),
    }
    let act = {
        let Some(a) = activities.get_mut(key) else {
            return Err("rollcall activity is gone".into());
        };
        if a.kind == RollcallKind::Qr && !accounts.values().any(|account| account.is_teacher) {
            return Err(
                "QR sign-in requires a teacher helper; stop monitoring, add a teacher account, then restart monitoring"
                    .into(),
            );
        }
        if !a.acted && a.mutation_blocked {
            return Err("definition_change_in_progress".to_string());
        }
        if !a.acted {
            if a.kind == RollcallKind::Number && a.number_code.is_none() {
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
                Act::Dispatch(a.participants.iter().cloned().collect())
            }
        } else {
            // Already dispatched: re-attempt ONLY the bounded retryable accounts.
            let retryable = retryable_accounts(a);
            if retryable.is_empty() {
                return Err(
                    "no retryable accounts — every participant is signed or beyond its retry bound"
                        .into(),
                );
            }
            Act::Dispatch(retryable)
        }
    };
    match act {
        Act::ReadCode(None) => Ok(()),
        Act::ReadCode(Some(acc_id)) => {
            spawn_code_read(accounts, tx, group, key, &acc_id);
            Ok(())
        }
        Act::Dispatch(ids) => {
            dispatch_signs_for(activities, accounts, tx, group, cfg, cb, key, ids);
            Ok(())
        }
    }
}

/// Dispatch a sign for the given participant ids — each with its own session/device id. Marks the
/// activity acted so it fires once (a later SignNow goes through the retryable path instead). QR
/// routes through teacher-assist.
#[allow(clippy::too_many_arguments)]
fn dispatch_signs_for(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
    account_ids: Vec<String>,
) {
    let Some(a) = activities.get_mut(key) else {
        return;
    };
    if !a.acted && a.mutation_blocked {
        return;
    }
    a.acted = true;
    a.countdown_deadline = None;
    let kind = a.kind;
    let code = a.number_code.clone();
    let activity_token = a.activity_token.clone();
    let radar_strategy = cfg.radar_strategy.clone();
    let ncfg = rollcall::NumberCfg {
        concurrency: cfg.number_concurrency,
        min_concurrency: cfg.number_min_concurrency,
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };
    if ncfg.was_clamped() {
        emit(
            cb,
            &json!({ "id": null, "event": "NumberConcurrencyClamped",
                "activity_token": activity_token.clone(),
                "rollcall_id": key.2.clone(),
                "configured_concurrency": ncfg.concurrency,
                "configured_min_concurrency": ncfg.min_concurrency,
                "effective_concurrency": ncfg.effective_concurrency(),
                "reason": "unknown-safe single-flight: at most one unresolved Number mutation per rollcall/profile" }),
        );
    }
    emit(
        cb,
        &json!({ "id": null, "event": "NumberConcurrencyStatus",
            "activity_token": activity_token.clone(),
            "rollcall_id": key.2.clone(),
            "effective_concurrency": ncfg.effective_concurrency(),
            "configured_concurrency": ncfg.concurrency }),
    );

    if kind == RollcallKind::Qr {
        // QR: needs a teacher account to source the rotating data; students then sign their own id on
        // their OWN endpoint. The token is portable across courses/tenants (confirmed: a THU teacher's
        // data signs a Longhua rollcall), so prefer a same-site teacher but fall back to ANY teacher
        // rather than giving up. `id` breaks ties deterministically.
        let teacher = accounts
            .values()
            .filter(|acc| acc.is_teacher)
            .min_by_key(|acc| (acc.base_url != key.0, acc.id.clone()))
            .cloned();
        match teacher {
            // course_id may be empty — the task falls back to the teacher's first my-course.
            Some(t) => {
                let students: Vec<Arc<Account>> = account_ids
                    .iter()
                    .filter_map(|id| accounts.get(id).cloned())
                    .filter(|acc| !acc.is_teacher)
                    .collect();
                // No student to sign → don't open a teacher source for nobody (it would create + stop a
                // rollcall on the teacher's real course to no purpose).
                if !students.is_empty() {
                    spawn_qr_teacher_assist(t, students, tx.clone(), key.clone(), group);
                }
            }
            None => {
                // No request was dispatched. Keep the activity unacted so a later manual command is
                // rejected with the specific teacher requirement rather than a false "already acted".
                a.acted = false;
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "warn",
                                         "code": "qr_needs_teacher",
                                         "activity_token": activity_token,
                                         "message": "偵測到 QR 點名，但目前沒有教師帳號可輔助。請先停止監控，到「帳號」新增教師帳號，再重新開始監控。" }),
                );
            }
        }
        return;
    }

    let rollcall_id = key.2.clone();
    for acc_id in account_ids {
        let Some(acc) = accounts.get(&acc_id).cloned() else {
            continue;
        };
        spawn_sign(
            acc,
            kind,
            code.clone(),
            rollcall_id.clone(),
            radar_strategy.clone(),
            ncfg,
            tx.clone(),
            key.clone(),
            group,
        );
    }
}

/// Dispatch a sign for every participant. Marks the activity acted so it fires once.
fn dispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
) {
    let participants: Vec<String> = activities
        .get(key)
        .map(|a| a.participants.iter().cloned().collect())
        .unwrap_or_default();
    dispatch_signs_for(activities, accounts, tx, group, cfg, cb, key, participants);
}

fn on_sign_result(
    activities: &mut HashMap<ActivityKey, Activity>,
    tx: &UnboundedSender<MonitorMsg>,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    result: Result<SignOutcome, String>,
) {
    let Some(a) = activities.get_mut(&key) else {
        return;
    };
    match result {
        Ok(outcome) => {
            a.signed.insert(account_id.clone());
            a.needs_resign.remove(&account_id);
            a.sign_failed.remove(&account_id);
            a.resign_attempts.remove(&account_id);
            if a.number_code.is_none() {
                a.number_code = outcome.discovered_code.clone(); // share a brute-forced code
            }
            emit(
                cb,
                &json!({ "id": null, "event": "SignedIn", "rollcall_id": key.2,
                              "activity_token": a.activity_token,
                              "account_id": account_id, "course": a.course, "method": outcome.method }),
            );
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
                a.sign_failed.remove(&account_id);
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                                  "code": "sign_failed", "activity_token": a.activity_token,
                                  "message": format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} re-logins)") }),
                );
            } else {
                a.needs_resign.insert(account_id.clone());
                a.sign_failed.remove(&account_id); // the resign path owns this account now
                emit(
                    cb,
                    &json!({ "id": null, "event": "LogLine", "level": "warn",
                                  "text": format!("rollcall {}: {account_id} session lost mid-sign, re-logging in", key.2) }),
                );
                tx.send(MonitorMsg::AuthLost { account_id }).ok();
            }
        }
        // Non-auth failure: track the account as retryable (a later manual SignNow re-attempts ONLY
        // these, bounded by the same per-account counter) — the reply must never claim a fake ok.
        Err(e) => {
            let n = a.resign_attempts.entry(account_id.clone()).or_insert(0);
            *n += 1;
            if *n > MAX_RESIGN {
                a.sign_failed.remove(&account_id);
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                                  "code": "sign_failed", "activity_token": a.activity_token,
                                  "message": format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} attempts)") }),
                );
            } else {
                a.sign_failed.insert(account_id.clone());
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                                  "code": "sign_failed", "activity_token": a.activity_token,
                                  "message": format!("{account_id}: {e}") }),
                );
            }
        }
    }
}

/// After a re-login (R4.1 #2), re-dispatch a sign for ONLY the accounts that lost their session mid-sign
/// on each activity — guarded by `signed` so an already-signed account is never re-signed (no double-sign).
/// A QR activity is never handed to the generic `spawn_sign` (it answers "unsupported here"): its
/// restore responsibility stays in the teacher-assist flow (`dispatch_signs_for`), which re-sources
/// the rotating token for the recovered account. Without a teacher there is nothing to dispatch, so
/// the account stays pending (`needs_resign`) for a later restore instead of producing a fake error.
fn redispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    account_id: &str,
) {
    let Some(acc) = accounts.get(account_id).cloned() else {
        return;
    };
    let ncfg = rollcall::NumberCfg {
        concurrency: cfg.number_concurrency,
        min_concurrency: cfg.number_min_concurrency,
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };
    if ncfg.was_clamped() {
        emit(
            cb,
            &json!({ "id": null, "event": "NumberConcurrencyClamped",
                "effective_concurrency": ncfg.effective_concurrency(),
                "configured_concurrency": ncfg.concurrency }),
        );
    }
    let has_teacher = accounts.values().any(|account| account.is_teacher);
    let mut qr_keys: Vec<ActivityKey> = Vec::new();
    for (key, a) in activities.iter_mut() {
        if a.signed.contains(account_id) || !a.needs_resign.contains(account_id) {
            continue;
        }
        if a.kind == RollcallKind::Qr {
            if has_teacher {
                a.needs_resign.remove(account_id);
                qr_keys.push(key.clone());
            }
            // No teacher → nothing dispatchable; the account STAYS pending (needs_resign) so a later
            // restore can still retry — never the unsupported generic sign.
        } else {
            a.needs_resign.remove(account_id);
            spawn_sign(
                acc.clone(),
                a.kind,
                a.number_code.clone(),
                key.2.clone(),
                cfg.radar_strategy.clone(),
                ncfg,
                tx.clone(),
                key.clone(),
                group,
            );
        }
    }
    for key in qr_keys {
        dispatch_signs_for(
            activities,
            accounts,
            tx,
            group,
            cfg,
            cb,
            &key,
            vec![account_id.to_string()],
        );
    }
}

fn on_defer(activities: &mut HashMap<ActivityKey, Activity>, cb: EventCb, key: &ActivityKey) {
    if let Some(a) = activities.get_mut(key) {
        a.countdown_deadline = None;
        a.gate_pending = false;
        // Any in-flight gate request is now stale: clear both flags so a late GateResult can neither
        // re-arm the countdown (on_gate's `!gate_pending` guard) nor block a future check.
        a.gate_in_flight = false;
        a.gate_next_check = None;
        emit(
            cb,
            &json!({ "id": null, "event": "PendingSignIn", "rollcall_id": key.2,
            "activity_token": a.activity_token }),
        );
    }
}

// --- spawned network tasks (results return as messages; the actor never awaits these) ---

/// Returns true iff a gate request was actually spawned (the caller then marks the activity's
/// in-flight flag — a missing account must not wedge the one-in-flight invariant).
fn spawn_gate_check(
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    key: &ActivityKey,
    acc_id: &str,
) -> bool {
    // Read the class attendance rate with a participant's authenticated session.
    let Some(acc) = accounts.get(acc_id).cloned() else {
        return false;
    };
    let (tx, key) = (tx.clone(), key.clone());
    let rollcall_id = key.2.clone();
    group.spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let rate = rollcall::attendance_rate(&acc.client, &ep, &rollcall_id).await;
        tx.send(MonitorMsg::GateResult { key, rate }).ok();
    });
    true
}

fn spawn_code_read(
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    key: &ActivityKey,
    acc_id: &str,
) {
    let Some(acc) = accounts.get(acc_id).cloned() else {
        return;
    };
    let (tx, key) = (tx.clone(), key.clone());
    let rollcall_id = key.2.clone();
    group.spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let code = rollcall::read_number_code(&acc.client, &ep, &rollcall_id).await;
        tx.send(MonitorMsg::CodeRead { key, code }).ok();
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_sign(
    acc: Arc<Account>,
    kind: RollcallKind,
    code: Option<String>,
    rollcall_id: String,
    radar_strategy: Vec<String>,
    ncfg: rollcall::NumberCfg,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    group: &TaskGroup,
) {
    group.spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result = match kind {
            RollcallKind::Number => {
                rollcall::sign_number(
                    &acc.client,
                    &ep,
                    &rollcall_id,
                    &acc.user_no,
                    &acc.device_id,
                    code.as_deref(),
                    ncfg,
                )
                .await
            }
            RollcallKind::Radar => {
                rollcall::sign_radar(
                    &acc.client,
                    &ep,
                    &rollcall_id,
                    &radar_strategy,
                    &acc.user_no,
                    &acc.device_id,
                )
                .await
            }
            RollcallKind::SelfRegistration => {
                rollcall::sign_self_registration(&acc.client, &ep, &rollcall_id, &acc.user_no).await
            }
            RollcallKind::Qr | RollcallKind::Unknown => Err("unsupported here".into()),
        };
        tx.send(MonitorMsg::SignResult {
            key,
            account_id: acc.id.clone(),
            result,
        })
        .ok();
    });
}

/// Teacher opens its OWN qr rollcall as the rotating-`data` source; each student then signs THEIR own
/// rollcall id on THEIR own endpoint with that data (docs 32). Because the token is valid only ~1–4 s,
/// this re-sources and re-sends every ~1.5 s for up to ~12 s until each student confirms. A session lost
/// mid-flight is recovered once (teacher and per-student). Actor shutdown aborts this task at its
/// next await point; the bounded abort-safe `QrCleanupGuard` still closes the teacher's data source.
/// Full `core_free` retains the same terminal-shutdown caveat as other runtime tasks.
fn spawn_qr_teacher_assist(
    teacher: Arc<Account>,
    students: Vec<Arc<Account>>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    group: &TaskGroup,
) {
    let student_rollcall_id = key.2.clone();
    group.spawn(async move {
        let ep = Endpoints::derive(&teacher.base_url);
        let mut teacher_recovered = false;
        let source = match prepare_teacher_source(&teacher, &ep, &mut teacher_recovered).await {
            Ok(source) => source,
            Err(_) => {
                for s in &students {
                    tx.send(MonitorMsg::SignResult {
                        key: key.clone(),
                        account_id: s.id.clone(),
                        result: Err("qr: teacher could not open a data source".into()),
                    })
                    .ok();
                }
                return;
            }
        };
        // Abort-safe cleanup: on a normal exit the source is taken out and stopped inline; if the task
        // group cancels us mid-flight, Drop runs the same bounded cleanup detached (see the guard).
        let mut cleanup = QrCleanupGuard {
            teacher: teacher.clone(),
            source: Some(source),
        };

        let mut confirmed: HashSet<String> = HashSet::new();
        let deadline = Instant::now() + teacher_qr::CONFIRM_WINDOW;
        while confirmed.len() < students.len() && Instant::now() < deadline && !tx.is_closed() {
            match teacher_qr::fetch_data(&teacher.client, &ep, cleanup.source()).await {
                Ok(data) => {
                    let pending: Vec<Arc<Account>> = students
                        .iter()
                        .filter(|s| !confirmed.contains(&s.id))
                        .cloned()
                        .collect();
                    // Bounded concurrent fan-out: many co-located students sign the same fresh token in
                    // parallel (the window is only ~1–4 s), capped so a big roster can't burst one tenant.
                    for chunk in pending.chunks(teacher_qr::FANOUT_LIMIT) {
                        let mut fanout = JoinSet::new();
                        for s in chunk {
                            let (s, data, rid) =
                                (s.clone(), data.clone(), student_rollcall_id.clone());
                            fanout.spawn(async move {
                                (s.id.clone(), sign_qr_student(s, &rid, &data).await)
                            });
                        }
                        while let Some(joined) = fanout.join_next().await {
                            if let Ok((account_id, Ok(outcome))) = joined {
                                if confirmed.insert(account_id.clone()) {
                                    tx.send(MonitorMsg::SignResult {
                                        key: key.clone(),
                                        account_id,
                                        result: Ok(outcome),
                                    })
                                    .ok();
                                }
                            }
                        }
                        if tx.is_closed() {
                            break;
                        }
                    }
                }
                // Teacher session died mid-window → re-login once, then re-fetch next iteration.
                Err(e)
                    if e.kind == FailureKind::AuthLost
                        && !teacher_recovered
                        && relogin(&teacher).await =>
                {
                    teacher_recovered = true;
                }
                // Transient/fatal token fetch (incl. a second auth-loss): cool down and retry within the window.
                Err(_) => {}
            }
            if confirmed.len() < students.len() && !tx.is_closed() {
                tokio::time::sleep(teacher_qr::POLL_INTERVAL).await;
            }
        }
        for s in &students {
            if !confirmed.contains(&s.id) {
                tx.send(MonitorMsg::SignResult {
                    key: key.clone(),
                    account_id: s.id.clone(),
                    result: Err("qr: could not confirm within the token window".into()),
                })
                .ok();
            }
        }
        // Normal exit: bounded cleanup inline (the guard would do the same from Drop on an abort).
        if let Some(source) = cleanup.take() {
            cleanup_teacher_source(&teacher, &ep, &source).await;
        }
    });
}

/// Abort-safe best-effort close of the teacher's QR data source. If actor shutdown drops the assist
/// future, a plain trailing cleanup would be skipped and leave a teacher rollcall open. This guard
/// starts the same bounded cleanup from `Drop`; `cleanup_teacher_source` caps each stop at 2 seconds
/// with at most one re-login.
struct QrCleanupGuard {
    teacher: Arc<Account>,
    source: Option<teacher_qr::Source>,
}

impl QrCleanupGuard {
    fn source(&self) -> &teacher_qr::Source {
        self.source.as_ref().expect("qr source present until taken")
    }

    fn take(&mut self) -> Option<teacher_qr::Source> {
        self.source.take()
    }
}

impl Drop for QrCleanupGuard {
    fn drop(&mut self) {
        // Only fires when the task is aborted (the normal path takes the source out first). During a
        // full runtime teardown there may be no runtime context left — try_current guards that, and the
        // terminal-shutdown caveat in spawn_qr_teacher_assist covers the rest.
        if let Some(source) = self.source.take() {
            let teacher = self.teacher.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let ep = Endpoints::derive(&teacher.base_url);
                    cleanup_teacher_source(&teacher, &ep, &source).await;
                });
            }
        }
    }
}

/// Resolve the teacher's course, then create + start its own QR rollcall as the data source. A session
/// lost during setup is recovered once; a start that fails after recovery drops the half-open source
/// before re-creating, so we never leak a created-but-unstarted rollcall.
async fn prepare_teacher_source(
    teacher: &Account,
    ep: &Endpoints,
    recovered: &mut bool,
) -> Result<teacher_qr::Source, teacher_qr::QrError> {
    loop {
        let course_id =
            match teacher_qr::resolve_course_id(&teacher.client, ep, teacher.course_id.as_deref())
                .await
            {
                Ok(course_id) => course_id,
                Err(e)
                    if e.kind == FailureKind::AuthLost && !*recovered && relogin(teacher).await =>
                {
                    *recovered = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
        let source = match teacher_qr::create(&teacher.client, ep, &course_id).await {
            Ok(source) => source,
            Err(e) if e.kind == FailureKind::AuthLost && !*recovered && relogin(teacher).await => {
                *recovered = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        match teacher_qr::start(&teacher.client, ep, &source).await {
            Ok(()) => return Ok(source),
            Err(e) if e.kind == FailureKind::AuthLost && !*recovered && relogin(teacher).await => {
                *recovered = true;
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    teacher_qr::stop(&teacher.client, ep, &source),
                )
                .await;
            }
            Err(e) => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    teacher_qr::stop(&teacher.client, ep, &source),
                )
                .await;
                return Err(e);
            }
        }
    }
}

/// Best-effort close of the teacher's data source (bounded; one auth-lost recovery).
async fn cleanup_teacher_source(teacher: &Account, ep: &Endpoints, source: &teacher_qr::Source) {
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        teacher_qr::stop(&teacher.client, ep, source),
    )
    .await;
    if matches!(first, Ok(Err(ref e)) if e.kind == FailureKind::AuthLost) && relogin(teacher).await
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            teacher_qr::stop(&teacher.client, ep, source),
        )
        .await;
    }
}

/// Sign one student on ITS OWN endpoint (never the teacher's), recovering a lost session once.
async fn sign_qr_student(
    student: Arc<Account>,
    rollcall_id: &str,
    data: &str,
) -> Result<SignOutcome, String> {
    let ep = Endpoints::derive(&student.base_url);
    let first = rollcall::sign_qr_with_teacher_data(
        &student.client,
        &ep,
        rollcall_id,
        &student.device_id,
        data,
        &student.user_no,
    )
    .await;
    if matches!(first.as_ref(), Err(e) if rollcall::is_auth_lost(e)) && relogin(&student).await {
        return rollcall::sign_qr_with_teacher_data(
            &student.client,
            &ep,
            rollcall_id,
            &student.device_id,
            data,
            &student.user_no,
        )
        .await;
    }
    first
}

// --- small helpers ---

fn find_activity_key(
    activities: &HashMap<ActivityKey, Activity>,
    activity_token: &str,
) -> Option<ActivityKey> {
    activities
        .iter()
        .find(|(_, activity)| activity.activity_token == activity_token)
        .map(|(key, _)| key.clone())
}

fn emit_rollcall_detected(cb: EventCb, rollcall_id: &str, base_url: &str, a: &Activity) {
    let accounts: Vec<&String> = a.participants.iter().collect();
    emit(
        cb,
        &json!({ "id": null, "event": "RollcallDetected", "rollcall_id": rollcall_id,
                      "activity_token": a.activity_token,
                      "base_url": base_url, "kind": a.kind.as_str(), "course": a.course,
                      "attendance_rate": a.attendance_rate, "accounts": accounts }),
    );
}

fn extract_rollcalls(v: &Value) -> Vec<Value> {
    v.get("rollcalls")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default()
}
fn rollcall_course_id(rollcall: &Value) -> Option<String> {
    ["course_id", "courseId", "cid"]
        .iter()
        .find_map(|key| {
            rollcall.get(*key).and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_i64().map(|number| number.to_string()))
            })
        })
        .or_else(|| rollcall.get("course").and_then(course_id_of))
}

fn rollcall_id(rc: &Value) -> Option<String> {
    rc.get("rollcall_id")
        .or_else(|| rc.get("id"))
        .and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })
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
    sources: HashSet<TargetId>,
    plan_generation: u64,
    mutation_blocked: bool,
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
    /// Classroom exams submit one subject per request. Preserve confirmed successes so a retry
    /// never resends subjects that the server already accepted.
    submitted_subjects: HashSet<String>,
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
            submitted_subjects: HashSet::new(),
        }
    }
}

pub(crate) struct QuizSubmitReport {
    detail: String,
    completed_subjects: Vec<String>,
    warning: Option<String>,
}

pub(crate) struct QuizSubmitFailure {
    error: String,
    completed_subjects: Vec<String>,
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
    plan: &MonitorPlan,
    generation: u64,
    base_url: String,
    source: String,
    course: String,
    course_id: String,
    activity_id: String,
    detector_account_id: String,
    stem: String,
) {
    let matching: Vec<&MonitorRoute> = plan
        .routes
        .iter()
        .filter(|route| {
            route.detector_account_id == detector_account_id
                && (route.course_ids.is_empty() || route.course_ids.contains(&course_id))
        })
        .collect();
    if matching.is_empty() {
        return;
    }
    let participants: HashSet<String> = matching
        .iter()
        .flat_map(|route| route.participant_account_ids.iter())
        .cloned()
        .collect();
    let sources: HashSet<TargetId> = matching
        .iter()
        .flat_map(|route| route.source_targets.iter().cloned())
        .collect();
    let key = (base_url, format!("quiz:{source}"), activity_id.clone());
    let quiz = quizzes.entry(key).or_insert_with(|| QuizActivity {
        activity_token: crate::config::new_id(),
        source: Source::parse(&source),
        course,
        course_id,
        activity_id,
        stem,
        attempts: HashMap::new(),
        countdown_deadline: None,
        held: false,
        discarded: false,
        sources: HashSet::new(),
        plan_generation: generation,
        mutation_blocked: false,
    });
    quiz.sources.extend(sources);
    if !quiz.attempts.values().any(|attempt| {
        matches!(
            attempt.state,
            AttemptState::Submitting | AttemptState::Submitted
        )
    }) {
        quiz.plan_generation = generation;
    }
    for participant in participants {
        if !quiz.discarded && !quiz.attempts.contains_key(&participant) {
            quiz.attempts
                .insert(participant, PerAccountAttempt::waiting(Instant::now()));
            quiz.countdown_deadline = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn on_quiz_prepared(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    prepared: Vec<PreparedAttempt>,
) {
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    if q.discarded {
        return;
    }
    for data in prepared {
        let Some(attempt) = q.attempts.get_mut(&data.account_id) else {
            continue;
        };
        if attempt.state != AttemptState::Preparing || attempt.prepare_generation != data.generation
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
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else {
        return;
    };
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
        emit(
            cb,
            &json!({ "id": null, "event": "Error", "severity": "error",
                          "code": "quiz_unanswerable", "activity_token": q.activity_token,
                          "account_id": account_id,
                          "message": format!("{}: {detail}", q.activity_id) }),
        );
        rearm_quiz_countdown(q, cfg);
        emit_quiz_prepared(cb, q); // publish the terminal account state to the UI completion gate
        return;
    }
    attempt.state = AttemptState::Waiting;
    attempt.prepare_at = now + Duration::from_secs(cfg.poll_idle_secs.max(1));
    q.countdown_deadline = None;
}

fn on_quiz_prepare_gone(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    generation: u64,
) {
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else {
        return;
    };
    if attempt.state == AttemptState::Preparing && attempt.prepare_generation == generation {
        attempt.state = AttemptState::Gone;
        rearm_quiz_countdown(q, cfg);
        emit_quiz_prepared(cb, q); // no later Ready event may exist
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
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else {
        return;
    };
    if attempt.state != AttemptState::Preparing || attempt.prepare_generation != generation {
        return;
    }
    attempt.state = AttemptState::Failed;
    emit(
        cb,
        &json!({ "id": null, "event": "Error", "severity": "error", "code": code,
        "activity_token": q.activity_token, "account_id": account_id, "message": message }),
    );
    rearm_quiz_countdown(q, cfg);
    emit_quiz_prepared(cb, q); // no later Ready event may exist
}

/// Hold: stop the auto-submit countdown and mark the quiz held. Once ANY attempt is Submitting or
/// Submitted the mutation may already be outbound (or committed) — holding must NOT claim it can
/// reverse that, so it is rejected and nothing is mutated.
fn on_quiz_hold(q: &mut QuizActivity) -> Result<(), String> {
    if q.attempts
        .values()
        .any(|a| matches!(a.state, AttemptState::Submitting | AttemptState::Submitted))
    {
        return Err("submission has begun".to_string());
    }
    q.countdown_deadline = None;
    q.held = true;
    Ok(())
}

/// Discard: same outbound-mutation rule as hold — a quiz whose submission already started cannot be
/// discarded as if nothing happened.
fn on_quiz_discard(q: &mut QuizActivity, cb: EventCb) -> Result<(), String> {
    if q.attempts
        .values()
        .any(|a| matches!(a.state, AttemptState::Submitting | AttemptState::Submitted))
    {
        return Err("submission has begun".to_string());
    }
    q.countdown_deadline = None;
    q.discarded = true;
    emit(
        cb,
        &json!({"id":null,"event":"LogLine","level":"info",
        "text":format!("quiz {} discarded", q.activity_id),
        "activity_token": q.activity_token}),
    );
    Ok(())
}

fn rearm_quiz_countdown(q: &mut QuizActivity, cfg: &MonitorConfig) {
    let preparation_pending = q.attempts.values().any(|attempt| {
        matches!(
            attempt.state,
            AttemptState::Waiting | AttemptState::Preparing
        )
    });
    let unresolved_conflict = q
        .attempts
        .values()
        .any(|attempt| !attempt.conflicts.is_empty());
    let has_ready = q
        .attempts
        .values()
        .any(|attempt| attempt.state == AttemptState::Ready);
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
    attempt
        .overrides
        .insert(subject_id.to_string(), answer.clone());
    attempt.conflicts.remove(subject_id);
    let answer_wire = AnswerWire::from_answer(&answer);
    let display_answer = answer_wire.display();
    emit(
        cb,
        &json!({ "id": null, "event": "AnswerUpdated", "quiz_id": q.activity_id,
                      "activity_token": q.activity_token, "account_id": account_id,
                      "subject_id": subject_id, "answer": answer_wire,
                      "display_answer": display_answer,
                      "source": "user", "conflict": false }),
    );
    rearm_quiz_countdown(q, cfg);
    Ok(())
}

fn on_quiz_tick(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
) {
    let now = Instant::now();
    let keys: Vec<ActivityKey> = quizzes.keys().cloned().collect();
    for key in keys {
        let Some(q) = quizzes.get_mut(&key) else {
            continue;
        };
        if q.discarded {
            continue; // discard must stop queued/retry preparation before any LLM/network work starts
        }
        let mut due_ids: Vec<String> = q
            .attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.state == AttemptState::Waiting && now >= attempt.prepare_at
            })
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
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                    "code": "quiz_account_unavailable", "activity_token": q.activity_token,
                    "account_id": account_id, "message": "測驗帳號已不在監控工作階段中" }),
                );
            }
            due_ids.retain(|account_id| accounts.contains_key(account_id));
            if due_ids.is_empty() {
                rearm_quiz_countdown(q, cfg);
                continue;
            }
            let mut participants: Vec<Arc<Account>> = Vec::new();
            for account_id in &due_ids {
                let Some(account) = accounts.get(account_id) else {
                    // Fail closed (defensive — the retain above already filtered): a participant
                    // missing from the session must never panic the actor. Report the same terminal
                    // state as the pre-filter and skip the account.
                    if let Some(attempt) = q.attempts.get_mut(account_id) {
                        attempt.state = AttemptState::Failed;
                    }
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "quiz_account_unavailable", "activity_token": q.activity_token,
                        "account_id": account_id, "message": "測驗帳號已不在監控工作階段中" }),
                    );
                    continue;
                };
                participants.push(account.clone());
            }
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
                            AttemptState::Ready
                                | AttemptState::Submitting
                                | AttemptState::Submitted
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
                cfg.prepare_retry_budget_secs,
                priors,
                generations,
                reusable,
                group,
                tx.clone(),
                key.clone(),
                cb,
            );
            continue;
        }

        let Some(deadline) = q.countdown_deadline else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(now).as_secs();
        emit(
            cb,
            &json!({ "id": null, "event": "Countdown", "scope": "quiz",
                          "activity_token": q.activity_token, "external_id": q.activity_id,
                          "remaining_secs": remaining }),
        );
        if now >= deadline {
            let _ = dispatch_quiz_submits(quizzes, accounts, tx, group, cfg, &key);
        }
    }
}

fn dispatch_quiz_submits(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    key: &ActivityKey,
) -> Result<(), String> {
    let Some(q) = quizzes.get_mut(key) else {
        return Err("unknown quiz activity".to_string());
    };
    if q.mutation_blocked {
        return Err("definition_change_in_progress".to_string());
    }
    if q.discarded {
        return Err("quiz was discarded".to_string());
    }
    if q.attempts.values().any(|attempt| {
        matches!(
            attempt.state,
            AttemptState::Waiting | AttemptState::Preparing
        )
    }) {
        return Err("quiz attempts are still preparing".to_string());
    }
    if q.attempts
        .values()
        .any(|attempt| !attempt.conflicts.is_empty())
    {
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
        let Some(attempt) = q.attempts.get(account_id) else {
            // Fail closed (defensive — ready_ids came from the same map): a vanished attempt must
            // not be submitted from stale state, nor wedged into Submitting below.
            continue;
        };
        let mut answers = attempt.generated_answers.clone();
        for (subject_id, answer) in &attempt.overrides {
            answers.insert(subject_id.clone(), answer.clone());
        }
        jobs.push((
            account,
            attempt.instance_id.clone(),
            attempt.subjects.clone(),
            answers,
            attempt.submitted_subjects.clone(),
        ));
    }
    for account_id in ready_ids {
        if let Some(attempt) = q.attempts.get_mut(&account_id) {
            attempt.state = AttemptState::Submitting;
        }
    }
    for (account, instance_id, subjects, answers, submitted_subjects) in jobs {
        spawn_quiz_submit(
            account,
            source,
            activity_id.clone(),
            instance_id,
            subjects,
            answers,
            submitted_subjects,
            resubmit,
            group,
            tx.clone(),
            key.clone(),
        );
    }
    Ok(())
}

fn on_quiz_submit_result(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    cb: EventCb,
    key: ActivityKey,
    account_id: String,
    result: Result<QuizSubmitReport, QuizSubmitFailure>,
) {
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    let Some(attempt) = q.attempts.get_mut(&account_id) else {
        return;
    };
    match result {
        Ok(report) => {
            attempt.submitted_subjects.extend(report.completed_subjects);
            attempt.state = AttemptState::Submitted;
            emit(
                cb,
                &json!({ "id": null, "event": "QuizSubmitted", "quiz_id": q.activity_id,
                "activity_token": q.activity_token, "account_id": account_id, "result": report.detail }),
            );
            if let Some(warning) = report.warning {
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "warn",
                    "code": "quiz_correction_failed", "activity_token": q.activity_token,
                    "account_id": account_id, "message": warning }),
                );
            }
        }
        Err(failure) => {
            attempt
                .submitted_subjects
                .extend(failure.completed_subjects);
            // No unconditional auto-retry of an ambiguous mutation (it could duplicate the submit);
            // the attempt stays Ready so the user can explicitly SubmitNow, and the message says so.
            attempt.state = AttemptState::Ready;
            emit(
                cb,
                &json!({ "id": null, "event": "Error", "severity": "error",
                "code": "quiz_submit_failed", "activity_token": q.activity_token,
                "account_id": account_id, "message": format!(
                    "{}: {}（可手動重試：於測驗頁按「立即送出」再次提交）", account_id, failure.error) }),
            );
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
    // Per-account overall deadline for one prepare round: `shared_answers` is cut off after this
    // (0 = no cap, matching a budget of "fail immediately on retry").
    prepare_budget_secs: u64,
    priors: HashMap<String, PriorAnswers>,
    generations: HashMap<String, u64>,
    mut reusable: Vec<ReusableAnswers>,
    group: &TaskGroup,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    cb: EventCb,
) {
    group.spawn(async move {
        // The LLM's own cookie-less client — school cookies must never reach the model endpoint.
        let llm_client = match crate::llm::build_client() {
            Ok(client) => client,
            Err(error) => {
                for account in &participants {
                    tx.send(MonitorMsg::QuizPrepareFailed {
                        key: key.clone(),
                        account_id: account.id.clone(),
                        generation: generations.get(&account.id).copied().unwrap_or_default(),
                        code: "llm_client_unavailable".to_string(),
                        message: format!("LLM 客戶端初始化失敗：{error}"),
                    })
                    .ok();
                }
                return;
            }
        };
        let mut prepared = Vec::new();
        for account in participants {
            let account_id = account.id.clone();
            let generation = generations.get(&account_id).copied().unwrap_or_default();
            let prior_snapshot = priors.get(&account_id).cloned();
            let endpoints = Endpoints::derive(&account.base_url);
            let paper =
                match answer::fetch_paper(&account.client, &endpoints, source, &activity_id, &stem)
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
                    let answering = answer::shared_answers(
                        &account.client,
                        &llm_client,
                        &llm,
                        cb,
                        &activity_token,
                        &account_id,
                        &course_id,
                        &account.base_url,
                        &paper.subjects,
                        max_reask,
                        &prior,
                    );
                    let result = if prepare_budget_secs > 0 {
                        tokio::time::timeout(Duration::from_secs(prepare_budget_secs), answering)
                            .await
                    } else {
                        Ok(answering.await)
                    };
                    match result {
                        Ok(answers) => answers,
                        Err(_) => {
                            // The overall budget ran out mid-answer: hand the actor the existing
                            // partial state so its retry-deadline path turns this into
                            // QuizPrepareFailed — the attempt must never sit in Preparing forever.
                            tx.send(MonitorMsg::QuizPrepareRetry {
                                key: key.clone(),
                                account_id,
                                generation,
                                contract,
                                partial: prior.clone(),
                                missing: answer::missing_subjects(&paper.subjects, &prior),
                            })
                            .ok();
                            continue;
                        }
                    }
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
fn spawn_quiz_submit(
    acc: Arc<Account>,
    source: Source,
    activity_id: String,
    instance_id: String,
    subjects: Vec<Value>,
    answers: Map<String, Answer>,
    submitted_subjects: HashSet<String>,
    resubmit: bool,
    group: &TaskGroup,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
) {
    group.spawn(async move {
        let ep = Endpoints::derive(&acc.base_url);
        let result: Result<QuizSubmitReport, QuizSubmitFailure> = match source {
            Source::Exam => match answer::submit_exam(
                &acc.client,
                &ep,
                &activity_id,
                &instance_id,
                &answers,
                &subjects,
            )
            .await
            {
                Ok((sid, retake)) => {
                    // resubmit gate: EXAM + pref + the SUBMIT RESPONSE's allow_retake_exam + a submission
                    // id (v1 answer_flow.py:456) — a single-attempt exam must not burn its one graded attempt.
                    if resubmit && retake && !sid.is_empty() {
                        let correction =
                            answer::resubmit_correct(&acc.client, &ep, &activity_id, &sid).await;
                        // The graded initial submit is already committed. A correction failure is
                        // visible but terminal; retrying the initial paper could burn another attempt.
                        Ok(finalize_exam_submit(&sid, correction))
                    } else {
                        Ok(submit_report(format!("submitted {sid}")))
                    }
                }
                Err(error) => Err(submit_failure(error)),
            },
            Source::ClassroomExam => {
                // per-subject POST with the full exam wrapper (flat body → 400). ponytail: v1 also gates
                // on the server's started_subjects_count≥1 (R2.5); here each answered subject is posted.
                submit_classroom(
                    &acc,
                    &ep,
                    &activity_id,
                    &instance_id,
                    &subjects,
                    &answers,
                    &submitted_subjects,
                )
                .await
                .map(|completed_subjects| QuizSubmitReport {
                    detail: "submitted (classroom)".into(),
                    completed_subjects,
                    warning: None,
                })
            }
            Source::Questionnaire => {
                // exam wrapper (NOT courseware), to the questionnaire endpoint.
                let entries: Vec<Value> = subjects
                    .iter()
                    .filter_map(|s| {
                        answers
                            .get(&crate::quiz::subject_id(s))
                            .map(|a| answer::exam_subject_entry(s, a))
                    })
                    .collect();
                post_json(
                    &acc.client,
                    &ep.questionnaire_submissions(&activity_id),
                    &answer::questionnaire_body(&instance_id, &entries),
                )
                .await
                .map(|_| submit_report("submitted (questionnaire)"))
                .map_err(submit_failure)
            }
            Source::Vote => {
                let letters: Vec<String> = answers.values().flat_map(vote_letters).collect();
                post_json(
                    &acc.client,
                    &ep.vote_cast(&activity_id),
                    &answer::vote_body(&letters),
                )
                .await
                .map(|_| submit_report("voted"))
                .map_err(submit_failure)
            }
            Source::CoursewareQuiz => {
                let items = source_items(&subjects, &answers);
                post_json(
                    &acc.client,
                    &ep.courseware_submissions(&activity_id),
                    &answer::courseware_body(&items),
                )
                .await
                .map(|_| submit_report("submitted (courseware)"))
                .map_err(submit_failure)
            }
            Source::Homework => {
                let text = answers
                    .values()
                    .filter_map(answer_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                post_json(
                    &acc.client,
                    &ep.homework_submissions(&activity_id),
                    &answer::homework_body(&text),
                )
                .await
                .map(|_| submit_report("submitted (homework)"))
                .map_err(submit_failure)
            }
        };
        tx.send(MonitorMsg::QuizSubmitResult {
            key,
            account_id: acc.id.clone(),
            result,
        })
        .ok();
    });
}

fn submit_report(detail: impl Into<String>) -> QuizSubmitReport {
    QuizSubmitReport {
        detail: detail.into(),
        completed_subjects: Vec::new(),
        warning: None,
    }
}

fn finalize_exam_submit(sid: &str, correction: Result<(), String>) -> QuizSubmitReport {
    match correction {
        Ok(()) => submit_report(format!("submitted {sid}; corrected resubmit completed")),
        Err(error) => QuizSubmitReport {
            detail: format!("submitted {sid}"),
            completed_subjects: Vec::new(),
            warning: Some(format!(
                "initial submit {sid} succeeded; correction pass failed: {error}"
            )),
        },
    }
}

fn submit_failure(error: impl Into<String>) -> QuizSubmitFailure {
    QuizSubmitFailure {
        error: error.into(),
        completed_subjects: Vec::new(),
    }
}

fn emit_quiz_prepared(cb: EventCb, q: &QuizActivity) {
    emit(cb, &quiz_prepared_event(q));
}

fn quiz_prepared_event(q: &QuizActivity) -> Value {
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
            json!({ "account_id": account_id, "instance_id": attempt.instance_id,
                "state": match attempt.state {
                    AttemptState::Ready => "ready",
                    AttemptState::Submitting => "submitting",
                    AttemptState::Submitted => "submitted",
                    AttemptState::Failed => "failed",
                    AttemptState::Gone => "gone",
                    AttemptState::Waiting => "waiting",
                    AttemptState::Preparing => "preparing",
                },
                "questions": questions })
        })
        .collect();
    let expected_accounts: Vec<Value> = q
        .attempts
        .iter()
        .map(|(account_id, attempt)| {
            json!({
                "account_id": account_id,
                "state": match attempt.state {
                    AttemptState::Ready => "ready",
                    AttemptState::Submitting => "submitting",
                    AttemptState::Submitted => "submitted",
                    AttemptState::Failed => "failed",
                    AttemptState::Gone => "gone",
                    AttemptState::Waiting => "waiting",
                    AttemptState::Preparing => "preparing",
                }
            })
        })
        .collect();
    let conflict_count: usize = q
        .attempts
        .values()
        .map(|attempt| attempt.conflicts.len())
        .sum();
    json!({ "id": null, "event": "QuizPrepared", "schema_version": 1,
        "activity_token": q.activity_token, "quiz_id": q.activity_id,
        "activity": { "external_id": q.activity_id, "source": q.source.as_str(),
            "course_id": q.course_id, "course": q.course },
        "course": q.course, "per_account": per_account, "expected_accounts": expected_accounts,
        "conflict_count": conflict_count })
}

fn subject_stem(subject: &Value) -> &str {
    ["description", "content", "stem"]
        .iter()
        .find_map(|key| {
            subject
                .get(*key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
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

fn find_quiz_key(
    quizzes: &HashMap<ActivityKey, QuizActivity>,
    activity_token: &str,
) -> Option<ActivityKey> {
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
    // mutation_checked is the ONE 2xx+business-error gate (the shared envelope parser owns success
    // semantics). A 2xx JSON body without an explicit business error is a success — no extra local
    // boolean gate: responses like `{"id":…}` would otherwise be rejected despite a committed submit.
    crate::http::mutation_checked(client.post(url).json(body), "submit activity")
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
    already_submitted: &HashSet<String>,
) -> Result<Vec<String>, QuizSubmitFailure> {
    let mut completed = Vec::new();
    let mut answered = 0_usize;
    for subject in subjects {
        let subject_id = crate::quiz::subject_id(subject);
        let Some(answer) = answers.get(&subject_id) else {
            continue;
        };
        answered += 1;
        if already_submitted.contains(&subject_id) {
            continue;
        }
        let body = answer::classroom_body(instance_id, subject, answer);
        let operation = format!("submit classroom subject {subject_id}");
        // mutation_checked is the single 2xx+business-error gate (shared envelope parser) — the same
        // rule as post_json. Confirmed subjects are retained so a retry never resends them.
        if let Err(error) = crate::http::mutation_checked(
            account
                .client
                .post(endpoints.classroom_submit(activity_id, &subject_id))
                .json(&body),
            &operation,
        )
        .await
        {
            return Err(QuizSubmitFailure {
                error,
                completed_subjects: completed,
            });
        }
        completed.push(subject_id);
    }
    if answered == 0 {
        return Err(submit_failure("submit classroom: no answered subjects"));
    }
    Ok(completed)
}
fn extract_array(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default()
}
fn id_of(v: &Value) -> Option<String> {
    v.get("id")
        .or_else(|| v.get("activity_id"))
        .or_else(|| v.get("course_id"))
        .and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })
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
fn source_items(
    subjects: &[Value],
    answers: &Map<String, Answer>,
) -> Vec<(String, String, Answer)> {
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
    fn production_quiz_prepared_emitter_matches_v1_contract() {
        let fixture: Value = serde_json::from_str(include_str!("assets/quiz_prepared_v1.json"))
            .expect("valid shared fixture");
        let fixture_attempt = &fixture["per_account"][0];
        let subjects = fixture_attempt["questions"]
            .as_array()
            .expect("fixture questions")
            .iter()
            .map(|question| {
                json!({
                    "id": question["subject_id"],
                    "parent_id": question["parent_id"],
                    "type": question["type"],
                    "answer_type": question["answer_type"],
                    "description": question["stem"],
                    "options": question["options"],
                })
            })
            .collect::<Vec<_>>();
        let generated_answers = fixture_attempt["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|question| {
                let id = question["subject_id"].as_str().unwrap().to_string();
                let answer = serde_json::from_value::<AnswerWire>(question["answer"].clone())
                    .unwrap()
                    .into_answer()
                    .unwrap();
                (id, answer)
            })
            .collect();
        let existing_answers = fixture_attempt["questions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|question| {
                serde_json::from_value::<AnswerWire>(question["existing_answer"].clone())
                    .ok()
                    .and_then(|wire| wire.into_answer().ok())
                    .map(|answer| (question["subject_id"].as_str().unwrap().to_string(), answer))
            })
            .collect();
        let mut attempts = HashMap::new();
        attempts.insert(
            "a1".to_string(),
            PerAccountAttempt {
                state: AttemptState::Ready,
                prepare_generation: 1,
                prepare_at: Instant::now(),
                prepare_deadline: None,
                answer_contract: None,
                instance_id: "attempt-a1".to_string(),
                subjects,
                generated_answers,
                existing_answers,
                overrides: Map::new(),
                conflicts: HashSet::from(["1".to_string()]),
                submitted_subjects: HashSet::new(),
            },
        );
        let activity = QuizActivity {
            activity_token: "fixture-quiz-prepared-v1".to_string(),
            source: Source::Exam,
            course: "行銷管理".to_string(),
            course_id: "55379".to_string(),
            activity_id: "32877".to_string(),
            stem: String::new(),
            attempts,
            countdown_deadline: None,
            held: false,
            discarded: false,
            sources: HashSet::new(),
            plan_generation: 0,
            mutation_blocked: false,
        };
        let emitted = quiz_prepared_event(&activity);
        for field in [
            "event",
            "schema_version",
            "activity_token",
            "quiz_id",
            "activity",
            "course",
        ] {
            assert_eq!(emitted.get(field), fixture.get(field), "field {field}");
        }
        assert_eq!(emitted["conflict_count"], 1);
        let emitted_account = &emitted["per_account"][0];
        assert_eq!(emitted_account["account_id"], fixture_attempt["account_id"]);
        assert_eq!(
            emitted_account["instance_id"],
            fixture_attempt["instance_id"]
        );
        assert_eq!(emitted_account["state"], "ready");
        assert_eq!(emitted_account["questions"], fixture_attempt["questions"]);
        assert_eq!(
            emitted["expected_accounts"],
            json!([{ "account_id": "a1", "state": "ready" }])
        );
    }

    #[test]
    fn iso8601_epoch_parses_z_offset_and_int() {
        // 2021-01-01T00:00:00Z = 1609459200.
        assert_eq!(
            iso8601_to_epoch("2021-01-01T00:00:00Z"),
            Some(1_609_459_200)
        );
        // same instant expressed as +08:00 local (08:00 local == 00:00 UTC).
        assert_eq!(
            iso8601_to_epoch("2021-01-01T08:00:00+08:00"),
            Some(1_609_459_200)
        );
        // The compact ISO-8601 offset form denotes the same instant.
        assert_eq!(
            iso8601_to_epoch("2021-01-01T08:00:00+0800"),
            Some(1_609_459_200)
        );
        // fractional seconds + space separator tolerated.
        assert_eq!(
            iso8601_to_epoch("2021-01-01 00:00:00.500Z"),
            Some(1_609_459_200)
        );
        assert_eq!(iso8601_to_epoch("not-a-date"), None);
        // end_epoch also accepts a bare integer epoch.
        assert_eq!(
            end_epoch(&json!({"end_time": 1_609_459_200_i64})),
            Some(1_609_459_200)
        );
        assert_eq!(
            end_epoch(&json!({"end_time": "2021-01-01T00:00:00Z"})),
            Some(1_609_459_200)
        );
        assert_eq!(end_epoch(&json!({})), None);
        assert_eq!(end_time(&json!({})), EndTime::Absent);
        assert_eq!(end_time(&json!({"end_time": null})), EndTime::Absent);
        assert_eq!(end_time(&json!({"end_time": "  "})), EndTime::Absent);
        assert_eq!(
            end_time(&json!({"end_time": "not-a-date"})),
            EndTime::Invalid
        );
    }

    #[test]
    fn iso8601_rejects_impossible_dates_times_and_offsets() {
        assert!(iso8601_to_epoch("2024-02-29T00:00:00Z").is_some());
        assert_eq!(iso8601_to_epoch("2023-02-29T00:00:00Z"), None);
        assert_eq!(iso8601_to_epoch("2024-04-31T00:00:00Z"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T24:00:00Z"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T23:60:00Z"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T23:59:60Z"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T00:00:00+24:00"), None);
        assert!(iso8601_to_epoch("2024-01-01T00:00:00+14:00").is_some());
        assert_eq!(iso8601_to_epoch("2024-01-01T00:00:00+14:01"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T00:00:00+2360"), None);
        assert_eq!(iso8601_to_epoch("2024-01-01T00:00:00+08:60"), None);
    }

    #[test]
    fn exam_answerable_gates_iso_expiry_and_absent_started() {
        let now = 1_700_000_000;
        // started, open, future end → answerable.
        assert!(exam_answerable(
            &json!({"is_started": true, "end_time": "2099-01-01T00:00:00Z"}),
            now
        ));
        // a PAST ISO end_time → not answerable even though is_closed is false (the bug this fixes).
        assert!(!exam_answerable(
            &json!({"is_started": true, "is_closed": false, "end_time": "2000-01-01T00:00:00Z"}),
            now
        ));
        // absent is_started → v1 treats as not-started → skip.
        assert!(!exam_answerable(
            &json!({"end_time": "2099-01-01T00:00:00Z"}),
            now
        ));
        // absent end_time → not past → answerable.
        assert!(exam_answerable(&json!({"is_started": true}), now));
        // A present but malformed deadline must fail closed; it must not be treated like absence.
        assert!(!exam_answerable(
            &json!({"is_started": true, "end_time": "2023-02-29T00:00:00Z"}),
            now
        ));
        assert!(!exam_answerable(
            &json!({"is_started": true, "end_time": "2024-01-01T24:00:00+0800"}),
            now
        ));
    }

    extern "C" fn noop_cb(_: *const u8, _: usize) {}

    // Capture every event emitted through the seam on THIS test thread (each #[tokio::test] runs
    // its own current-thread runtime on its own thread, so the thread-local never cross-talks).
    thread_local! {
        static EVENTS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    extern "C" fn record_cb(ptr: *const u8, len: usize) {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
            EVENTS.with(|events| events.borrow_mut().push(v));
        }
    }
    fn take_events() -> Vec<Value> {
        EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

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
        }
    }

    /// A single-account quiz with one unresolved conflict and no live countdown — the exact state
    /// after a user holds a paper that still has a conflict.
    fn quiz_with_conflict() -> (HashMap<ActivityKey, QuizActivity>, ActivityKey) {
        let key = (
            "http://x".to_string(),
            "quiz:exam".to_string(),
            "act1".to_string(),
        );
        let attempt = PerAccountAttempt {
            state: AttemptState::Ready,
            prepare_generation: 1,
            prepare_at: Instant::now(),
            prepare_deadline: None,
            answer_contract: Some(vec![
                json!({ "id": "subj1", "type": "short_answer", "content": "Question" }),
            ]),
            instance_id: "instance-acc1".to_string(),
            subjects: vec![json!({ "id": "subj1", "type": "short_answer", "content": "Question" })],
            generated_answers: Map::from([("subj1".to_string(), Answer::Text("llm".to_string()))]),
            existing_answers: Map::from([("subj1".to_string(), Answer::Text("old".to_string()))]),
            overrides: Map::new(),
            conflicts: HashSet::from(["subj1".to_string()]),
            submitted_subjects: HashSet::new(),
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
            sources: HashSet::new(),
            plan_generation: 0,
            mutation_blocked: false,
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
        assert!(
            q.attempts["acc1"].conflicts.is_empty(),
            "the conflict is resolved"
        );
        assert!(
            q.countdown_deadline.is_none(),
            "a HELD quiz must not re-arm auto-submit — only SubmitNow may"
        );
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
        assert!(
            q.countdown_deadline.is_some(),
            "an un-held quiz re-arms once its last conflict clears"
        );
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
        quizzes
            .get_mut(&key)
            .unwrap()
            .attempts
            .get_mut("acc1")
            .unwrap()
            .conflicts
            .clear();
        quizzes.get_mut(&key).unwrap().countdown_deadline = Some(Instant::now());

        on_quiz_detected(
            &mut quizzes,
            &plan_for("acc2", &["acc2"]),
            1,
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
        let attempt = quizzes
            .get_mut(&key)
            .unwrap()
            .attempts
            .get_mut("acc1")
            .unwrap();
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
            answers: Map::from([("s1".to_string(), Answer::Options(vec!["old".to_string()]))]),
        };
        let changed = vec![json!({ "id": "s1", "options": [{ "id": "new" }] })];

        assert!(compatible_prior(Some(&prior), &changed).is_empty());
        assert_eq!(
            compatible_prior(Some(&prior), &prior.contract),
            prior.answers
        );
    }

    #[test]
    fn gone_attempt_does_not_block_another_ready_account() {
        let (mut quizzes, key) = quiz_with_conflict();
        let quiz = quizzes.get_mut(&key).unwrap();
        quiz.attempts.get_mut("acc1").unwrap().conflicts.clear();
        quiz.attempts.insert(
            "acc2".to_string(),
            PerAccountAttempt {
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
                submitted_subjects: HashSet::new(),
            },
        );

        on_quiz_prepare_gone(
            &mut quizzes,
            &cfg_countdown(15),
            noop_cb,
            key.clone(),
            "acc2".to_string(),
            3,
        );

        assert_eq!(quizzes[&key].attempts["acc2"].state, AttemptState::Gone);
        assert!(quizzes[&key].countdown_deadline.is_some());
    }

    #[test]
    fn failed_classroom_submit_retains_completed_subjects_for_retry() {
        let (mut quizzes, key) = quiz_with_conflict();
        let attempt = quizzes
            .get_mut(&key)
            .unwrap()
            .attempts
            .get_mut("acc1")
            .unwrap();
        attempt.conflicts.clear();
        attempt.state = AttemptState::Submitting;

        on_quiz_submit_result(
            &mut quizzes,
            noop_cb,
            key.clone(),
            "acc1".to_string(),
            Err(QuizSubmitFailure {
                error: "second subject failed".to_string(),
                completed_subjects: vec!["subj1".to_string()],
            }),
        );

        let attempt = &quizzes[&key].attempts["acc1"];
        assert_eq!(attempt.state, AttemptState::Ready);
        assert_eq!(
            attempt.submitted_subjects,
            HashSet::from(["subj1".to_string()])
        );
    }

    #[tokio::test]
    async fn classroom_retry_skips_every_subject_already_confirmed_by_the_server() {
        let account = Account {
            id: "acc1".to_string(),
            device_id: String::new(),
            user_no: String::new(),
            is_teacher: false,
            course_id: None,
            base_url: "http://127.0.0.1:1".to_string(),
            client: Client::new(),
            username: String::new(),
            password: crate::secrets::Secret::default(),
        };
        let endpoints = Endpoints::derive(&account.base_url);
        let subjects = vec![
            json!({"id": "s1", "type": "short_answer"}),
            json!({"id": "s2", "type": "short_answer"}),
        ];
        let answers = Map::from([
            ("s1".to_string(), Answer::Text("a".to_string())),
            ("s2".to_string(), Answer::Text("b".to_string())),
        ]);
        let already_submitted = HashSet::from(["s1".to_string(), "s2".to_string()]);

        let completed = match submit_classroom(
            &account,
            &endpoints,
            "quiz",
            "instance",
            &subjects,
            &answers,
            &already_submitted,
        )
        .await
        {
            Ok(completed) => completed,
            Err(failure) => panic!("unexpected retry failure: {}", failure.error),
        };

        // Port 1 is deliberately unreachable: success proves the retry did not resend either
        // server-confirmed mutation.
        assert!(completed.is_empty());
    }

    #[test]
    fn hold_is_rejected_once_submission_has_begun() {
        let (mut quizzes, key) = quiz_with_conflict();
        let q = quizzes.get_mut(&key).unwrap();
        q.attempts.get_mut("acc1").unwrap().state = AttemptState::Submitting;
        q.countdown_deadline = Some(Instant::now());

        let err = on_quiz_hold(q).expect_err("a begun submission must not be held");
        assert!(err.contains("submission has begun"), "unexpected: {err}");
        let q = &quizzes[&key];
        assert!(!q.held, "hold must not mutate once submission began");
        assert!(
            q.countdown_deadline.is_some(),
            "hold must not abort the in-flight mutation"
        );
    }

    #[test]
    fn discard_is_rejected_once_submission_has_begun() {
        let (mut quizzes, key) = quiz_with_conflict();
        let q = quizzes.get_mut(&key).unwrap();
        q.attempts.get_mut("acc1").unwrap().state = AttemptState::Submitted;

        let err =
            on_quiz_discard(q, noop_cb).expect_err("a committed submission must not be discarded");
        assert!(err.contains("submission has begun"), "unexpected: {err}");
        assert!(!quizzes[&key].discarded);
    }

    #[test]
    fn hold_and_discard_work_before_submission() {
        let (mut quizzes, key) = quiz_with_conflict();
        assert!(on_quiz_hold(quizzes.get_mut(&key).unwrap()).is_ok());
        assert!(quizzes[&key].held);
        assert!(on_quiz_discard(quizzes.get_mut(&key).unwrap(), noop_cb).is_ok());
        assert!(quizzes[&key].discarded);
    }

    #[test]
    fn quiz_seen_key_includes_source_course_and_activity() {
        // The same activity id must not collide across families or courses.
        assert_ne!(
            quiz_seen_key("exam", "c1", "42"),
            quiz_seen_key("courseware-quiz", "c1", "42")
        );
        assert_ne!(
            quiz_seen_key("exam", "c1", "42"),
            quiz_seen_key("exam", "c2", "42")
        );
        assert_eq!(
            quiz_seen_key("exam", "c1", "42"),
            quiz_seen_key("exam", "c1", "42")
        );
    }

    #[test]
    fn emit_quiz_detects_same_activity_id_across_families() {
        let account = Arc::new(Account {
            id: "acc1".to_string(),
            device_id: String::new(),
            user_no: String::new(),
            is_teacher: false,
            course_id: None,
            base_url: "http://127.0.0.1:1".to_string(),
            client: Client::new(),
            username: String::new(),
            password: crate::secrets::Secret::default(),
        });
        let (tx, mut rx) = unbounded_channel();
        let mut seen = HashSet::new();
        emit_quiz(
            &tx,
            &account,
            &mut seen,
            QuizDetection {
                source: "exam",
                course_id: "c1",
                activity: &json!({"id": "42"}),
                stem: "",
                generation: 1,
            },
        );
        emit_quiz(
            &tx,
            &account,
            &mut seen,
            QuizDetection {
                source: "courseware-quiz",
                course_id: "c1",
                activity: &json!({"id": "42"}),
                stem: "",
                generation: 1,
            },
        );
        emit_quiz(
            &tx,
            &account,
            &mut seen,
            QuizDetection {
                source: "exam",
                course_id: "c1",
                activity: &json!({"id": "42"}),
                stem: "",
                generation: 1,
            },
        ); // duplicate

        let first = rx.try_recv().expect("exam detected");
        match first {
            MonitorMsg::QuizDetected {
                source,
                course_id,
                activity_id,
                ..
            } => {
                assert_eq!(source, "exam");
                assert_eq!(course_id, "c1");
                assert_eq!(activity_id, "42");
            }
            _ => panic!("unexpected first message variant"),
        }
        let second = rx
            .try_recv()
            .expect("same id detected under the courseware family");
        match second {
            MonitorMsg::QuizDetected { source, .. } => assert_eq!(source, "courseware-quiz"),
            _ => panic!("unexpected second message variant"),
        }
        assert!(
            rx.try_recv().is_err(),
            "a duplicate detection must be deduped"
        );
    }

    #[test]
    fn homework_answerable_gates_on_explicit_signals_only() {
        let now = 1_700_000_000;
        // open: started, not closed, not submitted, no window fields.
        assert!(homework_answerable(
            &json!({"is_started": true, "is_closed": false}),
            now
        ));
        // explicit is_started:false → skip.
        assert!(!homework_answerable(&json!({"is_started": false}), now));
        // closed / submitted → skip.
        assert!(!homework_answerable(&json!({"is_closed": true}), now));
        assert!(!homework_answerable(&json!({"has_submitted": true}), now));
        // start/end epochs showing not-started/ended → skip.
        assert!(!homework_answerable(
            &json!({"start_time": "2099-01-01T00:00:00Z"}),
            now
        ));
        assert!(!homework_answerable(
            &json!({"end_time": "2000-01-01T00:00:00Z"}),
            now
        ));
        assert!(!homework_answerable(
            &json!({"start_time": 2_000_000_000_i64}),
            now
        ));
        // absent fields keep the open default — no guessing from missing data.
        assert!(homework_answerable(&json!({}), now));
        // a present malformed deadline fails closed (same rule as exam deadlines).
        assert!(!homework_answerable(
            &json!({"end_time": "2023-02-29T00:00:00Z"}),
            now
        ));
        // a fully open window passes.
        assert!(homework_answerable(
            &json!({"start_time": "2000-01-01T00:00:00Z", "end_time": "2099-01-01T00:00:00Z"}),
            now
        ));
    }

    #[test]
    fn correction_failure_is_terminal_after_the_initial_exam_submit_commits() {
        let report = finalize_exam_submit("submission-1", Err("review unavailable".to_string()));
        assert_eq!(report.detail, "submitted submission-1");
        assert!(report.warning.as_deref().is_some_and(|warning| {
            warning.contains("review unavailable") && warning.contains("initial submit")
        }));
    }

    // --- rollcall gate / re-login / SignNow state machines (pure; no network) ---

    fn detected_for(account: &str, rollcall: &str) -> Detected {
        Detected {
            generation: 1,
            account_id: account.to_string(),
            base_url: "http://x".to_string(),
            rollcall_id: rollcall.to_string(),
            kind: RollcallKind::Number,
            course: String::new(),
            course_id: None,
        }
    }

    fn plan_for(detector: &str, participants: &[&str]) -> MonitorPlan {
        MonitorPlan {
            generation: 1,
            routes: vec![MonitorRoute {
                source_targets: vec![TargetId::account(detector)],
                detector_account_id: detector.to_string(),
                participant_account_ids: participants
                    .iter()
                    .map(|participant| (*participant).to_string())
                    .collect(),
                course_ids: Vec::new(),
            }],
        }
    }

    fn gate_activity(participants: &[&str]) -> Activity {
        Activity {
            activity_token: "token".to_string(),
            kind: RollcallKind::Number,
            course: String::new(),
            participants: participants.iter().map(|s| s.to_string()).collect(),
            attendance_rate: None,
            number_code: None,
            code_requested: false,
            gate_pending: true,
            gate_in_flight: false,
            gate_next_check: None,
            countdown_deadline: None,
            acted: false,
            sign_pending: false,
            signed: HashSet::new(),
            sign_failed: HashSet::new(),
            needs_resign: HashSet::new(),
            resign_attempts: HashMap::new(),
            sources: HashSet::new(),
            plan_generation: 1,
            mutation_blocked: false,
        }
    }

    #[test]
    fn qr_without_teacher_stays_unacted_and_manual_retry_explains_recovery() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let key = (
            "http://x".to_string(),
            "qrcode".to_string(),
            "qr1".to_string(),
        );
        let mut activity = gate_activity(&["acc1"]);
        activity.kind = RollcallKind::Qr;
        let mut activities = HashMap::from([(key.clone(), activity)]);

        dispatch_signs_for(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            &key,
            vec!["acc1".to_string()],
        );
        assert!(
            !activities[&key].acted,
            "no teacher means no outbound request, so the activity must not become acted"
        );
        let error =
            on_sign_now(&mut activities, &accounts, &tx, &group, &cfg, noop_cb, &key).unwrap_err();
        assert!(error.contains("teacher helper") && error.contains("stop monitoring"));
    }

    #[test]
    fn held_gate_rechecks_on_bounded_cadence_with_at_most_one_inflight() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let mut activities = HashMap::new();
        let t0 = Instant::now();

        // First detection starts the initial check (empty accounts → the spawn fails, so simulate the
        // in-flight request by setting the flag the way a successful spawn would).
        on_detected(
            &mut activities,
            DetectionContext {
                accounts: &accounts,
                tx: &tx,
                group: &group,
                cb: noop_cb,
                cfg: &cfg,
                plan: &plan_for("acc1", &["acc1"]),
            },
            detected_for("acc1", "rc1"),
        );
        let key = (
            "http://x".to_string(),
            "number".to_string(),
            "rc1".to_string(),
        );
        {
            let a = activities.get_mut(&key).unwrap();
            assert!(a.gate_pending && !a.gate_in_flight && a.gate_next_check.is_none());
            a.gate_in_flight = true; // the request is now on the wire
        }

        // Below-threshold result → hold, clear in-flight, schedule the next check on the cadence.
        on_gate(
            &mut activities,
            &accounts,
            &tx,
            &group,
            noop_cb,
            &cfg,
            key.clone(),
            Some(5.0),
        );
        let a = &activities[&key];
        assert!(a.gate_pending, "below threshold → still holding");
        assert!(
            !a.gate_in_flight,
            "the landed result cleared the in-flight flag"
        );
        assert!(a.gate_next_check.is_some(), "the next check is scheduled");
        assert!(a.countdown_deadline.is_none());

        // Before the deadline no new check may start, even on a tick.
        let before = t0 + GATE_RECHECK_INTERVAL - Duration::from_millis(1);
        assert!(!gate_check_due(&activities[&key], before));
        on_tick(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            before,
        );
        assert!(
            activities[&key].gate_next_check.is_some(),
            "not due → schedule untouched"
        );
        assert!(!activities[&key].gate_in_flight);

        // Due: one check may start. The empty accounts map makes the spawn fail, so the activity is
        // paced again instead of being wedged by a stuck in-flight flag.
        on_tick(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            t0 + GATE_RECHECK_INTERVAL,
        );
        assert!(!activities[&key].gate_in_flight);
        assert!(activities[&key].gate_next_check.is_some());

        // A live in-flight request at the due instant blocks a second one (one-in-flight invariant).
        {
            let a = activities.get_mut(&key).unwrap();
            a.gate_in_flight = true;
            a.gate_next_check = None;
        }
        on_tick(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            t0 + GATE_RECHECK_INTERVAL,
        );
        let a = &activities[&key];
        assert!(a.gate_in_flight, "the in-flight request is untouched");
        assert!(
            a.gate_next_check.is_none(),
            "no double-check while one is in flight"
        );
    }

    #[test]
    fn stale_gate_result_after_defer_never_rearms_the_countdown() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let mut activities = HashMap::new();
        on_detected(
            &mut activities,
            DetectionContext {
                accounts: &accounts,
                tx: &tx,
                group: &group,
                cb: noop_cb,
                cfg: &cfg,
                plan: &plan_for("acc1", &["acc1"]),
            },
            detected_for("acc1", "rc1"),
        );
        let key = (
            "http://x".to_string(),
            "number".to_string(),
            "rc1".to_string(),
        );
        activities.get_mut(&key).unwrap().gate_in_flight = true;

        on_defer(&mut activities, noop_cb, &key);
        let a = &activities[&key];
        assert!(a.countdown_deadline.is_none());
        assert!(!a.gate_pending && !a.gate_in_flight && a.gate_next_check.is_none());

        // The in-flight gate response lands AFTER the Defer — it must be completely inert.
        on_gate(
            &mut activities,
            &accounts,
            &tx,
            &group,
            noop_cb,
            &cfg,
            key.clone(),
            Some(90.0),
        );
        let a = &activities[&key];
        assert!(
            a.countdown_deadline.is_none(),
            "Defer is terminal: a stale gate must never re-arm the countdown"
        );
        assert!(!a.gate_pending);
        assert!(!a.gate_in_flight);
    }

    #[test]
    fn passing_gate_arms_the_countdown_exactly_once() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let mut activities = HashMap::new();
        on_detected(
            &mut activities,
            DetectionContext {
                accounts: &accounts,
                tx: &tx,
                group: &group,
                cb: noop_cb,
                cfg: &cfg,
                plan: &plan_for("acc1", &["acc1"]),
            },
            detected_for("acc1", "rc1"),
        );
        let key = (
            "http://x".to_string(),
            "number".to_string(),
            "rc1".to_string(),
        );
        activities.get_mut(&key).unwrap().gate_in_flight = true;

        on_gate(
            &mut activities,
            &accounts,
            &tx,
            &group,
            noop_cb,
            &cfg,
            key.clone(),
            Some(50.0),
        );
        let armed = activities[&key].countdown_deadline;
        assert!(armed.is_some(), "at/above threshold → countdown armed");
        assert!(!activities[&key].gate_pending);

        // A duplicate/stale result (e.g. from a late duplicate spawn) must not re-arm or extend it.
        on_gate(
            &mut activities,
            &accounts,
            &tx,
            &group,
            noop_cb,
            &cfg,
            key.clone(),
            Some(80.0),
        );
        assert_eq!(
            activities[&key].countdown_deadline, armed,
            "deadline is set exactly once"
        );
    }

    #[test]
    fn relogin_backoff_paces_attempts_and_gives_up_bounded() {
        let t0 = Instant::now();
        let mut backoff = HashMap::new();
        assert!(
            relogin_due(&backoff, "a", t0),
            "first session loss retries immediately"
        );
        assert_eq!(relogin_failed(&mut backoff, "a", t0), 1);
        assert!(
            !relogin_due(&backoff, "a", t0 + Duration::from_secs(4)),
            "cooling before the base delay"
        );
        assert!(
            relogin_due(&backoff, "a", t0 + Duration::from_secs(5)),
            "due after the base delay"
        );

        // Each subsequent failure doubles the wait; the cap turns the state terminal.
        let mut at = t0 + Duration::from_secs(5);
        for attempt in 2..=RELOGIN_MAX_ATTEMPTS {
            assert_eq!(relogin_failed(&mut backoff, "a", at), attempt);
            if attempt < RELOGIN_MAX_ATTEMPTS {
                let delay = RELOGIN_BASE_DELAY_SECS * 2_u64.pow(attempt - 1);
                assert!(!relogin_due(
                    &backoff,
                    "a",
                    at + Duration::from_secs(delay - 1)
                ));
                assert!(relogin_due(&backoff, "a", at + Duration::from_secs(delay)));
                at += Duration::from_secs(delay);
            }
        }
        assert!(
            !relogin_due(&backoff, "a", at + Duration::from_secs(86_400)),
            "a permanent credential failure never hammers the login endpoint again"
        );
        assert_eq!(
            relogin_failed(&mut backoff, "a", at),
            u32::MAX,
            "a terminal account is never double-reported"
        );
    }

    #[test]
    fn sign_now_after_dispatch_retries_only_bounded_failed_accounts() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let mut activities = HashMap::new();
        let key = (
            "http://x".to_string(),
            "number".to_string(),
            "rc1".to_string(),
        );
        let mut a = gate_activity(&["acc1", "acc2", "acc3"]);
        a.acted = true; // already dispatched once
        a.signed.insert("acc1".to_string());
        a.sign_failed.insert("acc2".to_string());
        a.needs_resign.insert("acc3".to_string()); // owned by the relogin path, not manual retry
        activities.insert(key.clone(), a);

        assert_eq!(
            retryable_accounts(&activities[&key]),
            vec!["acc2".to_string()],
            "only the non-auth failed, unsigned account is retryable"
        );

        // Bounded: an account past MAX_RESIGN leaves the retryable set.
        activities
            .get_mut(&key)
            .unwrap()
            .resign_attempts
            .insert("acc2".to_string(), MAX_RESIGN + 1);
        assert!(retryable_accounts(&activities[&key]).is_empty());

        // Nothing retryable → SignNow is a real Err, never a fake ok.
        let result = on_sign_now(&mut activities, &accounts, &tx, &group, &cfg, noop_cb, &key);
        assert!(result.is_err());
        assert!(
            activities[&key].acted,
            "acted is never cleared — no full re-dispatch / double sign"
        );
    }

    #[tokio::test]
    async fn quiz_prepare_budget_timeout_sends_retry_not_stuck_preparing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // School server: exam qualification (best-effort) + distribute with one pending subject.
        let school_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let school_addr = school_listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut stream, _)) = school_listener.accept().await else {
                    break;
                };
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await;
                let request_line = String::from_utf8_lossy(&request);
                let body = if request_line.contains("/distribute") {
                    r#"{"exam_paper_instance_id": 1001, "subjects": [{"id": "s1", "type": "short_answer", "description": "q?"}]}"#
                } else {
                    "{}"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        // LLM endpoint: accepts the POST, then NEVER responds — the deadline must cut it off.
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = llm_listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await;
            tokio::time::sleep(Duration::from_secs(10)).await; // silence — no bytes ever
        });

        let base_url = format!("http://{school_addr}");
        let account = Arc::new(Account {
            id: "acc1".to_string(),
            device_id: String::new(),
            user_no: "u".to_string(),
            is_teacher: false,
            course_id: None,
            base_url: base_url.clone(),
            client: reqwest::Client::new(),
            username: "user".to_string(),
            password: crate::secrets::Secret::default(),
        });
        let (tx, mut rx) = unbounded_channel::<MonitorMsg>();
        let key: ActivityKey = (base_url, "quiz:exam".to_string(), "act1".to_string());
        let mut generations = HashMap::new();
        generations.insert("acc1".to_string(), 1_u64);
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        spawn_quiz_prepare(
            vec![account],
            Source::Exam,
            "act1".to_string(),
            "tok".to_string(),
            "c1".to_string(),
            String::new(),
            LlmConfig {
                endpoint: format!("http://{llm_addr}/v1/chat/completions"),
                model: "m".to_string(),
                api_key: "k".to_string(),
                max_tokens: 0,
                enable_tools: false,
                max_tool_iterations: 0,
            },
            0,
            1, // 1s overall budget — far below the LLM's 180s read-idle
            HashMap::new(),
            generations,
            Vec::new(),
            &group,
            tx,
            key.clone(),
            noop_cb,
        );
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a deadline-capped prepare must report within the budget")
            .expect("a message must arrive");
        match message {
            MonitorMsg::QuizPrepareRetry {
                key: got_key,
                account_id,
                generation,
                partial,
                missing,
                ..
            } => {
                assert_eq!(got_key, key, "the retry carries the activity key");
                assert_eq!(account_id, "acc1");
                assert_eq!(generation, 1);
                assert!(
                    partial.is_empty(),
                    "nothing was answered before the cut-off"
                );
                assert_eq!(
                    missing,
                    vec!["s1".to_string()],
                    "the pending subject is reported missing"
                );
            }
            _ => panic!("expected QuizPrepareRetry on budget timeout, got a different message"),
        }
        // The silent LLM must NOT produce a second message. The task may close its last sender, so
        // either channel closure or an idle timeout proves there was no follow-up event.
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(200), rx.recv()).await,
                Ok(Some(_))
            ),
            "no follow-up message — the attempt is handed back to the actor's deadline path"
        );
    }

    // --- TaskGroup panic containment / cancellation (deterministic; no network) ---

    #[tokio::test]
    async fn task_group_panic_reports_core_panicked_once_and_pings() {
        let (panic_tx, mut panic_rx) = unbounded_channel();
        let group = TaskGroup::new(record_cb, panic_tx);
        group.spawn(async { panic!("boom: helper died") });

        tokio::time::timeout(Duration::from_secs(5), panic_rx.recv())
            .await
            .expect("a panicking helper must ping the watchdog")
            .expect("a ping must arrive");

        let events = take_events();
        assert_eq!(events.len(), 1, "exactly one panic event: {events:?}");
        assert_eq!(events[0]["event"].as_str(), Some("Error"));
        assert_eq!(events[0]["code"].as_str(), Some("core_panicked"));

        // One helper, one panic, one ping — never a duplicate report.
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(200), panic_rx.recv()).await,
                Ok(Some(_))
            ),
            "a single helper panic must ping exactly once"
        );
    }

    #[tokio::test]
    async fn task_group_normal_completion_and_cancel_report_no_panic() {
        let (panic_tx, mut panic_rx) = unbounded_channel();
        let group = TaskGroup::new(record_cb, panic_tx);

        // Normal completion → no ping, no event.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        group.spawn(async move {
            let _ = done_tx.send(());
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("the helper must complete");

        // Cancel → the child is aborted and nothing is reported (cancellation is not a panic).
        let started = Arc::new(AtomicBool::new(false));
        let ran_after_cancel = Arc::new(AtomicBool::new(false));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let (s, r) = (started.clone(), ran_after_cancel.clone());
        group.spawn(async move {
            s.store(true, Ordering::SeqCst);
            let _ = start_tx.send(());
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                r.store(true, Ordering::SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
            .await
            .expect("the looping helper must start");
        group.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            !ran_after_cancel.load(Ordering::SeqCst),
            "cancel must abort the helper at its next await point"
        );
        assert!(
            take_events().is_empty(),
            "normal completion and cancellation must not emit core_panicked"
        );
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(200), panic_rx.recv()).await,
                Ok(Some(_))
            ),
            "normal completion and cancellation must not ping the watchdog"
        );
    }

    #[tokio::test]
    async fn task_group_drop_aborts_children() {
        // `start` may unwind after some pollers were spawned but before the handle is returned; the
        // group's final Arc drop must abort every tracked child — no detached tasks may survive it.
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
        let started = Arc::new(AtomicBool::new(false));
        let ran_after_drop = Arc::new(AtomicBool::new(false));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let (s, r) = (started.clone(), ran_after_drop.clone());
        group.spawn(async move {
            s.store(true, Ordering::SeqCst);
            let _ = start_tx.send(());
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                r.store(true, Ordering::SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
            .await
            .expect("the child must start");
        assert!(started.load(Ordering::SeqCst));

        drop(group); // last Arc → TaskGroup::drop

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !ran_after_drop.load(Ordering::SeqCst),
            "the child must not run after the group drop"
        );
    }

    #[tokio::test]
    async fn task_group_spawn_after_cancel_aborts_immediately() {
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        group.cancel();
        let started = Arc::new(AtomicBool::new(false));
        let s = started.clone();
        group.spawn(async move {
            s.store(true, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !started.load(Ordering::SeqCst),
            "a spawn racing a completed stop must never run"
        );
    }

    #[tokio::test]
    async fn startup_guard_armed_cancels_children_even_with_live_strong_clone() {
        // The start-unwind hole: the actor future holds its own group clone (and the group's tasks
        // hold the actor wrapper — a cycle), so the group's OWN Drop cannot run while that clone
        // lives. The armed guard must cancel the group directly, aborting every child anyway.
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
        let strong = group.clone(); // the actor-style reference that outlives `start`'s frame
        let started = Arc::new(AtomicBool::new(false));
        let ran_after_drop = Arc::new(AtomicBool::new(false));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let (s, r) = (started.clone(), ran_after_drop.clone());
        group.spawn(async move {
            s.store(true, Ordering::SeqCst);
            let _ = start_tx.send(());
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                r.store(true, Ordering::SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
            .await
            .expect("the child must start");
        assert!(started.load(Ordering::SeqCst));

        let guard = StartupGuard::new(group); // armed, as if start unwound before returning
        drop(guard);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !ran_after_drop.load(Ordering::SeqCst),
            "an armed guard must abort every child even while a strong clone keeps the group alive"
        );
        drop(strong);
    }

    #[tokio::test]
    async fn startup_guard_disarmed_does_not_cancel() {
        // The normal start path disarms the guard just before returning the handle; dropping it
        // must be a no-op, otherwise every successful start would cancel its own monitor. The child
        // only starts AFTER the guard is dropped, so any stray cancel would abort it before it runs.
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
        let mut guard = StartupGuard::new(group.clone());
        let alive = Arc::new(AtomicBool::new(false));
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let a = alive.clone();
        group.spawn(async move {
            let _ = go_rx.await;
            a.store(true, Ordering::SeqCst);
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        guard.disarm();
        drop(guard);
        let _ = go_tx.send(());

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            alive.load(Ordering::SeqCst),
            "a disarmed guard must not cancel the group"
        );
        // The child would still be looping — clean it up like a real stop would.
        group.cancel();
    }

    // --- late participant / QR restore (state machines; no network) ---

    fn account_at(id: &str, is_teacher: bool) -> Arc<Account> {
        Arc::new(Account {
            id: id.to_string(),
            device_id: String::new(),
            user_no: String::new(),
            is_teacher,
            course_id: None,
            base_url: "http://127.0.0.1:1".to_string(), // port 1: connection refused → fails fast
            client: Client::new(),
            username: String::new(),
            password: crate::secrets::Secret::default(),
        })
    }

    #[tokio::test]
    async fn late_participant_after_acted_is_dispatched_alone_not_full_redispatch() {
        let cfg = cfg_countdown(15);
        let (tx, mut rx) = unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
        accounts.insert("acc1".to_string(), account_at("acc1", false));
        accounts.insert("acc2".to_string(), account_at("acc2", false));
        let key = (
            "http://127.0.0.1:1".to_string(),
            "number".to_string(),
            "rc1".to_string(),
        );
        let mut activity = gate_activity(&["acc1"]);
        activity.acted = true; // already dispatched
        activity.signed.insert("acc1".to_string());
        let mut activities = HashMap::from([(key.clone(), activity)]);

        // acc2 detects the SAME already-acted rollcall after the dispatch.
        on_detected(
            &mut activities,
            DetectionContext {
                accounts: &accounts,
                tx: &tx,
                group: &group,
                cb: noop_cb,
                cfg: &cfg,
                plan: &plan_for("acc2", &["acc2"]),
            },
            Detected {
                generation: 1,
                account_id: "acc2".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                rollcall_id: "rc1".to_string(),
                kind: RollcallKind::Number,
                course: String::new(),
                course_id: None,
            },
        );

        // Exactly ONE dispatch: acc2's own sign (the unreachable host fails it fast). acc1, already
        // signed, must never be re-signed — a full re-dispatch would have produced two messages.
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the late participant's sign must be dispatched")
            .expect("a message must arrive");
        match message {
            MonitorMsg::SignResult {
                key: got_key,
                account_id,
                ..
            } => {
                assert_eq!(got_key, key);
                assert_eq!(account_id, "acc2");
            }
            _ => panic!("expected a SignResult for the late participant"),
        }
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(200), rx.recv()).await,
                Ok(Some(_))
            ),
            "no second dispatch — an already-signed account must never be re-signed"
        );
        let a = &activities[&key];
        assert!(a.acted, "the activity stays acted");
        assert!(a.signed.contains("acc1"));
        assert!(a.participants.contains("acc2"));
    }

    #[tokio::test]
    async fn redispatch_qr_routes_through_teacher_assist_never_unsupported_spawn() {
        let cfg = cfg_countdown(15);
        let (tx, mut rx) = unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
        accounts.insert("acc1".to_string(), account_at("acc1", false));
        accounts.insert("teacher1".to_string(), account_at("teacher1", true));
        let key = (
            "http://127.0.0.1:1".to_string(),
            "qrcode".to_string(),
            "qr1".to_string(),
        );
        let mut activity = gate_activity(&["acc1"]);
        activity.kind = RollcallKind::Qr;
        activity.acted = true;
        activity.needs_resign.insert("acc1".to_string());
        let mut activities = HashMap::from([(key.clone(), activity)]);

        redispatch_signs(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            "acc1",
        );

        // The teacher-assist flow owns the restore: it tries (and fails) against the unreachable
        // host and reports a REAL qr error — never spawn_sign's generic "unsupported here".
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a teacher-assist result must arrive")
            .expect("a message must arrive");
        match message {
            MonitorMsg::SignResult {
                account_id,
                result: Err(error),
                ..
            } => {
                assert_eq!(account_id, "acc1");
                assert!(
                    error.contains("qr:"),
                    "expected a real teacher-assist error, got: {error}"
                );
                assert!(
                    !error.contains("unsupported"),
                    "the generic spawn_sign path must never run for QR: {error}"
                );
            }
            _ => panic!("expected a failed teacher-assist SignResult"),
        }
        assert!(
            !activities[&key].needs_resign.contains("acc1"),
            "the recovered account left the pending set"
        );
    }

    #[test]
    fn redispatch_qr_without_teacher_stays_pending_and_never_fake_dispatches() {
        let cfg = cfg_countdown(15);
        let (tx, mut rx) = unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let accounts: HashMap<String, Arc<Account>> = HashMap::new();
        let key = (
            "http://127.0.0.1:1".to_string(),
            "qrcode".to_string(),
            "qr1".to_string(),
        );
        let mut activity = gate_activity(&["acc1"]);
        activity.kind = RollcallKind::Qr;
        activity.acted = true;
        activity.needs_resign.insert("acc1".to_string());
        let mut activities = HashMap::from([(key.clone(), activity)]);

        redispatch_signs(
            &mut activities,
            &accounts,
            &tx,
            &group,
            &cfg,
            noop_cb,
            "acc1",
        );

        // 0 偽派發: no teacher → nothing dispatchable — no SignResult (not even an "unsupported"
        // one) may be produced, and the account stays pending for a later restore.
        assert!(activities[&key].needs_resign.contains("acc1"));
        assert!(
            rx.try_recv().is_err(),
            "no message may be produced without a teacher"
        );
    }

    #[tokio::test]
    async fn monitor_handle_drop_cancels_group_children() {
        // A discarded handle (re-Init replacing CoreState, teardown, error unwinds) must stop the
        // monitor: the actor ↔ group cycle keeps the group alive, so only the handle's Drop (or an
        // explicit stop) can abort the tracked tasks.
        let (tx, _rx) = unbounded_channel();
        let handle = MonitorHandle::new(tx);
        let started = Arc::new(AtomicBool::new(false));
        let ran_after_drop = Arc::new(AtomicBool::new(false));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let (s, r) = (started.clone(), ran_after_drop.clone());
        handle.group.spawn(async move {
            s.store(true, Ordering::SeqCst);
            let _ = start_tx.send(());
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                r.store(true, Ordering::SeqCst);
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
            .await
            .expect("the child must start");
        assert!(started.load(Ordering::SeqCst));

        drop(handle);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !ran_after_drop.load(Ordering::SeqCst),
            "dropping the handle must cancel every tracked child"
        );
    }
    #[test]
    fn overlapping_group_routes_fan_out_once_and_union_sources() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let plan = MonitorPlan {
            generation: 9,
            routes: vec![
                MonitorRoute {
                    source_targets: vec![TargetId::group("g1")],
                    detector_account_id: "detector".into(),
                    participant_account_ids: vec!["a".into(), "b".into()],
                    course_ids: Vec::new(),
                },
                MonitorRoute {
                    source_targets: vec![TargetId::group("g2")],
                    detector_account_id: "detector".into(),
                    participant_account_ids: vec!["b".into(), "c".into()],
                    course_ids: Vec::new(),
                },
            ],
        };
        let detection = Detected {
            generation: 9,
            account_id: "detector".into(),
            base_url: "https://example.test".into(),
            rollcall_id: "rollcall".into(),
            kind: RollcallKind::Number,
            course: "共同課程".into(),
            course_id: Some("course".into()),
        };
        let mut activities = HashMap::new();
        on_detected(
            &mut activities,
            DetectionContext {
                accounts: &HashMap::new(),
                tx: &tx,
                group: &group,
                cb: noop_cb,
                cfg: &cfg,
                plan: &plan,
            },
            detection,
        );
        let activity = activities.values().next().unwrap();
        assert_eq!(
            activity.participants,
            HashSet::from(["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            activity.sources,
            HashSet::from([TargetId::group("g1"), TargetId::group("g2")])
        );
    }

    #[test]
    fn removing_last_source_cancels_pending_but_retains_authorized_execution() {
        let mut pending = gate_activity(&["a"]);
        pending.sources.insert(TargetId::account("a"));
        let mut authorized = gate_activity(&["b"]);
        authorized.sources.insert(TargetId::account("b"));
        authorized.acted = true;
        let mut activities = HashMap::from([
            (("x".into(), "number".into(), "pending".into()), pending),
            (
                ("x".into(), "number".into(), "authorized".into()),
                authorized,
            ),
        ]);
        cancel_removed_sources(
            &mut activities,
            &mut HashMap::new(),
            &MonitorPlan {
                generation: 2,
                routes: Vec::new(),
            },
        );
        assert!(!activities.keys().any(|key| key.2 == "pending"));
        assert!(activities.keys().any(|key| key.2 == "authorized"));
    }

    #[test]
    fn definition_barrier_denies_mutation_until_rollback() {
        let cfg = cfg_countdown(15);
        let (tx, _rx) = unbounded_channel();
        let (panic_tx, _panic_rx) = unbounded_channel();
        let group = TaskGroup::new(noop_cb, panic_tx);
        let key = ("x".into(), "number".into(), "rollcall".into());
        let mut activity = gate_activity(&["a"]);
        activity.sources.insert(TargetId::account("a"));
        activity.mutation_blocked = true;
        let mut activities = HashMap::from([(key.clone(), activity)]);
        dispatch_signs(
            &mut activities,
            &HashMap::new(),
            &tx,
            &group,
            &cfg,
            noop_cb,
            &key,
        );
        assert!(
            !activities[&key].acted,
            "barrier must deny the mutation permit"
        );
        activities.get_mut(&key).unwrap().mutation_blocked = false;
        dispatch_signs(
            &mut activities,
            &HashMap::new(),
            &tx,
            &group,
            &cfg,
            noop_cb,
            &key,
        );
        assert!(
            activities[&key].acted,
            "rollback re-enables the pending permit"
        );
    }
}
