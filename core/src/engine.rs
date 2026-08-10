//! The core state machine: owns the tokio runtime, the event callback, a long-lived heartbeat,
//! and (after `Init`) the registry / config / vault behind one mutex. Commands lock, mutate,
//! persist, and emit; the one async command (Login) snapshots what it needs, drops the lock,
//! does the network round-trip, then re-locks to cache cookies. Secrets never enter an event.

use crate::config::{new_id, AccountMeta, Config};
use crate::login::{self, LoginOutcome};
use crate::monitor::{self, MonitorConfig};
use crate::persistence::{AccountJournal, AccountMutation};
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
use tokio::task::JoinHandle;

pub type EventCb = extern "C" fn(*const u8, usize);

struct CoreState {
    data_dir: PathBuf,
    registry: Registry,
    config: Config,
    vault: Option<VaultFile>,             // Some(..) once unlocked
    monitor: MonitorLifecycle,
    monitor_generation: u64,
    /// In-flight captcha logins: account_id → the channel that delivers the user's typed answer.
    pending_captcha: HashMap<String, oneshot::Sender<String>>,
}

struct StartingMonitor {
    generation: u64,
    command_id: u64,
    task: Option<JoinHandle<()>>,
}

struct RunningMonitor {
    generation: u64,
    handle: monitor::MonitorHandle,
}

enum MonitorLifecycle {
    Idle,
    Starting(StartingMonitor),
    Running(RunningMonitor),
}

enum StoppedMonitor {
    Idle,
    Starting(StartingMonitor),
    Running(monitor::MonitorHandle),
}

impl MonitorLifecycle {
    fn begin_start(&mut self, generation: u64, command_id: u64) -> Result<(), &'static str> {
        match self {
            Self::Idle => {
                *self = Self::Starting(StartingMonitor {
                    generation,
                    command_id,
                    task: None,
                });
                Ok(())
            }
            Self::Starting(_) => Err("monitor is already starting"),
            Self::Running(_) => Err("already monitoring"),
        }
    }

    fn is_starting(&self, generation: u64) -> bool {
        matches!(self, Self::Starting(starting) if starting.generation == generation)
    }

    fn attach_start_task(&mut self, generation: u64, task: JoinHandle<()>) {
        match self {
            Self::Starting(starting) if starting.generation == generation => {
                starting.task = Some(task);
            }
            // The task committed Running and replied before the caller reacquired the lock. It is
            // already finishing, so detaching is correct; aborting here could suppress its reply.
            Self::Running(running) if running.generation == generation => drop(task),
            _ => task.abort(),
        }
    }

    fn running_handle(&self) -> Option<&monitor::MonitorHandle> {
        match self {
            Self::Running(running) => Some(&running.handle),
            _ => None,
        }
    }

    fn take_for_stop(&mut self) -> StoppedMonitor {
        match std::mem::replace(self, Self::Idle) {
            Self::Idle => StoppedMonitor::Idle,
            Self::Starting(starting) => StoppedMonitor::Starting(starting),
            Self::Running(running) => StoppedMonitor::Running(running.handle),
        }
    }
}

impl CoreState {
    fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
}

/// Open (or first-run create) the vault with the persistent device key — auto-unlock, no master
/// password. Only a genuinely missing vault may be created. Every failure opening an existing vault
/// is returned without mutating either file so recovery remains possible.
fn open_vault_auto(
    dir: &std::path::Path,
    host_key: Option<[u8; 32]>,
) -> Result<VaultFile, String> {
    let key = match host_key {
        Some(key) => key,
        None => crate::secrets::load_or_create_device_key(&dir.join("device.key"))?,
    };
    let vault_path = dir.join("vault.bin");
    if VaultFile::exists(&vault_path) {
        VaultFile::unlock_with_key(&vault_path, key)
    } else {
        VaultFile::create_with_key(&vault_path, key)
    }
}

fn recover_account_transaction(
    dir: &std::path::Path,
    config: &Config,
    vault: &mut VaultFile,
) -> Result<Option<String>, String> {
    let Some(journal) = AccountJournal::load(dir)? else {
        return Ok(None);
    };
    let account_exists = config.account(&journal.account_id).is_some();
    // In either ordered transaction, an absent config record means the durable end state must not
    // retain a secret: rollback an unfinished Add or roll forward an already-configured Delete.
    if !account_exists {
        vault.delete(&journal.account_id)?;
    }
    AccountJournal::complete(dir)?;
    Ok(Some(format!(
        "已恢復未完成的帳號{}交易",
        match journal.mutation {
            AccountMutation::Add => "新增",
            AccountMutation::Delete => "刪除",
        }
    )))
}

pub struct Core {
    rt: Runtime,
    cb: EventCb,
    state: Arc<Mutex<Option<CoreState>>>,
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
    Box::new(Core { rt, cb, state: Arc::new(Mutex::new(None)) })
}

pub fn send(core: &Core, json_bytes: &[u8]) {
    let cb = core.cb;
    let cmd: Command = match serde_json::from_slice(json_bytes) {
        Ok(c) => c,
        Err(e) => {
            // Recover the correlation id from the raw JSON so the awaiting UI call always completes
            // (never hangs/leaks) — a Reply(ok:false) surfaces as an error toast. Only when the id is
            // unreadable (truly malformed JSON) do we fall back to an uncorrelated Error event.
            match serde_json::from_slice::<Value>(json_bytes)
                .ok()
                .and_then(|v| v.get("id").and_then(Value::as_u64))
            {
                Some(id) => reply(cb, id, false, Some(format!("未知或格式錯誤的命令：{e}"))),
                None => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                         "code": "bad_command", "message": e.to_string() })),
            }
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
        Command::Init {
            data_dir,
            mut device_key_b64,
            ..
        } => {
            let dir = PathBuf::from(&data_dir);
            let _ = std::fs::create_dir_all(&dir);
            let registry = match Registry::load_or_seed(&dir.join("providers.json")) {
                Ok(registry) => registry,
                Err(error) => {
                    emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "providers_unavailable", "message": error }));
                    return reply(cb, id, false, Some("providers registry unavailable".to_string()));
                }
            };
            let config_path = dir.join("config.json");
            let (config, config_healthy) = match Config::load(&config_path) {
                Ok(config) => (config, true),
                Err(error) => {
                    let recovery = match Config::quarantine(&config_path) {
                        Ok(path) => format!(
                            "；原檔已保留為 {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Err(quarantine_error) => format!("；原檔未移動：{quarantine_error}"),
                    };
                    emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "config_corrupt", "message": format!("{error}{recovery}") }));
                    (Config::default(), false)
                }
            };
            crate::redaction::set_level(&config.settings.log_level);
            // GUI hosts recover the vault key through their OS keystore and pass it only in memory.
            // Headless tests may omit it and retain the legacy keyfile path.
            let vault_result = (|| {
                let host_key = device_key_b64
                    .as_deref()
                    .map(crate::secrets::decode_device_key_base64)
                    .transpose()?;
                open_vault_auto(&dir, host_key)
            })();
            if let Some(encoded) = device_key_b64.as_mut() {
                use zeroize::Zeroize;
                encoded.zeroize();
            }
            let (mut vault, vault_error) = match vault_result {
                Ok(v) => (Some(v), None),
                Err(e) => {
                    emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                      "code": "vault_open_failed", "message": e }));
                    (None, Some(e))
                }
            };
            if config_healthy {
                if let Some(vault) = vault.as_mut() {
                    match recover_account_transaction(&dir, &config, vault) {
                        Ok(Some(message)) => emit(cb, &json!({ "id": null, "event": "LogLine",
                            "level": "warn", "text": message })),
                        Ok(None) => {}
                        Err(error) => emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                            "code": "account_transaction_recovery_failed", "message": error })),
                    }
                }
            }
            let unlocked = vault.is_some();
            *guard = Some(CoreState {
                data_dir: dir,
                registry,
                config,
                vault,
                monitor: MonitorLifecycle::Idle,
                monitor_generation: 0,
                pending_captcha: HashMap::new(),
            });
            let st = guard.as_ref().unwrap();
            emit_providers(cb, st);
            emit_accounts(cb, st);
            emit_settings(cb, st);
            emit(cb, &json!({ "id": null, "event": "VaultState", "exists": unlocked, "unlocked": unlocked }));
            emit_caps(cb);
            emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
            reply(cb, id, vault_error.is_none(), vault_error);
        }

        // The vault auto-unlocks with the device key at Init (no master password), so CreateVault and
        // Unlock are idempotent no-ops now — kept only for wire back-compat. Reply ok iff it is open.
        Command::CreateVault { .. } | Command::Unlock { .. } => {
            let ready = guard.as_ref().is_some_and(|st| st.vault.is_some());
            reply(cb, id, ready, (!ready).then(|| "vault not ready".into()));
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
            if let Err(error) = AccountJournal::begin(&st.data_dir, AccountMutation::Add, &acc_id) {
                return reply(cb, id, false, Some(error));
            }
            if let Err(e) = st.vault.as_mut().unwrap().set(
                &acc_id,
                AccountSecret { password, cookies: String::new() },
            ) {
                let _ = AccountJournal::complete(&st.data_dir);
                return reply(cb, id, false, Some(e));
            }
            let previous_config = st.config.clone();
            st.config.accounts.push(account);
            st.config.active_account.get_or_insert(acc_id.clone());
            if let Err(e) = st.config.save(&st.config_path()) {
                st.config = previous_config;
                let rollback = st.vault.as_mut().unwrap().delete(&acc_id);
                if rollback.is_ok() {
                    let _ = AccountJournal::complete(&st.data_dir);
                }
                let error = match rollback {
                    Ok(()) => e,
                    Err(rollback_error) => format!(
                        "{e}; vault rollback failed and will retry on restart: {rollback_error}"
                    ),
                };
                return reply(cb, id, false, Some(error));
            }
            if let Err(error) = AccountJournal::complete(&st.data_dir) {
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                    "code": "account_transaction_cleanup_failed", "message": error }));
            }
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::SwitchAccount { account_id, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if st.config.account(&account_id).is_none() {
                return reply(cb, id, false, Some("no such account".into()));
            }
            let previous = st.config.active_account.clone();
            st.config.active_account = Some(account_id);
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config.active_account = previous;
                return reply(cb, id, false, Some(error));
            }
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::DeleteAccount { account_id, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            if st.config.account(&account_id).is_none() {
                return reply(cb, id, false, Some("no such account".into()));
            }
            if st.vault.is_none() {
                return reply(cb, id, false, Some("vault is locked".into()));
            }
            if let Err(error) = AccountJournal::begin(&st.data_dir, AccountMutation::Delete, &account_id) {
                return reply(cb, id, false, Some(error));
            }
            let previous_config = st.config.clone();
            st.config.accounts.retain(|a| a.id != account_id);
            if st.config.active_account.as_deref() == Some(account_id.as_str()) {
                st.config.active_account = st.config.accounts.first().map(|a| a.id.clone());
            }
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous_config;
                let _ = AccountJournal::complete(&st.data_dir);
                return reply(cb, id, false, Some(error));
            }
            if let Err(vault_error) = st.vault.as_mut().unwrap().delete(&account_id) {
                let deleted_config = st.config.clone();
                st.config = previous_config;
                if let Err(config_error) = st.config.save(&st.config_path()) {
                    st.config = deleted_config;
                    return reply(
                        cb,
                        id,
                        false,
                        Some(format!(
                            "delete vault entry failed: {vault_error}; config rollback failed and recovery journal remains: {config_error}"
                        )),
                    );
                }
                let _ = AccountJournal::complete(&st.data_dir);
                return reply(cb, id, false, Some(format!("delete vault entry failed: {vault_error}")));
            }
            if let Err(error) = AccountJournal::complete(&st.data_dir) {
                emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                    "code": "account_transaction_cleanup_failed", "message": error }));
            }
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::StopMonitoring { .. } => {
            if let Some(st) = guard.as_mut() {
                stop_monitor(&mut st.monitor, cb, "monitor start cancelled");
            }
            // The actor emits `idle` when it breaks on Stop, but the abort() above can cancel it before
            // it gets there — so emit `idle` here too (idempotent) to GUARANTEE the UI leaves the
            // "monitoring" state. Without this the toggle stays stuck on 停止監控 and monitoring can
            // never be re-started (the reported "停不下來" bug).
            emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
            reply(cb, id, true, None);
        }

        Command::SignNow { activity_token, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::SignNow { command_id: id, activity_token });
        }
        Command::DeferSignIn { activity_token, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::Defer { command_id: id, activity_token });
        }

        Command::SubmitNow { activity_token, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizSubmitNow { command_id: id, activity_token });
        }
        Command::HoldAnswer { activity_token, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizHold { command_id: id, activity_token });
        }
        Command::DiscardAnswer { activity_token, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizDiscard { command_id: id, activity_token });
        }
        Command::SetAnswer { activity_token, account_id, subject_id, answer, .. } => {
            route_to_monitor(cb, guard.as_ref(), id, monitor::MonitorMsg::QuizSetAnswer {
                command_id: id,
                activity_token,
                account_id,
                subject_id,
                answer,
            });
        }

        Command::SetLlmKey { key, .. } => {
            let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
            let result = match st.vault.as_mut() {
                Some(v) => v.set_llm_key(key),
                None => Err("vault is locked".into()),
            };
            match result {
                Ok(()) => {
                    push_config(st); // a running monitor starts answering with the new key immediately
                    emit_settings(cb, st); // has_llm_key flips true → the Settings screen updates
                    reply(cb, id, true, None);
                }
                Err(e) => reply(cb, id, false, Some(e)),
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
            if let Err(error) = validate_config_patch(&patch) {
                return reply(cb, id, false, Some(error));
            }
            let previous_config = st.config.clone();
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
            }
            if let Some(v) = patch.get("tz_offset_minutes").and_then(Value::as_i64) {
                s.tz_offset_minutes = v;
            }
            if let Some(op) = patch.get("operating") {
                if let Ok(o) = serde_json::from_value::<crate::config::Operating>(op.clone()) {
                    s.operating = o;
                }
            }
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous_config;
                return reply(cb, id, false, Some(error));
            }
            crate::redaction::set_level(&st.config.settings.log_level);
            push_config(st); // a running monitor adopts the change live (no stop/start)
            emit_settings(cb, st); // echo the applied settings so the UI reflects the saved values
            reply(cb, id, true, None);
        }

        Command::Shutdown { .. } => {
            if let Some(st) = guard.as_mut() {
                stop_monitor(&mut st.monitor, cb, "shutdown cancelled monitor start");
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
    match state.and_then(|s| s.monitor.running_handle()) {
        Some(h) => {
            if h.tx.send(msg).is_err() {
                reply(cb, id, false, Some("monitor actor is unavailable".into()));
            }
        }
        None => reply(cb, id, false, Some("not monitoring".into())),
    }
}

fn stop_monitor(lifecycle: &mut MonitorLifecycle, cb: EventCb, cancelled_reason: &str) {
    match lifecycle.take_for_stop() {
        StoppedMonitor::Idle => {}
        StoppedMonitor::Starting(mut starting) => {
            // Complete the pending StartMonitoring request before aborting its task; otherwise the
            // UI's correlated SendAsync would wait forever for a reply that can no longer be emitted.
            reply(
                cb,
                starting.command_id,
                false,
                Some(cancelled_reason.to_string()),
            );
            if let Some(task) = starting.task.take() {
                task.abort();
            }
        }
        StoppedMonitor::Running(handle) => {
            let _ = handle.tx.send(monitor::MonitorMsg::Stop);
            for task in handle.tasks {
                task.abort();
            }
        }
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
        let (password, cookies) = secret.into_parts();
        (base_url, acc.username.clone(), password, cookies)
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
                let persist_error = match state.lock() {
                    Ok(mut guard) => match guard.as_mut().and_then(|st| st.vault.as_mut()) {
                        Some(vault) => vault
                            .set(&account_id, AccountSecret { password, cookies })
                            .err(),
                        None => Some("vault is locked".to_string()),
                    },
                    Err(_) => Some("core state lock poisoned".to_string()),
                };
                emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
                match persist_error {
                    Some(error) => emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false,
                        "reason": format!("login succeeded but session persistence failed: {error}") })),
                    None => emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": true,
                        "detail": if from_cache { "session restored from cache" } else { "logged in" } })),
                }
            }
            Err(e) => {
                // One report only: the correlated LoginResult(ok:false) already surfaces `reason` as a
                // toast+log via AppState.Send's error path; a second id:null Error would double it.
                emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "login_failed" }));
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

/// The monitor's view of the current settings. Built at `StartMonitoring` **and** on every settings
/// change (see `push_config`) so one definition serves both — the two must never drift.
fn monitor_config(st: &CoreState) -> MonitorConfig {
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
        number_concurrency: s.number_concurrency,
        number_min_concurrency: s.number_min_concurrency,
        number_cooldown_ms: s.number_cooldown_ms,
        number_max_cooldowns: s.number_max_cooldowns,
        poll_idle_secs: s.poll_idle_secs,
        quiz_detect_secs: s.quiz_detect_secs,
        operating: s.operating.clone(),
        tz_offset_minutes: s.tz_offset_minutes,
    }
}

/// Hand the just-changed settings to a RUNNING monitor. Without this the actor/pollers keep the snapshot
/// they took at `StartMonitoring`, so a settings change only bit after a manual stop→start (user-reported).
fn push_config(st: &CoreState) {
    if let Some(h) = st.monitor.running_handle() {
        let _ = h.tx.send(monitor::MonitorMsg::ConfigUpdated(Box::new(monitor_config(st))));
    }
}

/// Start concurrent monitoring: authenticate every account, then hand ready sessions to the monitor.
fn spawn_start_monitoring(core: &Core, id: u64) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let mut guard = state.lock().unwrap();
        let Some(st) = guard.as_mut() else { return reply(cb, id, false, Some("not initialized".into())) };
        let Some(vault) = st.vault.as_ref() else { return reply(cb, id, false, Some("vault is locked".into())) };
        let mut accts = Vec::new();
        for acc in &st.config.accounts {
            let Some(base_url) = st.registry.resolve(&acc.school_ref) else { continue };
            let secret = vault.get(&acc.id).unwrap_or_default();
            let (password, cookies) = secret.into_parts();
            accts.push((acc.clone(), base_url, password, cookies));
        }
        let cfg = monitor_config(st);
        st.monitor_generation = st.monitor_generation.wrapping_add(1).max(1);
        let generation = st.monitor_generation;
        if let Err(error) = st.monitor.begin_start(generation, id) {
            return reply(cb, id, false, Some(error.to_string()));
        }
        emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "starting" }));
        (accts, cfg, generation)
    };
    let (accts, cfg, generation) = snap;

    let task_state = state.clone();
    let task = core.rt.spawn(async move {
        let mut monitor_accounts = Vec::new();
        let mut refreshed: Vec<(String, String)> = Vec::new();
        for (meta, base_url, password, cookies) in accts {
            match authed_client(&base_url, &meta.username, &password, &cookies).await {
                Ok((client, new_cookies)) => {
                    // The account's own identity for per-account recheck (my_present) = its login username.
                    let user_no = login::user_no_from_username(&meta.username);
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
            if !monitor_start_is_current(&task_state, generation) {
                return;
            }
        }

        if let Ok(mut guard) = task_state.lock() {
            if let Some(st) = guard.as_mut() {
                if !st.monitor.is_starting(generation) {
                    return;
                }
                if let Some(v) = st.vault.as_mut() {
                    for (aid, ck) in &refreshed {
                        if let Some(mut sec) = v.get(aid) {
                            sec.cookies = ck.clone();
                            if let Err(error) = v.set(aid, sec) {
                                emit(cb, &json!({ "id": null, "event": "Error", "severity": "error",
                                    "code": "session_persistence_failed",
                                    "message": format!("{aid}: {error}") }));
                            }
                        }
                    }
                }
                if monitor_accounts.is_empty() {
                    st.monitor = MonitorLifecycle::Idle;
                    emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn",
                                      "code": "no_accounts_online", "message": "no account could authenticate" }));
                    emit(cb, &json!({ "id": null, "event": "StateChanged", "state": "idle" }));
                    reply(cb, id, false, Some("no account could authenticate".into()));
                } else {
                    // Fail-fast heads-up: with a monitored account but no LLM key, auto-answer can't run —
                    // say so up front instead of only after a quiz spins out 300s later. Rollcall is fine.
                    if cfg.llm_key.as_deref().is_none_or(|k| k.trim().is_empty()) {
                        emit(cb, &json!({ "id": null, "event": "Error", "severity": "warn", "code": "llm_key_missing",
                            "message": "尚未設定 LLM 金鑰：自動答題需要金鑰（設定 → 儲存金鑰）；點名不受影響。" }));
                    }
                    let handle = monitor::start(cb, monitor_accounts, cfg); // start() only spawns; no await under lock
                    st.monitor = MonitorLifecycle::Running(RunningMonitor { generation, handle });
                    reply(cb, id, true, None);
                }
            }
        }
    });

    if let Ok(mut guard) = state.lock() {
        match guard.as_mut() {
            Some(st) => st.monitor.attach_start_task(generation, task),
            None => task.abort(),
        }
    } else {
        task.abort();
    };
}

fn monitor_start_is_current(state: &Arc<Mutex<Option<CoreState>>>, generation: u64) -> bool {
    state
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|st| st.monitor.is_starting(generation)))
        .unwrap_or(false)
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
        let (password, _) = vault.get(&account_id).unwrap_or_default().into_parts();
        (base_url, password)
    };
    let (base_url, password) = snap;

    core.rt.spawn(async move {
        let endpoints = Endpoints::derive(&base_url);
        let (client, _jar) = build_client(&cookies_json);
        let verified = login::verify_session(&client, &endpoints).await;
        let persist_error = if verified {
            match state.lock() {
                Ok(mut guard) => match guard.as_mut().and_then(|st| st.vault.as_mut()) {
                    Some(vault) => vault
                        .set(&account_id, AccountSecret { password, cookies: cookies_json })
                        .err(),
                    None => Some("vault is locked".to_string()),
                },
                Err(_) => Some("core state lock poisoned".to_string()),
            }
        } else {
            None
        };
        let ok = verified && persist_error.is_none();
        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                          "state": if ok { "online" } else { "login_failed" } }));
        let error = if !verified {
            Some("imported cookies did not verify".into())
        } else {
            persist_error.map(|error| format!("cookies verified but persistence failed: {error}"))
        };
        reply(cb, id, ok, error);
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

fn emit_caps(cb: EventCb) {
    emit(cb, &caps_payload());
}

fn caps_payload() -> Value {
    // Captcha is human-in-loop (no OCR), so `ocr_captcha` stays false. QR teacher-assist IS implemented
    // (monitor::spawn_qr_teacher_assist) — the build supports it; it just needs a teacher account added.
    json!({ "id": null, "event": "Caps", "caps": {
        "background_monitoring": true,
        // No updater exists in this repository; never advertise a capability the product cannot run.
        "self_update": false,
        "qr_teacher_assist": true,
        "ocr_captcha": false
    }})
}

/// The current user-facing settings, so a Settings screen reflects what is actually saved (not just
/// defaults). The LLM key itself is a secret and NEVER crosses the seam — only a `has_llm_key` bool does.
fn validate_config_patch(patch: &Value) -> Result<(), String> {
    let patch = patch.as_object().ok_or_else(|| "config patch 必須是物件".to_string())?;
    let u64_range = |key: &str, min: u64, max: u64| -> Result<(), String> {
        if let Some(value) = patch.get(key) {
            let number = value.as_u64().ok_or_else(|| format!("{key} 必須是非負整數"))?;
            if !(min..=max).contains(&number) {
                return Err(format!("{key} 必須介於 {min} 與 {max}"));
            }
        }
        Ok(())
    };
    u64_range("countdown_secs", 1, 86_400)?;
    u64_range("llm_max_tokens", 1, 1_000_000)?;
    u64_range("max_answer_reask", 1, 100)?;
    u64_range("prepare_retry_budget_secs", 1, 86_400)?;
    u64_range("max_tool_iterations", 0, 100)?;
    u64_range("number_concurrency", 1, 256)?;
    u64_range("number_min_concurrency", 1, 256)?;
    u64_range("number_cooldown_ms", 1, 3_600_000)?;
    u64_range("number_max_cooldowns", 0, 1_000)?;
    u64_range("poll_idle_secs", 1, 86_400)?;
    u64_range("quiz_detect_secs", 1, 86_400)?;
    if let Some(value) = patch.get("attendance_gate_percent") {
        let number = value.as_f64().ok_or_else(|| "attendance_gate_percent 必須是數字".to_string())?;
        if !number.is_finite() || !(0.0..=100.0).contains(&number) {
            return Err("attendance_gate_percent 必須介於 0 與 100".to_string());
        }
    }
    if let Some(value) = patch.get("tz_offset_minutes") {
        let minutes = value.as_i64().ok_or_else(|| "tz_offset_minutes 必須是整數".to_string())?;
        if !(-840..=840).contains(&minutes) {
            return Err("tz_offset_minutes 必須介於 -840 與 840".to_string());
        }
    }
    let concurrency = patch.get("number_concurrency").and_then(Value::as_u64);
    let minimum = patch.get("number_min_concurrency").and_then(Value::as_u64);
    if matches!((concurrency, minimum), (Some(max), Some(min)) if min > max) {
        return Err("number_min_concurrency 不得大於 number_concurrency".to_string());
    }
    Ok(())
}

fn emit_settings(cb: EventCb, st: &CoreState) {
    let s = &st.config.settings;
    let has_llm_key = st
        .vault
        .as_ref()
        .and_then(|v| v.get_llm_key())
        .is_some_and(|k| !k.trim().is_empty());
    emit(cb, &json!({ "id": null, "event": "Settings", "settings": {
        "countdown_secs": s.countdown_secs,
        "attendance_gate_percent": s.attendance_gate_percent,
        "llm_endpoint": s.llm_endpoint,
        "llm_model": s.llm_model,
        "llm_max_tokens": s.llm_max_tokens,
        "resubmit_for_correct": s.resubmit_for_correct,
        "enable_llm_tools": s.enable_llm_tools,
        "has_llm_key": has_llm_key,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static TEST_EVENTS: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();

    extern "C" fn collect_event(ptr: *const u8, len: usize) {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        if let Ok(value) = serde_json::from_slice(bytes) {
            TEST_EVENTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap()
                .push(value);
        }
    }

    #[test]
    fn monitor_lifecycle_rejects_duplicate_start_while_starting() {
        let mut lifecycle = MonitorLifecycle::Idle;

        assert!(lifecycle.begin_start(1, 101).is_ok());
        assert_eq!(
            lifecycle.begin_start(2, 102),
            Err("monitor is already starting")
        );
        assert!(lifecycle.is_starting(1));
    }

    #[test]
    fn caps_never_advertise_an_unimplemented_self_updater() {
        let caps = caps_payload();
        assert_eq!(caps["caps"]["self_update"], false);
        assert_eq!(caps["caps"]["background_monitoring"], true);
    }

    #[test]
    fn cancelling_start_invalidates_stale_generation() {
        let mut lifecycle = MonitorLifecycle::Idle;
        lifecycle.begin_start(1, 101).unwrap();

        match lifecycle.take_for_stop() {
            StoppedMonitor::Starting(starting) => {
                assert_eq!(starting.command_id, 101);
                assert!(starting.task.is_none());
            }
            _ => panic!("starting lifecycle must be cancelled as Starting"),
        }

        lifecycle.begin_start(2, 102).unwrap();
        assert!(!lifecycle.is_starting(1), "generation 1 completion is stale");
        assert!(lifecycle.is_starting(2), "generation 2 remains authoritative");
    }

    #[test]
    fn running_handle_is_removed_exactly_once() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle = MonitorLifecycle::Running(RunningMonitor {
            generation: 7,
            handle: monitor::MonitorHandle { tx, tasks: vec![] },
        });

        assert!(lifecycle.running_handle().is_some());
        assert!(matches!(
            lifecycle.take_for_stop(),
            StoppedMonitor::Running(_)
        ));
        assert!(matches!(
            lifecycle.take_for_stop(),
            StoppedMonitor::Idle
        ));
    }

    #[test]
    fn stopping_during_start_completes_pending_command() {
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events.lock().unwrap().clear();
        let mut lifecycle = MonitorLifecycle::Idle;
        lifecycle.begin_start(1, 501).unwrap();

        stop_monitor(&mut lifecycle, collect_event, "cancelled by test");

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| {
            event["event"] == "Reply"
                && event["id"] == 501
                && event["ok"] == false
                && event["error"] == "cancelled by test"
        }));
        assert!(matches!(lifecycle, MonitorLifecycle::Idle));
    }

    #[test]
    fn corrupt_vault_is_preserved_instead_of_recreated() {
        let dir = std::env::temp_dir().join(format!(
            "tron-corrupt-vault-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();
        drop(VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap());
        let corrupt = b"existing vault evidence".to_vec();
        std::fs::write(dir.join("vault.bin"), &corrupt).unwrap();

        assert!(open_vault_auto(&dir, None).is_err());
        assert_eq!(
            std::fs::read(dir.join("vault.bin")).unwrap(),
            corrupt,
            "opening a corrupt vault must never replace its bytes"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn supplied_os_key_never_creates_a_plaintext_device_key_file() {
        let dir = std::env::temp_dir().join(format!(
            "tron-os-key-vault-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key = std::array::from_fn::<_, 32, _>(|index| index as u8);

        drop(open_vault_auto(&dir, Some(key)).unwrap());
        assert!(!dir.join("device.key").exists());
        drop(open_vault_auto(&dir, Some(key)).unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_os_key_fails_init_instead_of_reporting_boot_success() {
        const COMMAND_ID: u64 = 9_223_372_036_854_700_001;
        let dir = std::env::temp_dir().join(format!(
            "tron-invalid-os-key-init-{}",
            crate::config::new_id()
        ));
        let core = init(collect_event);
        send(
            &core,
            format!(
                r#"{{"id":{COMMAND_ID},"cmd":"Init","data_dir":{},"device_key_b64":"invalid"}}"#,
                serde_json::to_string(&dir).unwrap()
            )
            .as_bytes(),
        );

        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap();
        assert!(events.iter().any(|event| {
            event["event"] == "Reply"
                && event["id"] == COMMAND_ID
                && event["ok"] == false
                && event["error"].as_str().is_some_and(|error| error.contains("device key"))
        }));
        assert!(!dir.join("device.key").exists());
        drop(events);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn account_transaction_recovery_is_idempotent_for_add_and_delete() {
        for (mutation, account_exists, expect_secret) in [
            (AccountMutation::Add, false, false),
            (AccountMutation::Add, true, true),
            (AccountMutation::Delete, false, false),
            (AccountMutation::Delete, true, true),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "tron-account-recovery-{}",
                crate::config::new_id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();
            let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
            vault
                .set(
                    "acc",
                    AccountSecret {
                        password: "secret".into(),
                        cookies: String::new(),
                    },
                )
                .unwrap();
            let mut config = Config::default();
            if account_exists {
                config.accounts.push(AccountMeta {
                    id: "acc".into(),
                    label: "account".into(),
                    school_ref: "school".into(),
                    username: "user".into(),
                    device_id: "device".into(),
                    is_teacher: false,
                    course_id: None,
                });
            }
            AccountJournal::begin(&dir, mutation, "acc").unwrap();

            assert!(recover_account_transaction(&dir, &config, &mut vault)
                .unwrap()
                .is_some());
            assert_eq!(vault.get("acc").is_some(), expect_secret);
            assert!(AccountJournal::load(&dir).unwrap().is_none());
            assert!(recover_account_transaction(&dir, &config, &mut vault)
                .unwrap()
                .is_none());
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
