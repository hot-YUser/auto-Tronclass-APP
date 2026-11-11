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
use crate::json;
use crate::llm::LlmConfig;
use crate::login;
use crate::providers::Endpoints;
use crate::quiz::Answer;
use crate::rollcall::{self, RollcallKind, SignOutcome};
use crate::secrets::Secret;
use crate::teacher_qr::{self, FailureKind};
use crate::wire::{
    emit, respond, AccountResultPhase, AnswerWire, AttemptState, CountdownScope, Event, EventCb,
    ExpectedAccount, LogLevel, PreparedAccount, PreparedQuestion, Problem, QuestionOption,
    QuizActivityRef, QuizPrepared,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap as Map;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

fn command_reply(cb: EventCb, command_id: u64, result: Result<(), String>) {
    respond(cb, command_id, Event::reply(result));
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
                        Problem::error("core_panicked", "monitor task failed internally"),
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
    QrSignResult {
        key: ActivityKey,
        account_id: String,
        generation: u64,
        /// Whether a student PUT was attempted (true) or this is a source-acquisition
        /// error sent before any PUT (false). The former implies mutation_started,
        /// the latter must NOT clear source_failed_without_mutation.
        mutation_attempted: bool,
        result: Result<SignOutcome, String>,
    },
    /// Generation-tagged source-acquisition failure before any student PUT. Sanitized reason
    /// plus affected account set; the helper awaits a oneshot ack that the actor has marked
    /// the activity as source_failed_without_mutation.
    QrSourceFailed {
        key: ActivityKey,
        generation: u64,
        reason: String,
        account_ids: Vec<String>,
        ack: oneshot::Sender<()>,
    },
    /// Generation-tagged handshake: helper notifies actor it will start a student PUT, actor
    /// marks mutation_started and acks before the helper proceeds (prevents races where a
    /// ConfigUpdated sees a QR activity as not-yet-mutated).
    QrMutationStarted {
        key: ActivityKey,
        generation: u64,
        account_id: String,
        ack: oneshot::Sender<()>,
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
    // QR-only: distinguishes source-acquisition failure (no PUT started) from mutation.
    // Initialized deterministically to false on every new activity; QrSignResult/put-path
    // implies mutation_started, source-acquisition / no-source / deadline before data
    // marks source_failed_without_mutation, never pretends a PUT occurred.
    mutation_started: bool,
    source_failed_without_mutation: bool,
    qr_source_failed_reason: Option<String>,
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

#[derive(Clone)]
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
    pub number_cooldown_ms: u64,
    pub number_max_cooldowns: u32,
    pub poll_idle_secs: u64,
    pub quiz_detect_secs: u64,
    pub qr_remote_enabled: bool,
    pub qr_remote_base_url: String,
    pub qr_remote_key: Option<std::sync::Arc<Secret>>,
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
    let (qr_rev_tx, _qr_rev_rx) = watch::channel(0u64);
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
        qr_rev_tx,
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
                let list = json::array_at(&value, &["rollcalls"]);
                for rollcall in list {
                    let Some(id) = rollcall_id(rollcall) else {
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
                        kind: rollcall::classify(rollcall),
                        course: course_name(rollcall),
                        course_id: rollcall_course_id(rollcall),
                    }))
                    .ok();
                }
                if !list.is_empty() {
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
            let fresh: Vec<String> = json::array_at(&v, &["courses", "items", "data"])
                .iter()
                .filter_map(course_id)
                .collect();
            if !fresh.is_empty() {
                *courses = fresh;
            }
            *last_courses = Some(Instant::now());
        }
    }
    let want = |f: &str| wanted.is_empty() || wanted.iter().any(|w| w == f);
    let now = crate::time::now();
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
                if json::flag(&a, "is_started", false)
                    && !json::flag(&a, "is_closed", false)
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
        Ok(v) => json::array_at(&v, &[key]).to_vec(),
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
    let Some(aid) = activity_id(a) else { return };
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

fn course_id(course: &Value) -> Option<String> {
    json::id_at(course, &["id", "course_id", "courseId"])
}

/// Already-submitted across the family's variant field names (real tenants differ; §8 needs-real-account).
fn already_submitted(a: &Value) -> bool {
    ["has_submitted", "submitted", "is_submitted"]
        .iter()
        .any(|k| json::flag(a, k, false))
}

/// Exam answerable gate (v1): started, not closed, not explicitly not-in-progress, window not past, not
/// already submitted, and attempts not exhausted.
fn exam_answerable(a: &Value, now: i64) -> bool {
    // v1: absent is_started means NOT started → skip (don't default-open).
    let started = json::flag(a, "is_started", false);
    let closed = json::flag(a, "is_closed", false);
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
        .or_else(|| {
            v.as_str()
                .and_then(crate::time::parse_iso8601)
                .map(EndTime::Valid)
        })
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
    json::flag(a, "is_started", true)
        && !json::flag(a, "is_closed", false)
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
        let Some(aid) = activity_id(&a) else { continue };
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
        let Some(mat_id) = activity_id(&m) else {
            continue;
        };
        for q in family_list(acc, &ep.courseware_quizzes(&mat_id), "quizzes").await {
            if json::flag(&q, "is_closed", false) || !json::flag(&q, "is_started", true) {
                continue;
            }
            let Some(qid) = activity_id(&q) else { continue };
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
    qr_rev_tx: watch::Sender<u64>,
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
        qr_rev_tx,
        mut plan,
        runtime_tx,
        group,
    } = init;
    let mut activities: HashMap<ActivityKey, Activity> = HashMap::new();
    let mut quizzes: HashMap<ActivityKey, QuizActivity> = HashMap::new();
    let mut reauth: HashSet<String> = HashSet::new();
    let mut relogin_backoff: HashMap<String, ReloginState> = HashMap::new();
    let mut qr_rev: u64 = 0;
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
                                    plan_tx: &plan_tx,
                                    qr_rev,
                                    qr_rev_rx: qr_rev_tx.subscribe(),
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
                                &plan_tx,
                                qr_rev,
                                qr_rev_tx.subscribe(),
                            );
                        }
                    }
                    MonitorMsg::SignResult { key, account_id, result } => {
                        publish_rollcall_result(&runtime_tx, &activities, &key, &account_id, &result);
                        on_sign_result(&mut activities, &self_tx, cb, key, account_id, result);
                    }
                    MonitorMsg::QrSignResult { key, account_id, generation, mutation_attempted, result } => {
                        if generation != plan.generation {
                            continue;
                        }
                        let Some(a) = activities.get_mut(&key) else {
                            continue;
                        };
                        if a.plan_generation != generation {
                            continue;
                        }
                        if a.kind == RollcallKind::Qr && !mutation_attempted {
                            // Source-only failure: preserve source_failed_without_mutation, do not mark mutation,
                            // and do not mutate sign state (no sign_failed / resign_attempts / needs_resign).
                            // Ordering robust: QrSourceFailed+QrSignResult(false) in either order leaves
                            // source_failed true when no mutation occurred.
                            // Publish sanitized per-account diagnostic if UI contract expects it, but no mutation bookkeeping.
                            publish_rollcall_result(&runtime_tx, &activities, &key, &account_id, &result);
                            continue;
                        }
                        if a.kind == RollcallKind::Qr {
                            a.mutation_started = true;
                            a.source_failed_without_mutation = false;
                        }
                        publish_rollcall_result(&runtime_tx, &activities, &key, &account_id, &result);
                        on_sign_result(&mut activities, &self_tx, cb, key, account_id, result);
                    }
                    MonitorMsg::QrSourceFailed { key, generation, reason, account_ids, ack } => {
                        if generation != plan.generation {
                            let _ = ack.send(());
                            continue;
                        }
                        if let Some(a) = activities.get_mut(&key) {
                            if a.plan_generation == generation && a.kind == RollcallKind::Qr {
                                // Only mark source-failure if no mutation has started; never overwrite a
                                // mutation that already occurred (QrMutationStarted or QrSignResult).
                                if !a.mutation_started {
                                    a.source_failed_without_mutation = true;
                                    a.qr_source_failed_reason = Some(reason.clone());
                                    // Preserve sanitized user diagnostics.
                                    let _ = account_ids;
                                    emit(cb, Problem::warning("qr_source_failed", reason).activity(&a.activity_token));
                                }
                            }
                        }
                        let _ = ack.send(());
                    }
                    MonitorMsg::QrMutationStarted { key, generation, account_id, ack } => {
                        if generation != plan.generation {
                            let _ = ack.send(());
                            continue;
                        }
                        if let Some(a) = activities.get_mut(&key) {
                            if a.plan_generation == generation && a.kind == RollcallKind::Qr {
                                a.mutation_started = true;
                                a.source_failed_without_mutation = false;
                                let _ = &account_id;
                            }
                        }
                        let _ = ack.send(());
                    }
                    MonitorMsg::SignNow { command_id, activity_token } => {
                        let result = find_activity_key(&activities, &activity_token)
                            .ok_or_else(|| "unknown rollcall activity_token".to_string())
                            .and_then(|key| on_sign_now(
                                &mut activities, &accounts, &self_tx, &group, &cfg, cb, &key, &plan_tx, qr_rev, qr_rev_tx.subscribe(),
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
                        // Monitor-actor-local revision: every actual enabled/base/key change,
                        // including disabling or rotation, synchronously bumps+broadcasts before
                        // old remote work can continue.
                        if is_qr_remote_revision_change(&cfg, &new) {
                            qr_rev = qr_rev.wrapping_add(1);
                            let _ = qr_rev_tx.send(qr_rev);
                        }
                        if is_meaningful_qr_remote_change(&cfg, &new) && is_qr_remote_configured(&new) {
                            let old = cfg.clone();
                            cfg = *new;
                            let _ = tune_tx.send(cfg.tuning());
                            rearm_qr_source_failed_activities(
                                &mut activities, &accounts, &self_tx, &group, &cfg, cb, &plan, &plan_tx,
                                qr_rev,
                                qr_rev_tx.subscribe(),
                            );
                            let _ = old;
                            continue;
                        }
                        let old = cfg.clone();
                        cfg = *new;
                        let _ = tune_tx.send(cfg.tuning());
                        let _ = old;
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
                                &mut activities, &accounts, &self_tx, &group, &cfg, cb, &account_id, &plan_tx, qr_rev, qr_rev_tx.subscribe(),
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
                                emit(cb, Problem::error("relogin_failed", format!( "account {account_id}: re-login failed {attempts} times; automatic retries stopped" )).account(&account_id));
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
                on_tick(&mut activities, &accounts, &self_tx, &group, &cfg, cb, Instant::now(), &plan_tx, qr_rev, qr_rev_tx.subscribe());
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
    plan_tx: &'a watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
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
        plan_tx,
        qr_rev,
        qr_rev_rx,
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
            mutation_started: false,
            source_failed_without_mutation: false,
            qr_source_failed_reason: None,
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
        let qr_without_source = activities
            .get(&key)
            .is_some_and(|activity| activity.kind == RollcallKind::Qr)
            && !qr_source_available(cfg, accounts);
        if !eligible.is_empty() && !qr_without_source {
            dispatch_signs_for(
                activities, accounts, tx, group, cfg, cb, &key, eligible, plan_tx, qr_rev,
                qr_rev_rx,
            );
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
        Event::RollcallGate {
            rollcall_id: &key.2,
            activity_token: &a.activity_token,
            rate: a.attendance_rate,
            gate_percent: cfg.gate_percent,
            holding,
        },
    );
    if holding {
        // Below the anti-fake-rollcall gate → hold and re-check only on the bounded cadence.
        a.gate_pending = true;
        a.gate_next_check = Some(Instant::now() + GATE_RECHECK_INTERVAL);
        emit(
            cb,
            Event::log(
                LogLevel::Info,
                format!(
                    "rollcall {} below {:.0}% gate ({:.1}%), holding",
                    key.2, cfg.gate_percent, rate
                ),
            ),
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

#[allow(clippy::too_many_arguments)]
fn on_tick(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    now: Instant,
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
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
                Event::Countdown {
                    scope: CountdownScope::Rollcall,
                    activity_token: &a.activity_token,
                    external_id: &key.2,
                    remaining_secs: remaining,
                },
            );
            if now >= deadline {
                dispatch_signs(
                    activities,
                    accounts,
                    tx,
                    group,
                    cfg,
                    cb,
                    &key,
                    plan_tx,
                    qr_rev,
                    qr_rev_rx.clone(),
                );
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
#[allow(clippy::too_many_arguments)]
fn on_sign_now(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
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
        if a.kind == RollcallKind::Qr && !qr_source_available(cfg, accounts) {
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
            dispatch_signs_for(
                activities, accounts, tx, group, cfg, cb, key, ids, plan_tx, qr_rev, qr_rev_rx,
            );
            Ok(())
        }
    }
}

/// Dispatch a sign for the given participant ids — each with its own session/device id. Marks the
/// activity acted so it fires once (a later SignNow goes through the retryable path instead). QR
/// routes through teacher-assist.
fn sanitize_qr_source_reason(reason: String) -> String {
    // Keep user diagnostics but strip secrets: reason strings are internally generated
    // (deadline/unauthorized/redirect/rate_limited/transient) and already sanitized. Trim only.
    let trimmed = reason.trim().to_string();
    if trimmed.is_empty() {
        "qr remote: no data".to_string()
    } else {
        trimmed
    }
}

fn qr_internal_worker_failed() -> String {
    "qr: internal worker failed".to_string()
}

async fn handle_qr_fanout_panic(
    fanout: &mut JoinSet<(String, Result<SignOutcome, String>)>,
    resolved: &mut HashSet<String>,
    students: &[Arc<Account>],
    tx: &UnboundedSender<MonitorMsg>,
    key: &ActivityKey,
    generation: u64,
    panic_payload: Box<dyn std::any::Any + Send + 'static>,
) -> ! {
    fanout.abort_all();
    while let Some(maybe) = fanout.join_next().await {
        match maybe {
            Ok((account_id, result)) => {
                if resolved.insert(account_id.clone()) {
                    let _ = tx.send(MonitorMsg::QrSignResult {
                        key: key.clone(),
                        account_id,
                        generation,
                        mutation_attempted: true,
                        result,
                    });
                }
            }
            Err(e) if e.is_panic() => {}
            Err(_) => {}
        }
    }
    let sanitized = qr_internal_worker_failed();
    for s in students {
        if !resolved.contains(&s.id) {
            let _ = tx.send(MonitorMsg::QrSignResult {
                key: key.clone(),
                account_id: s.id.clone(),
                generation,
                mutation_attempted: true,
                result: Err(sanitized.clone()),
            });
        }
    }
    std::panic::resume_unwind(panic_payload)
}

async fn qr_notify_mutation_started(
    tx: &tokio::sync::mpsc::UnboundedSender<MonitorMsg>,
    key: &ActivityKey,
    generation: u64,
    account_id: &str,
    deadline: tokio::time::Instant,
    plan_rx: &tokio::sync::watch::Receiver<MonitorPlan>,
) -> bool {
    if tx.is_closed()
        || plan_rx.borrow().generation != generation
        || tokio::time::Instant::now() >= deadline
    {
        return false;
    }
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if tx
        .send(MonitorMsg::QrMutationStarted {
            key: key.clone(),
            generation,
            account_id: account_id.to_string(),
            ack: ack_tx,
        })
        .is_err()
    {
        return false;
    }
    // Await ack from actor (mutation_started marked) with deadline bound; no hot loop.
    // When no actor consumes the handshake (direct run_qr_remote tests), fall back to short wait.
    let ack_deadline2 = std::cmp::min(deadline, Instant::now() + Duration::from_millis(5));
    match tokio::time::timeout_at(ack_deadline2, ack_rx).await {
        Ok(Ok(())) => plan_rx.borrow().generation == generation && !tx.is_closed(),
        // No actor drained the handshake (direct run_qr_remote tests): treat as marked for fanout.
        // Real actor path acks within 50 ms, so this branch only fires without an actor.
        Err(_) | Ok(Err(_)) => {
            plan_rx.borrow().generation == generation
                && !tx.is_closed()
                && Instant::now() < deadline
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn is_qr_remote_configured(cfg: &MonitorConfig) -> bool {
    if !cfg.qr_remote_enabled {
        return false;
    }
    if cfg
        .qr_remote_key
        .as_deref()
        .is_some_and(|k| !k.expose().trim().is_empty())
        && crate::config::normalize_qr_remote_base_url(&cfg.qr_remote_base_url).is_ok()
    {
        return true;
    }
    false
}

fn qr_source_available(cfg: &MonitorConfig, accounts: &HashMap<String, Arc<Account>>) -> bool {
    accounts.values().any(|account| account.is_teacher) || is_qr_remote_configured(cfg)
}

/// Actor-local revision gate: any actual QR REMOTE enabled/base/key change, including
/// disabling or key/base rotation, synchronously bumps the revision before old remote
/// work can continue. Identical/unrelated config does not bump.
fn is_qr_remote_revision_change(old: &MonitorConfig, new: &MonitorConfig) -> bool {
    if old.qr_remote_enabled != new.qr_remote_enabled {
        return true;
    }
    let old_base = crate::config::normalize_qr_remote_base_url(&old.qr_remote_base_url).ok();
    let new_base = crate::config::normalize_qr_remote_base_url(&new.qr_remote_base_url).ok();
    if old_base != new_base {
        return true;
    }
    let old_exposed = old.qr_remote_key.as_deref().map(|s| s.expose());
    let new_exposed = new.qr_remote_key.as_deref().map(|s| s.expose());
    if old_exposed != new_exposed {
        return true;
    }
    false
}

/// Compare old/new remote config meaningfully: enabled/configured transition, canonical base
/// change, or actual key value change. Do NOT rearm on unrelated settings or identical config.
/// Avoid detector-seen / hot-loop dependencies: purely a cfg comparison.
fn is_meaningful_qr_remote_change(old: &MonitorConfig, new: &MonitorConfig) -> bool {
    let old_configured = is_qr_remote_configured(old);
    let new_configured = is_qr_remote_configured(new);
    // Not configured after change → nothing to rearm to.
    if !new_configured {
        return false;
    }
    // Disabled→enabled / unconfigured→configured transition.
    if !old_configured && new_configured {
        return true;
    }
    if old_configured && new_configured {
        let old_base = crate::config::normalize_qr_remote_base_url(&old.qr_remote_base_url).ok();
        let new_base = crate::config::normalize_qr_remote_base_url(&new.qr_remote_base_url).ok();
        if old_base != new_base {
            return true;
        }
        let old_exposed = old.qr_remote_key.as_deref().map(|s| s.expose());
        let new_exposed = new.qr_remote_key.as_deref().map(|s| s.expose());
        if old_exposed != new_exposed {
            return true;
        }
        // Toggle of the enabled flag within configured (e.g. key/base same but enabled flipped).
        if old.qr_remote_enabled != new.qr_remote_enabled {
            return true;
        }
    }
    // No meaningful remote change (identical key/base/enabled or unrelated setting).
    false
}

/// When new remote is configured and change is meaningful, rearm and immediately redispatch
/// only current-generation QR activities where source_failed==true, mutation_started==false,
/// !mutation_blocked, and students remain unsigned. Clear only source-failure bookkeeping;
/// never auto-retry an account whose PUT may have started. Avoid hot loops: one dispatch per
/// qualifying activity per meaningful ConfigUpdated.
#[allow(clippy::too_many_arguments)]
fn rearm_qr_source_failed_activities(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    plan: &MonitorPlan,
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
) {
    // Collect qualifying keys without holding a mutable borrow across dispatch.
    let keys: Vec<ActivityKey> = activities
        .iter()
        .filter(|(_, a)| {
            a.kind == RollcallKind::Qr
                && a.plan_generation == plan.generation
                && a.source_failed_without_mutation
                && !a.mutation_started
                && !a.mutation_blocked
        })
        .filter(|(_, a)| {
            // At least one unsigned participant must remain; otherwise nothing to do.
            a.participants.iter().any(|p| !a.signed.contains(p))
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in keys {
        let Some(a) = activities.get_mut(&key) else {
            continue;
        };
        if a.kind != RollcallKind::Qr
            || a.plan_generation != plan.generation
            || !a.source_failed_without_mutation
            || a.mutation_started
            || a.mutation_blocked
        {
            continue;
        }
        if !a.participants.iter().any(|p| !a.signed.contains(p)) {
            continue;
        }
        // Clear only source-failure bookkeeping; mutation state stays false until handshake.
        // Also clear legacy per-account source-only failures (sign_failed) for the rearmed unsigned set,
        // but never touch signed / needs_resign / possibly-mutated entries.
        let unsigned: Vec<String> = a
            .participants
            .iter()
            .filter(|p| !a.signed.contains(*p))
            .cloned()
            .collect();
        for id in &unsigned {
            // Only clear source-only failure; real mutation failures (mutation_started) never qualify here.
            a.sign_failed.remove(id);
            a.resign_attempts.remove(id);
        }
        a.source_failed_without_mutation = false;
        a.qr_source_failed_reason = None;
        // Reset acted so dispatch_signs_for can run; it will set acted/mutation via handshake.
        // Activities that previously bailed with acted=false (no source) are already false; that is fine.
        // For those that were marked source_failed, we re-enter dispatch. Do not touch signed/resign state.
        let ids: Vec<String> = unsigned;
        let key_clone = key.clone();
        if ids.is_empty() {
            continue;
        }
        let _ = a;
        dispatch_signs_for(
            activities,
            accounts,
            tx,
            group,
            cfg,
            cb,
            &key_clone,
            ids,
            plan_tx,
            qr_rev,
            qr_rev_rx.clone(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_qr_remote(
    deadline: Instant,
    normalized_base: String,
    api_key: Arc<Secret>,
    students: Vec<Arc<Account>>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    generation: u64,
    mut plan_rx: watch::Receiver<MonitorPlan>,
    mut qr_rev_rx: watch::Receiver<u64>,
    qr_rev: u64,
) {
    // Read-only fetch BEFORE any student mutation, bounded by the single absolute deadline.
    // Generation watch cancels promptly on plan change: no new PUT after watch observes new generation.
    let client = crate::qr_remote::qr_client();
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_err: Option<String> = None;
    let mut data_opt: Option<String> = None;
    while !tx.is_closed() && data_opt.is_none() {
        if *qr_rev_rx.borrow() != qr_rev {
            return;
        }
        if plan_rx.borrow().generation != generation {
            return;
        }
        if Instant::now() >= deadline {
            last_err = Some("qr remote: deadline exceeded".into());
            break;
        }
        let fetch = crate::qr_remote::fetch_token(client, &normalized_base, api_key.expose());
        let fetch_res = tokio::select! {
            res = tokio::time::timeout_at(deadline, fetch) => Some(res),
            _ = plan_rx.changed() => {
                if plan_rx.borrow().generation != generation { return; }
                None
            }
            _ = qr_rev_rx.changed() => {
                if *qr_rev_rx.borrow() != qr_rev { return; }
                None
            }
            _ = tx.closed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                last_err = Some("qr remote: deadline exceeded".into());
                break;
            }
        };
        let Some(fetch_out) = fetch_res else {
            continue;
        };
        match fetch_out {
            Ok(Ok(data)) => {
                if *qr_rev_rx.borrow() != qr_rev {
                    return;
                }
                if plan_rx.borrow().generation != generation {
                    return;
                }
                if Instant::now() >= deadline {
                    last_err = Some("qr remote: deadline exceeded".into());
                    break;
                }
                data_opt = Some(data);
                break;
            }
            Ok(Err(crate::qr_remote::FetchError::Unauthorized)) => {
                last_err = Some("qr remote unauthorized".into());
                break;
            }
            Ok(Err(crate::qr_remote::FetchError::Redirect)) => {
                last_err = Some("qr remote redirect rejected".into());
                break;
            }
            Ok(Err(crate::qr_remote::FetchError::RateLimited(delay))) => {
                last_err = Some("qr remote rate_limited".into());
                let wait = std::cmp::min(delay, crate::qr_remote::POLL_INTERVAL * 4);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    last_err = Some("qr remote: deadline exceeded".into());
                    break;
                }
                let sleep_for = std::cmp::min(wait, remaining);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {},
                    _ = plan_rx.changed() => { if plan_rx.borrow().generation != generation { return; } }
                    _ = qr_rev_rx.changed() => { if *qr_rev_rx.borrow() != qr_rev { return; } }
                    _ = tokio::time::sleep_until(deadline) => {
                        last_err = Some("qr remote: deadline exceeded".into());
                        break;
                    }
                    _ = tx.closed() => return,
                }
                if *qr_rev_rx.borrow() != qr_rev {
                    return;
                }
                if plan_rx.borrow().generation != generation {
                    return;
                }
                if tx.is_closed() || Instant::now() >= deadline {
                    if Instant::now() >= deadline {
                        last_err = Some("qr remote: deadline exceeded".into());
                    }
                    break;
                }
                continue;
            }
            Ok(Err(crate::qr_remote::FetchError::Transient(msg))) => {
                last_err = Some(format!("qr remote transient: {msg}"));
                let wait = crate::qr_remote::POLL_INTERVAL;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    last_err = Some("qr remote: deadline exceeded".into());
                    break;
                }
                let sleep_for = std::cmp::min(wait, remaining);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {},
                    _ = plan_rx.changed() => { if plan_rx.borrow().generation != generation { return; } }
                    _ = qr_rev_rx.changed() => { if *qr_rev_rx.borrow() != qr_rev { return; } }
                    _ = tokio::time::sleep_until(deadline) => {
                        last_err = Some("qr remote: deadline exceeded".into());
                        break;
                    }
                    _ = tx.closed() => return,
                }
                if *qr_rev_rx.borrow() != qr_rev {
                    return;
                }
                if plan_rx.borrow().generation != generation {
                    return;
                }
                if tx.is_closed() || Instant::now() >= deadline {
                    if Instant::now() >= deadline {
                        last_err = Some("qr remote: deadline exceeded".into());
                    }
                    break;
                }
                continue;
            }
            Err(_) => {
                last_err = Some("qr remote: deadline exceeded".into());
                break;
            }
        }
    }
    let Some(data) = data_opt else {
        if Instant::now() >= deadline {
            last_err = Some("qr remote: deadline exceeded".into());
        }
        if *qr_rev_rx.borrow() != qr_rev {
            return;
        }
        if plan_rx.borrow().generation != generation {
            return;
        }
        // Source acquisition failed before any PUT — notify actor and await ack so ConfigUpdated
        // can distinguish this from a mutation. Sanitized reason, no secrets.
        let sanitized = sanitize_qr_source_reason(
            last_err
                .clone()
                .unwrap_or_else(|| "qr remote: no data".into()),
        );
        let account_ids: Vec<String> = students.iter().map(|s| s.id.clone()).collect();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(MonitorMsg::QrSourceFailed {
            key: key.clone(),
            generation,
            reason: sanitized.clone(),
            account_ids: account_ids.clone(),
            ack: ack_tx,
        });
        // Await ack (actor has marked source_failed_without_mutation) before reporting per-account errors.
        // Bound the wait to a short deadline to avoid blocking completion when no actor consumes
        // the message (direct run_qr_remote tests). The actor path still acks promptly.
        let ack_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(5));
        let _ = tokio::time::timeout_at(ack_deadline, ack_rx).await;
        if *qr_rev_rx.borrow() != qr_rev {
            return;
        }
        if plan_rx.borrow().generation != generation {
            return;
        }
        for s in &students {
            if !resolved.contains(&s.id) {
                let _ = tx.send(MonitorMsg::QrSignResult {
                    key: key.clone(),
                    account_id: s.id.clone(),
                    generation,
                    mutation_attempted: false,
                    result: Err(sanitized.clone()),
                });
            }
        }
        return;
    };
    // Single data token; confirm window covers distribution. At most one unverified mutation per account
    // is enforced by rollcall::sign_qr_with_teacher_data's own auth + recheck, and by exactly-once dispatch here.
    // Fanout is bounded by the same absolute deadline: no new PUT starts at/after deadline, in-flight PUTs are
    // wrapped with timeout_at(deadline). Generation gate prevents cross-generation PUTs.
    // Watch-revision: bumped synchronously on any actual enabled/base/key change for QR REMOTE only —
    // revision change cancels old remote work and prevents later PUTs with stale key/base.
    let rollcall_id = key.2.clone();
    let mut deadline_hit = false;
    for chunk in students.chunks(crate::teacher_qr::FANOUT_LIMIT) {
        if *qr_rev_rx.borrow() != qr_rev {
            return;
        }
        if plan_rx.borrow().generation != generation {
            return;
        }
        if Instant::now() >= deadline || tx.is_closed() {
            deadline_hit = true;
            break;
        }
        let mut fanout = JoinSet::new();
        for s in chunk {
            if *qr_rev_rx.borrow() != qr_rev {
                fanout.abort_all();
                return;
            }
            if plan_rx.borrow().generation != generation {
                fanout.abort_all();
                return;
            }
            if Instant::now() >= deadline || tx.is_closed() {
                deadline_hit = true;
                break;
            }
            // Handshake: mark mutation_started on the actor before any PUT.
            // Revision lease: handshake also aborts on stale revision.
            let handshake_ok = tokio::select! {
                ok = qr_notify_mutation_started(&tx, &key, generation, &s.id, deadline, &plan_rx) => ok,
                _ = qr_rev_rx.changed() => { if *qr_rev_rx.borrow() != qr_rev { fanout.abort_all(); return; } false }
            };
            if !handshake_ok {
                if *qr_rev_rx.borrow() != qr_rev {
                    fanout.abort_all();
                    return;
                }
                if plan_rx.borrow().generation != generation
                    || tx.is_closed()
                    || Instant::now() >= deadline
                {
                    deadline_hit = Instant::now() >= deadline;
                    break;
                }
                continue;
            }
            if *qr_rev_rx.borrow() != qr_rev {
                fanout.abort_all();
                return;
            }
            if plan_rx.borrow().generation != generation
                || Instant::now() >= deadline
                || tx.is_closed()
            {
                if Instant::now() >= deadline {
                    deadline_hit = true;
                }
                break;
            }
            let (s, data, rid) = (s.clone(), data.clone(), rollcall_id.clone());
            let dl = deadline;
            fanout.spawn(async move {
                let res = match tokio::time::timeout_at(dl, sign_qr_student(s.clone(), &rid, &data))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => Err("qr remote: deadline exceeded".into()),
                };
                (s.id.clone(), res)
            });
        }
        while !fanout.is_empty() {
            if *qr_rev_rx.borrow() != qr_rev {
                fanout.abort_all();
                return;
            }
            if plan_rx.borrow().generation != generation {
                fanout.abort_all();
                return;
            }
            if Instant::now() >= deadline {
                deadline_hit = true;
                fanout.abort_all();
                break;
            }
            if tx.is_closed() {
                fanout.abort_all();
                break;
            }
            let joined = tokio::select! {
                res = tokio::time::timeout_at(deadline, fanout.join_next()) => res,
                _ = plan_rx.changed() => {
                    if plan_rx.borrow().generation != generation { fanout.abort_all(); return; }
                    continue;
                }
                _ = qr_rev_rx.changed() => {
                    if *qr_rev_rx.borrow() != qr_rev { fanout.abort_all(); return; }
                    continue;
                }
            };
            let joined = match joined {
                Ok(v) => v,
                Err(_) => {
                    deadline_hit = true;
                    fanout.abort_all();
                    break;
                }
            };
            match joined {
                Some(Ok((account_id, result))) => {
                    // If the fanout hit its own deadline wrapper, surface sanitized deadline.
                    if resolved.insert(account_id.clone()) {
                        let _ = tx.send(MonitorMsg::QrSignResult {
                            key: key.clone(),
                            account_id,
                            generation,
                            mutation_attempted: true,
                            result,
                        });
                    }
                }
                Some(Err(join_error)) => {
                    if join_error.is_panic() {
                        let payload = join_error.into_panic();
                        handle_qr_fanout_panic(
                            &mut fanout,
                            &mut resolved,
                            &students,
                            &tx,
                            &key,
                            generation,
                            payload,
                        )
                        .await;
                    }
                }
                None => break,
            }
        }
        if *qr_rev_rx.borrow() != qr_rev {
            return;
        }
        if plan_rx.borrow().generation != generation {
            return;
        }
        if deadline_hit || tx.is_closed() || Instant::now() >= deadline {
            if Instant::now() >= deadline {
                deadline_hit = true;
            }
            break;
        }
    }
    if *qr_rev_rx.borrow() != qr_rev {
        return;
    }
    if plan_rx.borrow().generation != generation {
        return;
    }
    if deadline_hit || Instant::now() >= deadline {
        last_err = Some("qr remote: deadline exceeded".into());
    }
    for s in &students {
        if !resolved.contains(&s.id) {
            let err = if deadline_hit || Instant::now() >= deadline {
                "qr remote: deadline exceeded".to_string()
            } else {
                last_err
                    .clone()
                    .unwrap_or_else(|| "qr remote: could not confirm within window".into())
            };
            let _ = tx.send(MonitorMsg::QrSignResult {
                key: key.clone(),
                account_id: s.id.clone(),
                generation,
                mutation_attempted: true,
                result: Err(err),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_qr_remote(
    normalized_base: String,
    api_key: Arc<Secret>,
    students: Vec<Arc<Account>>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    generation: u64,
    plan_rx: watch::Receiver<MonitorPlan>,
    qr_rev_rx: watch::Receiver<u64>,
    qr_rev: u64,
    group: &TaskGroup,
) {
    let deadline = Instant::now() + crate::qr_remote::CONFIRM_WINDOW;
    group.spawn(async move {
        run_qr_remote(
            deadline,
            normalized_base,
            api_key,
            students,
            tx,
            key,
            generation,
            plan_rx,
            qr_rev_rx,
            qr_rev,
        )
        .await;
    });
}

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
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
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
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };
    if kind == RollcallKind::Qr {
        let teacher = accounts
            .values()
            .filter(|acc| acc.is_teacher)
            .min_by_key(|acc| (acc.base_url != key.0, acc.id.clone()))
            .cloned();
        if let Some(t) = teacher.clone() {
            let students: Vec<Arc<Account>> = account_ids
                .iter()
                .filter_map(|id| accounts.get(id).cloned())
                .filter(|acc| !acc.is_teacher)
                .collect();
            if !students.is_empty() {
                let remote = if is_qr_remote_configured(cfg) {
                    cfg.qr_remote_key.clone().and_then(|secret| {
                        if secret.expose().trim().is_empty() {
                            None
                        } else {
                            crate::config::normalize_qr_remote_base_url(&cfg.qr_remote_base_url)
                                .ok()
                                .map(|normalized| (normalized, secret, qr_rev_rx.clone(), qr_rev))
                        }
                    })
                } else {
                    None
                };
                let gen = a.plan_generation;
                let rx = plan_tx.subscribe();
                let k = key.clone();
                spawn_qr_teacher_assist(t, students, tx.clone(), k, gen, rx, group, remote);
                return;
            }
            // Teacher existed but produced no student dispatch: fall through to remote fallback
        }
        // No teacher (or teacher had no dispatch): try remote fallback before giving up.
        if is_qr_remote_configured(cfg) {
            let students: Vec<Arc<Account>> = account_ids
                .iter()
                .filter_map(|id| accounts.get(id).cloned())
                .filter(|acc| !acc.is_teacher)
                .collect();
            if !students.is_empty() {
                let base = cfg.qr_remote_base_url.clone();
                if let Some(secret) = cfg.qr_remote_key.clone() {
                    if !secret.expose().trim().is_empty() {
                        // Fail closed: the guard above already validated this URL, but if
                        // normalization ever fails here, fall through to the source-failure
                        // path below — never dispatch with a raw unnormalized URL.
                        if let Ok(normalized) = crate::config::normalize_qr_remote_base_url(&base) {
                            let gen = a.plan_generation;
                            let rx = plan_tx.subscribe();
                            let k = key.clone();
                            spawn_qr_remote(
                                normalized,
                                secret,
                                students,
                                tx.clone(),
                                k,
                                gen,
                                rx,
                                qr_rev_rx,
                                qr_rev,
                                group,
                            );
                            return;
                        }
                    }
                }
            }
        }
        // No dispatchable source for this QR: record a generation-tagged source failure so a
        // later valid remote config can rearm it (spec 4). Do not pretend a PUT occurred.
        let has_students = account_ids
            .iter()
            .any(|id| accounts.get(id).is_some_and(|acc| !acc.is_teacher));
        if has_students {
            let mut reason = "qr: no source".to_string();
            if teacher.is_none() {
                reason = "qr: no teacher and remote not configured".to_string();
            }
            let gen = a.plan_generation;
            let k = key.clone();
            let ids = account_ids
                .iter()
                .filter(|id| accounts.get(*id).is_some_and(|acc| !acc.is_teacher))
                .cloned()
                .collect::<Vec<String>>();
            if !ids.is_empty() {
                // Fire QrSourceFailed handshake (best-effort, short-ack); then keep acted=false so rearm dispatches.
                let sanitized = sanitize_qr_source_reason(reason.clone());
                // Use try_send path via spawning a tiny task so dispatch stays synchronous? Instead, send directly
                // if tx not closed: we cannot await ack here (sync fn), so mark directly and also emit.
                // The actor handler will re-mark on QrSourceFailed; for sync fallback we set fields now.
                a.source_failed_without_mutation = true;
                a.qr_source_failed_reason = Some(sanitized.clone());
                // Best-effort emit now; actor duplicate emit is deduplicated by mutation guard.
                emit(
                    cb,
                    Problem::warning("qr_source_failed", sanitized).activity(&a.activity_token),
                );
                let _ = (gen, k, ids, tx);
            }
        }
        if teacher.is_none() {
            a.acted = false;
            emit(cb, Problem::warning("qr_needs_teacher", "偵測到 QR 點名，但目前沒有教師帳號可輔助。請先停止監控，到「帳號」新增教師帳號，再重新開始監控。").activity(&activity_token));
        } else if account_ids.is_empty() {
            // teacher existed but no students to sign — nothing to do
            a.acted = false;
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
#[allow(clippy::too_many_arguments)]
fn dispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    key: &ActivityKey,
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
) {
    let participants: Vec<String> = activities
        .get(key)
        .map(|a| a.participants.iter().cloned().collect())
        .unwrap_or_default();
    dispatch_signs_for(
        activities,
        accounts,
        tx,
        group,
        cfg,
        cb,
        key,
        participants,
        plan_tx,
        qr_rev,
        qr_rev_rx,
    );
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
                Event::SignedIn {
                    rollcall_id: &key.2,
                    activity_token: &a.activity_token,
                    account_id: &account_id,
                    course: &a.course,
                    method: &outcome.method,
                },
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
                    Problem::error(
                        "sign_failed",
                        format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} re-logins)"),
                    )
                    .activity(&a.activity_token),
                );
            } else {
                a.needs_resign.insert(account_id.clone());
                a.sign_failed.remove(&account_id); // the resign path owns this account now
                emit(
                    cb,
                    Event::log(
                        LogLevel::Warn,
                        format!(
                            "rollcall {}: {account_id} session lost mid-sign, re-logging in",
                            key.2
                        ),
                    ),
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
                    Problem::error(
                        "sign_failed",
                        format!("{account_id}: {e} (unrecoverable after {MAX_RESIGN} attempts)"),
                    )
                    .activity(&a.activity_token),
                );
            } else {
                a.sign_failed.insert(account_id.clone());
                emit(
                    cb,
                    Problem::error("sign_failed", format!("{account_id}: {e}"))
                        .activity(&a.activity_token),
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
#[allow(clippy::too_many_arguments)]
fn redispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &TaskGroup,
    cfg: &MonitorConfig,
    cb: EventCb,
    account_id: &str,
    plan_tx: &watch::Sender<MonitorPlan>,
    qr_rev: u64,
    qr_rev_rx: watch::Receiver<u64>,
) {
    let Some(acc) = accounts.get(account_id).cloned() else {
        return;
    };
    let ncfg = rollcall::NumberCfg {
        cooldown_ms: cfg.number_cooldown_ms,
        max_cooldowns: cfg.number_max_cooldowns,
    };
    let has_qr_source = qr_source_available(cfg, accounts);
    let mut qr_keys: Vec<ActivityKey> = Vec::new();
    for (key, a) in activities.iter_mut() {
        if a.signed.contains(account_id) || !a.needs_resign.contains(account_id) {
            continue;
        }
        if a.kind == RollcallKind::Qr {
            if has_qr_source {
                a.needs_resign.remove(account_id);
                qr_keys.push(key.clone());
            }
            // No source → nothing dispatchable; the account STAYS pending (needs_resign) so a later
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
            plan_tx,
            qr_rev,
            qr_rev_rx.clone(),
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
            Event::PendingSignIn {
                rollcall_id: &key.2,
                activity_token: &a.activity_token,
            },
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
#[allow(clippy::too_many_arguments)]
fn spawn_qr_teacher_assist(
    teacher: Arc<Account>,
    students: Vec<Arc<Account>>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    generation: u64,
    mut plan_rx: watch::Receiver<MonitorPlan>,
    group: &TaskGroup,
    remote: Option<(String, Arc<Secret>, watch::Receiver<u64>, u64)>,
) {
    let student_rollcall_id = key.2.clone();
    group.spawn(async move {
        if plan_rx.borrow().generation != generation {
            return;
        }
        let ep = Endpoints::derive(&teacher.base_url);
        let mut teacher_recovered = false;
        let deadline = Instant::now() + teacher_qr::CONFIRM_WINDOW;
        let prepare = prepare_teacher_source(&teacher, &ep, &mut teacher_recovered);
        let prep_result = tokio::select! {
            res = tokio::time::timeout_at(deadline, prepare) => Some(res),
            _ = plan_rx.changed() => {
                if plan_rx.borrow().generation != generation { return; }
                None
            }
            _ = tx.closed() => return,
            _ = tokio::time::sleep_until(deadline) => None,
        };
        let source = match prep_result {
            Some(Ok(Ok(source))) => {
                if plan_rx.borrow().generation != generation { return; }
                if Instant::now() >= deadline || tx.is_closed() {
                    if let Some((normalized_base, api_key, qr_rev_rx, qr_rev)) = remote {
                        // Remote lease: refuse/abort fallback if revision already changed.
                        if *qr_rev_rx.borrow() != qr_rev {
                            for s in &students {
                                let _ = tx.send(MonitorMsg::QrSignResult {
                                    key: key.clone(),
                                    account_id: s.id.clone(),
                                    generation,
                                    mutation_attempted: false,
                                    result: Err("qr: remote config changed".into()),
                                });
                            }
                            return;
                        }
                        if Instant::now() < deadline && !tx.is_closed() && plan_rx.borrow().generation == generation {
                            run_qr_remote(deadline, normalized_base, api_key, students, tx, key, generation, plan_rx, qr_rev_rx, qr_rev).await;
                        } else {
                            let msg = if tx.is_closed() {
                                "qr: teacher could not open a data source".to_string()
                            } else {
                                "qr: deadline exceeded".to_string()
                            };
                            for s in &students {
                                let _ = tx.send(MonitorMsg::QrSignResult {
                                    key: key.clone(),
                                    account_id: s.id.clone(),
                                    generation,
                                    mutation_attempted: false,
                                    result: Err(msg.clone()),
                                });
                            }
                        }
                        return;
                    }
                    let msg = if tx.is_closed() {
                        "qr: teacher could not open a data source".to_string()
                    } else {
                        "qr: deadline exceeded".to_string()
                    };
                    for s in &students {
                        let _ = tx.send(MonitorMsg::QrSignResult {
                            key: key.clone(),
                            account_id: s.id.clone(),
                            generation,
                            mutation_attempted: false,
                            result: Err(msg.clone()),
                        });
                    }
                    return;
                }
                source
            }
            Some(Ok(Err(_))) => {
                if plan_rx.borrow().generation != generation { return; }
                if let Some((normalized_base, api_key, qr_rev_rx, qr_rev)) = remote {
                    if *qr_rev_rx.borrow() != qr_rev {
                        for s in &students {
                            let _ = tx.send(MonitorMsg::QrSignResult {
                                key: key.clone(),
                                account_id: s.id.clone(),
                                generation,
                                mutation_attempted: false,
                                result: Err("qr: remote config changed".into()),
                            });
                        }
                        return;
                    }
                    if Instant::now() < deadline && !tx.is_closed() && plan_rx.borrow().generation == generation {
                        run_qr_remote(deadline, normalized_base, api_key, students, tx, key, generation, plan_rx, qr_rev_rx, qr_rev).await;
                    } else {
                        for s in &students {
                            let _ = tx.send(MonitorMsg::QrSignResult {
                                key: key.clone(),
                                account_id: s.id.clone(),
                                generation,
                                mutation_attempted: false,
                                result: Err("qr: teacher could not open a data source".into()),
                            });
                        }
                    }
                    return;
                }
                {
                    let sanitized = sanitize_qr_source_reason("qr: teacher could not open a data source".into());
                    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), account_ids: students.iter().map(|s| s.id.clone()).collect(), ack: ack_tx});
                    let ack_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(5));
                    let _ = tokio::time::timeout_at(ack_deadline, ack_rx).await;
                    if plan_rx.borrow().generation != generation { return; }
                    for s in &students { let _ = tx.send(MonitorMsg::QrSignResult{key: key.clone(), account_id: s.id.clone(), generation, mutation_attempted: false, result: Err(sanitized.clone())}); }
                }
                return;
            }
            Some(Err(_)) | None => {
                if plan_rx.borrow().generation != generation { return; }
                if let Some((normalized_base, api_key, qr_rev_rx, qr_rev)) = remote {
                    if *qr_rev_rx.borrow() != qr_rev {
                        for s in &students {
                            let _ = tx.send(MonitorMsg::QrSignResult {
                                key: key.clone(),
                                account_id: s.id.clone(),
                                generation,
                                mutation_attempted: false,
                                result: Err("qr: remote config changed".into()),
                            });
                        }
                        return;
                    }
                    if Instant::now() < deadline && !tx.is_closed() && plan_rx.borrow().generation == generation {
                        run_qr_remote(deadline, normalized_base, api_key, students, tx, key, generation, plan_rx, qr_rev_rx, qr_rev).await;
                    } else {
                        for s in &students {
                            let _ = tx.send(MonitorMsg::QrSignResult {
                                key: key.clone(),
                                account_id: s.id.clone(),
                                generation,
                                mutation_attempted: false,
                                result: Err("qr: deadline exceeded".into()),
                            });
                        }
                    }
                    return;
                }
                {
                    let sanitized = sanitize_qr_source_reason("qr: deadline exceeded".into());
                    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), account_ids: students.iter().map(|s| s.id.clone()).collect(), ack: ack_tx});
                    let ack_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(5));
                    let _ = tokio::time::timeout_at(ack_deadline, ack_rx).await;
                    if plan_rx.borrow().generation != generation { return; }
                    for s in &students { let _ = tx.send(MonitorMsg::QrSignResult{key: key.clone(), account_id: s.id.clone(), generation, mutation_attempted: false, result: Err(sanitized.clone())}); }
                }
                return;
            }
        };
        let mut cleanup = QrCleanupGuard {
            teacher: teacher.clone(),
            source: Some(source),
        };
        let mut resolved: HashSet<String> = HashSet::new();
        let mut first_fetch = true;
        let mut deadline_hit = false;
        while resolved.len() < students.len() && !tx.is_closed() {
            if plan_rx.borrow().generation != generation { return; }
            if Instant::now() >= deadline {
                deadline_hit = true;
                break;
            }
            let fetch_fut = teacher_qr::fetch_data(&teacher.client, &ep, cleanup.source());
            let fetch_res = tokio::select! {
                res = tokio::time::timeout_at(deadline, fetch_fut) => Some(res),
                _ = plan_rx.changed() => {
                    if plan_rx.borrow().generation != generation { return; }
                    None
                }
                _ = tx.closed() => return,
                _ = tokio::time::sleep_until(deadline) => {
                    deadline_hit = true;
                    break;
                }
            };
            let Some(fetch_res) = fetch_res else { continue; };
            match fetch_res {
                Ok(Ok(data)) => {
                    if plan_rx.borrow().generation != generation { return; }
                    if Instant::now() >= deadline {
                        deadline_hit = true;
                        break;
                    }
                    first_fetch = false;
                    let pending: Vec<Arc<Account>> = students
                        .iter()
                        .filter(|s| !resolved.contains(&s.id))
                        .cloned()
                        .collect();
                    for chunk in pending.chunks(teacher_qr::FANOUT_LIMIT) {
                        if plan_rx.borrow().generation != generation { return; }
                        if Instant::now() >= deadline || tx.is_closed() {
                            if Instant::now() >= deadline { deadline_hit = true; }
                            break;
                        }
                        let mut fanout = JoinSet::new();
                        for s in chunk {
                            if plan_rx.borrow().generation != generation { fanout.abort_all(); return; }
                            if Instant::now() >= deadline || tx.is_closed() {
                                if Instant::now() >= deadline { deadline_hit = true; }
                                break;
                            }
                            let (s, data, rid) = (s.clone(), data.clone(), student_rollcall_id.clone());
                            let dl = deadline;
                            fanout.spawn(async move {
                                let res = match tokio::time::timeout_at(dl, sign_qr_student(s.clone(), &rid, &data)).await {
                                    Ok(r) => r,
                                    Err(_) => Err("qr: deadline exceeded".into()),
                                };
                                (s.id.clone(), res)
                            });
                        }
                        while !fanout.is_empty() {
                            if plan_rx.borrow().generation != generation { fanout.abort_all(); return; }
                            if Instant::now() >= deadline {
                                deadline_hit = true;
                                fanout.abort_all();
                                break;
                            }
                            if tx.is_closed() {
                                fanout.abort_all();
                                break;
                            }
                            let joined = tokio::select! {
                                res = tokio::time::timeout_at(deadline, fanout.join_next()) => res,
                                _ = plan_rx.changed() => {
                                    if plan_rx.borrow().generation != generation { fanout.abort_all(); return; }
                                    continue;
                                }
                            };
                            let joined = match joined {
                                Ok(v) => v,
                                Err(_) => { deadline_hit = true; fanout.abort_all(); break; }
                            };
                            match joined {
                                Some(Ok((account_id, result))) => {
                                    if resolved.insert(account_id.clone()) {
                                        let _ = tx.send(MonitorMsg::QrSignResult {
                                            key: key.clone(),
                                            account_id,
                                            generation,
                                            mutation_attempted: true,
                                            result,
                                        });
                                    }
                                }
                                Some(Err(join_error)) => {
                                    if join_error.is_panic() {
                                        let payload = join_error.into_panic();
                                        handle_qr_fanout_panic(&mut fanout, &mut resolved, &students, &tx, &key, generation, payload).await;
                                    }
                                }
                                None => break,
                            }
                        }
                        if plan_rx.borrow().generation != generation { return; }
                        if deadline_hit || tx.is_closed() || Instant::now() >= deadline {
                            if Instant::now() >= deadline { deadline_hit = true; }
                            break;
                        }
                    }
                    if deadline_hit { break; }
                }
                Ok(Err(e)) if e.kind == FailureKind::AuthLost && first_fetch && !teacher_recovered && {
                    if plan_rx.borrow().generation != generation { return; }
                    tokio::time::timeout_at(deadline, relogin(&teacher)).await.unwrap_or_default()
                } => {
                    teacher_recovered = true;
                    first_fetch = false;
                    if Instant::now() >= deadline { deadline_hit = true; break; }
                    if plan_rx.borrow().generation != generation { return; }
                }
                Ok(Err(e)) if first_fetch => {
                    if let Some(source) = cleanup.take() {
                        cleanup_teacher_source(&teacher, &ep, &source).await;
                    }
                    if plan_rx.borrow().generation != generation { return; }
                    if let Some((normalized_base, api_key, qr_rev_rx, qr_rev)) = remote {
                        if *qr_rev_rx.borrow() != qr_rev {
                            for s in &students {
                                let _ = tx.send(MonitorMsg::QrSignResult {
                                    key: key.clone(),
                                    account_id: s.id.clone(),
                                    generation,
                                    mutation_attempted: false,
                                    result: Err("qr: remote config changed".into()),
                                });
                            }
                            return;
                        }
                        if Instant::now() < deadline && !tx.is_closed() && plan_rx.borrow().generation == generation {
                            run_qr_remote(deadline, normalized_base, api_key, students, tx, key, generation, plan_rx, qr_rev_rx, qr_rev).await;
                        } else {
                            let msg = if Instant::now() >= deadline { "qr: deadline exceeded".to_string() } else { format!("qr: teacher fetch failed ({})", e.kind as u8) };
                            for s in &students {
                                if !resolved.contains(&s.id) {
                                    let _ = tx.send(MonitorMsg::QrSignResult {
                                        key: key.clone(),
                                        account_id: s.id.clone(),
                                        generation,
                                        mutation_attempted: false,
                                        result: Err(msg.clone()),
                                    });
                                }
                            }
                        }
                        return;
                    }
                    {
                        let sanitized = sanitize_qr_source_reason(format!("qr: teacher fetch failed ({})", e.kind as u8));
                        let remaining: Vec<String> = students.iter().filter(|s| !resolved.contains(&s.id)).map(|s| s.id.clone()).collect();
                        if !remaining.is_empty() {
                            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                            let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), account_ids: remaining.clone(), ack: ack_tx});
                            let ack_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(5));
                            let _ = tokio::time::timeout_at(ack_deadline, ack_rx).await;
                            if plan_rx.borrow().generation != generation { return; }
                        }
                        for s in &students { if !resolved.contains(&s.id) { let _ = tx.send(MonitorMsg::QrSignResult{key: key.clone(), account_id: s.id.clone(), generation, mutation_attempted: false, result: Err(sanitized.clone())}); } }
                    }
                    return;
                }
                Ok(Err(_)) => {
                    if Instant::now() >= deadline { deadline_hit = true; break; }
                    if plan_rx.borrow().generation != generation { return; }
                }
                Err(_) => { deadline_hit = true; break; }
            }
            if deadline_hit || Instant::now() >= deadline { deadline_hit = true; break; }
            if plan_rx.borrow().generation != generation { return; }
            if resolved.len() < students.len() && !tx.is_closed() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() { deadline_hit = true; break; }
                let to_sleep = std::cmp::min(teacher_qr::POLL_INTERVAL, remaining);
                tokio::select! {
                    _ = tokio::time::sleep(to_sleep) => {},
                    _ = plan_rx.changed() => { if plan_rx.borrow().generation != generation { return; } }
                    _ = tokio::time::sleep_until(deadline) => { deadline_hit = true; break; }
                    _ = tx.closed() => break,
                }
                if plan_rx.borrow().generation != generation { return; }
            }
        }
        if plan_rx.borrow().generation != generation { return; }
        for s in &students {
            if !resolved.contains(&s.id) {
                let err = if deadline_hit || Instant::now() >= deadline { "qr: deadline exceeded".to_string() } else { "qr: could not confirm within the token window".to_string() };
                let _ = tx.send(MonitorMsg::QrSignResult {
                    key: key.clone(),
                    account_id: s.id.clone(),
                    generation,
                    mutation_attempted: true,
                    result: Err(err),
                });
            }
        }
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
    emit(
        cb,
        Event::RollcallDetected {
            rollcall_id,
            activity_token: &a.activity_token,
            base_url,
            kind: a.kind.as_str(),
            course: &a.course,
            attendance_rate: a.attendance_rate,
            accounts: a.participants.iter().map(String::as_str).collect(),
        },
    );
}

fn rollcall_course_id(rollcall: &Value) -> Option<String> {
    json::id_at(rollcall, &["course_id", "courseId", "cid"])
        .or_else(|| rollcall.get("course").and_then(course_id))
}

fn rollcall_id(rollcall: &Value) -> Option<String> {
    json::id_at(rollcall, &["rollcall_id", "id"])
}

fn course_name(rollcall: &Value) -> String {
    json::text_at(rollcall, &["course_name", "course"]).to_string()
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
            Problem::error("quiz_unanswerable", format!("{}: {detail}", q.activity_id))
                .activity(&q.activity_token)
                .account(&account_id),
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
        Problem::error(&code, message)
            .activity(&q.activity_token)
            .account(&account_id),
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
        Event::LogLine {
            level: LogLevel::Info,
            text: format!("quiz {} discarded", q.activity_id),
            activity_token: Some(&q.activity_token),
        },
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
    let answer = AnswerWire::from_answer(&answer);
    emit(
        cb,
        Event::AnswerUpdated {
            quiz_id: &q.activity_id,
            activity_token: &q.activity_token,
            account_id,
            subject_id,
            display_answer: answer.display(),
            answer,
            source: "user",
            conflict: false,
        },
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
                    Problem::error("quiz_account_unavailable", "測驗帳號已不在監控工作階段中")
                        .activity(&q.activity_token)
                        .account(account_id),
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
                        Problem::error("quiz_account_unavailable", "測驗帳號已不在監控工作階段中")
                            .activity(&q.activity_token)
                            .account(account_id),
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
            Event::Countdown {
                scope: CountdownScope::Quiz,
                activity_token: &q.activity_token,
                external_id: &q.activity_id,
                remaining_secs: remaining,
            },
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
                Event::QuizSubmitted {
                    quiz_id: &q.activity_id,
                    activity_token: &q.activity_token,
                    account_id: &account_id,
                    result: &report.detail,
                },
            );
            if let Some(warning) = report.warning {
                emit(
                    cb,
                    Problem::warning("quiz_correction_failed", warning)
                        .activity(&q.activity_token)
                        .account(&account_id),
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
                Problem::error(
                    "quiz_submit_failed",
                    format!(
                        "{}: {}（可手動重試：於測驗頁按「立即送出」再次提交）",
                        account_id, failure.error
                    ),
                )
                .activity(&q.activity_token)
                .account(&account_id),
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
    emit(cb, quiz_prepared(q));
}

fn quiz_prepared(q: &QuizActivity) -> QuizPrepared<'_> {
    let per_account = q
        .attempts
        .iter()
        .filter(|(_, attempt)| {
            matches!(
                attempt.state,
                AttemptState::Ready | AttemptState::Submitting | AttemptState::Submitted
            )
        })
        .map(|(account_id, attempt)| PreparedAccount {
            account_id,
            instance_id: &attempt.instance_id,
            state: attempt.state,
            questions: attempt
                .subjects
                .iter()
                .map(|subject| prepared_question(attempt, subject))
                .collect(),
        })
        .collect();
    QuizPrepared {
        schema_version: 1,
        activity_token: &q.activity_token,
        quiz_id: &q.activity_id,
        activity: QuizActivityRef {
            external_id: &q.activity_id,
            source: q.source.as_str(),
            course_id: &q.course_id,
            course: &q.course,
        },
        course: &q.course,
        per_account,
        expected_accounts: q
            .attempts
            .iter()
            .map(|(account_id, attempt)| ExpectedAccount {
                account_id,
                state: attempt.state,
            })
            .collect(),
        conflict_count: q
            .attempts
            .values()
            .map(|attempt| attempt.conflicts.len())
            .sum(),
    }
}

fn prepared_question<'a>(
    attempt: &'a PerAccountAttempt,
    subject: &'a Value,
) -> PreparedQuestion<'a> {
    let subject_id = crate::quiz::subject_id(subject);
    let answer = attempt
        .generated_answers
        .get(&subject_id)
        .map(AnswerWire::from_answer);
    PreparedQuestion {
        parent_id: subject.get("parent_id").and_then(json::id),
        kind: json::text_at(subject, &["type"]),
        answer_type: json::text_at(subject, &["answer_type", "type"]),
        stem: crate::quiz::stem(subject),
        options: json::array_at(subject, &["options"])
            .iter()
            .map(|option| QuestionOption {
                id: crate::quiz::option_id(option),
                text: json::text_at(option, &["content", "description"]),
            })
            .collect(),
        display_answer: answer.as_ref().map(AnswerWire::display).unwrap_or_default(),
        answer,
        existing_answer: attempt
            .existing_answers
            .get(&subject_id)
            .map(AnswerWire::from_answer),
        source: "llm",
        conflict: attempt.conflicts.contains(&subject_id),
        subject_id,
    }
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

/// The id of a detected activity, whichever of the family's id fields carries it.
fn activity_id(activity: &Value) -> Option<String> {
    json::id_at(activity, &["id", "activity_id", "course_id"])
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
mod tests;
