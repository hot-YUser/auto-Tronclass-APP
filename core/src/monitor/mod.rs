//! Multi-account monitoring. One **poller** per student account watches its own rollcalls and
//! quizzes and reports what it finds to a single **actor**. The actor merges detections from
//! several accounts into one activity, runs the attendance gate and the countdown, and dispatches
//! each participant's sign-in or submission.
//!
//! The actor never awaits the network. Every HTTP step runs as a task in the session's
//! `TaskGroup` and answers with a `MonitorMsg`, so one slow account cannot stall another's
//! countdown, and stopping the session cancels everything in flight.

mod actor;
mod detect;
mod poller;
mod qr;
mod quiz;
mod rollcall;
mod task_group;

#[cfg(test)]
mod tests;

use crate::answer::Source;
use crate::config::TargetId;
use crate::llm::LlmConfig;
use crate::rollcall::{RollcallKind, SignOutcome};
use crate::secrets::Secret;
use crate::wire::{AccountResultPhase, AnswerWire, EventCb};
use actor::{Actor, Env};
use poller::PollTuning;
use quiz::{PrepareRetry, PreparedAttempt, QuizSubmitFailure, QuizSubmitReport};
use reqwest::Client;
use std::collections::HashSet;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;
use task_group::StartupGuard;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::oneshot;

pub use task_group::TaskGroup;

/// One logged-in account, with what a task needs to act as it. The credentials stay here so a
/// lost session can be restored without a round trip to the engine. Deliberately not `Debug`:
/// the password must never reach a formatter.
pub struct Account {
    pub id: String,
    pub device_id: String,
    /// The account's own user number; empty when the engine could not determine it.
    pub user_no: String,
    pub is_teacher: bool,
    pub course_id: Option<String>,
    pub base_url: String,
    pub client: Client,
    pub username: String,
    pub password: Secret,
}

/// Who detects, who participates, and on behalf of which targets. A new plan carries a higher
/// generation, and work started under an older generation discards its own results.
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
    /// Empty means every course.
    pub course_ids: Vec<String>,
}

impl MonitorPlan {
    /// The routes a detection by `detector` in `course_id` belongs to.
    fn routes_for<'p, 'q>(
        &'p self,
        detector: &'q str,
        course_id: Option<&'q str>,
    ) -> impl Iterator<Item = &'p MonitorRoute> + use<'p, 'q> {
        self.routes.iter().filter(move |route| {
            route.detector_account_id == detector
                && (route.course_ids.is_empty()
                    || course_id.is_some_and(|id| route.course_ids.iter().any(|c| c == id)))
        })
    }
}

/// What the engine folds into the monitoring snapshot.
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

/// Identifies one activity across accounts: the same rollcall seen by two accounts of one school
/// is one activity, and an exam and a vote that share an id are two.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ActivityKey {
    pub base_url: String,
    pub kind: String,
    pub id: String,
}

impl ActivityKey {
    pub fn new(
        base_url: impl Into<String>,
        kind: impl Into<String>,
        id: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            kind: kind.into(),
            id: id.into(),
        }
    }

    fn rollcall(base_url: &str, kind: RollcallKind, id: &str) -> Self {
        Self::new(base_url, kind.as_str(), id)
    }

    fn quiz(base_url: &str, source: Source, id: &str) -> Self {
        Self::new(base_url, format!("quiz:{}", source.as_str()), id)
    }
}

/// A rollcall a poller saw for the first time.
pub(crate) struct Detected {
    generation: u64,
    account_id: String,
    base_url: String,
    rollcall_id: String,
    kind: RollcallKind,
    course: String,
    course_id: Option<String>,
}

/// An answerable quiz a poller saw for the first time.
pub(crate) struct QuizDetected {
    generation: u64,
    account_id: String,
    base_url: String,
    source: Source,
    course: String,
    course_id: String,
    activity_id: String,
    /// The homework prompt, which only the listing carries.
    stem: String,
}

pub(crate) enum MonitorMsg {
    // ---- rollcalls ----
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
        /// False when the job never obtained a QR token, so no sign-in request was sent.
        mutation_attempted: bool,
        result: Result<SignOutcome, String>,
    },
    /// A QR job could not obtain a token. It waits for `ack` so the actor records the failure
    /// before the per-account results arrive.
    QrSourceFailed {
        key: ActivityKey,
        generation: u64,
        reason: String,
        ack: oneshot::Sender<()>,
    },
    /// A QR job is about to send its first sign-in request. It waits for `ack` so a concurrent
    /// settings change cannot still see the activity as untouched.
    QrMutationStarted {
        key: ActivityKey,
        generation: u64,
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

    // ---- quizzes ----
    QuizDetected(QuizDetected),
    QuizPrepared {
        key: ActivityKey,
        attempts: Vec<PreparedAttempt>,
    },
    QuizPrepareRetry(PrepareRetry),
    /// The activity closed before the paper could be fetched.
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

    // ---- sessions ----
    AuthLost {
        account_id: String,
    },
    AuthRestored {
        account_id: String,
        ok: bool,
    },

    // ---- the session itself ----
    ConfigUpdated(Box<MonitorConfig>),
    ApplyPlan {
        plan: MonitorPlan,
        cancel_removed_pending: bool,
    },
    UpsertAccounts(Vec<Account>),
    /// Hold back every not-yet-started mutation that belongs only to `affected_sources`.
    PrepareDefinitionChange {
        affected_sources: HashSet<TargetId>,
        reply: SyncSender<()>,
    },
    CommitDefinitionChange {
        plan: MonitorPlan,
        reply: SyncSender<()>,
    },
    RollbackDefinitionChange {
        reply: SyncSender<()>,
    },
    Stop,
}

/// The engine's grip on a running session. Dropping it stops the session.
pub struct MonitorHandle {
    pub tx: UnboundedSender<MonitorMsg>,
    group: Arc<TaskGroup>,
}

impl MonitorHandle {
    /// A handle with no session behind it.
    #[cfg(test)]
    pub(crate) fn new(tx: UnboundedSender<MonitorMsg>) -> Self {
        extern "C" fn discard(_: *const u8, _: usize) {}
        let (panic_tx, _) = unbounded_channel();
        Self {
            tx,
            group: Arc::new(TaskGroup::new(discard, panic_tx)),
        }
    }

    pub fn apply_plan(
        &self,
        plan: MonitorPlan,
        cancel_removed_pending: bool,
    ) -> Result<(), String> {
        self.send(MonitorMsg::ApplyPlan {
            plan,
            cancel_removed_pending,
        })
    }

    pub fn upsert_accounts(&self, accounts: Vec<Account>) -> Result<(), String> {
        self.send(MonitorMsg::UpsertAccounts(accounts))
    }

    pub fn prepare_definition_change(
        &self,
        affected_sources: HashSet<TargetId>,
    ) -> Result<(), String> {
        self.confirmed("barrier", |reply| MonitorMsg::PrepareDefinitionChange {
            affected_sources,
            reply,
        })
    }

    pub fn commit_definition_change(&self, plan: MonitorPlan) -> Result<(), String> {
        self.confirmed("commit", |reply| MonitorMsg::CommitDefinitionChange {
            plan,
            reply,
        })
    }

    pub fn rollback_definition_change(&self) -> Result<(), String> {
        self.confirmed("rollback", |reply| MonitorMsg::RollbackDefinitionChange {
            reply,
        })
    }

    /// Ask the actor to finish, then cancel every task of the session.
    pub fn stop(&self) {
        let _ = self.tx.send(MonitorMsg::Stop);
        self.group.cancel();
    }

    fn send(&self, message: MonitorMsg) -> Result<(), String> {
        self.tx
            .send(message)
            .map_err(|_| "monitor actor stopped".to_string())
    }

    /// Send a message and block until the actor has applied it. The engine calls this while
    /// holding its state lock, hence the bound.
    fn confirmed(
        &self,
        step: &str,
        message: impl FnOnce(SyncSender<()>) -> MonitorMsg,
    ) -> Result<(), String> {
        let (reply, applied) = sync_channel(0);
        self.send(message(reply))?;
        applied
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| format!("monitor definition {step} timed out"))
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        // Covers every path that discards the handle without `stop`.
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
    pub qr_remote_key: Option<Arc<Secret>>,
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

    fn tuning(&self) -> PollTuning {
        PollTuning {
            idle: Duration::from_secs(self.poll_idle_secs.max(1)),
            quiz_detect: Duration::from_secs(self.quiz_detect_secs.max(1)),
            wanted_types: self.autoanswer_types.clone(),
        }
    }

    fn number_pacing(&self) -> crate::rollcall::NumberCfg {
        crate::rollcall::NumberCfg {
            cooldown_ms: self.number_cooldown_ms,
            max_cooldowns: self.number_max_cooldowns,
        }
    }
}

/// Start a session on the current Tokio runtime: the actor, plus one poller per student account.
pub fn start(
    cb: EventCb,
    accounts: Vec<Account>,
    plan: MonitorPlan,
    cfg: MonitorConfig,
    panic_tx: UnboundedSender<()>,
    runtime_tx: UnboundedSender<RuntimeEvent>,
) -> MonitorHandle {
    let (tx, rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(cb, panic_tx));
    let mut startup = StartupGuard::new(group.clone());
    let mut env = Env::new(cb, cfg, plan, tx.clone(), group.clone(), runtime_tx);
    for account in accounts {
        env.add_account(account);
    }
    group.spawn(Actor::new(env).run(rx));
    startup.disarm();
    MonitorHandle { tx, group }
}
