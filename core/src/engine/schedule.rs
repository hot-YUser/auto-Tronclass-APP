//! Schedules, manual overrides and platform limits.

use super::commands::{Handled, Reply};
use super::definitions::{change_runtime, check_revision, redefine, routed_sources, Redefinition};
use super::monitoring::{emit_monitoring_snapshot, reconcile_monitor_with};
use super::state::CoreState;
use super::{lock_state, Core};
use crate::config::{ScheduleBinding, TargetId, TimeZoneSpec, WeeklySchedule};
use crate::wire::{EventCb, ScheduleClockEntry, WakeMode};
use std::collections::HashSet;
use std::time::Duration;

pub(super) fn set_target_schedule(
    st: &mut CoreState,
    cb: EventCb,
    target: TargetId,
    expected_revision: u64,
    schedule: ScheduleBinding,
) -> Handled {
    check_revision(st, expected_revision)?;
    schedule.validate()?;
    let mut next = st.config.clone();
    let slot = match &target {
        TargetId::Account { account_id } => next
            .account_mut(account_id)
            .filter(|account| !account.is_teacher)
            .map(|account| &mut account.schedule),
        TargetId::Group { group_id } => next
            .groups
            .iter_mut()
            .find(|group| &group.id == group_id)
            .map(|group| &mut group.schedule),
    };
    *slot.ok_or("unknown_target")? = schedule;
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: Some(HashSet::from([target])),
            schedule_changed: true,
            check_registry: false,
        },
    )?;
    Ok(Reply::Done)
}

pub(super) fn set_preferences(
    st: &mut CoreState,
    cb: EventCb,
    expected_revision: u64,
    global_schedule: WeeklySchedule,
    time_zone: TimeZoneSpec,
) -> Handled {
    check_revision(st, expected_revision)?;
    global_schedule.validate()?;
    let mut next = st.config.clone();
    next.monitoring.global_schedule = global_schedule;
    next.monitoring.time_zone = time_zone;
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: Some(routed_sources(st)),
            schedule_changed: true,
            check_registry: false,
        },
    )?;
    Ok(Reply::Done)
}

/// Adopt the host's evaluation of every schedule. The host owns wall-clock time and time zones;
/// the core only checks that the evaluation is for the definitions it holds.
pub(super) fn apply_clock(
    st: &mut CoreState,
    cb: EventCb,
    clock_revision: u64,
    config_revision: u64,
    schedule_revision: u64,
    evaluated_at_utc: &str,
    targets: Vec<ScheduleClockEntry>,
) -> Handled {
    st.supervisor.apply_clock(
        &st.config,
        clock_revision,
        config_revision,
        schedule_revision,
        evaluated_at_utc,
        targets,
    )?;
    let previous = st.config.clone();
    if st.supervisor.reconcile(&mut st.config) {
        if let Err(error) = st.config.save(&st.config_path()) {
            st.config = previous;
            st.supervisor.invalidate_clock();
            return Err(error.into());
        }
    }
    emit_monitoring_snapshot(cb, st);
    Ok(Reply::Done)
}

/// The user's "start now" or "stop now" for one target. Stopping may cut pending work short, so
/// only stopping holds the running session back.
pub(super) fn override_target(
    st: &mut CoreState,
    cb: EventCb,
    target: TargetId,
    force_open: bool,
) -> Handled {
    if st.config.monitoring.all_suspended {
        return Err("all_monitoring_suspended".into());
    }
    let affected = (!force_open).then(|| HashSet::from([target.clone()]));
    change_runtime(st, cb, affected, |supervisor, config| {
        supervisor.set_target_override(config, &target, force_open)
    })?;
    Ok(Reply::Done)
}

pub(super) fn stop_all(st: &mut CoreState, cb: EventCb) -> Handled {
    change_runtime(st, cb, Some(routed_sources(st)), |supervisor, config| {
        supervisor.stop_all(config);
        Ok(true)
    })?;
    Ok(Reply::Done)
}

pub(super) fn resume(st: &mut CoreState, cb: EventCb) -> Handled {
    change_runtime(st, cb, None, |supervisor, config| {
        supervisor.resume_schedules(config);
        Ok(true)
    })?;
    Ok(Reply::Done)
}

pub(super) fn acknowledge_merge(
    st: &mut CoreState,
    cb: EventCb,
    component_id: &str,
    plan_revision: u64,
) -> Handled {
    st.supervisor
        .acknowledge_merge(component_id, plan_revision)?;
    st.supervisor.reconcile(&mut st.config);
    emit_monitoring_snapshot(cb, st);
    Ok(Reply::Done)
}

/// The platform can no longer keep monitoring alive: stop detecting until it can.
pub(super) fn suspend(st: &mut CoreState, cb: EventCb, reason: String) -> Handled {
    change_runtime(st, cb, Some(routed_sources(st)), |supervisor, config| {
        supervisor.suspend_for_platform_limit(config, reason);
        Ok(true)
    })?;
    Ok(Reply::Done)
}

/// The platform limit is gone. A `wake_mode:*` reason only reports how the platform can wake
/// the app, which changes nothing on disk.
pub(super) fn clear_limit(st: &mut CoreState, cb: EventCb, reason: &str) -> Handled {
    if let Some(mode) = platform_wake_mode(reason) {
        st.supervisor.set_wake_mode(mode);
        emit_monitoring_snapshot(cb, st);
        return Ok(Reply::Done);
    }
    change_runtime(st, cb, None, |supervisor, config| {
        supervisor
            .clear_platform_limit(config, reason)
            .map(|()| true)
    })?;
    Ok(Reply::Done)
}

pub(super) fn platform_wake_mode(reason: &str) -> Option<WakeMode> {
    match reason {
        "wake_mode:exact" => Some(WakeMode::Exact),
        "wake_mode:inexact_user_action_required" => Some(WakeMode::InexactUserActionRequired),
        "wake_mode:unavailable" => Some(WakeMode::Unavailable),
        _ => None,
    }
}

pub(super) fn arm_clock_stale_timer(core: &Core) {
    let (revision, deadline) = {
        let guard = lock_state(&core.state);
        let Some(st) = guard.as_ref() else {
            return;
        };
        let Some(revision) = st.supervisor.clock_revision() else {
            return;
        };
        let Some(deadline) = st.supervisor.next_clock_deadline_epoch() else {
            return;
        };
        (revision, deadline)
    };
    let state = core.state.clone();
    let cb = core.cb;
    let runtime = core.rt.handle().clone();
    let wake_runtime = runtime.clone();
    runtime.spawn(async move {
        let seconds = (deadline - crate::time::now()).max(0) as u64;
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let changed = {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                return;
            };
            if st.supervisor.clock_revision() != Some(revision) || crate::time::now() < deadline {
                return;
            }
            let before = st.supervisor.plan().revision;
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            st.supervisor.plan().revision != before
        };
        if changed {
            reconcile_monitor_with(wake_runtime, state, cb, false);
        }
    });
}
