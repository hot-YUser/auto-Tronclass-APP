//! A rollcall's life in the actor: detected, held at the attendance gate, counted down, then
//! signed once per participant.

use super::actor::Env;
use super::qr;
use super::{Account, ActivityKey, Detected, MonitorMsg, RuntimeEvent};
use crate::config::{normalize_qr_remote_base_url, TargetId};
use crate::providers::Endpoints;
use crate::rollcall::{self, RollcallKind, SignOutcome};
use crate::secrets::Secret;
use crate::wire::{emit, AccountResultPhase, CountdownScope, Event, EventCb, LogLevel, Problem};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// How many times one account's sign-in may fail on one rollcall before it is given up on.
/// A session that keeps dying after a successful re-login is a permanent refusal, not an expiry.
pub(super) const MAX_RESIGN: u32 = 3;

/// How often a rollcall held below the attendance gate is checked again.
pub(super) const GATE_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct Activity {
    pub activity_token: String,
    pub kind: RollcallKind,
    pub course: String,
    pub participants: HashSet<String>,
    pub sources: HashSet<TargetId>,
    pub plan_generation: u64,

    pub attendance_rate: Option<f64>,
    /// Waiting for the class attendance rate to reach the gate.
    pub gate_pending: bool,
    /// At most one gate request per activity is on the wire.
    pub gate_in_flight: bool,
    pub gate_next_check: Option<Instant>,
    pub countdown_deadline: Option<Instant>,

    pub number_code: Option<String>,
    pub code_requested: bool,
    /// The user asked to sign now, and the number code is still being read.
    pub sign_pending: bool,

    /// Sign-ins have been dispatched. From here on only individual accounts are retried.
    pub acted: bool,
    /// A definition change is in progress that may remove this activity.
    pub mutation_blocked: bool,
    pub signed: HashSet<String>,
    /// Accounts whose sign-in failed for a reason other than a lost session.
    pub sign_failed: HashSet<String>,
    /// Accounts to sign again once their session is restored.
    pub needs_resign: HashSet<String>,
    pub resign_attempts: HashMap<String, u32>,
    pub qr: QrSource,
}

/// How far a QR rollcall got with obtaining the rotating token it signs with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum QrSource {
    #[default]
    Untried,
    /// No token could be obtained, so nothing was sent: safe to try again with a new source.
    Failed { reason: String },
    /// A sign-in request may have been sent. Never retried automatically.
    Mutating,
}

impl Activity {
    pub(super) fn new(kind: RollcallKind, course: String, plan_generation: u64) -> Self {
        Self {
            activity_token: crate::config::new_id(),
            kind,
            course,
            participants: HashSet::new(),
            sources: HashSet::new(),
            plan_generation,
            attendance_rate: None,
            gate_pending: true,
            gate_in_flight: false,
            gate_next_check: None,
            countdown_deadline: None,
            number_code: None,
            code_requested: false,
            sign_pending: false,
            acted: false,
            mutation_blocked: false,
            signed: HashSet::new(),
            sign_failed: HashSet::new(),
            needs_resign: HashSet::new(),
            resign_attempts: HashMap::new(),
            qr: QrSource::Untried,
        }
    }

    fn unsigned(&self) -> Vec<String> {
        self.participants
            .difference(&self.signed)
            .cloned()
            .collect()
    }

    /// One more failed sign-in for the account. True while it may still be retried.
    fn count_failure(&mut self, account_id: &str) -> bool {
        let attempts = self
            .resign_attempts
            .entry(account_id.to_string())
            .or_insert(0);
        *attempts += 1;
        *attempts <= MAX_RESIGN
    }
}

/// May a gate request start now? One in flight at most, and no sooner than the next check.
pub(super) fn gate_check_due(activity: &Activity, now: Instant) -> bool {
    activity.gate_pending
        && !activity.acted
        && !activity.gate_in_flight
        && activity.countdown_deadline.is_none()
        && activity.gate_next_check.is_none_or(|at| now >= at)
}

/// The accounts a second "sign now" may try again: failed, not signed, and within their bound.
pub(super) fn retryable_accounts(activity: &Activity) -> Vec<String> {
    activity
        .sign_failed
        .iter()
        .filter(|id| activity.participants.contains(*id) && !activity.signed.contains(*id))
        .filter(|id| activity.resign_attempts.get(*id).copied().unwrap_or(0) <= MAX_RESIGN)
        .cloned()
        .collect()
}

pub(super) fn find_key(
    activities: &HashMap<ActivityKey, Activity>,
    activity_token: &str,
) -> Result<ActivityKey, String> {
    activities
        .iter()
        .find(|(_, activity)| activity.activity_token == activity_token)
        .map(|(key, _)| key.clone())
        .ok_or_else(|| "unknown rollcall activity_token".to_string())
}

/// Merge a detection into its activity. The first detection starts the gate check; a participant
/// who joins after the sign-ins went out is signed alone.
pub(super) fn on_detected(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    detected: Detected,
) {
    let routes: Vec<_> = env
        .plan
        .routes_for(&detected.account_id, detected.course_id.as_deref())
        .collect();
    let participants: HashSet<&String> = routes
        .iter()
        .flat_map(|route| &route.participant_account_ids)
        .collect();
    if participants.is_empty() {
        return;
    }
    let key = ActivityKey::rollcall(&detected.base_url, detected.kind, &detected.rollcall_id);
    let activity = activities.entry(key.clone()).or_insert_with(|| {
        Activity::new(detected.kind, detected.course.clone(), detected.generation)
    });
    activity.sources.extend(
        routes
            .iter()
            .flat_map(|route| &route.source_targets)
            .cloned(),
    );
    if !activity.acted {
        activity.plan_generation = detected.generation;
    }
    let newcomers: Vec<String> = participants
        .into_iter()
        .filter(|id| activity.participants.insert((*id).clone()))
        .cloned()
        .collect();
    if newcomers.is_empty() {
        return;
    }
    emit(
        env.cb,
        Event::RollcallDetected {
            rollcall_id: &key.id,
            activity_token: &activity.activity_token,
            base_url: &key.base_url,
            kind: activity.kind.as_str(),
            course: &activity.course,
            attendance_rate: activity.attendance_rate,
            accounts: activity.participants.iter().map(String::as_str).collect(),
        },
    );
    if activity.acted {
        let unsigned: Vec<String> = newcomers
            .into_iter()
            .filter(|id| !activity.signed.contains(id))
            .collect();
        let sourced =
            activity.kind != RollcallKind::Qr || qr::qr_source_available(&env.cfg, &env.accounts);
        if !unsigned.is_empty() && sourced {
            dispatch_signs_for(activities, env, &key, unsigned);
        }
    } else if gate_check_due(activity, Instant::now())
        && spawn_gate_check(env, &key, &detected.account_id)
    {
        activity.gate_in_flight = true;
    }
}

/// The class attendance rate arrived. Below the gate the rollcall is held and checked again
/// later; at or above it the countdown starts.
pub(super) fn on_gate(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    rate: Option<f64>,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    activity.gate_in_flight = false;
    if activity.acted || !activity.gate_pending || activity.countdown_deadline.is_some() {
        // The user deferred or signed while the request was out: the answer is stale and must
        // not start a countdown.
        return;
    }
    activity.attendance_rate = rate;
    let gate = env.cfg.gate_percent;
    let rate = rate.unwrap_or(0.0);
    let holding = rate + f64::EPSILON < gate;
    emit(
        env.cb,
        Event::RollcallGate {
            rollcall_id: &key.id,
            activity_token: &activity.activity_token,
            rate: activity.attendance_rate,
            gate_percent: gate,
            holding,
        },
    );
    if holding {
        activity.gate_next_check = Some(Instant::now() + GATE_RECHECK_INTERVAL);
        emit(
            env.cb,
            Event::log(
                LogLevel::Info,
                format!(
                    "rollcall {} below {gate:.0}% gate ({rate:.1}%), holding",
                    key.id
                ),
            ),
        );
        return;
    }
    activity.gate_pending = false;
    activity.gate_next_check = None;
    activity.countdown_deadline =
        Some(Instant::now() + Duration::from_secs(env.cfg.countdown_secs));
    if activity.kind == RollcallKind::Number && !activity.code_requested {
        activity.code_requested = true;
        if let Some(reader) = activity.participants.iter().next() {
            spawn_code_read(env, key, reader);
        }
    }
}

/// The shared number code was read. If the user is waiting on it, sign now.
pub(super) fn on_code_read(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    code: Option<String>,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    activity.number_code = code;
    activity.code_requested = true;
    if std::mem::take(&mut activity.sign_pending) {
        dispatch_signs(activities, env, key);
    }
}

pub(super) fn on_tick(activities: &mut HashMap<ActivityKey, Activity>, env: &Env, now: Instant) {
    let mut due = Vec::new();
    for (key, activity) in activities.iter_mut() {
        if let Some(deadline) = activity.countdown_deadline {
            if activity.acted {
                continue;
            }
            emit(
                env.cb,
                Event::Countdown {
                    scope: CountdownScope::Rollcall,
                    activity_token: &activity.activity_token,
                    external_id: &key.id,
                    remaining_secs: deadline.saturating_duration_since(now).as_secs(),
                },
            );
            if now >= deadline {
                due.push(key.clone());
            }
        } else if gate_check_due(activity, now) {
            let Some(reader) = activity.participants.iter().next() else {
                continue;
            };
            if spawn_gate_check(env, key, reader) {
                activity.gate_in_flight = true;
                activity.gate_next_check = None;
            } else {
                // No usable account right now. Pace the retry so the tick does not spin on it.
                activity.gate_next_check = Some(now + GATE_RECHECK_INTERVAL);
            }
        }
    }
    for key in due {
        dispatch_signs(activities, env, &key);
    }
}

/// The user's "sign now": skip the gate and the countdown. A number rollcall whose code has not
/// been read yet reads it first and signs when it arrives, rather than guessing codes against
/// the server. Pressed again after the sign-ins went out, it retries only the failed accounts.
pub(super) fn on_sign_now(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
) -> Result<(), String> {
    let Some(activity) = activities.get_mut(key) else {
        return Err("rollcall activity is gone".into());
    };
    if activity.kind == RollcallKind::Qr && !qr::qr_source_available(&env.cfg, &env.accounts) {
        return Err(
            "QR sign-in requires a teacher helper; stop monitoring, add a teacher account, then restart monitoring"
                .into(),
        );
    }
    if activity.acted {
        let retryable = retryable_accounts(activity);
        if retryable.is_empty() {
            return Err(
                "no retryable accounts — every participant is signed or beyond its retry bound"
                    .into(),
            );
        }
        dispatch_signs_for(activities, env, key, retryable);
        return Ok(());
    }
    if activity.mutation_blocked {
        return Err("definition_change_in_progress".to_string());
    }
    if activity.kind == RollcallKind::Number && activity.number_code.is_none() {
        activity.gate_pending = false;
        activity.countdown_deadline = None;
        activity.sign_pending = true;
        if !activity.code_requested {
            activity.code_requested = true;
            if let Some(reader) = activity.participants.iter().next() {
                spawn_code_read(env, key, reader);
            }
        }
        return Ok(());
    }
    dispatch_signs(activities, env, key);
    Ok(())
}

/// The user's "not now": stop the gate and the countdown, and leave the rollcall for a manual
/// sign-in.
pub(super) fn on_defer(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    activity.countdown_deadline = None;
    activity.gate_pending = false;
    // A gate answer still on its way is stale now; `on_gate` drops it because the gate is no
    // longer pending.
    activity.gate_in_flight = false;
    activity.gate_next_check = None;
    emit(
        env.cb,
        Event::PendingSignIn {
            rollcall_id: &key.id,
            activity_token: &activity.activity_token,
        },
    );
}

/// Sign in every participant.
pub(super) fn dispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
) {
    if let Some(participants) = activities
        .get(key)
        .map(|activity| activity.participants.iter().cloned().collect())
    {
        dispatch_signs_for(activities, env, key, participants);
    }
}

/// Sign in the given accounts, each on its own session. Marks the activity acted, so the gate
/// and the countdown are over for it.
pub(super) fn dispatch_signs_for(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    account_ids: Vec<String>,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    if !activity.acted && activity.mutation_blocked {
        return;
    }
    activity.acted = true;
    activity.countdown_deadline = None;
    if activity.kind == RollcallKind::Qr {
        return dispatch_qr(activity, env, key, &account_ids);
    }
    for account in account_ids.iter().filter_map(|id| env.accounts.get(id)) {
        spawn_sign(
            env,
            account.clone(),
            activity.kind,
            activity.number_code.clone(),
            key,
        );
    }
}

/// The remote token service as a QR job takes it: address, key, and the settings revision the
/// job is bound to.
type RemoteSource = (String, Arc<Secret>, watch::Receiver<u64>, u64);

fn remote_source(env: &Env) -> Option<RemoteSource> {
    if !qr::is_qr_remote_configured(&env.cfg) {
        return None;
    }
    let api_key = env.cfg.qr_remote_key.clone()?;
    let base_url = normalize_qr_remote_base_url(&env.cfg.qr_remote_base_url).ok()?;
    let (revision_rx, revision) = env.qr_revision();
    Some((base_url, api_key, revision_rx, revision))
}

/// A QR rollcall is signed with a token only a teacher's session or the remote token service can
/// supply. Prefer a teacher of the same school, with the remote service as its fallback; without
/// a teacher use the remote service alone; with neither, record that nothing was sent so the
/// rollcall can be retried once a source appears.
fn dispatch_qr(activity: &mut Activity, env: &Env, key: &ActivityKey, account_ids: &[String]) {
    let teacher = env
        .accounts
        .values()
        .filter(|account| account.is_teacher)
        .min_by_key(|account| (account.base_url != key.base_url, account.id.as_str()));
    let students = env.students(account_ids);
    if students.is_empty() {
        if teacher.is_none() || account_ids.is_empty() {
            activity.acted = false;
        }
        if teacher.is_none() {
            warn_needs_teacher(env.cb, &activity.activity_token);
        }
        return;
    }
    let (tx, key, generation) = (env.tx.clone(), key.clone(), activity.plan_generation);
    match (teacher, remote_source(env)) {
        (Some(teacher), remote) => qr::spawn_qr_teacher_assist(
            teacher.clone(),
            students,
            tx,
            key,
            generation,
            env.plan_rx(),
            &env.group,
            remote,
        ),
        (None, Some((base_url, api_key, revision_rx, revision))) => qr::spawn_qr_remote(
            base_url,
            api_key,
            students,
            tx,
            key,
            generation,
            env.plan_rx(),
            revision_rx,
            revision,
            &env.group,
        ),
        (None, None) => {
            let reason = "qr: no teacher and remote not configured";
            if activity.qr != QrSource::Mutating {
                activity.qr = QrSource::Failed {
                    reason: reason.to_string(),
                };
            }
            emit(
                env.cb,
                Problem::warning("qr_source_failed", reason).activity(&activity.activity_token),
            );
            activity.acted = false;
            warn_needs_teacher(env.cb, &activity.activity_token);
        }
    }
}

fn warn_needs_teacher(cb: EventCb, activity_token: &str) {
    emit(
        cb,
        Problem::warning(
            "qr_needs_teacher",
            "偵測到 QR 點名，但目前沒有教師帳號可輔助。請先停止監控，到「帳號」新增教師帳號，再重新開始監控。",
        )
        .activity(activity_token),
    );
}

/// Record one account's sign-in result. A lost session is restored and the account signed again;
/// any other failure is left for the user's "sign now". Both are bounded per account.
pub(super) fn on_sign_result(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    account_id: String,
    result: Result<SignOutcome, String>,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    publish(env, activity, &account_id, &result);
    let error = match result {
        Ok(outcome) => {
            activity.signed.insert(account_id.clone());
            activity.needs_resign.remove(&account_id);
            activity.sign_failed.remove(&account_id);
            activity.resign_attempts.remove(&account_id);
            if activity.number_code.is_none() {
                // A code found by trial is good for every other participant.
                activity.number_code = outcome.discovered_code;
            }
            return emit(
                env.cb,
                Event::SignedIn {
                    rollcall_id: &key.id,
                    activity_token: &activity.activity_token,
                    account_id: &account_id,
                    course: &activity.course,
                    method: &outcome.method,
                },
            );
        }
        Err(error) => error,
    };
    let retryable = activity.count_failure(&account_id);
    let token = activity.activity_token.as_str();
    let failed = |message: String| {
        emit(
            env.cb,
            Problem::error("sign_failed", message).activity(token),
        )
    };
    if rollcall::is_auth_lost(&error) {
        // From here the re-login path owns the account, not the manual retry.
        activity.sign_failed.remove(&account_id);
        if retryable {
            activity.needs_resign.insert(account_id.clone());
            emit(
                env.cb,
                Event::log(
                    LogLevel::Warn,
                    format!(
                        "rollcall {}: {account_id} session lost mid-sign, re-logging in",
                        key.id
                    ),
                ),
            );
            env.tx.send(MonitorMsg::AuthLost { account_id }).ok();
        } else {
            activity.needs_resign.remove(&account_id);
            failed(format!(
                "{account_id}: {error} (unrecoverable after {MAX_RESIGN} re-logins)"
            ));
        }
    } else if retryable {
        failed(format!("{account_id}: {error}"));
        activity.sign_failed.insert(account_id);
    } else {
        activity.sign_failed.remove(&account_id);
        failed(format!(
            "{account_id}: {error} (unrecoverable after {MAX_RESIGN} attempts)"
        ));
    }
}

/// A QR job's result for one account. A result from before the current plan, or for an activity
/// re-detected since, is dropped. A result that says no request was sent is shown to the user but
/// changes nothing: the activity stays retryable.
pub(super) fn on_qr_sign_result(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    account_id: String,
    generation: u64,
    mutation_attempted: bool,
    result: Result<SignOutcome, String>,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    if activity.plan_generation != generation {
        return;
    }
    if activity.kind == RollcallKind::Qr {
        if !mutation_attempted {
            return publish(env, activity, &account_id, &result);
        }
        activity.qr = QrSource::Mutating;
    }
    on_sign_result(activities, env, key, account_id, result);
}

pub(super) fn on_qr_source_failed(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    key: &ActivityKey,
    generation: u64,
    reason: String,
) {
    let Some(activity) = activities.get_mut(key) else {
        return;
    };
    if activity.plan_generation != generation
        || activity.kind != RollcallKind::Qr
        || activity.qr == QrSource::Mutating
    {
        return;
    }
    emit(
        env.cb,
        Problem::warning("qr_source_failed", &reason).activity(&activity.activity_token),
    );
    activity.qr = QrSource::Failed { reason };
}

pub(super) fn on_qr_mutation_started(
    activities: &mut HashMap<ActivityKey, Activity>,
    key: &ActivityKey,
    generation: u64,
) {
    if let Some(activity) = activities.get_mut(key) {
        if activity.plan_generation == generation && activity.kind == RollcallKind::Qr {
            activity.qr = QrSource::Mutating;
        }
    }
}

/// The account's session is back: sign it in wherever it lost the session mid-sign. A QR
/// rollcall needs a fresh token, so it goes back through `dispatch_signs_for`; without a source
/// the account stays marked for a later attempt.
pub(super) fn redispatch_signs(
    activities: &mut HashMap<ActivityKey, Activity>,
    env: &Env,
    account_id: &str,
) {
    let Some(account) = env.accounts.get(account_id) else {
        return;
    };
    let sourced = qr::qr_source_available(&env.cfg, &env.accounts);
    let mut qr_keys = Vec::new();
    for (key, activity) in activities.iter_mut() {
        if activity.signed.contains(account_id) || !activity.needs_resign.contains(account_id) {
            continue;
        }
        if activity.kind != RollcallKind::Qr {
            activity.needs_resign.remove(account_id);
            spawn_sign(
                env,
                account.clone(),
                activity.kind,
                activity.number_code.clone(),
                key,
            );
        } else if sourced {
            activity.needs_resign.remove(account_id);
            qr_keys.push(key.clone());
        }
    }
    for key in qr_keys {
        dispatch_signs_for(activities, env, &key, vec![account_id.to_string()]);
    }
}

/// A QR source just became usable: try again every QR rollcall of the current plan that failed
/// only for want of one. A rollcall that may have sent a request is never retried here.
pub(super) fn retry_without_source(activities: &mut HashMap<ActivityKey, Activity>, env: &Env) {
    let waiting: Vec<ActivityKey> = activities
        .iter()
        .filter(|(_, activity)| {
            activity.kind == RollcallKind::Qr
                && activity.plan_generation == env.plan.generation
                && matches!(activity.qr, QrSource::Failed { .. })
                && !activity.mutation_blocked
                && !activity.unsigned().is_empty()
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in waiting {
        let Some(activity) = activities.get_mut(&key) else {
            continue;
        };
        let unsigned = activity.unsigned();
        for account_id in &unsigned {
            activity.sign_failed.remove(account_id);
            activity.resign_attempts.remove(account_id);
        }
        activity.qr = QrSource::Untried;
        dispatch_signs_for(activities, env, &key, unsigned);
    }
}

/// Tell the engine how one account fared, for the monitoring snapshot.
fn publish<T>(env: &Env, activity: &Activity, account_id: &str, result: &Result<T, String>) {
    let (phase, error) = match result {
        Ok(_) => (AccountResultPhase::Succeeded, None),
        Err(error) => (AccountResultPhase::Failed, Some(error.clone())),
    };
    let _ = env.runtime_tx.send(RuntimeEvent::AccountResult {
        sources: activity.sources.iter().cloned().collect(),
        account_id: account_id.to_string(),
        phase,
        activity_kind: activity.kind.as_str().to_string(),
        course_name: activity.course.clone(),
        error,
    });
}

// ---- network tasks: each answers with a message, the actor never waits ----

/// Read the class attendance rate through one participant's session. False when that account
/// is not in the session, so the caller does not wait for an answer that will never come.
fn spawn_gate_check(env: &Env, key: &ActivityKey, account_id: &str) -> bool {
    let Some(account) = env.accounts.get(account_id).cloned() else {
        return false;
    };
    let (tx, key) = (env.tx.clone(), key.clone());
    env.spawn(async move {
        let endpoints = Endpoints::derive(&account.base_url);
        let rate = rollcall::attendance_rate(&account.client, &endpoints, &key.id).await;
        tx.send(MonitorMsg::GateResult { key, rate }).ok();
    });
    true
}

fn spawn_code_read(env: &Env, key: &ActivityKey, account_id: &str) {
    let Some(account) = env.accounts.get(account_id).cloned() else {
        return;
    };
    let (tx, key) = (env.tx.clone(), key.clone());
    env.spawn(async move {
        let endpoints = Endpoints::derive(&account.base_url);
        let code = rollcall::read_number_code(&account.client, &endpoints, &key.id).await;
        tx.send(MonitorMsg::CodeRead { key, code }).ok();
    });
}

fn spawn_sign(
    env: &Env,
    account: Arc<Account>,
    kind: RollcallKind,
    code: Option<String>,
    key: &ActivityKey,
) {
    let (tx, key) = (env.tx.clone(), key.clone());
    let radar_strategy = env.cfg.radar_strategy.clone();
    let pacing = env.cfg.number_pacing();
    env.spawn(async move {
        let endpoints = Endpoints::derive(&account.base_url);
        let (client, id) = (&account.client, key.id.as_str());
        let result = match kind {
            RollcallKind::Number => {
                rollcall::sign_number(
                    client,
                    &endpoints,
                    id,
                    &account.user_no,
                    &account.device_id,
                    code.as_deref(),
                    pacing,
                )
                .await
            }
            RollcallKind::Radar => {
                rollcall::sign_radar(
                    client,
                    &endpoints,
                    id,
                    &radar_strategy,
                    &account.user_no,
                    &account.device_id,
                )
                .await
            }
            RollcallKind::SelfRegistration => {
                rollcall::sign_self_registration(client, &endpoints, id, &account.user_no).await
            }
            RollcallKind::Qr | RollcallKind::Unknown => Err("unsupported here".into()),
        };
        let account_id = account.id.clone();
        tx.send(MonitorMsg::SignResult {
            key,
            account_id,
            result,
        })
        .ok();
    });
}
