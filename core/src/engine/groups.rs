//! Account groups.

use super::commands::{handle_sync_state, Handled, Reply};
use super::course_query::{begin_course_query, course_failures_json, query_common_courses};
use super::definitions::{check_revision, redefine, Redefinition};
use super::monitoring::reconcile_monitor_with;
use super::state::CoreState;
use super::{reply_data, Core, FollowUp};
use crate::config::{canonical_tenant, new_group_id, AccountGroup, DetectorSelection, TargetId};
use crate::wire::{Command, EventCb, GroupInput};
use serde_json::json;
use std::collections::HashSet;

pub(super) fn create(
    st: &mut CoreState,
    cb: EventCb,
    expected_revision: u64,
    input: GroupInput,
) -> Handled {
    check_revision(st, expected_revision)?;
    let group = group_definition(st, new_group_id(), input, true)?;
    let group_id = group.id.clone();
    let mut next = st.config.clone();
    next.groups.push(group);
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: None,
            schedule_changed: true,
            check_registry: true,
        },
    )?;
    Ok(Reply::Data(json!({ "group_id": group_id })))
}

pub(super) fn update(
    st: &mut CoreState,
    cb: EventCb,
    group_id: String,
    expected_revision: u64,
    input: GroupInput,
) -> Handled {
    check_revision(st, expected_revision)?;
    let mut next = st.config.clone();
    let Some(group) = next.groups.iter_mut().find(|group| group.id == group_id) else {
        return Err("unknown_group".into());
    };
    let definition = group_definition(st, group_id.clone(), input, false)?;
    let schedule_changed = group.schedule != definition.schedule;
    *group = definition;
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: Some(HashSet::from([TargetId::group(group_id)])),
            schedule_changed,
            check_registry: true,
        },
    )?;
    Ok(Reply::Done)
}

pub(super) fn delete(
    st: &mut CoreState,
    cb: EventCb,
    group_id: &str,
    expected_revision: u64,
) -> Handled {
    check_revision(st, expected_revision)?;
    if st.config.group(group_id).is_none() {
        return Err("unknown_group".into());
    }
    let mut next = st.config.clone();
    next.groups.retain(|group| group.id != group_id);
    next.runtime.group_rotation.remove(group_id);
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: Some(HashSet::from([TargetId::group(group_id)])),
            schedule_changed: true,
            check_registry: false,
        },
    )?;
    Ok(Reply::Done)
}

/// Replace several groups with one.
pub(super) fn merge(
    st: &mut CoreState,
    cb: EventCb,
    mut group_ids: Vec<String>,
    expected_revision: u64,
    input: GroupInput,
) -> Handled {
    check_revision(st, expected_revision)?;
    group_ids.sort();
    group_ids.dedup();
    if group_ids.len() < 2 || group_ids.iter().any(|id| st.config.group(id).is_none()) {
        return Err("merge_requires_existing_groups".into());
    }
    let merged = group_definition(st, new_group_id(), input, true)?;
    let merged_id = merged.id.clone();
    let mut next = st.config.clone();
    next.groups.retain(|group| !group_ids.contains(&group.id));
    for group_id in &group_ids {
        next.runtime.group_rotation.remove(group_id);
    }
    next.groups.push(merged);
    redefine(
        st,
        cb,
        Redefinition {
            next,
            affected: Some(group_ids.into_iter().map(TargetId::group).collect()),
            schedule_changed: true,
            check_registry: true,
        },
    )?;
    Ok(Reply::Data(json!({ "group_id": merged_id })))
}

pub(super) fn group_definition(
    st: &CoreState,
    id: String,
    input: GroupInput,
    creating: bool,
) -> Result<AccountGroup, String> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err("group_name_empty".to_string());
    }
    let minimum = if creating { 2 } else { 1 };
    if input.member_account_ids.len() < minimum {
        return Err(if creating {
            "group_requires_two_students".to_string()
        } else {
            "group_requires_one_student".to_string()
        });
    }
    let mut unique_members = HashSet::new();
    if input
        .member_account_ids
        .iter()
        .any(|account_id| !unique_members.insert(account_id.as_str()))
    {
        return Err("group_members_duplicate".to_string());
    }
    let mut tenant = None::<String>;
    for account_id in &input.member_account_ids {
        let account = st
            .config
            .account(account_id)
            .ok_or_else(|| format!("unknown_group_member:{account_id}"))?;
        if account.is_teacher {
            return Err("teacher_cannot_join_group".to_string());
        }
        let canonical = canonical_tenant(&st.registry, &account.school_ref)?;
        match &tenant {
            Some(expected) if expected != &canonical => {
                return Err("group_members_must_share_tenant".to_string())
            }
            None => tenant = Some(canonical),
            _ => {}
        }
    }
    if let DetectorSelection::Preferred { account_id } = &input.detector {
        if !input.member_account_ids.contains(account_id) {
            return Err("preferred_detector_must_be_member".to_string());
        }
    }
    let mut unique_courses = HashSet::new();
    if input
        .course_ids
        .iter()
        .any(|course_id| course_id.trim().is_empty() || !unique_courses.insert(course_id.as_str()))
    {
        return Err("group_courses_invalid".to_string());
    }
    input.schedule.validate()?;
    Ok(AccountGroup {
        id,
        name,
        tenant: tenant.expect("group has at least one member"),
        member_account_ids: input.member_account_ids,
        course_ids: input.course_ids,
        detector: input.detector,
        schedule: input.schedule,
    })
}

pub(super) fn spawn_validated_group_command(core: &Core, command: Command) {
    let cb = core.cb;
    let state = core.state.clone();
    let runtime = core.rt.handle().clone();
    let (id, group) = match &command {
        Command::CreateGroup { id, group, .. }
        | Command::UpdateGroup { id, group, .. }
        | Command::MergeGroups { id, group, .. } => (*id, group),
        _ => unreachable!("only a group bound to courses is validated"),
    };
    let (members, requested) = (group.member_account_ids.clone(), group.course_ids.clone());
    let cancel_removed_pending = FollowUp::of(&command).reconcile().unwrap_or(false);
    let inputs = match begin_course_query(&state, cb, &members) {
        Ok(inputs) => inputs,
        Err(failures) => {
            return reply_data(
                cb,
                id,
                false,
                Some("無法驗證所有成員課程".to_string()),
                json!({ "courses": [], "account_errors": course_failures_json(&failures) }),
            );
        }
    };
    core.rt.spawn(async move {
        match query_common_courses(state.clone(), cb, inputs).await {
            Ok(courses) => {
                let common: HashSet<&str> = courses
                    .iter()
                    .map(|course| course.course_id.as_str())
                    .collect();
                if requested
                    .iter()
                    .any(|course_id| !common.contains(course_id.as_str()))
                {
                    return reply_data(
                        cb,
                        id,
                        false,
                        Some("group_courses_not_common".to_string()),
                        json!({ "courses": courses, "account_errors": [] }),
                    );
                }
                handle_sync_state(&state, cb, command);
                reconcile_monitor_with(runtime, state, cb, cancel_removed_pending);
            }
            Err(failures) => reply_data(
                cb,
                id,
                false,
                Some("無法驗證所有成員課程".to_string()),
                json!({ "courses": [], "account_errors": course_failures_json(&failures) }),
            ),
        }
    });
}
