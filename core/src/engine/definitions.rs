//! Changing what is monitored while a session may be running. Every such change is one
//! transaction: the running session holds back the work the change could invalidate, the new
//! state is persisted, and only then is the session told to carry on with the new plan. A
//! failure at any step leaves memory, disk and the session as they were.

use super::commands::Failure;
use super::monitoring::{emit_monitoring_snapshot, monitor_plan};
use super::state::{CoreState, MonitorLifecycle};
use crate::store::config::{Config, TargetId};
use crate::supervisor::TargetSupervisor;
use crate::wire::EventCb;
use serde_json::json;
use std::collections::HashSet;

/// Refuse a change made against a stale view of the definitions. The reply carries the current
/// snapshot so the UI can catch up and ask again.
pub(super) fn check_revision(st: &CoreState, expected: u64) -> Result<(), Failure> {
    if expected == st.config.config_revision {
        return Ok(());
    }
    Err(Failure {
        error: "revision_conflict".to_string(),
        data: Some(json!({ "snapshot": st.snapshot() })),
    })
}

/// Every target the current plan monitors on behalf of.
pub(super) fn routed_sources(st: &CoreState) -> HashSet<TargetId> {
    st.supervisor
        .plan()
        .routes
        .iter()
        .flat_map(|route| route.source_targets.iter().cloned())
        .collect()
}

/// A new version of the persisted definitions.
pub(super) struct Redefinition {
    pub next: Config,
    /// The targets whose pending work the change may invalidate; `None` when it can invalidate
    /// nothing, as when something is only added.
    pub affected: Option<HashSet<TargetId>>,
    /// Whether any schedule changed, which makes the schedule clock stale.
    pub schedule_changed: bool,
    /// Whether the new definitions must still resolve against the school registry.
    pub check_registry: bool,
}

/// Replace the persisted definitions.
pub(super) fn redefine(
    st: &mut CoreState,
    cb: EventCb,
    change: Redefinition,
) -> Result<(), String> {
    let Redefinition {
        mut next,
        affected,
        schedule_changed,
        check_registry,
    } = change;
    let barrier = Barrier::raise(st, affected)?;
    let saved = next
        .bump_definition_revision(schedule_changed)
        .and_then(|()| match check_registry {
            true => next.validate_with_registry(&st.registry),
            false => Ok(()),
        })
        .and_then(|()| next.save(&st.config_path()));
    if let Err(error) = saved {
        barrier.lower(st);
        return Err(error);
    }
    st.config = next;
    if schedule_changed {
        st.supervisor.invalidate_clock();
    }
    st.supervisor.reconcile(&mut st.config);
    barrier.commit(st)?;
    emit_monitoring_snapshot(cb, st);
    Ok(())
}

/// Change the persisted runtime state through the supervisor. `edit` reports whether anything
/// needs saving.
pub(super) fn change_runtime(
    st: &mut CoreState,
    cb: EventCb,
    affected: Option<HashSet<TargetId>>,
    edit: impl FnOnce(&mut TargetSupervisor, &mut Config) -> Result<bool, String>,
) -> Result<(), String> {
    let barrier = Barrier::raise(st, affected)?;
    let previous = st.config.clone();
    let saved = edit(&mut st.supervisor, &mut st.config).and_then(|persist| match persist {
        true => st.config.save(&st.config_path()),
        false => Ok(()),
    });
    if let Err(error) = saved {
        st.config = previous;
        barrier.lower(st);
        return Err(error);
    }
    st.supervisor.reconcile(&mut st.config);
    barrier.commit(st)?;
    emit_monitoring_snapshot(cb, st);
    Ok(())
}

/// The running session's promise to start no new mutation for the affected targets until the
/// change is committed or abandoned. Without a running session there is nothing to hold back.
struct Barrier {
    raised: bool,
}

impl Barrier {
    fn raise(st: &CoreState, affected: Option<HashSet<TargetId>>) -> Result<Self, String> {
        let raised = match (&st.monitor, affected) {
            (MonitorLifecycle::Running(running), Some(affected)) => {
                running.handle.prepare_definition_change(affected)?;
                true
            }
            _ => false,
        };
        Ok(Self { raised })
    }

    /// The change is in effect: hand the session its new plan.
    fn commit(self, st: &CoreState) -> Result<(), String> {
        match &st.monitor {
            MonitorLifecycle::Running(running) if self.raised => running
                .handle
                .commit_definition_change(monitor_plan(st.supervisor.plan())),
            _ => Ok(()),
        }
    }

    /// The change did not happen: let the session carry on as before.
    fn lower(self, st: &CoreState) {
        if let MonitorLifecycle::Running(running) = &st.monitor {
            if self.raised {
                let _ = running.handle.rollback_definition_change();
            }
        }
    }
}
