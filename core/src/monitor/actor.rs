//! The actor: the one task that owns a session's state. It reacts to messages and to a
//! once-a-second tick, and never awaits anything else.

use super::poller::{self, PollTuning};
use super::qr;
use super::quiz::{self, QuizActivity};
use super::rollcall::{self, Activity};
use super::{
    Account, ActivityKey, MonitorConfig, MonitorMsg, MonitorPlan, RuntimeEvent, TaskGroup,
};
use crate::config::TargetId;
use crate::wire::{emit, respond, Event, EventCb, Problem};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::time::Instant;

/// Re-login backoff: the delay doubles from the base up to the cap, and after this many
/// consecutive failures the account is given up on until the session restarts.
pub(super) const RELOGIN_BASE_DELAY: Duration = Duration::from_secs(5);
const RELOGIN_MAX_DELAY: Duration = Duration::from_secs(300);
pub(super) const RELOGIN_MAX_ATTEMPTS: u32 = 6;

/// What every handler reads and none of them changes: the session's accounts, settings and
/// plan, and the means to spawn work and report back.
pub(super) struct Env {
    pub cb: EventCb,
    pub accounts: HashMap<String, Arc<Account>>,
    pub cfg: MonitorConfig,
    pub plan: MonitorPlan,
    pub tx: UnboundedSender<MonitorMsg>,
    pub group: Arc<TaskGroup>,
    pub runtime_tx: UnboundedSender<RuntimeEvent>,
    // The copies running tasks watch. They are written with `send_replace`, which stores the
    // value even while nothing is subscribed: a task that subscribes later must still see it.
    plan_tx: watch::Sender<MonitorPlan>,
    tune_tx: watch::Sender<PollTuning>,
    qr_revision: watch::Sender<u64>,
}

impl Env {
    pub(super) fn new(
        cb: EventCb,
        cfg: MonitorConfig,
        plan: MonitorPlan,
        tx: UnboundedSender<MonitorMsg>,
        group: Arc<TaskGroup>,
        runtime_tx: UnboundedSender<RuntimeEvent>,
    ) -> Self {
        Self {
            cb,
            accounts: HashMap::new(),
            plan_tx: watch::Sender::new(plan.clone()),
            tune_tx: watch::Sender::new(cfg.tuning()),
            qr_revision: watch::Sender::new(0),
            cfg,
            plan,
            tx,
            group,
            runtime_tx,
        }
    }

    /// Admit an account to the session. A student account gets its poller; an account that is
    /// already here is left alone.
    pub(super) fn add_account(&mut self, account: Account) {
        if self.accounts.contains_key(&account.id) {
            return;
        }
        let account = Arc::new(account);
        if !account.is_teacher {
            self.spawn(poller::poll(
                account.clone(),
                self.tx.clone(),
                self.tune_tx.subscribe(),
                self.plan_tx.subscribe(),
            ));
        }
        self.accounts.insert(account.id.clone(), account);
    }

    pub(super) fn set_plan(&mut self, plan: MonitorPlan) {
        self.plan_tx.send_replace(plan.clone());
        self.plan = plan;
    }

    /// Adopt new settings. Returns whether activities that failed for want of a QR source should
    /// be tried again with them.
    fn set_config(&mut self, cfg: MonitorConfig) -> bool {
        if qr::is_qr_remote_revision_change(&self.cfg, &cfg) {
            // Remote jobs holding the old revision stop before they use a stale key or host.
            self.qr_revision
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
        let retry = qr::is_meaningful_qr_remote_change(&self.cfg, &cfg)
            && qr::is_qr_remote_configured(&cfg);
        self.tune_tx.send_replace(cfg.tuning());
        self.cfg = cfg;
        retry
    }

    pub(super) fn plan_rx(&self) -> watch::Receiver<MonitorPlan> {
        self.plan_tx.subscribe()
    }

    /// The revision of the QR remote settings and a watch on it. A remote job stops as soon as
    /// the two differ.
    pub(super) fn qr_revision(&self) -> (watch::Receiver<u64>, u64) {
        let revision_rx = self.qr_revision.subscribe();
        let revision = *revision_rx.borrow();
        (revision_rx, revision)
    }

    pub(super) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.group.spawn(task);
    }

    pub(super) fn students(&self, ids: &[String]) -> Vec<Arc<Account>> {
        ids.iter()
            .filter_map(|id| self.accounts.get(id))
            .filter(|account| !account.is_teacher)
            .cloned()
            .collect()
    }

    fn spawn_relogin(&self, account: Arc<Account>) {
        let tx = self.tx.clone();
        self.spawn(async move {
            let ok = poller::relogin(&account).await;
            let account_id = account.id.clone();
            tx.send(MonitorMsg::AuthRestored { account_id, ok }).ok();
        });
    }

    fn reply(&self, command_id: u64, result: Result<(), String>) {
        respond(self.cb, command_id, Event::reply(result));
    }

    fn report_login(&self, account_id: &str, online: bool, error: Option<&str>) {
        let _ = self.runtime_tx.send(RuntimeEvent::AccountLogin {
            account_id: account_id.to_string(),
            online,
            error: error.map(str::to_string),
        });
    }
}

pub(super) struct Actor {
    pub env: Env,
    pub activities: HashMap<ActivityKey, Activity>,
    pub quizzes: HashMap<ActivityKey, QuizActivity>,
    /// Accounts with a re-login in flight.
    reauth: HashSet<String>,
    backoff: HashMap<String, ReloginState>,
}

impl Actor {
    pub(super) fn new(env: Env) -> Self {
        Self {
            env,
            activities: HashMap::new(),
            quizzes: HashMap::new(),
            reauth: HashSet::new(),
            backoff: HashMap::new(),
        }
    }

    pub(super) async fn run(mut self, mut rx: UnboundedReceiver<MonitorMsg>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                message = rx.recv() => match message {
                    None | Some(MonitorMsg::Stop) => break,
                    Some(message) => self.handle(message),
                },
                _ = ticker.tick() => self.tick(Instant::now()),
            }
        }
    }

    pub(super) fn tick(&mut self, now: Instant) {
        rollcall::on_tick(&mut self.activities, &self.env, now);
        quiz::on_tick(&mut self.quizzes, &self.env, now);
    }

    pub(super) fn handle(&mut self, message: MonitorMsg) {
        let Self {
            env,
            activities,
            quizzes,
            ..
        } = self;
        match message {
            MonitorMsg::Stop => {}

            MonitorMsg::Detected(detected) => {
                if detected.generation == env.plan.generation {
                    rollcall::on_detected(activities, env, detected);
                }
            }
            MonitorMsg::GateResult { key, rate } => rollcall::on_gate(activities, env, &key, rate),
            MonitorMsg::CodeRead { key, code } => {
                rollcall::on_code_read(activities, env, &key, code)
            }
            MonitorMsg::SignResult {
                key,
                account_id,
                result,
            } => rollcall::on_sign_result(activities, env, &key, account_id, result),
            MonitorMsg::QrSignResult {
                key,
                account_id,
                generation,
                mutation_attempted,
                result,
            } => {
                if generation == env.plan.generation {
                    rollcall::on_qr_sign_result(
                        activities,
                        env,
                        &key,
                        account_id,
                        generation,
                        mutation_attempted,
                        result,
                    );
                }
            }
            MonitorMsg::QrSourceFailed {
                key,
                generation,
                reason,
                ack,
            } => {
                if generation == env.plan.generation {
                    rollcall::on_qr_source_failed(activities, env, &key, generation, reason);
                }
                let _ = ack.send(());
            }
            MonitorMsg::QrMutationStarted {
                key,
                generation,
                ack,
            } => {
                if generation == env.plan.generation {
                    rollcall::on_qr_mutation_started(activities, &key, generation);
                }
                let _ = ack.send(());
            }
            MonitorMsg::SignNow {
                command_id,
                activity_token,
            } => {
                let result = rollcall::find_key(activities, &activity_token)
                    .and_then(|key| rollcall::on_sign_now(activities, env, &key));
                env.reply(command_id, result);
            }
            MonitorMsg::Defer {
                command_id,
                activity_token,
            } => {
                let result = rollcall::find_key(activities, &activity_token)
                    .map(|key| rollcall::on_defer(activities, env, &key));
                env.reply(command_id, result);
            }

            MonitorMsg::QuizDetected(detected) => {
                if detected.generation == env.plan.generation {
                    quiz::on_detected(quizzes, &env.plan, detected);
                }
            }
            MonitorMsg::QuizPrepared { key, attempts } => {
                quiz::on_prepared(quizzes, env, &key, attempts)
            }
            MonitorMsg::QuizPrepareRetry(retry) => quiz::on_prepare_retry(quizzes, env, retry),
            MonitorMsg::QuizPrepareGone {
                key,
                account_id,
                generation,
            } => quiz::on_prepare_gone(quizzes, env, &key, &account_id, generation),
            MonitorMsg::QuizPrepareFailed {
                key,
                account_id,
                generation,
                code,
                message,
            } => {
                quiz::on_prepare_failed(quizzes, env, &key, &account_id, generation, &code, message)
            }
            MonitorMsg::QuizSubmitResult {
                key,
                account_id,
                result,
            } => quiz::on_submit_result(quizzes, env, &key, &account_id, result),
            MonitorMsg::QuizSetAnswer {
                command_id,
                activity_token,
                account_id,
                subject_id,
                answer,
            } => {
                let result = quiz::find_mut(quizzes, &activity_token)
                    .and_then(|quiz| quiz::set_answer(quiz, env, &account_id, &subject_id, answer));
                env.reply(command_id, result);
            }
            MonitorMsg::QuizSubmitNow {
                command_id,
                activity_token,
            } => {
                let result = quiz::find_key(quizzes, &activity_token)
                    .and_then(|key| quiz::dispatch_submits(quizzes, env, &key));
                env.reply(command_id, result);
            }
            MonitorMsg::QuizHold {
                command_id,
                activity_token,
            } => {
                let result = quiz::find_mut(quizzes, &activity_token).and_then(quiz::hold);
                env.reply(command_id, result);
            }
            MonitorMsg::QuizDiscard {
                command_id,
                activity_token,
            } => {
                let result = quiz::find_mut(quizzes, &activity_token)
                    .and_then(|quiz| quiz::discard(quiz, env.cb));
                env.reply(command_id, result);
            }

            MonitorMsg::AuthLost { account_id } => self.on_auth_lost(account_id),
            MonitorMsg::AuthRestored { account_id, ok } => self.on_auth_restored(&account_id, ok),

            MonitorMsg::ConfigUpdated(cfg) => {
                if env.set_config(*cfg) {
                    rollcall::retry_without_source(activities, env);
                }
            }
            MonitorMsg::UpsertAccounts(accounts) => {
                for account in accounts {
                    env.add_account(account);
                }
            }
            MonitorMsg::ApplyPlan {
                plan,
                cancel_removed_pending,
            } => {
                if plan.generation > env.plan.generation {
                    if cancel_removed_pending {
                        drop_removed_sources(activities, quizzes, &plan);
                    }
                    env.set_plan(plan);
                }
            }
            MonitorMsg::PrepareDefinitionChange {
                affected_sources,
                reply,
            } => {
                for activity in activities.values_mut().filter(|activity| !activity.acted) {
                    activity.mutation_blocked = only_from(&activity.sources, &affected_sources);
                }
                for quiz in quizzes.values_mut().filter(|quiz| !quiz.submission_begun()) {
                    quiz.mutation_blocked = only_from(&quiz.sources, &affected_sources);
                }
                let _ = reply.send(());
            }
            MonitorMsg::CommitDefinitionChange { plan, reply } => {
                drop_removed_sources(activities, quizzes, &plan);
                unblock(activities, quizzes);
                if plan.generation >= env.plan.generation {
                    env.set_plan(plan);
                }
                let _ = reply.send(());
            }
            MonitorMsg::RollbackDefinitionChange { reply } => {
                unblock(activities, quizzes);
                let _ = reply.send(());
            }
        }
    }

    fn on_auth_lost(&mut self, account_id: String) {
        if self.reauth.contains(&account_id)
            || !relogin_due(&self.backoff, &account_id, Instant::now())
        {
            return;
        }
        if let Some(account) = self.env.accounts.get(&account_id).cloned() {
            self.reauth.insert(account_id);
            self.env.spawn_relogin(account);
        }
    }

    fn on_auth_restored(&mut self, account_id: &str, ok: bool) {
        self.reauth.remove(account_id);
        if ok {
            self.backoff.remove(account_id);
            rollcall::redispatch_signs(&mut self.activities, &self.env, account_id);
            self.env.report_login(account_id, true, None);
            return;
        }
        let attempts = relogin_failed(&mut self.backoff, account_id, Instant::now());
        if attempts == RELOGIN_MAX_ATTEMPTS {
            emit(
                self.env.cb,
                Problem::error(
                    "relogin_failed",
                    format!(
                        "account {account_id}: re-login failed {attempts} times; automatic retries stopped"
                    ),
                )
                .account(account_id),
            );
            self.env
                .report_login(account_id, false, Some("登入狀態已無法恢復"));
        }
    }
}

/// True when every source of an activity is among those about to change.
fn only_from(sources: &HashSet<TargetId>, affected: &HashSet<TargetId>) -> bool {
    !sources.is_empty() && sources.is_subset(affected)
}

fn unblock(
    activities: &mut HashMap<ActivityKey, Activity>,
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
) {
    for activity in activities.values_mut() {
        activity.mutation_blocked = false;
    }
    for quiz in quizzes.values_mut() {
        quiz.mutation_blocked = false;
    }
}

/// Forget the sources `next` no longer routes, and with them every activity left without one.
/// An activity that has already acted stays: its results still have to be received.
pub(super) fn drop_removed_sources(
    activities: &mut HashMap<ActivityKey, Activity>,
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    next: &MonitorPlan,
) {
    let routed: HashSet<&TargetId> = next
        .routes
        .iter()
        .flat_map(|route| &route.source_targets)
        .collect();
    activities.retain(|_, activity| {
        activity.sources.retain(|source| routed.contains(source));
        !activity.sources.is_empty() || activity.acted
    });
    quizzes.retain(|_, quiz| {
        quiz.sources.retain(|source| routed.contains(source));
        !quiz.sources.is_empty() || quiz.submission_begun()
    });
}

#[derive(Clone, Copy)]
pub(super) enum ReloginState {
    /// `attempts` consecutive failures so far; the next attempt may start at `next_at`.
    Cooling { attempts: u32, next_at: Instant },
    /// A wrong password does not get better by retrying it every poll.
    GivenUp,
}

/// May a re-login for the account start now?
pub(super) fn relogin_due(
    backoff: &HashMap<String, ReloginState>,
    account_id: &str,
    now: Instant,
) -> bool {
    match backoff.get(account_id) {
        None => true,
        Some(ReloginState::Cooling { next_at, .. }) => now >= *next_at,
        Some(ReloginState::GivenUp) => false,
    }
}

/// Record a failed re-login and return how many have failed in a row. The count reaches
/// `RELOGIN_MAX_ATTEMPTS` exactly once; an account already given up on reports `u32::MAX`.
pub(super) fn relogin_failed(
    backoff: &mut HashMap<String, ReloginState>,
    account_id: &str,
    now: Instant,
) -> u32 {
    let attempts = match backoff.get(account_id) {
        Some(ReloginState::GivenUp) => return u32::MAX,
        Some(ReloginState::Cooling { attempts, .. }) => attempts + 1,
        None => 1,
    };
    let state = if attempts >= RELOGIN_MAX_ATTEMPTS {
        ReloginState::GivenUp
    } else {
        let delay = RELOGIN_BASE_DELAY
            .saturating_mul(2_u32.pow(attempts - 1))
            .min(RELOGIN_MAX_DELAY);
        ReloginState::Cooling {
            attempts,
            next_at: now + delay,
        }
    };
    backoff.insert(account_id.to_string(), state);
    attempts
}
