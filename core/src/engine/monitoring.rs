//! The engine's side of a monitoring session: starting and stopping it, keeping it in step with
//! the plan and the settings, and folding what it reports into the snapshot.

use super::commands::{Handled, Reply};
use super::session::authed_client;
use super::state::{CoreState, MonitorLifecycle, RunningMonitor, StoppedMonitor};
use super::{lock_state, reply, Core, INTERNAL_ERROR};
use crate::config::AccountMeta;
use crate::monitor::MonitorConfig;
use crate::supervisor::EffectivePlan;
use crate::wire::{emit, Event, EventCb, LoginState, Problem, WireError};
use crate::{login, monitor};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;

pub(super) fn emit_monitoring_snapshot(cb: EventCb, st: &CoreState) {
    let snapshot = st.supervisor.snapshot(&st.config, &st.login_in_flight);
    emit(
        cb,
        Event::MonitoringSnapshot {
            snapshot: &snapshot,
        },
    );
}

/// Pass a command on to the running session, which answers it itself.
pub(super) fn route_to_monitor(st: &CoreState, message: monitor::MonitorMsg) -> Handled {
    let session = st.monitor.running_handle().ok_or("not monitoring")?;
    session
        .tx
        .send(message)
        .map_err(|_| "monitor actor is unavailable")?;
    Ok(Reply::Deferred)
}

pub(super) fn stop_monitor(lifecycle: &mut MonitorLifecycle, cb: EventCb, cancelled_reason: &str) {
    match lifecycle.take_for_stop() {
        StoppedMonitor::Idle => {}
        StoppedMonitor::Starting(mut starting) => {
            if starting.command_id != 0 {
                reply(
                    cb,
                    starting.command_id,
                    false,
                    Some(cancelled_reason.to_string()),
                );
            }
            if let Some(task) = starting.task.take() {
                task.abort();
            }
        }
        StoppedMonitor::Running(handle) => {
            handle.stop();
        }
    }
}

/// Watch the monitor's panic channel. The monitor's TaskGroup pings it (`Some(())`) when one of its
/// futures panicked; only a RUNNING monitor whose generation matches this start is torn down — a
/// stale watchdog from an earlier start can never cancel a newer session. The handle is taken and
/// the lock released BEFORE `stop()` (TaskGroup::cancel aborts tasks, whose drops may run
/// callbacks). `None` (senders dropped by a normal stop) is a no-op.
pub(super) fn spawn_monitor_watchdog(
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mut panic_rx: UnboundedReceiver<()>,
) {
    let restart_runtime = rt.clone();
    rt.spawn(async move {
        while let Some(()) = panic_rx.recv().await {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                continue;
            };
            let MonitorLifecycle::Running(running) = &st.monitor else {
                continue;
            };
            if running.generation != generation {
                continue;
            }
            let handle = match st.monitor.take_for_stop() {
                StoppedMonitor::Running(handle) => handle,
                _ => unreachable!("Running verified above"),
            };
            drop(guard);
            handle.stop();
            emit(cb, Problem::error("monitor_panicked", INTERNAL_ERROR));
            reconcile_monitor_with(restart_runtime.clone(), state.clone(), cb, false);
        }
    });
}

/// Runtime events are consumed asynchronously; definition/control commands themselves stay short.
pub(super) fn spawn_monitor_runtime_listener(
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mut events: UnboundedReceiver<monitor::RuntimeEvent>,
) {
    let reconcile_runtime = rt.clone();
    rt.spawn(async move {
        while let Some(event) = events.recv().await {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                break;
            };
            if !matches!(
                &st.monitor,
                MonitorLifecycle::Running(running) if running.generation == generation
            ) {
                break;
            }
            let needs_reconcile = match event {
                monitor::RuntimeEvent::AccountResult {
                    sources,
                    account_id,
                    phase,
                    activity_kind,
                    course_name,
                    error,
                } => {
                    st.supervisor.set_account_result(
                        &sources,
                        &account_id,
                        phase,
                        Some(activity_kind),
                        Some(course_name),
                        error.map(|message| WireError {
                            code: "mutation_failed".to_string(),
                            message,
                        }),
                    );
                    false
                }
                monitor::RuntimeEvent::AccountLogin {
                    account_id,
                    online,
                    error,
                } => {
                    st.supervisor.set_login_state(
                        account_id,
                        if online {
                            LoginState::Online
                        } else {
                            LoginState::Error
                        },
                        error.map(|message| WireError {
                            code: "relogin_failed".to_string(),
                            message,
                        }),
                    );
                    st.supervisor.reconcile(&mut st.config);
                    true
                }
            };
            emit_monitoring_snapshot(cb, st);
            drop(guard);
            if needs_reconcile {
                reconcile_monitor_with(reconcile_runtime.clone(), state.clone(), cb, false);
            }
        }
    });
}

/// The monitor's live settings snapshot. Built at actor startup and after every settings change.
pub(super) fn monitor_config(st: &CoreState) -> MonitorConfig {
    let s = &st.config.settings;
    MonitorConfig {
        countdown_secs: s.countdown_secs,
        gate_percent: s.attendance_gate_percent,
        llm_endpoint: s.llm_endpoint.clone(),
        llm_model: s.llm_model.clone(),
        llm_key: st.vault.as_ref().and_then(|v| v.get_llm_key()),
        llm_max_tokens: s.llm_max_tokens,
        max_answer_reask: s.max_answer_reask,
        prepare_retry_budget_secs: s.prepare_retry_budget_secs,
        autoanswer_types: s.autoanswer_types.clone(),
        enable_llm_tools: s.enable_llm_tools,
        max_tool_iterations: s.max_tool_iterations,
        resubmit_for_correct: s.resubmit_for_correct,
        radar_strategy: s.radar_strategy.clone(),
        number_cooldown_ms: s.number_cooldown_ms,
        number_max_cooldowns: s.number_max_cooldowns,
        poll_idle_secs: s.poll_idle_secs,
        quiz_detect_secs: s.quiz_detect_secs,
        qr_remote_enabled: s.qr_remote_enabled,
        qr_remote_base_url: s.qr_remote_base_url.clone(),
        qr_remote_key: st
            .vault
            .as_ref()
            .and_then(|v| v.get_qr_remote_key_secret())
            .map(std::sync::Arc::new),
    }
}

/// Push changed non-schedule settings into the long-lived actor and its pollers.
pub(super) fn push_config(st: &CoreState) {
    if let Some(h) = st.monitor.running_handle() {
        let _ = h.tx.send(monitor::MonitorMsg::ConfigUpdated(Box::new(
            monitor_config(st),
        )));
    }
}

pub(super) struct MonitorLoginInput {
    pub(super) meta: AccountMeta,
    pub(super) base_url: String,
    pub(super) password: String,
    pub(super) cookies: String,
}

#[derive(Clone, Copy)]
pub(super) enum MonitorAuthMode {
    Start,
    Upsert,
}

pub(super) fn monitor_plan(plan: &EffectivePlan) -> monitor::MonitorPlan {
    monitor::MonitorPlan {
        generation: plan.revision,
        routes: plan
            .routes
            .iter()
            .map(|route| monitor::MonitorRoute {
                source_targets: route.source_targets.clone(),
                detector_account_id: route.detector_account_id.clone(),
                participant_account_ids: route.participant_account_ids.clone(),
                course_ids: route.course_ids.clone(),
            })
            .collect(),
    }
}

pub(super) fn required_monitor_accounts(st: &CoreState, plan: &EffectivePlan) -> HashSet<String> {
    let mut required: HashSet<String> = plan
        .routes
        .iter()
        .flat_map(|route| {
            route
                .participant_account_ids
                .iter()
                .chain(std::iter::once(&route.detector_account_id))
        })
        .filter(|account_id| !st.supervisor.account_is_terminal(account_id))
        .cloned()
        .collect();
    // Teacher accounts never poll. They are authenticated only as potential QR helpers.
    required.extend(
        st.config
            .accounts
            .iter()
            .filter(|account| account.is_teacher && !st.supervisor.account_is_terminal(&account.id))
            .map(|account| account.id.clone()),
    );
    required
}

pub(super) fn monitor_login_inputs(
    st: &CoreState,
    account_ids: &HashSet<String>,
) -> Vec<MonitorLoginInput> {
    let Some(vault) = st.vault.as_ref() else {
        return Vec::new();
    };
    st.config
        .accounts
        .iter()
        .filter(|account| account_ids.contains(&account.id))
        .filter_map(|meta| {
            let base_url = st.registry.resolve(&meta.school_ref)?;
            let (password, cookies) = vault.get(&meta.id).unwrap_or_default().into_parts();
            Some(MonitorLoginInput {
                meta: meta.clone(),
                base_url,
                password,
                cookies,
            })
        })
        .collect()
}

pub(super) fn reconcile_monitor(core: &Core, cancel_removed_pending: bool) {
    reconcile_monitor_with(
        core.rt.handle().clone(),
        core.state.clone(),
        core.cb,
        cancel_removed_pending,
    );
}

pub(super) fn reconcile_monitor_with(
    runtime: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    cancel_removed_pending: bool,
) {
    let launch = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return;
        };
        let runtime_changed = st.supervisor.reconcile(&mut st.config);
        if runtime_changed {
            if let Err(error) = st.config.save(&st.config_path()) {
                emit(cb, Problem::error("runtime_persistence_failed", error));
                return;
            }
        }
        let desired = st.supervisor.plan().clone();
        if desired.routes.is_empty() {
            match &mut st.monitor {
                MonitorLifecycle::Running(running) => {
                    let _ = running
                        .handle
                        .apply_plan(monitor_plan(&desired), cancel_removed_pending);
                }
                MonitorLifecycle::Starting(_) => {
                    stop_monitor(&mut st.monitor, cb, "no effective monitoring target");
                }
                MonitorLifecycle::Idle => {}
            }
            emit_monitoring_snapshot(cb, st);
            return;
        }
        if st.vault.is_none() {
            emit_monitoring_snapshot(cb, st);
            return;
        }
        let required = required_monitor_accounts(st, &desired);
        match &mut st.monitor {
            MonitorLifecycle::Starting(_) => return,
            MonitorLifecycle::Running(running) => {
                let _ = running
                    .handle
                    .apply_plan(monitor_plan(&desired), cancel_removed_pending);
                let missing: HashSet<String> = required
                    .difference(&running.loaded_account_ids)
                    .filter(|account_id| !running.loading_account_ids.contains(*account_id))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    return;
                }
                let inputs = monitor_login_inputs(st, &missing);
                let ids: HashSet<String> =
                    inputs.iter().map(|input| input.meta.id.clone()).collect();
                if let MonitorLifecycle::Running(running) = &mut st.monitor {
                    running.loading_account_ids.extend(ids.iter().cloned());
                }
                st.login_in_flight.extend(ids);
                emit_monitoring_snapshot(cb, st);
                Some((
                    MonitorAuthMode::Upsert,
                    running_generation(&st.monitor),
                    inputs,
                ))
            }
            MonitorLifecycle::Idle => {
                let inputs = monitor_login_inputs(st, &required);
                if inputs.is_empty() {
                    emit_monitoring_snapshot(cb, st);
                    return;
                }
                st.login_in_flight
                    .extend(inputs.iter().map(|input| input.meta.id.clone()));
                st.monitor_generation = st.monitor_generation.wrapping_add(1).max(1);
                let generation = st.monitor_generation;
                if st.monitor.begin_start(generation, 0).is_err() {
                    return;
                }
                emit_monitoring_snapshot(cb, st);
                Some((MonitorAuthMode::Start, generation, inputs))
            }
        }
    };
    if let Some((mode, generation, inputs)) = launch {
        spawn_monitor_auth(runtime, state, cb, generation, mode, inputs);
    }
}

pub(super) fn running_generation(lifecycle: &MonitorLifecycle) -> u64 {
    match lifecycle {
        MonitorLifecycle::Running(running) => running.generation,
        MonitorLifecycle::Idle | MonitorLifecycle::Starting(_) => 0,
    }
}

pub(super) fn spawn_monitor_auth(
    runtime: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mode: MonitorAuthMode,
    inputs: Vec<MonitorLoginInput>,
) {
    let task_state = state.clone();
    let attach_state = state;
    let restart_runtime = runtime.clone();
    let task = runtime.spawn(async move {
        let attempted_ids: HashSet<String> =
            inputs.iter().map(|input| input.meta.id.clone()).collect();
        let mut tasks = JoinSet::new();
        let mut task_accounts = HashMap::new();
        for input in inputs {
            let account_id = input.meta.id.clone();
            let abort = tasks.spawn(async move {
                let outcome = authed_client(
                    &input.base_url,
                    &input.meta.username,
                    &input.password,
                    &input.cookies,
                )
                .await;
                (
                    input.meta,
                    input.base_url,
                    crate::secrets::Secret::new(input.password),
                    outcome,
                )
            });
            task_accounts.insert(abort.id(), account_id);
        }

        let mut ready = Vec::new();
        let mut refreshed = Vec::new();
        let mut failures = Vec::new();
        while let Some(joined) = tasks.join_next_with_id().await {
            match joined {
                Ok((task_id, (meta, base_url, password, Ok((client, cookies))))) => {
                    task_accounts.remove(&task_id);
                    refreshed.push((meta.id.clone(), cookies));
                    ready.push(monitor::Account {
                        id: meta.id.clone(),
                        device_id: meta.device_id.clone(),
                        user_no: login::user_no_from_username(&meta.username),
                        is_teacher: meta.is_teacher,
                        course_id: meta.course_id.clone(),
                        base_url,
                        client,
                        username: meta.username,
                        password,
                    });
                }
                Ok((task_id, (meta, _base_url, _password, Err(error)))) => {
                    task_accounts.remove(&task_id);
                    failures.push((meta.id, error));
                }
                Err(error) => {
                    let account_id = task_accounts
                        .remove(&error.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    failures.push((account_id, INTERNAL_ERROR.to_string()));
                }
            }
        }

        {
            let mut guard = lock_state(&task_state);
            let Some(st) = guard.as_mut() else {
                return;
            };
            let current = match mode {
                MonitorAuthMode::Start => st.monitor.is_starting(generation),
                MonitorAuthMode::Upsert => {
                    matches!(&st.monitor, MonitorLifecycle::Running(running) if running.generation == generation)
                }
            };
            if !current {
                return;
            }
            for account_id in &attempted_ids {
                st.login_in_flight.remove(account_id);
            }
            if let Some(vault) = st.vault.as_mut() {
                for (account_id, cookies) in refreshed {
                    if let Some(mut secret) = vault.get(&account_id) {
                        secret.cookies = cookies;
                        if let Err(error) = vault.set(&account_id, secret) {
                            failures.push((account_id, format!("session persistence failed: {error}")));
                        }
                    }
                }
            }
            for account in &ready {
                st.supervisor
                    .set_login_state(account.id.clone(), LoginState::Online, None);
            }
            for (account_id, message) in failures {
                st.supervisor.set_login_state(
                    account_id,
                    LoginState::Error,
                    Some(WireError {
                        code: "login_failed".to_string(),
                        message,
                    }),
                );
            }
            if st.supervisor.reconcile(&mut st.config) {
                if let Err(error) = st.config.save(&st.config_path()) {
                    emit(cb, Problem::error("runtime_persistence_failed", error));
                }
            }
            let desired = monitor_plan(st.supervisor.plan());
            match mode {
                MonitorAuthMode::Start => {
                    let loaded: HashSet<String> =
                        ready.iter().map(|account| account.id.clone()).collect();
                    let has_detector = desired
                        .routes
                        .iter()
                        .any(|route| loaded.contains(&route.detector_account_id));
                    if !has_detector {
                        st.monitor = MonitorLifecycle::Idle;
                    } else {
                        let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                        let (runtime_tx, runtime_rx) =
                            tokio::sync::mpsc::unbounded_channel::<monitor::RuntimeEvent>();
                        let handle = monitor::start(
                            cb,
                            ready,
                            desired,
                            monitor_config(st),
                            panic_tx,
                            runtime_tx,
                        );
                        st.monitor = MonitorLifecycle::Running(RunningMonitor {
                            generation,
                            handle,
                            loaded_account_ids: loaded,
                            loading_account_ids: HashSet::new(),
                        });
                        spawn_monitor_watchdog(
                            restart_runtime.clone(),
                            task_state.clone(),
                            cb,
                            generation,
                            panic_rx,
                        );
                        spawn_monitor_runtime_listener(
                            restart_runtime.clone(),
                            task_state.clone(),
                            cb,
                            generation,
                            runtime_rx,
                        );
                    }
                }
                MonitorAuthMode::Upsert => {
                    if let MonitorLifecycle::Running(running) = &mut st.monitor {
                        running
                            .loading_account_ids
                            .retain(|account_id| !attempted_ids.contains(account_id));
                        running
                            .loaded_account_ids
                            .extend(ready.iter().map(|account| account.id.clone()));
                        let _ = running.handle.upsert_accounts(ready);

                        let _ = running.handle.apply_plan(desired, false);
                    }
                }
            }
            emit_monitoring_snapshot(cb, st);
        }
        reconcile_monitor_with(restart_runtime, task_state, cb, false);
    });

    if matches!(mode, MonitorAuthMode::Start) {
        let mut guard = lock_state(&attach_state);
        match guard.as_mut() {
            Some(st) => st.monitor.attach_start_task(generation, task),
            None => task.abort(),
        }
    }
}
