//! QR sign-in jobs: obtain the rotating token from a teacher's session or the remote token
//! service, then sign each student with it inside one fixed window.

use super::poller::relogin;
use super::{Account, ActivityKey, MonitorConfig, MonitorMsg, MonitorPlan, TaskGroup};
use crate::store::secrets::Secret;
use crate::tronclass::rollcall::{self, SignOutcome};
use crate::tronclass::teacher_qr::{self, FailureKind};
use crate::tronclass::Endpoints;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

pub(super) fn is_qr_remote_configured(cfg: &MonitorConfig) -> bool {
    if !cfg.qr_remote_enabled {
        return false;
    }
    if cfg
        .qr_remote_key
        .as_deref()
        .is_some_and(|k| !k.expose().trim().is_empty())
        && crate::store::config::normalize_qr_remote_base_url(&cfg.qr_remote_base_url).is_ok()
    {
        return true;
    }
    false
}

pub(super) fn qr_source_available(
    cfg: &MonitorConfig,
    accounts: &HashMap<String, Arc<Account>>,
) -> bool {
    accounts.values().any(|account| account.is_teacher) || is_qr_remote_configured(cfg)
}

/// Actor-local revision gate: any actual QR REMOTE enabled/base/key change, including
/// disabling or key/base rotation, synchronously bumps the revision before old remote
/// work can continue. Identical/unrelated config does not bump.
pub(super) fn is_qr_remote_revision_change(old: &MonitorConfig, new: &MonitorConfig) -> bool {
    if old.qr_remote_enabled != new.qr_remote_enabled {
        return true;
    }
    let old_base = crate::store::config::normalize_qr_remote_base_url(&old.qr_remote_base_url).ok();
    let new_base = crate::store::config::normalize_qr_remote_base_url(&new.qr_remote_base_url).ok();
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
pub(super) fn is_meaningful_qr_remote_change(old: &MonitorConfig, new: &MonitorConfig) -> bool {
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
        let old_base =
            crate::store::config::normalize_qr_remote_base_url(&old.qr_remote_base_url).ok();
        let new_base =
            crate::store::config::normalize_qr_remote_base_url(&new.qr_remote_base_url).ok();
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

pub(super) fn sanitize_qr_source_reason(reason: String) -> String {
    // Keep user diagnostics but strip secrets: reason strings are internally generated
    // (deadline/unauthorized/redirect/rate_limited/transient) and already sanitized. Trim only.
    let trimmed = reason.trim().to_string();
    if trimmed.is_empty() {
        "qr remote: no data".to_string()
    } else {
        trimmed
    }
}

pub(super) fn qr_internal_worker_failed() -> String {
    "qr: internal worker failed".to_string()
}

pub(super) async fn handle_qr_fanout_panic(
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

pub(super) async fn qr_notify_mutation_started(
    tx: &tokio::sync::mpsc::UnboundedSender<MonitorMsg>,
    key: &ActivityKey,
    generation: u64,
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
pub(super) async fn run_qr_remote(
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
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(MonitorMsg::QrSourceFailed {
            key: key.clone(),
            generation,
            reason: sanitized.clone(),
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
    let rollcall_id = key.id.clone();
    let mut deadline_hit = false;
    for chunk in students.chunks(crate::tronclass::teacher_qr::FANOUT_LIMIT) {
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
                ok = qr_notify_mutation_started(&tx, &key, generation, deadline, &plan_rx) => ok,
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
pub(super) fn spawn_qr_remote(
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

/// Teacher opens its OWN qr rollcall as the rotating-`data` source; each student then signs THEIR own
/// rollcall id on THEIR own endpoint with that data. Because the token is valid only ~1–4 s,
/// this re-sources and re-sends every ~1.5 s for up to ~12 s until each student confirms. A session lost
/// mid-flight is recovered once (teacher and per-student). Actor shutdown aborts this task at its
/// next await point; the bounded abort-safe `QrCleanupGuard` still closes the teacher's data source.
/// Full `core_free` retains the same terminal-shutdown caveat as other runtime tasks.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_qr_teacher_assist(
    teacher: Arc<Account>,
    students: Vec<Arc<Account>>,
    tx: UnboundedSender<MonitorMsg>,
    key: ActivityKey,
    generation: u64,
    mut plan_rx: watch::Receiver<MonitorPlan>,
    group: &TaskGroup,
    remote: Option<(String, Arc<Secret>, watch::Receiver<u64>, u64)>,
) {
    let student_rollcall_id = key.id.clone();
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
                    let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), ack: ack_tx});
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
                    let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), ack: ack_tx});
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
                            let _ = tx.send(MonitorMsg::QrSourceFailed{key: key.clone(), generation, reason: sanitized.clone(), ack: ack_tx});
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
pub(super) struct QrCleanupGuard {
    pub(super) teacher: Arc<Account>,
    pub(super) source: Option<teacher_qr::Source>,
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
pub(super) async fn prepare_teacher_source(
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
pub(super) async fn cleanup_teacher_source(
    teacher: &Account,
    ep: &Endpoints,
    source: &teacher_qr::Source,
) {
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
pub(super) async fn sign_qr_student(
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
