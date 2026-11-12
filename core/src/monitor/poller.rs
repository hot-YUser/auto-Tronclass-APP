//! One poller per student account: watch the account's rollcalls and quizzes, and tell the actor
//! about each one once.

use super::detect::QuizScanner;
use super::{Account, Detected, MonitorMsg, MonitorPlan};
use crate::http::{self, MAX_API_JSON};
use crate::json;
use crate::tronclass::login::{self, LoginOutcome};
use crate::tronclass::rollcall;
use crate::tronclass::Endpoints;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

/// How fast to poll while a rollcall is open: its countdown is seconds long.
const ACTIVE_INTERVAL: Duration = Duration::from_secs(1);

/// The settings a poller follows live.
#[derive(Clone)]
pub(super) struct PollTuning {
    pub idle: Duration,
    pub quiz_detect: Duration,
    /// The quiz families to detect; empty means all of them.
    pub wanted_types: Vec<String>,
}

/// Poll until the session ends. A poller whose account detects for no route sleeps until the plan
/// changes.
pub(super) async fn poll(
    account: Arc<Account>,
    tx: UnboundedSender<MonitorMsg>,
    mut tune_rx: watch::Receiver<PollTuning>,
    mut plan_rx: watch::Receiver<MonitorPlan>,
) {
    let endpoints = Endpoints::derive(&account.base_url);
    let mut tune = tune_rx.borrow_and_update().clone();
    let mut seen = HashSet::new();
    let mut quizzes = QuizScanner::default();
    let mut generation = None;

    while !tx.is_closed() {
        let (current, detecting) = {
            let plan = plan_rx.borrow_and_update();
            let detecting = plan
                .routes
                .iter()
                .any(|route| route.detector_account_id == account.id);
            (plan.generation, detecting)
        };
        // A new plan may route the same activity to different participants: report it again.
        if generation != Some(current) {
            generation = Some(current);
            seen.clear();
            quizzes.forget();
        }

        if !detecting {
            if !settings_changed(&mut plan_rx, &mut tune_rx, &mut tune).await {
                break;
            }
            continue;
        }

        let pause = match fetch_classified(&account.client, &endpoints.rollcalls()).await {
            Fetched::Ok(listing) => {
                let open = json::array_at(&listing, &["rollcalls"]);
                for found in open {
                    report(&account, &tx, &mut seen, current, found);
                }
                if open.is_empty() {
                    tune.idle
                } else {
                    ACTIVE_INTERVAL
                }
            }
            Fetched::AuthLost => {
                let account_id = account.id.clone();
                tx.send(MonitorMsg::AuthLost { account_id }).ok();
                tune.idle
            }
            Fetched::Down => tune.idle,
        };
        if quizzes.due(tune.quiz_detect) {
            quizzes
                .scan(&account, &endpoints, &tx, &tune.wanted_types, current)
                .await;
        }
        if tx.is_closed() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(pause) => {}
            alive = settings_changed(&mut plan_rx, &mut tune_rx, &mut tune) => if !alive { break },
        }
    }
}

/// Wait for a new plan or new tuning, adopting the tuning. False once the session is gone.
async fn settings_changed(
    plan_rx: &mut watch::Receiver<MonitorPlan>,
    tune_rx: &mut watch::Receiver<PollTuning>,
    tune: &mut PollTuning,
) -> bool {
    tokio::select! {
        changed = plan_rx.changed() => changed.is_ok(),
        changed = tune_rx.changed() => {
            if changed.is_ok() {
                *tune = tune_rx.borrow_and_update().clone();
            }
            changed.is_ok()
        }
    }
}

fn report(
    account: &Account,
    tx: &UnboundedSender<MonitorMsg>,
    seen: &mut HashSet<String>,
    generation: u64,
    found: &Value,
) {
    let Some(rollcall_id) = json::id_at(found, &["rollcall_id", "id"]) else {
        return;
    };
    if !seen.insert(rollcall_id.clone()) {
        return;
    }
    tx.send(MonitorMsg::Detected(Detected {
        generation,
        account_id: account.id.clone(),
        base_url: account.base_url.clone(),
        rollcall_id,
        kind: rollcall::classify(found),
        course: json::text_at(found, &["course_name", "course"]).to_string(),
        course_id: json::id_at(found, &["course_id", "courseId", "cid"])
            .or_else(|| found.get("course").and_then(course_id)),
    }))
    .ok();
}

pub(super) fn course_id(course: &Value) -> Option<String> {
    json::id_at(course, &["id", "course_id", "courseId"])
}

enum Fetched {
    Ok(Value),
    AuthLost,
    Down,
}

/// GET a listing and tell a dead session from a dead server. The session is lost on a 401, on a
/// redirect to a login page, and on a 2xx whose body is not JSON: that is the login page itself.
async fn fetch_classified(client: &Client, url: &str) -> Fetched {
    let Ok(response) = client.get(url).send().await else {
        return Fetched::Down;
    };
    if response.status().as_u16() == 401 {
        return Fetched::AuthLost;
    }
    if !response.status().is_success() {
        return Fetched::Down;
    }
    if http::response_url_is_login(response.url()) {
        return Fetched::AuthLost;
    }
    let Ok(body) = http::read_bounded(response, MAX_API_JSON, "monitor poll").await else {
        return Fetched::Down;
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(listing) if listing.is_object() || listing.is_array() => Fetched::Ok(listing),
        _ => Fetched::AuthLost,
    }
}

/// Log the account in again on its own client, whose cookie jar then carries the new session.
/// A school that demands a captcha cannot be driven unattended and counts as a failure.
pub(super) async fn relogin(account: &Account) -> bool {
    let endpoints = Endpoints::derive(&account.base_url);
    matches!(
        login::login(
            &account.client,
            &endpoints,
            &account.username,
            account.password.expose()
        )
        .await,
        LoginOutcome::Ok
    )
}
