//! The courses a set of accounts have in common, for choosing a group's courses.

use super::monitoring::{emit_monitoring_snapshot, MonitorLoginInput};
use super::session::authed_client;
use super::state::CoreState;
use super::{lock_state, reply_data, Core, INTERNAL_ERROR};
use crate::config::canonical_tenant;
use crate::wire::{CourseSnapshot, EventCb, LoginState, WireError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;

#[derive(Clone)]
pub(super) struct CourseQueryFailure {
    pub(super) account_id: String,
    pub(super) code: String,
    pub(super) message: String,
}

pub(super) fn begin_course_query(
    state: &Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    member_account_ids: &[String],
) -> Result<Vec<MonitorLoginInput>, Vec<CourseQueryFailure>> {
    let mut failures = Vec::new();
    let mut seen = HashSet::new();
    if member_account_ids.is_empty()
        || member_account_ids
            .iter()
            .any(|account_id| !seen.insert(account_id.as_str()))
    {
        return Err(vec![CourseQueryFailure {
            account_id: String::new(),
            code: "invalid_members".to_string(),
            message: "成員不得為空或重複".to_string(),
        }]);
    }
    let mut guard = lock_state(state);
    let Some(st) = guard.as_mut() else {
        return Err(vec![CourseQueryFailure {
            account_id: String::new(),
            code: "not_initialized".to_string(),
            message: "核心尚未初始化".to_string(),
        }]);
    };
    let Some(vault) = st.vault.as_ref() else {
        return Err(member_account_ids
            .iter()
            .map(|account_id| CourseQueryFailure {
                account_id: account_id.clone(),
                code: "vault_locked".to_string(),
                message: "保險庫尚未解鎖".to_string(),
            })
            .collect());
    };
    let mut tenant = None::<String>;
    let mut inputs = Vec::new();
    for account_id in member_account_ids {
        let Some(account) = st.config.account(account_id) else {
            failures.push(CourseQueryFailure {
                account_id: account_id.clone(),
                code: "unknown_account".to_string(),
                message: "找不到帳號".to_string(),
            });
            continue;
        };
        if account.is_teacher {
            failures.push(CourseQueryFailure {
                account_id: account_id.clone(),
                code: "teacher_not_allowed".to_string(),
                message: "教師帳號不能加入群組".to_string(),
            });
            continue;
        }
        if st.login_in_flight.contains(account_id) {
            failures.push(CourseQueryFailure {
                account_id: account_id.clone(),
                code: "login_in_flight".to_string(),
                message: "帳號正在驗證中".to_string(),
            });
            continue;
        }
        let canonical = match canonical_tenant(&st.registry, &account.school_ref) {
            Ok(canonical) => canonical,
            Err(message) => {
                failures.push(CourseQueryFailure {
                    account_id: account_id.clone(),
                    code: "tenant_invalid".to_string(),
                    message,
                });
                continue;
            }
        };
        if tenant
            .as_ref()
            .is_some_and(|expected| expected != &canonical)
        {
            failures.push(CourseQueryFailure {
                account_id: account_id.clone(),
                code: "tenant_mismatch".to_string(),
                message: "群組成員必須屬於同一租戶".to_string(),
            });
            continue;
        }
        tenant.get_or_insert(canonical);
        let Some(base_url) = st.registry.resolve(&account.school_ref) else {
            failures.push(CourseQueryFailure {
                account_id: account_id.clone(),
                code: "tenant_invalid".to_string(),
                message: "無法解析學校網址".to_string(),
            });
            continue;
        };
        let (password, cookies) = vault.get(account_id).unwrap_or_default().into_parts();
        inputs.push(MonitorLoginInput {
            meta: account.clone(),
            base_url,
            password,
            cookies,
        });
    }
    if !failures.is_empty() {
        return Err(failures);
    }
    st.login_in_flight
        .extend(member_account_ids.iter().cloned());
    emit_monitoring_snapshot(cb, st);
    Ok(inputs)
}

pub(super) async fn query_common_courses(
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    inputs: Vec<MonitorLoginInput>,
) -> Result<Vec<CourseSnapshot>, Vec<CourseQueryFailure>> {
    let mut tasks = JoinSet::new();
    let mut task_accounts = HashMap::new();
    for input in inputs {
        let account_id = input.meta.id.clone();
        let abort = tasks.spawn(async move {
            let result = match authed_client(
                &input.base_url,
                &input.meta.username,
                &input.password,
                &input.cookies,
            )
            .await
            {
                Ok((client, refreshed_cookies)) => crate::courses::list(&client, &input.base_url)
                    .await
                    .map(|courses| (refreshed_cookies, courses)),
                Err(error) => Err(error),
            };
            (input.meta.id, result)
        });
        task_accounts.insert(abort.id(), account_id);
    }

    let mut successes = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((task_id, (account_id, Ok((cookies, courses))))) => {
                task_accounts.remove(&task_id);
                successes.push((account_id, cookies, courses));
            }
            Ok((task_id, (account_id, Err(message)))) => {
                task_accounts.remove(&task_id);
                failures.push(CourseQueryFailure {
                    account_id,
                    code: "course_query_failed".to_string(),
                    message,
                });
            }
            Err(error) => failures.push(CourseQueryFailure {
                account_id: task_accounts
                    .remove(&error.id())
                    .unwrap_or_else(|| "unknown".to_string()),
                code: "internal_error".to_string(),
                message: INTERNAL_ERROR.to_string(),
            }),
        }
    }

    {
        let mut guard = lock_state(&state);
        if let Some(st) = guard.as_mut() {
            let completed: HashSet<&str> = successes
                .iter()
                .map(|(account_id, _, _)| account_id.as_str())
                .chain(failures.iter().map(|failure| failure.account_id.as_str()))
                .collect();
            st.login_in_flight
                .retain(|account_id| !completed.contains(account_id.as_str()));
            for (account_id, cookies, courses) in &successes {
                if let Some(vault) = st.vault.as_mut() {
                    if let Some(mut secret) = vault.get(account_id) {
                        secret.cookies = cookies.clone();
                        if let Err(message) = vault.set(account_id, secret) {
                            failures.push(CourseQueryFailure {
                                account_id: account_id.clone(),
                                code: "session_persistence_failed".to_string(),
                                message,
                            });
                            continue;
                        }
                    }
                }
                st.supervisor
                    .set_login_state(account_id.clone(), LoginState::Online, None);
                st.supervisor
                    .set_account_courses(account_id.clone(), courses.clone());
            }
            for failure in &failures {
                st.supervisor.set_login_state(
                    failure.account_id.clone(),
                    LoginState::Error,
                    Some(WireError {
                        code: failure.code.clone(),
                        message: failure.message.clone(),
                    }),
                );
            }
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
        }
    }
    if !failures.is_empty() {
        return Err(failures);
    }
    let mut all = successes.into_iter();
    let Some((_first_id, _cookies, first)) = all.next() else {
        return Ok(Vec::new());
    };
    let remaining: Vec<HashSet<String>> = all
        .map(|(_, _, courses)| courses.into_iter().map(|course| course.course_id).collect())
        .collect();
    Ok(first
        .into_iter()
        .filter(|course| remaining.iter().all(|ids| ids.contains(&course.course_id)))
        .collect())
}

pub(super) fn course_failures_json(failures: &[CourseQueryFailure]) -> Value {
    Value::Array(
        failures
            .iter()
            .map(|failure| {
                json!({
                    "account_id": failure.account_id,
                    "code": failure.code,
                    "message": failure.message,
                })
            })
            .collect(),
    )
}

pub(super) fn spawn_list_common_courses(core: &Core, id: u64, member_account_ids: Vec<String>) {
    let cb = core.cb;
    let state = core.state.clone();
    let inputs = match begin_course_query(&state, cb, &member_account_ids) {
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
        match query_common_courses(state, cb, inputs).await {
            Ok(courses) => reply_data(
                cb,
                id,
                true,
                None,
                json!({ "courses": courses, "account_errors": [] }),
            ),
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
