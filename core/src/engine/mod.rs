//! The core behind the FFI: one Tokio runtime, the host's event callback, and (after `Init`)
//! the registry, config, vault and target supervisor behind one mutex.
//!
//! A command either runs to completion under that lock (`commands`), or, when it needs the
//! network, copies what it needs, releases the lock, and locks again to store the result
//! (`session`, `course_query`, `monitoring`). Every command gets exactly one reply, including
//! when its task panics: the reply is then a fixed message, never the panic's own text.

mod accounts;
mod commands;
mod course_query;
mod definitions;
mod groups;
mod monitoring;
mod schedule;
mod session;
mod settings;
mod state;
#[cfg(test)]
mod tests;

use crate::wire::{emit, respond, Command, Event, EventCb, Problem};
use commands::handle_sync;
use course_query::spawn_list_common_courses;
use groups::spawn_validated_group_command;
use monitoring::reconcile_monitor;
use schedule::arm_clock_stale_timer;
use serde_json::Value;
#[cfg(test)]
pub(crate) use session::build_client;
use session::{spawn_import_cookies, spawn_login};
use state::CoreState;
use std::future::Future;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::task::{JoinHandle, JoinSet};

pub struct Core {
    rt: Runtime,
    cb: EventCb,
    state: Arc<Mutex<Option<CoreState>>>,
    /// Per-core captcha answer window (tests only). Defaults to the production 180 s; ONLY the
    /// timeout e2e shortens it on its own core, so other captcha tests (even in other modules,
    /// running in parallel) are never affected. Production builds have no field — always 180 s.
    #[cfg(test)]
    captcha_answer_timeout: Duration,
}

/// Reply to a correlated command. `error` is None on success.
fn reply(cb: EventCb, id: u64, ok: bool, error: Option<String>) {
    respond(
        cb,
        id,
        Event::Reply {
            ok,
            error,
            data: None,
        },
    );
}

fn reply_data(cb: EventCb, id: u64, ok: bool, error: Option<String>, data: Value) {
    respond(
        cb,
        id,
        Event::Reply {
            ok,
            error,
            data: Some(data),
        },
    );
}

/// Fixed seam-error text for commands that fail to parse. Deliberately constant: a malformed command
/// may embed secret-shaped payloads in the very fields that fail to parse, so the error must never
/// reflect input content (no serde literals).
const MALFORMED_COMMAND: &str = "未知或格式錯誤的命令";

/// Fixed error text after a caught internal panic. The input is never echoed — it may carry secrets.
const INTERNAL_ERROR: &str = "核心內部錯誤";

impl Core {
    /// Build the runtime and a fresh core. The only fallible step is the Tokio runtime build
    /// (resource exhaustion); the failure must surface to the FFI caller as a null handle, not abort
    /// the host.
    fn new(cb: EventCb) -> Result<Box<Core>, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("build tokio runtime: {error}"))?;

        Ok(Box::new(Core {
            rt,
            cb,
            state: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            captcha_answer_timeout: session::CAPTCHA_ANSWER_TIMEOUT,
        }))
    }
}

pub fn init(cb: EventCb) -> Result<Box<Core>, String> {
    Core::new(cb)
}

/// Lock the core state, recovering from a poisoned mutex. A panic caught at the FFI seam unwinds
/// through a held guard and poisons the mutex; the state itself stays coherent, so the core must
/// keep serving commands instead of failing every one from then on.
fn lock_state(state: &Mutex<Option<CoreState>>) -> std::sync::MutexGuard<'_, Option<CoreState>> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Complete a command whose handling panicked at the FFI boundary. The reply is a fixed message:
/// the input is never echoed, since it may hold a secret.
pub fn panic_reply(core: &Core, json_bytes: &[u8]) {
    fail_unparsed(core.cb, json_bytes, "core_panicked", INTERNAL_ERROR);
}

pub fn send(core: &Core, json_bytes: &[u8]) {
    let Ok(command) = serde_json::from_slice::<Command>(json_bytes) else {
        return fail_unparsed(core.cb, json_bytes, "bad_command", MALFORMED_COMMAND);
    };
    match command {
        Command::Login { id, account_id } => spawn_login(core, id, account_id),
        Command::ImportCookies {
            id,
            account_id,
            cookies_json,
        } => spawn_import_cookies(core, id, account_id, cookies_json),
        Command::ListCommonCourses {
            id,
            member_account_ids,
        } => spawn_list_common_courses(core, id, member_account_ids),
        // A group bound to courses is first checked against the courses its members really share.
        Command::CreateGroup { ref group, .. }
        | Command::UpdateGroup { ref group, .. }
        | Command::MergeGroups { ref group, .. }
            if !group.course_ids.is_empty() =>
        {
            spawn_validated_group_command(core, command)
        }
        command => {
            let follow_up = FollowUp::of(&command);
            handle_sync(core, command);
            if let Some(cancel_removed_pending) = follow_up.reconcile() {
                reconcile_monitor(core, cancel_removed_pending);
            }
            if follow_up == FollowUp::ReconcileAndWatchClock {
                arm_clock_stale_timer(core);
            }
        }
    }
}

/// Fail a command that could not be parsed or whose handling panicked. With an id the waiting
/// caller gets its reply; without one the failure is at least announced.
fn fail_unparsed(cb: EventCb, json_bytes: &[u8], code: &str, message: &str) {
    let id = serde_json::from_slice::<Value>(json_bytes)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_u64));
    match id {
        Some(id) => reply(cb, id, false, Some(message.to_string())),
        None => emit(cb, Problem::error(code, message)),
    }
}

/// What has to happen once a command has been handled, whether or not it succeeded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FollowUp {
    Nothing,
    /// The plan may have changed: bring the running session in line with it.
    Reconcile,
    /// As above, and targets may be gone: their work that has not started is dropped.
    ReconcileAndCancelRemoved,
    /// As above, and the schedule clock is fresh: watch for it going stale.
    ReconcileAndWatchClock,
}

impl FollowUp {
    fn of(command: &Command) -> Self {
        match command {
            Command::AddAccount { .. }
            | Command::CreateGroup { .. }
            | Command::SetMonitoringPreferences { .. }
            | Command::StartTarget { .. }
            | Command::ResumeScheduledMonitoring { .. }
            | Command::AcknowledgeTemporaryMerge { .. }
            | Command::ClearPlatformLimit { .. } => Self::Reconcile,
            Command::DeleteAccount { .. }
            | Command::UpdateGroup { .. }
            | Command::DeleteGroup { .. }
            | Command::MergeGroups { .. }
            | Command::SetTargetSchedule { .. }
            | Command::StopTarget { .. }
            | Command::StopAllMonitoring { .. }
            | Command::SuspendForPlatformLimit { .. } => Self::ReconcileAndCancelRemoved,
            Command::ApplyScheduleClock { .. } => Self::ReconcileAndWatchClock,
            _ => Self::Nothing,
        }
    }

    /// Whether to reconcile, and if so whether removed targets' pending work is dropped.
    fn reconcile(self) -> Option<bool> {
        match self {
            Self::Nothing => None,
            Self::Reconcile | Self::ReconcileAndWatchClock => Some(false),
            Self::ReconcileAndCancelRemoved => Some(true),
        }
    }
}

/// Run one async command inside a single-task `JoinSet` so a panic is observed at the join point
/// instead of vanishing silently: a bare spawned task's panic is captured by its JoinHandle, and
/// dropping that handle without joining would quietly kill the command mid-flight, leaving the
/// awaiting UI with no reply. `on_panic` runs the command's fixed INTERNAL_ERROR backstop. The
/// returned outer handle keeps Start-task abort semantics: dropping/aborting it drops the JoinSet,
/// which aborts the inner task and everything it spawned/awaited.
fn spawn_observed<F>(
    rt: &Runtime,
    on_panic: impl FnOnce() + Send + 'static,
    future: F,
) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    rt.spawn(async move {
        let mut set = JoinSet::new();
        set.spawn(future);
        // A single-task set joins exactly once — `if let`, not a loop, so the FnOnce backstop is
        // provably called at most once.
        if let Some(joined) = set.join_next().await {
            match joined {
                Ok(()) => {}
                // The panic payload is never echoed — the fixed INTERNAL_ERROR text is the only
                // thing that may cross the seam.
                Err(join_error) if join_error.is_panic() => on_panic(),
                Err(_) => {} // cancelled (abort) — the aborting side owns the terminal path
            }
        }
    })
}
