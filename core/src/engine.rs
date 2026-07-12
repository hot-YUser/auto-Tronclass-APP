//! The core state machine: owns the tokio runtime, the event callback, a long-lived heartbeat,
//! and (after `Init`) the registry / config / vault behind one mutex. Commands lock, mutate,
//! persist, and emit; the one async command (Login) snapshots what it needs, drops the lock,
//! does the network round-trip, then re-locks to cache cookies. Secrets never enter an event.

use crate::config::{new_id, AccountMeta, Config};
use crate::keystore::{KeyStore, MemKeyStore};
use crate::login::{self, LoginOutcome};
use crate::monitor::{self, MonitorConfig};
use crate::protocol::Command;
use crate::providers::{Endpoints, Registry};
use crate::secrets::{AccountSecret, VaultFile};
use cookie_store::CookieStore;
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

pub type EventCb = extern "C" fn(*const u8, usize);

struct CoreState {
    data_dir: PathBuf,
    registry: Registry,
    config: Config,
    vault: Option<VaultFile>,             // Some(..) once unlocked
    monitor: Option<monitor::MonitorHandle>, // Some(..) while monitoring
    /// In-flight captcha logins: account_id → the channel that delivers the user's typed answer.
    pending_captcha: HashMap<String, oneshot::Sender<String>>,
}

impl CoreState {
    fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
    fn vault_path(&self) -> PathBuf {
        self.data_dir.join("vault.bin")
    }
}

pub struct Core {
    rt: Runtime,
    cb: EventCb,
    state: Arc<Mutex<Option<CoreState>>>,
    /// Optional unlock layer: holds the vault's one key for passwordless unlock. In-memory stub this
    /// slice; real Keychain/Keystore/DPAPI → Phase B (docs 10 unlock layer).
    keystore: Box<dyn KeyStore>,
}

/// All events cross the seam through the single audited redaction pass (docs 90 §4).
fn emit(cb: EventCb, v: &Value) {
    crate::redaction::emit(cb, v);
}

/// Reply to a correlated command. `error` is None on success.
fn reply(cb: EventCb, id: u64, ok: bool, error: Option<String>) {
    emit(cb, &json!({ "id": id, "event": "Reply", "ok": ok, "error": error }));
}

pub fn init(cb: EventCb) -> Box<Core> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // Heartbeat: unsolicited reverse-channel + process-alive proof (unchanged from slice 0).
    rt.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut n: u64 = 0;
        loop {
            ticker.tick().await;
            n += 1;
            emit(cb, &json!({ "id": null, "event": "Tick", "n": n }));
        }
    });

    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "starting" }));
    Box::new(Core { rt, cb, state: Arc::new(Mutex::new(None)), keystore: Box::new(MemKeyStore::default()) })
}

pub fn send(core: &Core, json_bytes: &[u8]) {
    let cb = core.cb;
    let cmd: Command = match serde_json::from_slice(json_bytes) {
        Ok(c) => c,
        Err(e) => {
            emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                              "code": "bad_command", "message": e.to_string() }));
            return;
        }
    };

    match cmd {
        Command::Login { id, account_id } => spawn_login(core, id, account_id),
        Command::StartMonitoring { id } => spawn_start_monitoring(core, id),
        Command::ImportCookies { id, account_id, cookies_json } => spawn_import_cookies(core, id, account_id, cookies_json),
        other => handle_sync(core, other),
    }
}

/// Everything except Login is quick and runs inline under the state lock.
fn handle_sync(core: &Core, cmd: Command) {
    let cb = core.cb;
    let id = cmd.id();
    let mut guard = core.state.lock().unwrap();

    match cmd {
        Command::Init { data_dir, .. } => {
            let dir = PathBuf::from(&data_dir);
            let _ = std::fs::create_dir_all(&dir);
            let registry = Registry::load_or_seed(&dir.join("providers.json"));
            let config = Config::load(&dir.join("config.json"));
            let vault_exists = VaultFile::exists(&dir.join("vault.bin"));
            crate::redaction::set_level(&config.settings.log_level);
            *guard = Some(CoreState { data_dir: dir, registry, config, vault: None, monitor: None, pending_captcha: HashMap::new() });
            let st = guard.as_ref().unwrap();
            emit_providers(cb, st);
            emit_accounts(cb, st);
            emit(cb, &json!({ "id": null, "event": "VaultState", "exists": vault_exists, "unlocked": false }));
            emit_caps(cb, core.keystore.available());
            emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
            reply(cb, id, true, None);
        }

        Command::CreateVault { master_password, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if VaultFile::exists(&st.vault_path()) {
                return reply(cb, id, false, Some("vault already exists".into()));
            }
            match VaultFile::create(&st.vault_path(), &master_password) {
                Ok(v) => {
                    // Hand the vault key to the keystore so subsequent unlocks can be passwordless.
                    if let Some(k) = v.key_bytes() {
                        let _ = core.keystore.store(&k);
                    }
                    st.vault = Some(v);
                    emit(cb, &json!({ "id": null, "event": "VaultState", "exists": true, "unlocked": true }));
                    reply(cb, id, true, None);
                }
                Err(e) => reply(cb, id, false, Some(e)),
            }
        }

        Command::Unlock { master_password, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            match VaultFile::unlock(&st.vault_path(), &master_password) {
                Ok(v) => {
                    if let Some(k) = v.key_bytes() {
                        let _ = core.keystore.store(&k);
                    }
                    st.vault = Some(v);
                    emit(cb, &json!({ "id": null, "event": "VaultState", "exists": true, "unlocked": true }));
                    reply(cb, id, true, None);
                }
                Err(e) => reply(cb, id, false, Some(e)),
            }
        }

        Command::UnlockWithKeystore { .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            match core.keystore.load() {
                Some(key) => match VaultFile::unlock_with_key(&st.vault_path(), key) {
                    Ok(v) => {
                        st.vault = Some(v);
                        emit(cb, &json!({ "id": null, "event": "VaultState", "exists": true, "unlocked": true }));
                        reply(cb, id, true, None);
                    }
                    Err(e) => reply(cb, id, false, Some(e)),
                },
                None => reply(cb, id, false, Some("no stored key in keystore".into())),
            }
        }

        Command::LockVault { .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if let Some(mut v) = st.vault.take() {
                v.lock(); // zeroize the in-memory key; the keystore's stored copy remains
            }
            emit(cb, &json!({ "id": null, "event": "VaultState", "exists": true, "unlocked": false }));
            reply(cb, id, true, None);
        }

        Command::AddAccount { label, school, username, password, is_teacher, course_id, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if st.vault.is_none() {
                return reply(cb, id, false, Some("vault is locked".into()));
            }
            // Accept a registry key/alias or a raw base_url; store what the user gave verbatim.
            let account = AccountMeta {
                id: new_id(),
                label,
                school_ref: school,
                username,
                device_id: new_id(), // stable random per-account device code
                is_teacher,
                course_id,
            };
            let acc_id = account.id.clone();
            if let Err(e) = st.vault.as_mut().unwrap().set(
                &acc_id,
                AccountSecret { password, cookies: String::new() },
            ) {
                return reply(cb, id, false, Some(e));
            }
            st.config.accounts.push(account);
            st.config.active_account.get_or_insert(acc_id);
            if let Err(e) = st.config.save(&st.config_path()) {
                return reply(cb, id, false, Some(e));
            }
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::SwitchAccount { account_id, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if st.config.account(&account_id).is_none() {
                return reply(cb, id, false, Some("no such account".into()));
            }
            st.config.active_account = Some(account_id);
            let _ = st.config.save(&st.config_path());
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::DeleteAccount { account_id, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            st.config.accounts.retain(|a| a.id != account_id);
            if st.config.active_account.as_deref() == Some(account_id.as_str()) {
                st.config.active_account = st.config.accounts.first().map(|a| a.id.clone());
            }
            if let Some(v) = st.vault.as_mut() {
                let _ = v.delete(&account_id);
            }
            let _ = st.config.save(&st.config_path());
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::StopMonitoring { .. } => {
            if let Some(st) = guard.as_mut() {
                if let Some(h) = st.monitor.take() {
                    let _ = h.tx.send(monitor::MonitorMsg::Stop);
                    for t in h.tasks {
                        t.abort();
                    }
                }
            }
            reply(cb, id, true, None);
        }

        Command::SignNow { rollcall_id, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::SignNow { rollcall_id });
        }
        Command::DeferSignIn { rollcall_id, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::Defer { rollcall_id });
        }

        Command::SubmitNow { quiz_id, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizSubmitNow { quiz_id });
        }
        Command::HoldAnswer { quiz_id, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizHold { quiz_id });
        }
        Command::DiscardAnswer { quiz_id, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizDiscard { quiz_id });
        }
        Command::SetAnswer { quiz_id, account_id, subject_id, answer, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizSetAnswer { quiz_id, account_id, subject_id, answer });
        }

        Command::SetLlmKey { key, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            match st.vault.as_mut() {
                Some(v) => match v.set_llm_key(key) {
                    Ok(()) => reply(cb, id, true, None),
                    Err(e) => reply(cb, id, false, Some(e)),
                },
                None => reply(cb, id, false, Some("vault is locked".into())),
            }
        }

        Command::SubmitCaptcha { account_id, text, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            match st.pending_captcha.remove(&account_id) {
                Some(txc) => {
                    let _ = txc.send(text); // wakes the awaiting login task
                    reply(cb, id, true, None);
                }
                None => reply(cb, id, false, Some("no captcha pending for this account".into())),
            }
        }

        Command::UpdateConfig { patch, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            let s = &mut st.config.settings;
            if let Some(v) = patch.get("countdown_secs").and_then(Value::as_u64) {
                s.countdown_secs = v;
            }
            if let Some(v) = patch.get("attendance_gate_percent").and_then(Value::as_f64) {
                s.attendance_gate_percent = v;
            }
            if let Some(v) = patch.get("llm_endpoint").and_then(Value::as_str) {
                s.llm_endpoint = v.to_string();
            }
            if let Some(v) = patch.get("llm_model").and_then(Value::as_str) {
                s.llm_model = v.to_string();
            }
            if let Some(v) = patch.get("llm_max_tokens").and_then(Value::as_u64) {
                s.llm_max_tokens = v as u32;
            }
            if let Some(v) = patch.get("resubmit_for_correct").and_then(Value::as_bool) {
                s.resubmit_for_correct = v;
            }
            if let Some(v) = patch.get("max_answer_reask").and_then(Value::as_u64) {
                s.max_answer_reask = v as u32;
            }
            if let Some(v) = patch.get("prepare_retry_budget_secs").and_then(Value::as_u64) {
                s.prepare_retry_budget_secs = v;
            }
            if let Some(v) = patch.get("autoanswer_types").and_then(Value::as_array) {
                s.autoanswer_types = v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
            }
            if let Some(v) = patch.get("enable_llm_tools").and_then(Value::as_bool) {
                s.enable_llm_tools = v;
            }
            if let Some(v) = patch.get("max_tool_iterations").and_then(Value::as_u64) {
                s.max_tool_iterations = v as u32;
            }
            if let Some(v) = patch.get("radar_strategy").and_then(Value::as_array) {
                s.radar_strategy = v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
            }
            if let Some(v) = patch.get("number_concurrency").and_then(Value::as_u64) {
                s.number_concurrency = v as u32;
            }
            if let Some(v) = patch.get("number_min_concurrency").and_then(Value::as_u64) {
                s.number_min_concurrency = v as u32;
            }
            if let Some(v) = patch.get("number_cooldown_ms").and_then(Value::as_u64) {
                s.number_cooldown_ms = v;
            }
            if let Some(v) = patch.get("number_max_cooldowns").and_then(Value::as_u64) {
                s.number_max_cooldowns = v as u32;
            }
            if let Some(v) = patch.get("poll_idle_secs").and_then(Value::as_u64) {
                s.poll_idle_secs = v;
            }
            if let Some(v) = patch.get("quiz_detect_secs").and_then(Value::as_u64) {
                s.quiz_detect_secs = v;
            }
            if let Some(v) = patch.get("log_level").and_then(Value::as_str) {
                s.log_level = v.to_string();
                crate::redaction::set_level(&s.log_level);
            }
            if let Some(v) = patch.get("tz_offset_minutes").and_then(Value::as_i64) {
                s.tz_offset_minutes = v;
            }
            if let Some(op) = patch.get("operating") {
                if let Ok(o) = serde_json::from_value::<crate::config::Operating>(op.clone()) {
                    s.operating = o;
                }
            }
            let _ = st.config.save(&st.config_path());
            reply(cb, id, true, None);
        }

        Command::Shutdown { .. } => {
            if let Some(st) = guard.as_mut() {
                if let Some(h) = st.monitor.take() {
                    let _ = h.tx.send(monitor::MonitorMsg::Stop);
                }
                if let Some(v) = st.vault.as_mut() {
                    v.lock();
                }
            }
            reply(cb, id, true, None);
        }

        Command::Login { .. } | Command::StartMonitoring { .. } | Command::ImportCookies { .. } => {
            unreachable!("handled asynchronously")
        }
    }
}

fn route_to_monitor(cb: EventCb, state: Option<&CoreState>, id: u64, msg: monitor::MonitorMsg) {
    match state.and_then(|s| s.monitor.as_ref()) {
        Some(h) => {
            let _ = h.tx.send(msg);
            reply(cb, id, true, None);
        }
        None => reply(cb, id, false, Some("not monitoring".into())),
    }
}

/// Login: snapshot (base_url, username, password, cached cookies) under the lock, release it, do
/// the async round-trip, then re-lock to persist refreshed cookies. Reuses a cached session if it
/// still verifies, so we don't re-login unnecessarily.
fn spawn_login(core: &Core, id: u64, account_id: String) {
    let cb = core.cb;
    let state = core.state.clone();

    // Snapshot under the lock (no await while holding it).
    let snap = {
        let guard = state.lock().unwrap();
        let Some(st) = guard.as_ref() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        let Some(vault) = st.vault.as_ref() else {
            return reply(cb, id, false, Some("vault is locked".into()));
        };
        let Some(acc) = st.config.account(&account_id) else {
            return reply(cb, id, false, Some("no such account".into()));
        };
        let Some(base_url) = st.registry.resolve(&acc.school_ref) else {
            return reply(cb, id, false, Some(format!("unknown school: {}", acc.school_ref)));
        };
        let secret = vault.get(&account_id).unwrap_or_default();
        (base_url, acc.username.clone(), secret.password, secret.cookies)
    };
    let (base_url, username, password, cached_cookies) = snap;

    core.rt.spawn(async move {
        emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "logging_in" }));
        emit(cb, &json!({ "id": null, "event": "LogLine", "level": "info",
                          "text": format!("login → {base_url}") })); // base_url only, never creds

        let endpoints = Endpoints::derive(&base_url);
        let (client, jar) = build_client(&cached_cookies);

        // Restore path: a cached session that still verifies skips the password login entirely.
        let result: Result<bool, String> = if !cached_cookies.is_empty() && login::verify_session(&client, &endpoints).await {
            Ok(true)
        } else {
            match login::login(&client, &endpoints, &username, &password).await {
                LoginOutcome::Ok => Ok(false),
                LoginOutcome::Failed(e) => Err(e),
                LoginOutcome::NeedCaptcha { image_bytes, pending } => {
                    // Register a one-shot for the answer, show the challenge (image is not a secret),
                    // and await a SubmitCaptcha command. Credentials stay inside `pending`, never emitted.
                    let (txc, rxc) = oneshot::channel::<String>();
                    if let Ok(mut guard) = state.lock() {
                        if let Some(st) = guard.as_mut() {
                            st.pending_captcha.insert(account_id.clone(), txc);
                        }
                    }
                    emit(cb, &json!({ "id": null, "event": "CaptchaChallenge",
                                      "account_id": account_id, "image_b64": login::encode_base64(&image_bytes) }));
                    match tokio::time::timeout(Duration::from_secs(180), rxc).await {
                        Ok(Ok(text)) => login::complete_captcha(&client, &endpoints, pending, &text).await.map(|_| false),
                        _ => {
                            // timeout or dropped sender → drop the stale pending entry
                            if let Ok(mut guard) = state.lock() {
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
                // Re-lock to cache the refreshed cookies into the vault.
                if let Ok(mut guard) = state.lock() {
                    if let Some(st) = guard.as_mut() {
                        if let Some(v) = st.vault.as_mut() {
                            let _ = v.set(&account_id, AccountSecret { password, cookies });
                        }
                    }
                }
                emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
                emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": true,
                                  "detail": if from_cache { "session restored from cache" } else { "logged in" } }));
            }
            Err(e) => {
                emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "login_failed" }));
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                  "code": "login_failed", "message": e.clone() }));
                emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": e }));
            }
        }
    });
}

/// Build an authenticated client: reuse a cached session if it still verifies, else log in.
/// Returns the client + refreshed cookie JSON to cache.
async fn authed_client(base_url: &str, username: &str, password: &str, cached: &str) -> Result<(Client, String), String> {
    let endpoints = Endpoints::derive(base_url);
    let (client, jar) = build_client(cached);
    if cached.is_empty() || !login::verify_session(&client, &endpoints).await {
        match login::login(&client, &endpoints, username, password).await {
            LoginOutcome::Ok => {}
            LoginOutcome::Failed(e) => return Err(e),
            // A captcha needs a human; monitoring startup can't prompt. Log in interactively first
            // (which caches the session), then StartMonitoring reuses the cached cookies.
            LoginOutcome::NeedCaptcha { .. } => return Err("需要圖形驗證碼，請先用 Login 登入一次".into()),
        }
    }
    Ok((client, dump_cookies(&jar)))
}

/// Start concurrent monitoring: authenticate every account, then hand ready sessions to the monitor.
fn spawn_start_monitoring(core: &Core, id: u64) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let guard = state.lock().unwrap();
        let Some(st) = guard.as_ref() else { return reply(cb, id, false, Some("not initialized".into())) };
        let Some(vault) = st.vault.as_ref() else { return reply(cb, id, false, Some("vault is locked".into())) };
        if st.monitor.is_some() {
            return reply(cb, id, false, Some("already monitoring".into()));
        }
        let mut accts = Vec::new();
        for acc in &st.config.accounts {
            let Some(base_url) = st.registry.resolve(&acc.school_ref) else { continue };
            let secret = vault.get(&acc.id).unwrap_or_default();
            accts.push((acc.clone(), base_url, secret.password, secret.cookies));
        }
        let s = &st.config.settings;
        let cfg = MonitorConfig {
            countdown_secs: s.countdown_secs,
            gate_percent: s.attendance_gate_percent,
            llm_endpoint: s.llm_endpoint.clone(),
            llm_model: s.llm_model.clone(),
            llm_key: vault.get_llm_key(),
            llm_max_tokens: s.llm_max_tokens,
            max_answer_reask: s.max_answer_reask,
            prepare_retry_budget_secs: s.prepare_retry_budget_secs,
            autoanswer_types: s.autoanswer_types.clone(),
            enable_llm_tools: s.enable_llm_tools,
            max_tool_iterations: s.max_tool_iterations,
            resubmit_for_correct: s.resubmit_for_correct,
            radar_strategy: s.radar_strategy.clone(),
            number_concurrency: s.number_concurrency,
            number_min_concurrency: s.number_min_concurrency,
            number_cooldown_ms: s.number_cooldown_ms,
            number_max_cooldowns: s.number_max_cooldowns,
            poll_idle_secs: s.poll_idle_secs,
            quiz_detect_secs: s.quiz_detect_secs,
            operating: s.operating.clone(),
            tz_offset_minutes: s.tz_offset_minutes,
        };
        (accts, cfg)
    };
    let (accts, cfg) = snap;

    core.rt.spawn(async move {
        let mut monitor_accounts = Vec::new();
        let mut refreshed: Vec<(String, String)> = Vec::new();
        for (meta, base_url, password, cookies) in accts {
            match authed_client(&base_url, &meta.username, &password, &cookies).await {
                Ok((client, new_cookies)) => {
                    // Capture the account's own user id for per-account recheck (my_present).
                    let user_no = login::fetch_user_no(&client, &Endpoints::derive(&base_url)).await;
                    emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": meta.id, "state": "online" }));
                    refreshed.push((meta.id.clone(), new_cookies));
                    monitor_accounts.push(monitor::Account {
                        id: meta.id.clone(),
                        device_id: meta.device_id.clone(),
                        user_no,
                        is_teacher: meta.is_teacher,
                        course_id: meta.course_id.clone(),
                        base_url,
                        client,
                        username: meta.username.clone(),
                        password: crate::secrets::Secret::new(password),
                    });
                }
                Err(e) => emit(cb, &json!({ "id": null, "event": "AccountStatus",
                                            "account_id": meta.id, "state": "login_failed", "error": e })),
            }
        }

        if let Ok(mut guard) = state.lock() {
            if let Some(st) = guard.as_mut() {
                if let Some(v) = st.vault.as_mut() {
                    for (aid, ck) in &refreshed {
                        if let Some(mut sec) = v.get(aid) {
                            sec.cookies = ck.clone();
                            let _ = v.set(aid, sec);
                        }
                    }
                }
                if monitor_accounts.is_empty() {
                    emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                                      "code": "no_accounts_online", "message": "no account could authenticate" }));
                } else {
                    st.monitor = Some(monitor::start(cb, monitor_accounts, cfg)); // start() only spawns; no await under lock
                }
            }
        }
        reply(cb, id, true, None);
    });
}

/// Import a supplied cookie set for an account → store in vault → verify (browser-cookie login).
fn spawn_import_cookies(core: &Core, id: u64, account_id: String, cookies_json: String) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let guard = state.lock().unwrap();
        let Some(st) = guard.as_ref() else { return reply(cb, id, false, Some("not initialized".into())) };
        let Some(vault) = st.vault.as_ref() else { return reply(cb, id, false, Some("vault is locked".into())) };
        let Some(acc) = st.config.account(&account_id) else { return reply(cb, id, false, Some("no such account".into())) };
        let Some(base_url) = st.registry.resolve(&acc.school_ref) else { return reply(cb, id, false, Some("unknown school".into())) };
        (base_url, vault.get(&account_id).unwrap_or_default().password)
    };
    let (base_url, password) = snap;

    core.rt.spawn(async move {
        let endpoints = Endpoints::derive(&base_url);
        let (client, _jar) = build_client(&cookies_json);
        let ok = login::verify_session(&client, &endpoints).await;
        if let Ok(mut guard) = state.lock() {
            if let Some(st) = guard.as_mut() {
                if let Some(v) = st.vault.as_mut() {
                    let _ = v.set(&account_id, AccountSecret { password, cookies: cookies_json });
                }
            }
        }
        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                          "state": if ok { "online" } else { "login_failed" } }));
        reply(cb, id, ok, if ok { None } else { Some("imported cookies did not verify".into()) });
    });
}

fn emit_providers(cb: EventCb, st: &CoreState) {
    emit(cb, &json!({ "id": null, "event": "Providers",
                      "default_key": st.registry.default_key,
                      "schools": st.registry.schools }));
}

fn emit_accounts(cb: EventCb, st: &CoreState) {
    emit(cb, &json!({ "id": null, "event": "Accounts",
                      "active": st.config.active_account,
                      "accounts": st.config.accounts }));
}

fn emit_caps(cb: EventCb, biometric_unlock: bool) {
    // `biometric_unlock` reflects a real, persistent platform keystore. The in-memory stub reports
    // false, so this stays false until a Keychain/Keystore/DPAPI backend lands (Phase B). Captcha is
    // human-in-loop this slice (no OCR), so `ocr_captcha` also stays false.
    emit(cb, &json!({ "id": null, "event": "Caps", "caps": {
        "background_monitoring": true,
        "self_update": true,
        "biometric_unlock": biometric_unlock,
        "qr_teacher_assist": false,
        "ocr_captcha": false
    }}));
}

fn build_client(cookies_json: &str) -> (Client, Arc<CookieStoreMutex>) {
    let store = if cookies_json.is_empty() {
        CookieStore::default()
    } else {
        cookie_store::serde::json::load_all(std::io::Cursor::new(cookies_json.as_bytes()))
            .unwrap_or_default()
    };
    let jar = Arc::new(CookieStoreMutex::new(store));
    let client = Client::builder()
        .cookie_provider(jar.clone())
        .build()
        .expect("reqwest client");
    (client, jar)
}

fn dump_cookies(jar: &CookieStoreMutex) -> String {
    let store = jar.lock().unwrap();
    let mut buf = Vec::new();
    // Include session (non-persistent) cookies — the TronClass session cookie is one.
    let _ = cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf);
    String::from_utf8(buf).unwrap_or_default()
}
