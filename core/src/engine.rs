//! The core state machine: owns the tokio runtime, the event callback, a long-lived heartbeat,
//! and (after `Init`) the registry / config / vault behind one mutex. Commands lock, mutate,
//! persist, and emit; the one async command (Login) snapshots what it needs, drops the lock,
//! does the network round-trip, then re-locks to cache cookies. Secrets never enter an event.

use crate::config::{new_id, AccountMeta, Config, Operating, Settings};
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
use std::collections::{HashMap, HashSet};
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
    vault: Option<VaultFile>, // Some(..) once unlocked
    monitor: MonitorLifecycle,
    monitor_generation: u64,
    /// In-flight captcha logins: account_id → the channel that delivers the user's typed answer.
    pending_captcha: HashMap<String, oneshot::Sender<String>>,
    /// Same-account login single-flight: account ids with a Login task currently in flight. A second
    /// Login for the same account is rejected until the first finishes; every terminal path removes it.
    login_in_flight: HashSet<String>,
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

    /// True while a monitor start is in progress or a monitor is running. Account mutations and
    /// manual logins are rejected in this window (the UI must stop monitoring first); SwitchAccount
    /// stays usable so the user can pick another account before stopping.
    fn is_active(&self) -> bool {
        matches!(self, Self::Starting(_) | Self::Running(_))
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
fn open_vault_auto(dir: &std::path::Path, host_key: Option<[u8; 32]>) -> Result<VaultFile, String> {
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
    emit(
        cb,
        &json!({ "id": id, "event": "Reply", "ok": ok, "error": error }),
    );
}

/// Fixed seam-error text for commands that fail to parse. Deliberately constant: a malformed command
/// may embed secret-shaped payloads in the very fields that fail to parse, so the error must never
/// reflect input content (no serde literals).
const MALFORMED_COMMAND: &str = "未知或格式錯誤的命令";

/// Fixed error text after a caught internal panic. The input is never echoed — it may carry secrets.
const INTERNAL_ERROR: &str = "核心內部錯誤";

impl Core {
    /// Build the runtime and a fresh core. The only fallible step is the Tokio runtime build
    /// (resource exhaustion); the failure must surface to the FFI caller as a null handle, not abort
    /// the host.
    fn new(cb: EventCb) -> Result<Box<Core>, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("build tokio runtime: {error}"))?;

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

        Ok(Box::new(Core {
            rt,
            cb,
            state: Arc::new(Mutex::new(None)),
        }))
    }
}

pub fn init(cb: EventCb) -> Result<Box<Core>, String> {
    let core = Core::new(cb)?;
    emit(
        cb,
        &json!({ "id": null, "event": "StateChanged", "state": "starting" }),
    );
    Ok(core)
}

/// Lock the core state, recovering from a poisoned mutex. A panic caught at the FFI seam unwinds
/// through a held guard and poisons the mutex; the state itself stays coherent, so the core must
/// keep serving commands instead of failing every one from then on.
fn lock_state(state: &Mutex<Option<CoreState>>) -> std::sync::MutexGuard<'_, Option<CoreState>> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Complete the awaiting command after a panic was caught at the FFI seam. Replies with a FIXED
/// error — the input is never echoed (it may contain secrets). Recovers the correlation id from the
/// raw JSON when possible so the awaiting UI call completes; otherwise emits an uncorrelated Error
/// event so the caller never hangs on a lost reply.
pub fn panic_reply(core: &Core, json_bytes: &[u8]) {
    let cb = core.cb;
    match serde_json::from_slice::<Value>(json_bytes)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_u64))
    {
        Some(id) => reply(cb, id, false, Some(INTERNAL_ERROR.to_string())),
        None => emit(
            cb,
            &json!({ "id": null, "event": "Error", "severity": "error",
                                 "code": "core_panicked", "message": INTERNAL_ERROR }),
        ),
    }
}

pub fn send(core: &Core, json_bytes: &[u8]) {
    let cb = core.cb;
    let cmd: Command = match serde_json::from_slice(json_bytes) {
        Ok(c) => c,
        // Fixed message, never the serde literal: a malformed command may carry a secret-shaped
        // payload (e.g. a numeric `key`) in the very field that fails to parse.
        Err(_) => {
            // Recover the correlation id from the raw JSON so the awaiting UI call always completes
            // (never hangs/leaks) — a Reply(ok:false) surfaces as an error toast. Only when the id is
            // unreadable (truly malformed JSON) do we fall back to an uncorrelated Error event.
            match serde_json::from_slice::<Value>(json_bytes)
                .ok()
                .and_then(|v| v.get("id").and_then(Value::as_u64))
            {
                Some(id) => reply(cb, id, false, Some(MALFORMED_COMMAND.to_string())),
                None => emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                                         "code": "bad_command", "message": MALFORMED_COMMAND }),
                ),
            }
            return;
        }
    };

    match cmd {
        Command::Login { id, account_id } => spawn_login(core, id, account_id),
        Command::StartMonitoring { id } => spawn_start_monitoring(core, id),
        Command::ImportCookies {
            id,
            account_id,
            cookies_json,
        } => spawn_import_cookies(core, id, account_id, cookies_json),
        other => handle_sync(core, other),
    }
}

/// Everything except Login is quick and runs inline under the state lock.
fn handle_sync(core: &Core, cmd: Command) {
    let cb = core.cb;
    let id = cmd.id();
    let mut guard = lock_state(&core.state);

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
                // Fixed message — the loader's error bundles the serde literal, which can echo file
                // content verbatim; nothing from the file may cross the seam.
                Err(_) => {
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "providers_unavailable", "message": "providers registry unavailable" }),
                    );
                    return reply(
                        cb,
                        id,
                        false,
                        Some("providers registry unavailable".to_string()),
                    );
                }
            };
            let config_path = dir.join("config.json");
            let (config, config_healthy) = match Config::load(&config_path) {
                Ok(config) => (config, true),
                // Fixed message, never the loader's serde literal: it can echo file content verbatim.
                Err(_) => {
                    let recovery = match Config::quarantine(&config_path) {
                        Ok(path) => format!(
                            "；原檔已保留為 {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Err(quarantine_error) => format!("；原檔未移動：{quarantine_error}"),
                    };
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "config_corrupt", "message": format!("設定檔損毀，已改用預設設定{recovery}") }),
                    );
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
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                                      "code": "vault_open_failed", "message": e }),
                    );
                    (None, Some(e))
                }
            };
            if config_healthy {
                if let Some(vault) = vault.as_mut() {
                    match recover_account_transaction(&dir, &config, vault) {
                        Ok(Some(message)) => emit(
                            cb,
                            &json!({ "id": null, "event": "LogLine",
                            "level": "warn", "text": message }),
                        ),
                        Ok(None) => {}
                        // Fixed message — the journal loader's error bundles a serde literal.
                        Err(_) => emit(
                            cb,
                            &json!({ "id": null, "event": "Error", "severity": "error",
                            "code": "account_transaction_recovery_failed",
                            "message": "account transaction journal unreadable; recovery skipped" }),
                        ),
                    }
                }
            }
            let unlocked = vault.is_some();
            let state = CoreState {
                data_dir: dir,
                registry,
                config,
                vault,
                monitor: MonitorLifecycle::Idle,
                monitor_generation: 0,
                pending_captcha: HashMap::new(),
                login_in_flight: HashSet::new(),
            };
            // Emit the whole snapshot from the local while the state lock is held (commands are
            // serialized on this mutex, so no command can observe an uninstalled state in between),
            // then install before handle_sync returns. No unwrap needed: `state` is a plain local.
            emit_providers(cb, &state);
            emit_accounts(cb, &state);
            emit_settings(cb, &state);
            emit(
                cb,
                &json!({ "id": null, "event": "VaultState", "exists": unlocked, "unlocked": unlocked }),
            );
            emit_caps(cb);
            emit(
                cb,
                &json!({ "id": null, "event": "StateChanged", "state": "idle" }),
            );
            reply(cb, id, vault_error.is_none(), vault_error);
            *guard = Some(state);
        }

        // The vault auto-unlocks with the device key at Init (no master password), so CreateVault and
        // Unlock are idempotent no-ops now — kept only for wire back-compat. Reply ok iff it is open.
        Command::CreateVault { .. } | Command::Unlock { .. } => {
            let ready = guard.as_ref().is_some_and(|st| st.vault.is_some());
            reply(cb, id, ready, (!ready).then(|| "vault not ready".into()));
        }

        Command::AddAccount {
            label,
            school,
            username,
            password,
            is_teacher,
            course_id,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if st.vault.is_none() {
                return reply(cb, id, false, Some("vault is locked".into()));
            }
            if st.monitor.is_active() {
                return reply(cb, id, false, Some("stop monitoring first".into()));
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
                AccountSecret {
                    password,
                    cookies: String::new(),
                },
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
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "warn",
                    "code": "account_transaction_cleanup_failed", "message": error }),
                );
            }
            emit_accounts(cb, st);
            reply(cb, id, true, None);
        }

        Command::SwitchAccount { account_id, .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
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
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if st.config.account(&account_id).is_none() {
                return reply(cb, id, false, Some("no such account".into()));
            }
            if st.vault.is_none() {
                return reply(cb, id, false, Some("vault is locked".into()));
            }
            if st.monitor.is_active() {
                return reply(cb, id, false, Some("stop monitoring first".into()));
            }
            // Cancel any pending captcha challenge for this account: dropping the sender wakes the
            // awaiting login task with a closed channel, which it treats as a cancelled login (and
            // releases its single-flight marker). Drop the marker too, so a fresh login can start
            // the moment the account is re-added.
            st.pending_captcha.remove(&account_id);
            st.login_in_flight.remove(&account_id);
            if let Err(error) =
                AccountJournal::begin(&st.data_dir, AccountMutation::Delete, &account_id)
            {
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
                return reply(
                    cb,
                    id,
                    false,
                    Some(format!("delete vault entry failed: {vault_error}")),
                );
            }
            if let Err(error) = AccountJournal::complete(&st.data_dir) {
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "warn",
                    "code": "account_transaction_cleanup_failed", "message": error }),
                );
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
            emit(
                cb,
                &json!({ "id": null, "event": "StateChanged", "state": "idle" }),
            );
            reply(cb, id, true, None);
        }

        Command::SignNow { activity_token, .. } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::SignNow {
                    command_id: id,
                    activity_token,
                },
            );
        }
        Command::DeferSignIn { activity_token, .. } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::Defer {
                    command_id: id,
                    activity_token,
                },
            );
        }

        Command::SubmitNow { activity_token, .. } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::QuizSubmitNow {
                    command_id: id,
                    activity_token,
                },
            );
        }
        Command::HoldAnswer { activity_token, .. } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::QuizHold {
                    command_id: id,
                    activity_token,
                },
            );
        }
        Command::DiscardAnswer { activity_token, .. } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::QuizDiscard {
                    command_id: id,
                    activity_token,
                },
            );
        }
        Command::SetAnswer {
            activity_token,
            account_id,
            subject_id,
            answer,
            ..
        } => {
            route_to_monitor(
                cb,
                guard.as_ref(),
                id,
                monitor::MonitorMsg::QuizSetAnswer {
                    command_id: id,
                    activity_token,
                    account_id,
                    subject_id,
                    answer,
                },
            );
        }

        Command::SetLlmKey { mut key, .. } => {
            use zeroize::Zeroize;
            let Some(st) = guard.as_mut() else {
                key.zeroize(); // the key never reached the vault — wipe the intermediate before drop
                return reply(cb, id, false, Some("not initialized".into()));
            };
            let result = match st.vault.as_mut() {
                Some(v) => v.set_llm_key(key),
                None => {
                    key.zeroize();
                    Err("vault is locked".into())
                }
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

        Command::SubmitCaptcha {
            account_id, text, ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            match st.pending_captcha.remove(&account_id) {
                Some(txc) => {
                    let _ = txc.send(text); // wakes the awaiting login task
                    reply(cb, id, true, None);
                }
                None => reply(
                    cb,
                    id,
                    false,
                    Some("no captcha pending for this account".into()),
                ),
            }
        }

        Command::UpdateConfig { patch, .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            // Transaction: patch a CLONE, validate everything (per-field strict types, unknown-key
            // rejection, strict `operating` parse, then the COMPLETE post-patch ranges + cross-field
            // invariants + endpoint URL shape), persist the clone, and only then swap it in. A failed
            // patch leaves memory AND disk untouched.
            let mut next = st.config.clone();
            if let Err(error) = apply_config_patch(&mut next.settings, &patch) {
                return reply(cb, id, false, Some(error));
            }
            if let Err(error) = validate_config(&next.settings) {
                return reply(cb, id, false, Some(error));
            }
            if let Err(error) = next.save(&st.config_path()) {
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            crate::redaction::set_level(&st.config.settings.log_level);
            push_config(st); // a running monitor adopts the change live (no stop/start)
            emit_settings(cb, st); // echo the applied settings so the UI reflects the saved values
            reply(cb, id, true, None);
        }

        Command::Shutdown { .. } => {
            if let Some(st) = guard.as_mut() {
                stop_monitor(&mut st.monitor, cb, "shutdown cancelled monitor start");
                // Drop captcha senders so their login tasks wake immediately, and prevent stale
                // single-flight markers from keeping credentials alive after shutdown.
                st.pending_captcha.clear();
                st.login_in_flight.clear();
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
            // Stop the actor (it emits `idle`) and cancel the whole task group — pollers, actor, and
            // every tracked network helper (brute-force signs, QR assist, quiz prepare/submit) stop at
            // their next await point; a helper spawned by a racing actor message aborts immediately.
            handle.stop();
        }
    }
}

/// Login: snapshot (base_url, username, password, cached cookies) under the lock, release it, do
/// the async round-trip, then re-lock to persist refreshed cookies. Reuses a cached session if it
/// still verifies, so we don't re-login unnecessarily.
///
/// Lifecycle contract: progress is per-account `AccountStatus` (logging_in → online / login_failed),
/// never a global `StateChanged`. One Login per account at a time (`login_in_flight`); the marker is
/// released on every terminal path (success, failure, captcha timeout, captcha cancelled). A Delete
/// issued mid-flight cancels the pending captcha, and the persist step re-checks (under the same
/// lock it writes under) that the account still exists — a stale completion must not resurrect the
/// secret of a deleted account.
fn spawn_login(core: &Core, id: u64, account_id: String) {
    let cb = core.cb;
    let state = core.state.clone();

    // Snapshot under the lock (no await while holding it).
    let snap = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        if st.monitor.is_active() {
            return reply(cb, id, false, Some("stop monitoring first".into()));
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

    core.rt.spawn(async move {
        // RAII is the panic/abort backstop; explicit clears below keep terminal paths obvious.
        let _flight_guard = LoginFlightGuard::new(state.clone(), account_id.clone());
        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id, "state": "logging_in" }));
        emit(cb, &json!({ "id": null, "event": "LogLine", "level": "info",
                          "text": format!("login → {base_url}") })); // base_url only, never creds

        let endpoints = Endpoints::derive(&base_url);
        let (client, jar) = match build_client(&cached_cookies) {
            Ok(pair) => pair,
            Err(error) => {
                emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                                  "state": "login_failed", "error": error }));
                emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": error }));
                clear_login_in_flight(&state, &account_id);
                return;
            }
        };

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
                    {
                        let mut guard = lock_state(&state);
                        if let Some(st) = guard.as_mut() {
                            st.pending_captcha.insert(account_id.clone(), txc);
                        }
                    }
                    emit(cb, &json!({ "id": null, "event": "CaptchaChallenge",
                                      "account_id": account_id, "image_b64": login::encode_base64(&image_bytes) }));
                    match tokio::time::timeout(Duration::from_secs(180), rxc).await {
                        Ok(Ok(text)) => login::complete_captcha(&client, &endpoints, pending, &text).await.map(|_| false),
                        // DeleteAccount dropped the pending sender: the challenge is withdrawn.
                        Ok(Err(_)) => Err("login cancelled because the account was deleted".to_string()),
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
                let outcome = persist_login_session(&state, &account_id, AccountSecret { password, cookies });
                match outcome {
                    Ok(PersistOutcome::Saved) => {
                        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id, "state": "online" }));
                        emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": true,
                            "detail": if from_cache { "session restored from cache" } else { "logged in" } }));
                    }
                    Ok(PersistOutcome::AccountGone) => {
                        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                                          "state": "login_failed", "error": "account was deleted during login" }));
                        emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false,
                            "reason": "login succeeded but the account was deleted during login" }));
                    }
                    Err(error) => {
                        emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                                          "state": "login_failed", "error": error }));
                        emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false,
                            "reason": format!("login succeeded but session persistence failed: {error}") }));
                    }
                }
                clear_login_in_flight(&state, &account_id);
            }
            Err(e) => {
                // One report only: the correlated LoginResult(ok:false) already surfaces `reason` as a
                // toast+log via AppState.Send's error path; a second id:null Error would double it.
                emit(cb, &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                                  "state": "login_failed", "error": e }));
                emit(cb, &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": e }));
                clear_login_in_flight(&state, &account_id);
            }
        }
    });
}

/// Build an authenticated client: reuse a cached session if it still verifies, else log in.
/// Returns the client + refreshed cookie JSON to cache.
async fn authed_client(
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
            // A captcha needs a human; monitoring startup can't prompt. Log in interactively first
            // (which caches the session), then StartMonitoring reuses the cached cookies.
            LoginOutcome::NeedCaptcha { .. } => {
                return Err("需要圖形驗證碼，請先用 Login 登入一次".into())
            }
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
        let _ = h.tx.send(monitor::MonitorMsg::ConfigUpdated(Box::new(
            monitor_config(st),
        )));
    }
}

/// Start concurrent monitoring: authenticate every account, then hand ready sessions to the monitor.
fn spawn_start_monitoring(core: &Core, id: u64) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        let Some(vault) = st.vault.as_ref() else {
            return reply(cb, id, false, Some("vault is locked".into()));
        };
        let mut accts = Vec::new();
        for acc in &st.config.accounts {
            let Some(base_url) = st.registry.resolve(&acc.school_ref) else {
                continue;
            };
            warn_insecure_http(cb, &base_url);
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
        emit(
            cb,
            &json!({ "id": null, "event": "StateChanged", "state": "starting" }),
        );
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

        let mut guard = lock_state(&task_state);
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
    });

    let mut guard = lock_state(&state);
    match guard.as_mut() {
        Some(st) => st.monitor.attach_start_task(generation, task),
        None => task.abort(),
    }
}

fn monitor_start_is_current(state: &Arc<Mutex<Option<CoreState>>>, generation: u64) -> bool {
    lock_state(state)
        .as_ref()
        .is_some_and(|st| st.monitor.is_starting(generation))
}

/// Import a supplied cookie set for an account → store in vault → verify (browser-cookie login).
fn spawn_import_cookies(core: &Core, id: u64, account_id: String, cookies_json: String) {
    let cb = core.cb;
    let state = core.state.clone();
    let snap = {
        let guard = lock_state(&state);
        let Some(st) = guard.as_ref() else {
            return reply(cb, id, false, Some("not initialized".into()));
        };
        if st.monitor.is_active() {
            return reply(cb, id, false, Some("stop monitoring first".into()));
        }
        let Some(vault) = st.vault.as_ref() else {
            return reply(cb, id, false, Some("vault is locked".into()));
        };
        let Some(acc) = st.config.account(&account_id) else {
            return reply(cb, id, false, Some("no such account".into()));
        };
        let Some(base_url) = st.registry.resolve(&acc.school_ref) else {
            return reply(cb, id, false, Some("unknown school".into()));
        };
        warn_insecure_http(cb, &base_url);
        let (password, _) = vault.get(&account_id).unwrap_or_default().into_parts();
        (base_url, password)
    };
    let (base_url, password) = snap;

    core.rt.spawn(async move {
        let endpoints = Endpoints::derive(&base_url);
        let (verified, client_error) = match build_client(&cookies_json) {
            Ok((client, _jar)) => (login::verify_session(&client, &endpoints).await, None),
            Err(error) => (false, Some(error)),
        };
        let (ok, error) = if !verified {
            (
                false,
                client_error.or_else(|| Some("imported cookies did not verify".into())),
            )
        } else {
            // Same guard as Login: store the imported session only while the account still exists —
            // a Delete committed during the verify round-trip must not resurrect the secret.
            match persist_login_session(
                &state,
                &account_id,
                AccountSecret {
                    password,
                    cookies: cookies_json,
                },
            ) {
                Ok(PersistOutcome::Saved) => (true, None),
                Ok(PersistOutcome::AccountGone) => {
                    (false, Some("account was deleted during import".into()))
                }
                Err(error) => (
                    false,
                    Some(format!("cookies verified but persistence failed: {error}")),
                ),
            }
        };
        emit(
            cb,
            &json!({ "id": null, "event": "AccountStatus", "account_id": account_id,
                          "state": if ok { "online" } else { "login_failed" },
                          "error": error.as_deref() }),
        );
        reply(cb, id, ok, error);
    });
}

fn emit_providers(cb: EventCb, st: &CoreState) {
    emit(
        cb,
        &json!({ "id": null, "event": "Providers",
                      "default_key": st.registry.default_key,
                      "schools": st.registry.schools }),
    );
}

fn emit_accounts(cb: EventCb, st: &CoreState) {
    emit(
        cb,
        &json!({ "id": null, "event": "Accounts",
                      "active": st.config.active_account,
                      "accounts": st.config.accounts }),
    );
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

/// Apply a settings patch to `settings` (a caller-owned clone). Strict per-field typing: a
/// wrong-typed value or an unknown key is an error, never a silent ignore; `operating` parses
/// strictly so a malformed schedule fails the whole patch.
fn apply_config_patch(settings: &mut Settings, patch: &Value) -> Result<(), String> {
    let patch = patch
        .as_object()
        .ok_or_else(|| "config patch 必須是物件".to_string())?;
    for (key, value) in patch {
        match key.as_str() {
            "countdown_secs" => settings.countdown_secs = u64_field(key, value)?,
            "attendance_gate_percent" => settings.attendance_gate_percent = f64_field(key, value)?,
            "llm_endpoint" => settings.llm_endpoint = str_field(key, value)?,
            "llm_model" => settings.llm_model = str_field(key, value)?,
            "llm_max_tokens" => settings.llm_max_tokens = u32_field(key, value)?,
            "resubmit_for_correct" => settings.resubmit_for_correct = bool_field(key, value)?,
            "max_answer_reask" => settings.max_answer_reask = u32_field(key, value)?,
            "prepare_retry_budget_secs" => {
                settings.prepare_retry_budget_secs = u64_field(key, value)?
            }
            "autoanswer_types" => settings.autoanswer_types = str_vec_field(key, value)?,
            "enable_llm_tools" => settings.enable_llm_tools = bool_field(key, value)?,
            "max_tool_iterations" => settings.max_tool_iterations = u32_field(key, value)?,
            "radar_strategy" => settings.radar_strategy = str_vec_field(key, value)?,
            "number_concurrency" => settings.number_concurrency = u32_field(key, value)?,
            "number_min_concurrency" => settings.number_min_concurrency = u32_field(key, value)?,
            "number_cooldown_ms" => settings.number_cooldown_ms = u64_field(key, value)?,
            "number_max_cooldowns" => settings.number_max_cooldowns = u32_field(key, value)?,
            "poll_idle_secs" => settings.poll_idle_secs = u64_field(key, value)?,
            "quiz_detect_secs" => settings.quiz_detect_secs = u64_field(key, value)?,
            "log_level" => settings.log_level = str_field(key, value)?,
            "operating" => {
                settings.operating = serde_json::from_value::<Operating>(value.clone())
                    .map_err(|error| format!("operating 無效：{error}"))?
            }
            "tz_offset_minutes" => settings.tz_offset_minutes = i64_field(key, value)?,
            _ => return Err(format!("unknown config field: {key}")),
        }
    }
    Ok(())
}

fn u64_field(key: &str, value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{key} 必須是非負整數"))
}
fn i64_field(key: &str, value: &Value) -> Result<i64, String> {
    value.as_i64().ok_or_else(|| format!("{key} 必須是整數"))
}
fn u32_field(key: &str, value: &Value) -> Result<u32, String> {
    u32::try_from(u64_field(key, value)?).map_err(|_| format!("{key} 超出範圍"))
}
fn f64_field(key: &str, value: &Value) -> Result<f64, String> {
    value.as_f64().ok_or_else(|| format!("{key} 必須是數字"))
}
fn bool_field(key: &str, value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| format!("{key} 必須是布林"))
}
fn str_field(key: &str, value: &Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{key} 必須是字串"))
}
fn str_vec_field(key: &str, value: &Value) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("{key} 必須是字串陣列"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key} 必須全是字串"))
        })
        .collect()
}

/// Validate the COMPLETE post-patch settings — every range, the cross-field min<=max invariant, and
/// the LLM endpoint URL shape — on the clone before anything is saved. Patch-only checks are not
/// enough: a one-field patch must not be able to push `number_min_concurrency` above the CURRENT
/// `number_concurrency`.
fn validate_config(settings: &Settings) -> Result<(), String> {
    let range = |name: &str, value: u64, min: u64, max: u64| -> Result<(), String> {
        if !(min..=max).contains(&value) {
            return Err(format!("{name} 必須介於 {min} 與 {max}"));
        }
        Ok(())
    };
    range("countdown_secs", settings.countdown_secs, 1, 86_400)?;
    if !settings.attendance_gate_percent.is_finite()
        || !(0.0..=100.0).contains(&settings.attendance_gate_percent)
    {
        return Err("attendance_gate_percent 必須介於 0 與 100".to_string());
    }
    if !valid_http_url(&settings.llm_endpoint) {
        return Err("llm_endpoint 必須是有效的 http(s) URL".to_string());
    }
    range(
        "llm_max_tokens",
        u64::from(settings.llm_max_tokens),
        1,
        1_000_000,
    )?;
    range(
        "max_answer_reask",
        u64::from(settings.max_answer_reask),
        1,
        100,
    )?;
    range(
        "prepare_retry_budget_secs",
        settings.prepare_retry_budget_secs,
        1,
        86_400,
    )?;
    range(
        "max_tool_iterations",
        u64::from(settings.max_tool_iterations),
        0,
        100,
    )?;
    range(
        "number_concurrency",
        u64::from(settings.number_concurrency),
        1,
        256,
    )?;
    range(
        "number_min_concurrency",
        u64::from(settings.number_min_concurrency),
        1,
        256,
    )?;
    if settings.number_min_concurrency > settings.number_concurrency {
        return Err("number_min_concurrency 不得大於 number_concurrency".to_string());
    }
    range(
        "number_cooldown_ms",
        settings.number_cooldown_ms,
        1,
        3_600_000,
    )?;
    range(
        "number_max_cooldowns",
        u64::from(settings.number_max_cooldowns),
        0,
        1_000,
    )?;
    range("poll_idle_secs", settings.poll_idle_secs, 1, 86_400)?;
    range("quiz_detect_secs", settings.quiz_detect_secs, 1, 86_400)?;
    if !(-840..=840).contains(&settings.tz_offset_minutes) {
        return Err("tz_offset_minutes 必須介於 -840 與 840".to_string());
    }
    Ok(())
}

/// Minimal URL sanity for the LLM endpoint: an http(s) scheme with a non-empty, whitespace-free
/// target. Plain `http://` is allowed (local LLM servers); only the scheme/shape is enforced.
fn valid_http_url(url: &str) -> bool {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .is_some_and(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace))
}

/// Re-lock and cache a refreshed session — but only while the account still exists in config. The
/// existence check and the vault write happen under the SAME lock, so a Delete committed while the
/// network round-trip was in flight cannot race past it; on `AccountGone` nothing is written and a
/// deleted account is never resurrected by a stale async completion.
fn persist_login_session(
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

enum PersistOutcome {
    /// The refreshed session was written to the vault.
    Saved,
    /// The account was deleted while the round-trip was in flight: nothing was written.
    AccountGone,
}

struct LoginFlightGuard {
    state: Arc<Mutex<Option<CoreState>>>,
    account_id: String,
}

impl LoginFlightGuard {
    fn new(state: Arc<Mutex<Option<CoreState>>>, account_id: String) -> Self {
        Self { state, account_id }
    }
}

impl Drop for LoginFlightGuard {
    fn drop(&mut self) {
        clear_login_in_flight(&self.state, &self.account_id);
    }
}

/// Remove the same-account login single-flight marker. Called exactly once per login task on every
/// terminal path (success, failure, captcha timeout, captcha cancelled).
fn clear_login_in_flight(state: &Arc<Mutex<Option<CoreState>>>, account_id: &str) {
    let mut guard = lock_state(state);
    if let Some(st) = guard.as_mut() {
        st.login_in_flight.remove(account_id);
    }
}

/// Advisory-only warning for a plain-HTTP school endpoint (never blocks — test/intranet deployments
/// legitimately run on http). `base_url` is already resolved by the caller.
fn warn_insecure_http(cb: EventCb, base_url: &str) {
    if base_url.starts_with("http://") {
        emit(
            cb,
            &json!({ "id": null, "event": "LogLine", "level": "warn",
                          "text": "學校網址使用未加密 HTTP，建議改用 HTTPS" }),
        );
    }
}

fn emit_settings(cb: EventCb, st: &CoreState) {
    let s = &st.config.settings;
    let has_llm_key = st
        .vault
        .as_ref()
        .and_then(|v| v.get_llm_key())
        .is_some_and(|k| !k.trim().is_empty());
    emit(
        cb,
        &json!({ "id": null, "event": "Settings", "settings": {
            "countdown_secs": s.countdown_secs,
            "attendance_gate_percent": s.attendance_gate_percent,
            "llm_endpoint": s.llm_endpoint,
            "llm_model": s.llm_model,
            "llm_max_tokens": s.llm_max_tokens,
            "resubmit_for_correct": s.resubmit_for_correct,
            "enable_llm_tools": s.enable_llm_tools,
            "has_llm_key": has_llm_key,
        }}),
    );
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
    let client = Client::builder()
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

fn dump_cookies(jar: &CookieStoreMutex) -> String {
    let store = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
                .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    #[tokio::test]
    async fn school_client_refuses_cross_origin_private_redirects_but_follows_same_origin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A public school must never be able to redirect the authed client into a private host:
        // 302 → http://127.0.0.1:<other-port> is cross-origin AND a loopback literal → refused.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let (hit_tx, hit_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let accepted = target.accept().await.is_ok();
            let _ = hit_tx.send(accepted);
        });
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let (client, _jar) = build_client("").unwrap();
        let result = client
            .get(format!("http://{source_addr}/start"))
            .send()
            .await;
        assert!(
            result.is_err(),
            "a cross-origin redirect to a loopback literal must be refused"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), hit_rx)
                .await
                .is_err(),
            "the private redirect target must never be contacted"
        );

        // Same-origin redirects (an intranet school redirecting within itself) keep working.
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = "HTTP/1.1 302 Found\r\nLocation: /landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
            let (mut stream2, _) = source.accept().await.unwrap();
            let mut request2 = [0_u8; 4096];
            let _ = stream2.read(&mut request2).await;
            let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            stream2.write_all(ok.as_bytes()).await.unwrap();
        });
        let (client, _jar) = build_client("").unwrap();
        let response = client
            .get(format!("http://{source_addr}/start"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "same-origin redirects are followed"
        );
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
        assert!(
            !lifecycle.is_starting(1),
            "generation 1 completion is stale"
        );
        assert!(
            lifecycle.is_starting(2),
            "generation 2 remains authoritative"
        );
    }

    #[test]
    fn running_handle_is_removed_exactly_once() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle = MonitorLifecycle::Running(RunningMonitor {
            generation: 7,
            handle: monitor::MonitorHandle::new(tx),
        });

        assert!(lifecycle.running_handle().is_some());
        assert!(matches!(
            lifecycle.take_for_stop(),
            StoppedMonitor::Running(_)
        ));
        assert!(matches!(lifecycle.take_for_stop(), StoppedMonitor::Idle));
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
        let dir =
            std::env::temp_dir().join(format!("tron-corrupt-vault-{}", crate::config::new_id()));
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
        let dir =
            std::env::temp_dir().join(format!("tron-os-key-vault-{}", crate::config::new_id()));
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
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(
                r#"{{"id":{COMMAND_ID},"cmd":"Init","data_dir":{},"device_key_b64":"invalid"}}"#,
                serde_json::to_string(&dir).unwrap()
            )
            .as_bytes(),
        );

        let events = TEST_EVENTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap();
        assert!(events.iter().any(|event| {
            event["event"] == "Reply"
                && event["id"] == COMMAND_ID
                && event["ok"] == false
                && event["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("device key"))
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
            let dir = std::env::temp_dir()
                .join(format!("tron-account-recovery-{}", crate::config::new_id()));
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

    #[test]
    fn core_init_with_null_callback_returns_null_handle() {
        let handle = crate::core_init(None);
        assert!(
            handle.is_null(),
            "null callback must yield a null handle, never a live core"
        );
    }

    #[test]
    fn poisoned_state_lock_recovers_instead_of_wedging_the_core() {
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events.lock().unwrap().clear();
        let core = init(collect_event).unwrap();

        // A panic while holding the state lock poisons it exactly like a caught seam panic would.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = core.state.lock().unwrap();
            panic!("deliberate test poison");
        }))
        .ok();

        const ID: u64 = 424_242;
        send(
            &core,
            format!(r#"{{"id":{ID},"cmd":"CreateVault"}}"#).as_bytes(),
        );
        let events = events.lock().unwrap();
        let reply = events
            .iter()
            .find(|e| e["event"] == "Reply" && e["id"] == ID)
            .expect("a command must complete despite a poisoned lock");
        assert_eq!(reply["ok"], false);
        assert_eq!(
            reply["error"], "vault not ready",
            "the command must run normally (poison recovered), not fail with the seam panic error"
        );
    }

    #[test]
    fn malformed_command_errors_never_echo_input_payloads() {
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events.lock().unwrap().clear();
        let core = init(collect_event).unwrap();

        // A numeric secret-shaped payload in a sensitive field: serde fails on `key`, and the seam
        // must not reflect the value (the old message embedded the serde literal, echoing the number).
        send(&core, br#"{"id":77,"cmd":"SetLlmKey","key":123456789}"#);
        // Truly malformed JSON without a recoverable id → uncorrelated Error event, still no echo.
        send(&core, br#"{"cmd":"SetLlmKey","key":987654321"#);

        let events = events.lock().unwrap();
        let reply = events
            .iter()
            .find(|e| e["event"] == "Reply" && e["id"] == 77)
            .expect("correlated reply for the id-recoverable command");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], MALFORMED_COMMAND);
        let serialized = events.iter().map(|e| e.to_string()).collect::<String>();
        assert!(
            !serialized.contains("123456789"),
            "reply must not echo the numeric secret payload"
        );
        assert!(
            !serialized.contains("987654321"),
            "error event must not echo the numeric secret payload"
        );
    }

    #[test]
    fn panic_reply_completes_command_with_fixed_error_and_never_echoes_input() {
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events.lock().unwrap().clear();
        let core = init(collect_event).unwrap();

        panic_reply(
            &core,
            br#"{"id":88,"cmd":"SetLlmKey","key":"super-secret-llm-key"}"#,
        );

        let events = events.lock().unwrap();
        let reply = events
            .iter()
            .find(|e| e["event"] == "Reply" && e["id"] == 88)
            .expect("correlated reply completes the awaiting command");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], INTERNAL_ERROR);
        assert!(
            !events
                .iter()
                .any(|e| e.to_string().contains("super-secret-llm-key")),
            "panic reply must never echo the input"
        );
    }

    #[test]
    fn init_parse_errors_never_echo_file_content() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Each corrupt file gets its own Init: a providers error short-circuits the arm before the
        // config is even loaded. Both files hold a bare string — serde's error literal would echo it
        // verbatim if it crossed the seam.
        for (file, marker, code) in [
            (
                "providers.json",
                "PROVIDER-SECRET-ECHO",
                "providers_unavailable",
            ),
            ("config.json", "CONFIG-SECRET-ECHO", "config_corrupt"),
        ] {
            TEST_EVENTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            let dir = std::env::temp_dir().join(format!(
                "tron-init-echo-{}-{}",
                file,
                crate::config::new_id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), format!("\"{marker}\"")).unwrap();

            let core = init(collect_event).unwrap();
            const ID: u64 = 424_243;
            send(
                &core,
                format!(
                    r#"{{"id":{ID},"cmd":"Init","data_dir":{}}}"#,
                    serde_json::to_string(&dir).unwrap()
                )
                .as_bytes(),
            );

            let events = TEST_EVENTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let event = events
                .iter()
                .find(|e| e["code"] == code)
                .unwrap_or_else(|| panic!("corrupt {file} surfaces a {code} event"));
            assert!(
                !event["message"].as_str().unwrap_or("").contains(marker),
                "{code} must not echo file content"
            );
            drop(events);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    // ===================== lifecycle / config contract tests =====================
    // The e2e tests below share the global TEST_EVENTS sink, so they serialize among themselves.
    static LIFECYCLE_SEQ: Mutex<()> = Mutex::new(());

    fn events() -> &'static Mutex<Vec<Value>> {
        TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn wait_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> Option<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Some(v) = events()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .find(|v| pred(v))
                .cloned()
            {
                return Some(v);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    fn start_fake() -> String {
        let (ptx, prx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let (port, listener) = crate::fake::bind_ephemeral().await;
                ptx.send(port).unwrap();
                crate::fake::serve(listener).await;
            });
        });
        format!("http://127.0.0.1:{}", prx.recv().unwrap())
    }

    fn account_id_by_label(label: &str) -> Option<String> {
        for ev in events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
        {
            if ev["event"] == "Accounts" {
                if let Some(a) = ev["accounts"]
                    .as_array()
                    .and_then(|list| list.iter().find(|a| a["label"] == label))
                {
                    return a["id"].as_str().map(str::to_string);
                }
            }
        }
        None
    }

    fn data_dir(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("tron-engine-{tag}-{}", new_id()))
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn test_state(
        data_dir: PathBuf,
        config: Config,
        vault: Option<VaultFile>,
    ) -> Arc<Mutex<Option<CoreState>>> {
        let registry = Registry::load_or_seed(&data_dir.join("providers.json")).unwrap();
        Arc::new(Mutex::new(Some(CoreState {
            data_dir,
            registry,
            config,
            vault,
            monitor: MonitorLifecycle::Idle,
            monitor_generation: 0,
            pending_captcha: HashMap::new(),
            login_in_flight: HashSet::new(),
        })))
    }

    #[test]
    fn monitoring_rejects_account_mutations_and_login_but_keeps_switch_usable() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("guard");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(
            &core,
            r#"{"id":2,"cmd":"CreateVault","master_password":"pw"}"#.as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
            5
        )
        .is_some());
        send(
            &core,
            r#"{"id":3,"cmd":"AddAccount","label":"a","school":"thu","username":"u","password":"p"}"#
                .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
            5
        )
        .is_some());
        assert!(wait_for(
            |v| {
                v["event"] == "Accounts"
                    && v["accounts"].as_array().is_some_and(|accounts| {
                        accounts.iter().any(|account| account["label"] == "a")
                    })
            },
            5
        )
        .is_some());
        let acc = account_id_by_label("a").expect("account id");

        // Inject a RUNNING monitor without spawning actor work; this test covers engine guards.
        {
            let mut guard = lock_state(&core.state);
            let st = guard.as_mut().unwrap();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            st.monitor = MonitorLifecycle::Running(RunningMonitor {
                generation: 1,
                handle: monitor::MonitorHandle::new(tx),
            });
        }

        let mutations: [(u64, String); 4] = [
            (
                10,
                r#"{"id":10,"cmd":"AddAccount","label":"b","school":"thu","username":"u2","password":"p"}"#
                    .into(),
            ),
            (
                11,
                format!(r#"{{"id":11,"cmd":"DeleteAccount","account_id":"{acc}"}}"#),
            ),
            (
                12,
                format!(r#"{{"id":12,"cmd":"Login","account_id":"{acc}"}}"#),
            ),
            (
                13,
                format!(
                    r#"{{"id":13,"cmd":"ImportCookies","account_id":"{acc}","cookies_json":"[]"}}"#
                ),
            ),
        ];
        for (id, cmd) in mutations {
            send(&core, cmd.as_bytes());
            let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == id, 5)
                .unwrap_or_else(|| panic!("no guard reply for command {id}"));
            assert_eq!(
                reply["ok"], false,
                "command {id} must be rejected while monitoring"
            );
            assert!(
                reply["error"]
                    .as_str()
                    .unwrap()
                    .contains("stop monitoring first"),
                "command {id} must ask to stop monitoring first"
            );
        }

        send(
            &core,
            format!(r#"{{"id":14,"cmd":"SwitchAccount","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 14 && v["ok"] == true,
            5
        )
        .is_some());

        {
            let mut guard = lock_state(&core.state);
            let st = guard.as_mut().unwrap();
            st.monitor = MonitorLifecycle::Idle;
            st.monitor.begin_start(2, 999).unwrap();
        }
        send(
            &core,
            format!(r#"{{"id":15,"cmd":"DeleteAccount","account_id":"{acc}"}}"#).as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 15, 5)
            .expect("starting-state guard reply");
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("stop monitoring first"));
        send(
            &core,
            format!(r#"{{"id":16,"cmd":"SwitchAccount","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 16 && v["ok"] == true,
            5
        )
        .is_some());
    }

    #[test]
    fn login_reports_per_account_status_not_global_state_changed() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let base = start_fake();
        let dir = data_dir("login-status");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(
            &core,
            r#"{"id":2,"cmd":"CreateVault","master_password":"pw"}"#.as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        send(
            &core,
            format!(
                r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"test","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
        assert!(wait_for(
            |v| {
                v["event"] == "Accounts"
                    && v["accounts"].as_array().is_some_and(|accounts| {
                        accounts.iter().any(|account| account["label"] == "dave")
                    })
            },
            5
        )
        .is_some());
        let acc = account_id_by_label("dave").expect("account id");

        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        send(
            &core,
            format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "LoginResult" && v["id"] == 4 && v["ok"] == true,
                15
            )
            .is_some(),
            "login succeeds against the fake"
        );

        let events = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let logging_in = events.iter().position(|v| {
            v["event"] == "AccountStatus" && v["account_id"] == acc && v["state"] == "logging_in"
        });
        let online = events.iter().position(|v| {
            v["event"] == "AccountStatus" && v["account_id"] == acc && v["state"] == "online"
        });
        assert!(
            logging_in.is_some(),
            "login starts with a per-account logging_in status"
        );
        assert!(
            online.is_some(),
            "login ends with a per-account online status"
        );
        assert!(
            logging_in.unwrap() < online.unwrap(),
            "logging_in must precede online"
        );
        // The login flow must never touch the global monitor state machine (old code emitted
        // StateChanged logging_in / idle / login_failed). A foreign "starting" from a concurrently
        // running Init test is the only tolerated StateChanged, so only login-era states are banned.
        assert!(
            !events.iter().any(|v| {
                v["event"] == "StateChanged"
                    && matches!(
                        v["state"].as_str(),
                        Some("logging_in" | "login_failed" | "idle")
                    )
            }),
            "login must not emit a global StateChanged"
        );
        drop(events);
    }

    #[test]
    fn deleting_an_account_cancels_its_pending_captcha_login_without_resurrecting_the_secret() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("captcha-cancel");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(
            &core,
            r#"{"id":2,"cmd":"CreateVault","master_password":"pw"}"#.as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        send(
            &core,
            r#"{"id":3,"cmd":"AddAccount","label":"dave","school":"thu","username":"dave","password":"secret"}"#
                .as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
        assert!(wait_for(
            |v| {
                v["event"] == "Accounts"
                    && v["accounts"].as_array().is_some_and(|accounts| {
                        accounts.iter().any(|account| account["label"] == "dave")
                    })
            },
            5
        )
        .is_some());
        let acc = account_id_by_label("dave").expect("account id");

        // Inject exactly the state owned by an in-flight captcha login. This keeps the lifecycle
        // contract deterministic and independent of network scheduling on loaded CI runners.
        let (captcha_tx, captcha_rx) = oneshot::channel::<String>();
        {
            let mut guard = lock_state(&core.state);
            let state = guard.as_mut().expect("initialized state");
            state.pending_captcha.insert(acc.clone(), captcha_tx);
            state.login_in_flight.insert(acc.clone());
        }

        send(
            &core,
            format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        let rejected = wait_for(
            |v| v["event"] == "Reply" && v["id"] == 4 && v["ok"] == false,
            5,
        )
        .expect("second login rejected");
        assert!(rejected["error"]
            .as_str()
            .unwrap()
            .contains("already in progress"));

        send(
            &core,
            format!(r#"{{"id":5,"cmd":"DeleteAccount","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == true,
            5
        )
        .is_some());
        assert!(
            matches!(
                core.rt.block_on(async move {
                    tokio::time::timeout(Duration::from_secs(1), captcha_rx).await
                }),
                Ok(Err(_))
            ),
            "DeleteAccount must drop the pending captcha sender"
        );
        {
            let guard = lock_state(&core.state);
            let state = guard.as_ref().expect("initialized state");
            assert!(!state.login_in_flight.contains(&acc));
            assert!(!state
                .config
                .accounts
                .iter()
                .any(|account| account.id == acc));
            assert!(state.vault.as_ref().expect("vault").get(&acc).is_none());
        }
    }

    #[test]
    fn stale_login_completion_never_resurrects_a_deleted_account() {
        let dir = std::env::temp_dir().join(format!("tron-stale-persist-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();

        // Account present → the async completion persists and updates the cached session.
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        vault
            .set(
                "acc",
                AccountSecret {
                    password: "pw".into(),
                    cookies: String::new(),
                },
            )
            .unwrap();
        let mut config = Config::default();
        config.accounts.push(AccountMeta {
            id: "acc".into(),
            label: "account".into(),
            school_ref: "school".into(),
            username: "user".into(),
            device_id: "device".into(),
            is_teacher: false,
            course_id: None,
        });
        let state = test_state(dir.clone(), config, Some(vault));
        assert!(matches!(
            persist_login_session(
                &state,
                "acc",
                AccountSecret {
                    password: "pw".into(),
                    cookies: "session=NEW".into()
                }
            ),
            Ok(PersistOutcome::Saved)
        ));
        {
            let guard = state.lock().unwrap();
            let st = guard.as_ref().unwrap();
            assert_eq!(
                st.vault.as_ref().unwrap().get("acc").unwrap().cookies,
                "session=NEW"
            );
        }

        // Deleted account (gone from config AND vault): the same stale completion must be refused
        // and must not write anything back.
        let gone_dir = dir.join("gone");
        std::fs::create_dir_all(&gone_dir).unwrap();
        let gone = test_state(
            gone_dir.clone(),
            Config::default(),
            Some(VaultFile::create_with_key(&gone_dir.join("vault.bin"), key).unwrap()),
        );
        assert!(matches!(
            persist_login_session(
                &gone,
                "acc",
                AccountSecret {
                    password: "pw".into(),
                    cookies: "session=STALE".into()
                }
            ),
            Ok(PersistOutcome::AccountGone)
        ));
        let guard = gone.lock().unwrap();
        assert!(
            guard
                .as_ref()
                .unwrap()
                .vault
                .as_ref()
                .unwrap()
                .get("acc")
                .is_none(),
            "stale completion must not write the deleted account's secret"
        );
    }

    #[test]
    fn single_field_patch_cannot_break_cross_field_invariants() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("xfield");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        // Materialize config.json (Init does not write when the file is missing).
        send(
            &core,
            r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2}}"#.as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
            5
        )
        .is_some());

        // Defaults: number_concurrency=100, number_min_concurrency=5. A SINGLE-field patch must not
        // be able to break the invariant against the CURRENT value (patch-only checks would miss it).
        send(
            &core,
            r#"{"id":3,"cmd":"UpdateConfig","patch":{"number_min_concurrency":300}}"#.as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).expect("min>max reply");
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("number_min_concurrency"));

        send(
            &core,
            r#"{"id":4,"cmd":"UpdateConfig","patch":{"number_concurrency":1}}"#.as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 4, 5).expect("max<min reply");
        assert_eq!(reply["ok"], false);

        // Both failures left memory AND disk untouched.
        let config_path = std::path::Path::new(&dir).join("config.json");
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(on_disk["settings"]["number_concurrency"], 100);
        assert_eq!(on_disk["settings"]["number_min_concurrency"], 5);

        // A coherent pair still applies.
        send(
                &core,
                r#"{"id":5,"cmd":"UpdateConfig","patch":{"number_concurrency":8,"number_min_concurrency":8}}"#
                    .as_bytes(),
            );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == true,
            5
        )
        .is_some());
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(on_disk["settings"]["number_concurrency"], 8);
        assert_eq!(on_disk["settings"]["number_min_concurrency"], 8);
    }

    #[test]
    fn update_config_rejects_malformed_or_unknown_fields_without_touching_disk() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("badpatch");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        // Materialize config.json, then freeze its bytes as the untouched baseline.
        send(
            &core,
            r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2}}"#.as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
            5
        )
        .is_some());
        let config_path = std::path::Path::new(&dir).join("config.json");
        let before = std::fs::read(&config_path).unwrap();

        let bad_patches = [
            r#"{"operating":5}"#,
            r#"{"operating":{"days":[{"weekday":"monday"}]}}"#,
            r#"{"countdown_secs":"fast"}"#,
            r#"{"countdown_secs":true}"#,
            r#"{"autoanswer_types":["exam",5]}"#,
            r#"{"llm_endpoint":"not-a-url"}"#,
            r#"{"llm_endpoint":"ftp://example.com/v1"}"#,
            r#"{"llm_endpoint":""}"#,
            r#"{"attendance_gate_percent":150}"#,
        ];
        for (index, patch) in bad_patches.iter().enumerate() {
            let id = 3 + index as u64;
            send(
                &core,
                format!(r#"{{"id":{id},"cmd":"UpdateConfig","patch":{patch}}}"#).as_bytes(),
            );
            let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == id, 5)
                .unwrap_or_else(|| panic!("no reply for patch {patch}"));
            assert_eq!(reply["ok"], false, "patch {patch} must be rejected");
        }
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "failed patches must not touch the file"
        );

        // An unknown key is rejected and names the key...
        send(
            &core,
            r#"{"id":99,"cmd":"UpdateConfig","patch":{"bogus_field":1}}"#.as_bytes(),
        );
        let reply =
            wait_for(|v| v["event"] == "Reply" && v["id"] == 99, 5).expect("unknown key reply");
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"].as_str().unwrap().contains("bogus_field"),
            "unknown key must be named: {}",
            reply["error"]
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "unknown key must not touch the file"
        );
        // ...and a valid patch still applies afterwards.
        send(
            &core,
            r#"{"id":100,"cmd":"UpdateConfig","patch":{"poll_idle_secs":7}}"#.as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 100 && v["ok"] == true,
            5
        )
        .is_some());
    }
}
