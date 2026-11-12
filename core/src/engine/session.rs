//! Account sessions: logging in, importing cookies, and keeping the result.

use super::commands::{Handled, Reply};
use super::monitoring::emit_monitoring_snapshot;
use super::state::CoreState;
use super::{lock_state, reply, spawn_observed, Core, INTERNAL_ERROR};
use crate::login;
use crate::login::LoginOutcome;
use crate::providers::Endpoints;
use crate::secrets::AccountSecret;
use crate::wire::{emit, respond, Event, EventCb, LogLevel, LoginState, WireError};
use cookie_store::CookieStore;
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a login waits for the user to answer a captcha.
pub(super) const CAPTCHA_ANSWER_TIMEOUT: Duration = Duration::from_secs(180);

/// Panic backstop for the Login task: drop any pending captcha entry and the single-flight marker,
/// then complete the command with the fixed INTERNAL_ERROR (the payload is never echoed). The
/// flight guard already cleared the marker on unwind; this also removes the stale captcha sender so
/// a later SubmitCaptcha reports no pending challenge.
pub(super) fn recover_login_panic(
    state: &Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    id: u64,
    account_id: &str,
) {
    {
        let mut guard = lock_state(state);
        if let Some(st) = guard.as_mut() {
            st.pending_captcha.remove(account_id);
        }
    }
    publish_login_state(
        state,
        cb,
        account_id,
        LoginState::Error,
        Some(WireError {
            code: "internal_error".to_string(),
            message: INTERNAL_ERROR.to_string(),
        }),
        true,
    );
    respond(cb, id, Event::login_failed(INTERNAL_ERROR));
}

/// Login: snapshot (base_url, username, password, cached cookies) under the lock, release it, do
/// the async round-trip, then re-lock to persist refreshed cookies. Reuses a cached session if it
/// still verifies, so we don't re-login unnecessarily.
///
/// Lifecycle contract: each progress/error transition publishes the full MonitoringSnapshot with
/// the account's `login_state`; there is no secondary account/global authority. One Login per
/// account at a time (`login_in_flight`), released on every terminal path. Delete mid-flight
/// cancels the pending captcha, and persistence re-checks under the write lock that the account
/// still exists, so stale completion cannot resurrect a deleted secret.
pub(super) fn spawn_login(core: &Core, id: u64, account_id: String) {
    let cb = core.cb;
    let state = core.state.clone();

    // Snapshot under the lock (no await while holding it).
    let snap = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        if st.supervisor.plan().routes.iter().any(|route| {
            route.detector_account_id == account_id
                || route.participant_account_ids.contains(&account_id)
        }) {
            return reply(cb, id, false, Some("account_is_in_use".into()));
        }
        let Some(vault) = st.vault.as_ref() else {
            return reply(cb, id, false, Some("vault is locked".into()));
        };
        let Some(acc) = st.config.account(&account_id) else {
            return reply(cb, id, false, Some("no such account".into()));
        };
        let Some(base_url) = st.registry.resolve(&acc.school_ref) else {
            return reply(
                cb,
                id,
                false,
                Some(format!("unknown school: {}", acc.school_ref)),
            );
        };
        warn_insecure_http(cb, &base_url);
        // Single-flight: the marker is set here and removed by the task itself on every exit path.
        if !st.login_in_flight.insert(account_id.clone()) {
            return reply(
                cb,
                id,
                false,
                Some("login already in progress for this account".into()),
            );
        }
        let secret = vault.get(&account_id).unwrap_or_default();
        let (password, cookies) = secret.into_parts();
        let username = acc.username.clone();
        (base_url, username, password, cookies)
    };
    let (base_url, username, password, cached_cookies) = snap;

    // Snapshot the per-core captcha window before spawning (production is a fixed 180 s; only the
    // timeout e2e shortens its own core's field, and a mid-flight change must not apply mid-login).
    #[cfg(test)]
    let captcha_answer_timeout = core.captcha_answer_timeout;
    #[cfg(not(test))]
    let captcha_answer_timeout = CAPTCHA_ANSWER_TIMEOUT;

    let on_panic = {
        let state = state.clone();
        let account_id = account_id.clone();
        move || recover_login_panic(&state, cb, id, &account_id)
    };
    spawn_observed(&core.rt, on_panic, async move {
        // The flight guard clears the single-flight marker on EVERY exit, including panic unwind;
        // the explicit clears below keep the terminal paths obvious. The JoinSet observer turns a
        // panic into the fixed INTERNAL_ERROR backstop instead of killing the runtime.
        let _flight_guard = LoginFlightGuard::new(state.clone(), account_id.clone());
        publish_login_state(&state, cb, &account_id, LoginState::LoggingIn, None, false);
        emit(
            cb,
            Event::log(LogLevel::Info, format!("login → {base_url}")),
        ); // base_url only, never creds

        let endpoints = Endpoints::derive(&base_url);
        let (client, jar) = match build_client(&cached_cookies) {
            Ok(pair) => pair,
            Err(error) => {
                publish_login_state(
                    &state,
                    cb,
                    &account_id,
                    LoginState::Error,
                    Some(WireError {
                        code: "login_failed".to_string(),
                        message: error.clone(),
                    }),
                    true,
                );
                respond(cb, id, Event::login_failed(error));
                return;
            }
        };

        // Restore path: a cached session that still verifies skips the password login entirely.
        let result: Result<bool, String> =
            if !cached_cookies.is_empty() && login::verify_session(&client, &endpoints).await {
                Ok(true)
            } else {
                match login::login(&client, &endpoints, &username, &password).await {
                    LoginOutcome::Ok => Ok(false),
                    LoginOutcome::Failed(e) => Err(e),
                    LoginOutcome::NeedCaptcha {
                        image_bytes,
                        pending,
                    } => {
                        // Register a one-shot for the answer, show the challenge (image is not a secret),
                        // and await a SubmitCaptcha command. Credentials stay inside `pending`, never emitted.
                        let (txc, rxc) = oneshot::channel::<String>();
                        {
                            let mut guard = lock_state(&state);
                            if let Some(st) = guard.as_mut() {
                                st.pending_captcha.insert(account_id.clone(), txc);
                            }
                        }
                        emit(
                            cb,
                            Event::CaptchaChallenge {
                                account_id: &account_id,
                                image_b64: login::encode_base64(&image_bytes),
                            },
                        );
                        match tokio::time::timeout(captcha_answer_timeout, rxc).await {
                            Ok(Ok(text)) => {
                                login::complete_captcha(&client, &endpoints, pending, &text)
                                    .await
                                    .map(|_| false)
                            }
                            // DeleteAccount dropped the pending sender: the challenge is withdrawn.
                            Ok(Err(_)) => {
                                Err("login cancelled because the account was deleted".to_string())
                            }
                            _ => {
                                // timeout → drop the stale pending entry
                                {
                                    let mut guard = lock_state(&state);
                                    if let Some(st) = guard.as_mut() {
                                        st.pending_captcha.remove(&account_id);
                                    }
                                }
                                Err("captcha timed out".to_string())
                            }
                        }
                    }
                }
            };

        match result {
            Ok(from_cache) => {
                let cookies = dump_cookies(&jar);
                // Re-lock to cache the refreshed cookies — but only while the account still exists
                // (the existence check and the vault write happen under the same lock). A Delete
                // committed mid-flight must not resurrect the deleted account's secret.
                let outcome =
                    persist_login_session(&state, &account_id, AccountSecret { password, cookies });
                match outcome {
                    Ok(PersistOutcome::Saved) => {
                        publish_login_state(
                            &state,
                            cb,
                            &account_id,
                            LoginState::Online,
                            None,
                            true,
                        );
                        respond(
                            cb,
                            id,
                            Event::logged_in(if from_cache {
                                "session restored from cache"
                            } else {
                                "logged in"
                            }),
                        );
                    }
                    Ok(PersistOutcome::AccountGone) => {
                        publish_login_state(
                            &state,
                            cb,
                            &account_id,
                            LoginState::Error,
                            Some(WireError {
                                code: "account_deleted".to_string(),
                                message: "account was deleted during login".to_string(),
                            }),
                            true,
                        );
                        respond(
                            cb,
                            id,
                            Event::login_failed(
                                "login succeeded but the account was deleted during login",
                            ),
                        );
                    }
                    Err(error) => {
                        publish_login_state(
                            &state,
                            cb,
                            &account_id,
                            LoginState::Error,
                            Some(WireError {
                                code: "session_persistence_failed".to_string(),
                                message: error.clone(),
                            }),
                            true,
                        );
                        respond(
                            cb,
                            id,
                            Event::login_failed(format!(
                                "login succeeded but session persistence failed: {error}"
                            )),
                        );
                    }
                }
            }
            Err(e) => {
                // One report only: the correlated LoginResult(ok:false) already surfaces `reason` as a
                // toast+log via AppState.Send's error path; a second id:null Error would double it.
                publish_login_state(
                    &state,
                    cb,
                    &account_id,
                    LoginState::Error,
                    Some(WireError {
                        code: "login_failed".to_string(),
                        message: e.clone(),
                    }),
                    true,
                );
                respond(cb, id, Event::login_failed(e));
            }
        }
    });
}

/// Build an authenticated client: reuse a cached session if it still verifies, else log in.
/// Returns the client + refreshed cookie JSON to cache.
pub(super) async fn authed_client(
    base_url: &str,
    username: &str,
    password: &str,
    cached: &str,
) -> Result<(Client, String), String> {
    let endpoints = Endpoints::derive(base_url);
    let (client, jar) = build_client(cached).map_err(|error| format!("client: {error}"))?;
    if cached.is_empty() || !login::verify_session(&client, &endpoints).await {
        match login::login(&client, &endpoints, username, password).await {
            LoginOutcome::Ok => {}
            LoginOutcome::Failed(e) => return Err(e),
            // A captcha needs a human; automatic target activation cannot prompt. An interactive
            // Login caches the session before the supervisor retries the target.
            LoginOutcome::NeedCaptcha { .. } => {
                return Err("需要圖形驗證碼，請先用 Login 登入一次".into())
            }
        }
    }
    Ok((client, dump_cookies(&jar)))
}

/// Import supplied browser cookies, verify them, then persist them for one account.
pub(super) fn spawn_import_cookies(core: &Core, id: u64, account_id: String, cookies_json: String) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        if st.supervisor.plan().routes.iter().any(|route| {
            route.detector_account_id == account_id
                || route.participant_account_ids.contains(&account_id)
        }) {
            return reply(cb, id, false, Some("account_is_in_use".into()));
        }
        let Some(vault) = st.vault.as_ref() else {
            return reply(cb, id, false, Some("vault is locked".into()));
        };
        let Some(acc) = st.config.account(&account_id) else {
            return reply(cb, id, false, Some("no such account".into()));
        };
        let Some(base_url) = st.registry.resolve(&acc.school_ref) else {
            return reply(
                cb,
                id,
                false,
                Some(format!("unknown school: {}", acc.school_ref)),
            );
        };
        warn_insecure_http(cb, &base_url);
        let (password, _) = vault.get(&account_id).unwrap_or_default().into_parts();
        if !st.login_in_flight.insert(account_id.clone()) {
            return reply(
                cb,
                id,
                false,
                Some("login already in progress for this account".into()),
            );
        }
        (base_url, password)
    };
    let (base_url, password) = snap;

    let on_panic = {
        let state = state.clone();
        let account_id = account_id.clone();
        move || {
            publish_login_state(
                &state,
                cb,
                &account_id,
                LoginState::Error,
                Some(WireError {
                    code: "internal_error".to_string(),
                    message: INTERNAL_ERROR.to_string(),
                }),
                true,
            );
            reply(cb, id, false, Some(INTERNAL_ERROR.to_string()));
        }
    };
    spawn_observed(&core.rt, on_panic, async move {
        let _flight_guard = LoginFlightGuard::new(state.clone(), account_id.clone());
        publish_login_state(&state, cb, &account_id, LoginState::LoggingIn, None, false);

        let endpoints = Endpoints::derive(&base_url);
        let result = match build_client(&cookies_json) {
            Ok((client, _jar)) if login::verify_session(&client, &endpoints).await => {
                match persist_login_session(
                    &state,
                    &account_id,
                    AccountSecret {
                        password,
                        cookies: cookies_json,
                    },
                ) {
                    Ok(PersistOutcome::Saved) => Ok(()),
                    Ok(PersistOutcome::AccountGone) => Err((
                        "account_deleted",
                        "account was deleted during import".to_string(),
                    )),
                    Err(error) => Err((
                        "session_persistence_failed",
                        format!("cookies verified but persistence failed: {error}"),
                    )),
                }
            }
            Ok(_) => Err((
                "login_failed",
                "imported cookies did not verify".to_string(),
            )),
            Err(error) => Err(("login_failed", error)),
        };

        match result {
            Ok(()) => {
                publish_login_state(&state, cb, &account_id, LoginState::Online, None, true);
                reply(cb, id, true, None);
            }
            Err((code, error)) => {
                publish_login_state(
                    &state,
                    cb,
                    &account_id,
                    LoginState::Error,
                    Some(WireError {
                        code: code.to_string(),
                        message: error.clone(),
                    }),
                    true,
                );
                reply(cb, id, false, Some(error));
            }
        }
    });
}

/// Re-lock and cache a refreshed session — but only while the account still exists in config. The
/// existence check and the vault write happen under the SAME lock, so a Delete committed while the
/// network round-trip was in flight cannot race past it; on `AccountGone` nothing is written and a
/// deleted account is never resurrected by a stale async completion.
pub(super) fn persist_login_session(
    state: &Arc<Mutex<Option<CoreState>>>,
    account_id: &str,
    secret: AccountSecret,
) -> Result<PersistOutcome, String> {
    let mut guard = lock_state(state);
    let Some(st) = guard.as_mut() else {
        return Err("core not initialized".to_string());
    };
    if st.config.account(account_id).is_none() {
        return Ok(PersistOutcome::AccountGone);
    }
    let Some(vault) = st.vault.as_mut() else {
        return Err("vault is locked".to_string());
    };
    vault.set(account_id, secret)?;
    Ok(PersistOutcome::Saved)
}

pub(super) enum PersistOutcome {
    /// The refreshed session was written to the vault.
    Saved,
    /// The account was deleted while the round-trip was in flight: nothing was written.
    AccountGone,
}

pub(super) struct LoginFlightGuard {
    pub(super) state: Arc<Mutex<Option<CoreState>>>,
    pub(super) account_id: String,
}

impl LoginFlightGuard {
    pub(super) fn new(state: Arc<Mutex<Option<CoreState>>>, account_id: String) -> Self {
        Self { state, account_id }
    }
}

impl Drop for LoginFlightGuard {
    fn drop(&mut self) {
        clear_login_in_flight(&self.state, &self.account_id);
    }
}

pub(super) fn publish_login_state(
    state: &Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    account_id: &str,
    login_state: LoginState,
    error: Option<WireError>,
    terminal: bool,
) {
    let mut guard = lock_state(state);
    if let Some(st) = guard.as_mut() {
        if terminal {
            st.login_in_flight.remove(account_id);
        }
        st.supervisor
            .set_login_state(account_id.to_string(), login_state, error);
        st.supervisor.reconcile(&mut st.config);
        emit_monitoring_snapshot(cb, st);
    }
}

/// Remove the same-account login single-flight marker. Called exactly once per login task on every
/// terminal path (success, failure, captcha timeout, captcha cancelled).
pub(super) fn clear_login_in_flight(state: &Arc<Mutex<Option<CoreState>>>, account_id: &str) {
    let mut guard = lock_state(state);
    if let Some(st) = guard.as_mut() {
        st.login_in_flight.remove(account_id);
    }
}

/// Advisory-only warning for a plain-HTTP school endpoint (never blocks — test/intranet deployments
/// legitimately run on http). `base_url` is already resolved by the caller.
pub(super) fn warn_insecure_http(cb: EventCb, base_url: &str) {
    if base_url.starts_with("http://") {
        emit(
            cb,
            Event::log(LogLevel::Warn, "學校網址使用未加密 HTTP，建議改用 HTTPS"),
        );
    }
}

/// Build the school (account) client: cookie jar + connect timeout + a per-read idle timeout
/// (an accepted-but-silent tenant request must not hold the flow forever, while a progressing large
/// bounded download is not killed by a short total deadline) + a redirect policy that refuses
/// CROSS-ORIGIN redirects to literal private/link-local/loopback hosts. Same-origin redirects —
/// including explicitly configured intranet schools — keep working. This is a bounded best-effort
/// guard, not DNS-rebinding protection; attachment/image entry points validate their initial URL too.
pub(crate) fn build_client(cookies_json: &str) -> Result<(Client, Arc<CookieStoreMutex>), String> {
    let store = if cookies_json.is_empty() {
        CookieStore::default()
    } else {
        cookie_store::serde::json::load_all(std::io::Cursor::new(cookies_json.as_bytes()))
            .unwrap_or_default()
    };
    let jar = Arc::new(CookieStoreMutex::new(store));
    let client = crate::http::client_builder()
        .cookie_provider(jar.clone())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if crate::http::is_private_host(attempt.url().host_str().unwrap_or(""))
                && !crate::http::same_origin(
                    attempt.previous().last().unwrap_or_else(|| attempt.url()),
                    attempt.url(),
                )
            {
                return attempt.error("refusing redirect to a literal private address");
            }
            attempt.follow()
        }))
        .build()
        .map_err(|error| format!("client: {error}"))?;
    Ok((client, jar))
}

pub(super) fn dump_cookies(jar: &CookieStoreMutex) -> String {
    let store = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut buf = Vec::new();
    // Include session (non-persistent) cookies — the TronClass session cookie is one.
    let _ = cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf);
    String::from_utf8(buf).unwrap_or_default()
}

/// Hand the user's captcha answer to the login that is waiting for it.
pub(super) fn submit_captcha(st: &mut CoreState, account_id: &str, text: String) -> Handled {
    let waiting = st
        .pending_captcha
        .remove(account_id)
        .ok_or("no captcha pending for this account")?;
    // A login that was cancelled or failed meanwhile has withdrawn its challenge.
    waiting
        .send(text)
        .map_err(|_| "captcha challenge already withdrawn")?;
    Ok(Reply::Done)
}
