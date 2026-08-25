//! Core state machine: owns the tokio runtime, event callback, long-lived heartbeat, target
//! supervisor, and (after `Init`) registry/config/vault behind one mutex. Commands lock, mutate,
//! persist, and emit; async login/import/monitor reconciliation snapshots what it needs, drops the
//! lock for network work, then re-locks to persist. Each async command runs in a single-task
//! JoinSet observer so a panic becomes the fixed INTERNAL_ERROR reply instead of killing the
//! runtime. The long-lived monitor actor has a generation-bound watchdog. Secrets never enter an
//! event.

use crate::config::{
    canonical_tenant, new_id, AccountGroup, AccountMeta, Config, DetectorSelection,
    ScheduleBinding, Settings, TargetId,
};
use crate::login;
use crate::login::LoginOutcome;
use crate::monitor::{self, MonitorConfig};
use crate::persistence::{AccountJournal, AccountMutation};
use crate::protocol::{
    Command, CourseSnapshot, GroupInput, LoginState, Notice, WakeMode, WireError,
};
use crate::providers::{Endpoints, Registry};
use crate::secrets::{AccountSecret, VaultFile};
use crate::supervisor::{EffectivePlan, TargetSupervisor};
use cookie_store::CookieStore;
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tokio::task::{JoinHandle, JoinSet};

pub type EventCb = extern "C" fn(*const u8, usize);

fn platform_wake_mode(reason: &str) -> Option<WakeMode> {
    match reason {
        "wake_mode:exact" => Some(WakeMode::Exact),
        "wake_mode:inexact_user_action_required" => Some(WakeMode::InexactUserActionRequired),
        "wake_mode:unavailable" => Some(WakeMode::Unavailable),
        _ => None,
    }
}

struct CoreState {
    data_dir: PathBuf,
    registry: Registry,
    config: Config,
    vault: Option<VaultFile>, // Some(..) once unlocked
    monitor: MonitorLifecycle,
    monitor_generation: u64,
    supervisor: TargetSupervisor,
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
    loaded_account_ids: HashSet<String>,
    loading_account_ids: HashSet<String>,
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

/// Lazy journal recovery before an account mutation. A crash between `begin` and `complete` leaves
/// a stale journal; resolving it FIRST means a leftover record can never wedge the next mutation.
/// Success warns with the recovery message; failure returns the fixed error — the journal loader's
/// own error can echo file content, so it must never cross the seam. The caller fails the mutation
/// on `Err`; the vault must already be unlocked.
fn recover_stale_journal(st: &mut CoreState, cb: EventCb) -> Result<(), String> {
    match recover_account_transaction(&st.data_dir, &st.config, st.vault.as_mut().unwrap()) {
        Ok(Some(message)) => {
            emit(
                cb,
                &json!({ "id": null, "event": "LogLine", "level": "warn", "text": message }),
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(_) => Err("account transaction journal unreadable; recovery skipped".to_string()),
    }
}

pub struct Core {
    rt: Runtime,
    cb: EventCb,
    state: Arc<Mutex<Option<CoreState>>>,
    /// Per-core captcha answer window (tests only). Defaults to the production 180 s; ONLY the
    /// timeout e2e shortens it on its own core, so other captcha tests (even in other modules,
    /// running in parallel) are never affected. Production builds have no field — always 180 s.
    #[cfg(test)]
    captcha_answer_timeout: Duration,
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

fn reply_data(cb: EventCb, id: u64, ok: bool, error: Option<String>, data: Value) {
    emit(
        cb,
        &json!({ "id": id, "event": "Reply", "ok": ok, "error": error, "data": data }),
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
            #[cfg(test)]
            captcha_answer_timeout: Duration::from_secs(180),
        }))
    }
}

pub fn init(cb: EventCb) -> Result<Box<Core>, String> {
    Core::new(cb)
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
        Ok(command) => command,
        Err(_) => {
            match serde_json::from_slice::<Value>(json_bytes)
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_u64))
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

    if matches!(
        &cmd,
        Command::CreateGroup { group, .. }
            | Command::UpdateGroup { group, .. }
            | Command::MergeGroups { group, .. }
            if !group.course_ids.is_empty()
    ) {
        spawn_validated_group_command(core, cmd);
        return;
    }

    match cmd {
        Command::Login { id, account_id } => spawn_login(core, id, account_id),
        Command::ImportCookies {
            id,
            account_id,
            cookies_json,
        } => spawn_import_cookies(core, id, account_id, cookies_json),
        Command::ListCommonCourses {
            id,
            member_account_ids,
        } => spawn_list_common_courses(core, id, member_account_ids),
        other => {
            let cancel_removed_pending = matches!(
                &other,
                Command::DeleteAccount { .. }
                    | Command::UpdateGroup { .. }
                    | Command::DeleteGroup { .. }
                    | Command::MergeGroups { .. }
                    | Command::SetTargetSchedule { .. }
                    | Command::StopTarget { .. }
                    | Command::StopAllMonitoring { .. }
                    | Command::SuspendForPlatformLimit { .. }
            );
            let reconcile = matches!(
                &other,
                Command::AddAccount { .. }
                    | Command::DeleteAccount { .. }
                    | Command::CreateGroup { .. }
                    | Command::UpdateGroup { .. }
                    | Command::DeleteGroup { .. }
                    | Command::MergeGroups { .. }
                    | Command::SetTargetSchedule { .. }
                    | Command::SetMonitoringPreferences { .. }
                    | Command::ApplyScheduleClock { .. }
                    | Command::StartTarget { .. }
                    | Command::StopTarget { .. }
                    | Command::StopAllMonitoring { .. }
                    | Command::ResumeScheduledMonitoring { .. }
                    | Command::AcknowledgeTemporaryMerge { .. }
                    | Command::SuspendForPlatformLimit { .. }
                    | Command::ClearPlatformLimit { .. }
            );
            let arm_clock = matches!(&other, Command::ApplyScheduleClock { .. });
            handle_sync(core, other);
            if reconcile {
                reconcile_monitor(core, cancel_removed_pending);
            }
            if arm_clock {
                arm_clock_stale_timer(core);
            }
        }
    }
}

/// Run one async command inside a single-task `JoinSet` so a panic is observed at the join point
/// instead of vanishing silently: a bare spawned task's panic is captured by its JoinHandle, and
/// dropping that handle without joining would quietly kill the command mid-flight, leaving the
/// awaiting UI with no reply. `on_panic` runs the command's fixed INTERNAL_ERROR backstop. The
/// returned outer handle keeps Start-task abort semantics: dropping/aborting it drops the JoinSet,
/// which aborts the inner task and everything it spawned/awaited.
fn spawn_observed<F>(
    rt: &Runtime,
    on_panic: impl FnOnce() + Send + 'static,
    future: F,
) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    rt.spawn(async move {
        let mut set = JoinSet::new();
        set.spawn(future);
        // A single-task set joins exactly once — `if let`, not a loop, so the FnOnce backstop is
        // provably called at most once.
        if let Some(joined) = set.join_next().await {
            match joined {
                Ok(()) => {}
                // The panic payload is never echoed — the fixed INTERNAL_ERROR text is the only
                // thing that may cross the seam.
                Err(join_error) if join_error.is_panic() => on_panic(),
                Err(_) => {} // cancelled (abort) — the aborting side owns the terminal path
            }
        }
    })
}

/// Panic backstop for the Login task: drop any pending captcha entry and the single-flight marker,
/// then complete the command with the fixed INTERNAL_ERROR (the payload is never echoed). The
/// flight guard already cleared the marker on unwind; this also removes the stale captcha sender so
/// a later SubmitCaptcha reports no pending challenge.
fn recover_login_panic(
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
    emit(
        cb,
        &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": INTERNAL_ERROR }),
    );
}

/// Watch the monitor's panic channel. The monitor's TaskGroup pings it (`Some(())`) when one of its
/// futures panicked; only a RUNNING monitor whose generation matches this start is torn down — a
/// stale watchdog from an earlier start can never cancel a newer session. The handle is taken and
/// the lock released BEFORE `stop()` (TaskGroup::cancel aborts tasks, whose drops may run
/// callbacks). `None` (senders dropped by a normal stop) is a no-op.
fn spawn_monitor_watchdog(
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mut panic_rx: UnboundedReceiver<()>,
) {
    let restart_runtime = rt.clone();
    rt.spawn(async move {
        while let Some(()) = panic_rx.recv().await {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                continue;
            };
            let MonitorLifecycle::Running(running) = &st.monitor else {
                continue;
            };
            if running.generation != generation {
                continue;
            }
            let handle = match st.monitor.take_for_stop() {
                StoppedMonitor::Running(handle) => handle,
                _ => unreachable!("Running verified above"),
            };
            drop(guard);
            handle.stop();
            emit(
                cb,
                &json!({ "id": null, "event": "Error", "severity": "error",
                "code": "monitor_panicked", "message": INTERNAL_ERROR }),
            );
            reconcile_monitor_with(restart_runtime.clone(), state.clone(), cb, false);
        }
    });
}

/// Runtime events are consumed asynchronously; definition/control commands themselves stay short.
fn spawn_monitor_runtime_listener(
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mut events: UnboundedReceiver<monitor::RuntimeEvent>,
) {
    let reconcile_runtime = rt.clone();
    rt.spawn(async move {
        while let Some(event) = events.recv().await {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                break;
            };
            if !matches!(
                &st.monitor,
                MonitorLifecycle::Running(running) if running.generation == generation
            ) {
                break;
            }
            let needs_reconcile = match event {
                monitor::RuntimeEvent::AccountResult {
                    sources,
                    account_id,
                    phase,
                    activity_kind,
                    course_name,
                    error,
                } => {
                    st.supervisor.set_account_result(
                        &sources,
                        &account_id,
                        phase,
                        Some(activity_kind),
                        Some(course_name),
                        error.map(|message| WireError {
                            code: "mutation_failed".to_string(),
                            message,
                        }),
                    );
                    false
                }
                monitor::RuntimeEvent::AccountLogin {
                    account_id,
                    online,
                    error,
                } => {
                    st.supervisor.set_login_state(
                        account_id,
                        if online {
                            LoginState::Online
                        } else {
                            LoginState::Error
                        },
                        error.map(|message| WireError {
                            code: "relogin_failed".to_string(),
                            message,
                        }),
                    );
                    st.supervisor.reconcile(&mut st.config);
                    true
                }
            };
            emit_monitoring_snapshot(cb, st);
            drop(guard);
            if needs_reconcile {
                reconcile_monitor_with(reconcile_runtime.clone(), state.clone(), cb, false);
            }
        }
    });
}

fn arm_clock_stale_timer(core: &Core) {
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
        let seconds = (deadline - crate::supervisor::now_epoch_seconds()).max(0) as u64;
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let changed = {
            let mut guard = lock_state(&state);
            let Some(st) = guard.as_mut() else {
                return;
            };
            if st.supervisor.clock_revision() != Some(revision)
                || crate::supervisor::now_epoch_seconds() < deadline
            {
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

/// inline under the state lock; those three are dispatched in `send` before this arm.
fn handle_sync(core: &Core, cmd: Command) {
    handle_sync_state(&core.state, core.cb, cmd);
}

fn handle_sync_state(state: &Arc<Mutex<Option<CoreState>>>, cb: EventCb, cmd: Command) {
    let id = cmd.id();
    let mut guard = lock_state(state);

    match cmd {
        Command::Init {
            data_dir,
            mut device_key_b64,
            ..
        } => {
            let dir = PathBuf::from(&data_dir);
            // 建目錄失敗是後續每一步失敗的根因,不能像以前那樣 `let _ =` 丟掉 —— 那會讓
            // 真正的原因(權限/唯讀/路徑不存在)完全消失,只剩下游一句黑箱訊息。
            let dir_error = std::fs::create_dir_all(&dir).err();
            let registry = match Registry::load_or_seed(&dir.join("providers.json")) {
                Ok(loaded) => {
                    // 播種沒能保存:App 照常啟動(內容全部來自內建種子),只是把話講清楚。
                    if let Some(warning) = loaded.persist_warning {
                        emit(
                            cb,
                            &json!({ "id": null, "event": "Error", "severity": "warn",
                            "code": "providers_not_persisted",
                            "message": format!(
                                "學校清單未能保存（{warning}）；本次使用內建清單，之後新增的學校不會被保存"
                            ) }),
                        );
                    }
                    loaded.registry
                }
                Err(error) => {
                    // safe_detail 只含操作名與 errno:serde 的錯誤(可能逐字回吐檔案內容)
                    // 一律收斂成固定描述,IO 失敗則據實以告,讓這類問題可被診斷。
                    let mut message =
                        format!("providers registry unavailable：{}", error.safe_detail());
                    if let Some(dir_error) = &dir_error {
                        message.push_str(&format!("；資料目錄建立失敗（{:?}）", dir_error.kind()));
                    }
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "providers_unavailable", "message": message }),
                    );
                    return reply(cb, id, false, Some(message));
                }
            };
            let config_path = dir.join("config.json");
            let initialized = match Config::initialize(&config_path) {
                Ok(initialized) => initialized,
                Err(error) => {
                    let message = format!("設定檔無法載入：{error}");
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "config_unavailable", "message": message }),
                    );
                    return reply(cb, id, false, Some(message));
                }
            };
            let config_notice = initialized.reset_notice.map(|notice| Notice {
                code: "config_reset".to_string(),
                message: "舊版設定已備份；帳號、群組與時間表已重設".to_string(),
                backup_path: Some(notice.backup_path.to_string_lossy().into_owned()),
            });
            let config = initialized.config;
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
            let unlocked = vault.is_some();
            let mut state = CoreState {
                data_dir: dir,
                registry,
                config,
                vault,
                monitor: MonitorLifecycle::Idle,
                monitor_generation: 0,
                supervisor: TargetSupervisor::new(config_notice),
                pending_captcha: HashMap::new(),
                login_in_flight: HashSet::new(),
            };
            state.supervisor.reconcile(&mut state.config);
            // Emit the whole snapshot from the local while the state lock is held (commands are
            // serialized on this mutex, so no command can observe an uninstalled state in between),
            // then install before handle_sync returns. No unwrap needed: `state` is a plain local.
            emit_providers(cb, &state);
            emit_settings(cb, &state);
            emit_monitoring_snapshot(cb, &state);
            emit(
                cb,
                &json!({ "id": null, "event": "VaultState", "exists": unlocked, "unlocked": unlocked }),
            );
            emit_caps(cb);

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
            if let Err(error) = recover_stale_journal(st, cb) {
                return reply(cb, id, false, Some(error));
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
                schedule: ScheduleBinding::Disabled,
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
            if let Err(error) = st.config.bump_definition_revision(true) {
                st.config = previous_config;
                let _ = st.vault.as_mut().unwrap().delete(&acc_id);
                let _ = AccountJournal::complete(&st.data_dir);
                return reply(cb, id, false, Some(error));
            }
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
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply_data(cb, id, true, None, json!({ "account_id": acc_id }));
        }

        Command::DeleteAccount {
            account_id,
            expected_revision,
            remove_from_groups,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            if st.config.account(&account_id).is_none() {
                return reply(cb, id, false, Some("no such account".into()));
            }
            if st.vault.is_none() {
                return reply(cb, id, false, Some("vault is locked".into()));
            }
            let referenced = st
                .config
                .groups
                .iter()
                .any(|group| group.member_account_ids.contains(&account_id));
            if referenced && !remove_from_groups {
                return reply(
                    cb,
                    id,
                    false,
                    Some("account_is_referenced_by_groups".into()),
                );
            }
            let in_use = st.supervisor.plan().routes.iter().any(|route| {
                route.detector_account_id == account_id
                    || route.participant_account_ids.contains(&account_id)
            });
            if in_use {
                return reply(cb, id, false, Some("account_is_in_use".into()));
            }
            if let Err(error) = recover_stale_journal(st, cb) {
                return reply(cb, id, false, Some(error));
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
            st.config
                .accounts
                .retain(|account| account.id != account_id);
            for group in &mut st.config.groups {
                group
                    .member_account_ids
                    .retain(|member| member != &account_id);
                if matches!(
                    &group.detector,
                    DetectorSelection::Preferred { account_id: preferred } if preferred == &account_id
                ) {
                    group.detector = DetectorSelection::Auto;
                }
            }
            st.config
                .groups
                .retain(|group| !group.member_account_ids.is_empty());
            if let Err(error) = st.config.bump_definition_revision(true) {
                st.config = previous_config;
                let _ = AccountJournal::complete(&st.data_dir);
                return reply(cb, id, false, Some(error));
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
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::CreateGroup {
            expected_revision,
            group,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            let definition = match group_definition(st, crate::config::new_group_id(), group, true)
            {
                Ok(group) => group,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let group_id = definition.id.clone();
            let mut next = st.config.clone();
            next.groups.push(definition);
            if let Err(error) = next
                .bump_definition_revision(true)
                .and_then(|_| next.validate_with_registry(&st.registry))
                .and_then(|_| next.save(&st.config_path()))
            {
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply_data(cb, id, true, None, json!({ "group_id": group_id }));
        }

        Command::UpdateGroup {
            group_id,
            expected_revision,
            group,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            let Some(previous) = st.config.group(&group_id).cloned() else {
                return reply(cb, id, false, Some("unknown_group".into()));
            };
            let definition = match group_definition(st, group_id.clone(), group, false) {
                Ok(group) => group,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let prepared = match prepare_definition_change(
                st,
                HashSet::from([TargetId::group(group_id.clone())]),
            ) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let schedule_changed = previous.schedule != definition.schedule;
            let mut next = st.config.clone();
            *next
                .groups
                .iter_mut()
                .find(|candidate| candidate.id == group_id)
                .expect("group existence checked") = definition;
            if let Err(error) = next
                .bump_definition_revision(schedule_changed)
                .and_then(|_| next.validate_with_registry(&st.registry))
                .and_then(|_| next.save(&st.config_path()))
            {
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            if schedule_changed {
                st.supervisor.invalidate_clock();
            }
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::DeleteGroup {
            group_id,
            expected_revision,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            if st.config.group(&group_id).is_none() {
                return reply(cb, id, false, Some("unknown_group".into()));
            }
            let prepared = match prepare_definition_change(
                st,
                HashSet::from([TargetId::group(group_id.clone())]),
            ) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let mut next = st.config.clone();
            next.groups.retain(|group| group.id != group_id);
            next.runtime.group_rotation.remove(&group_id);
            if let Err(error) = next
                .bump_definition_revision(true)
                .and_then(|_| next.save(&st.config_path()))
            {
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::MergeGroups {
            mut group_ids,
            expected_revision,
            group,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            group_ids.sort();
            group_ids.dedup();
            if group_ids.len() < 2
                || group_ids
                    .iter()
                    .any(|group_id| st.config.group(group_id).is_none())
            {
                return reply(cb, id, false, Some("merge_requires_existing_groups".into()));
            }
            let definition = match group_definition(st, crate::config::new_group_id(), group, true)
            {
                Ok(group) => group,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let prepared = match prepare_definition_change(
                st,
                group_ids
                    .iter()
                    .map(|group_id| TargetId::group(group_id.clone()))
                    .collect(),
            ) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let merged_group_id = definition.id.clone();
            let mut next = st.config.clone();
            next.groups
                .retain(|candidate| !group_ids.contains(&candidate.id));
            for group_id in &group_ids {
                next.runtime.group_rotation.remove(group_id);
            }
            next.groups.push(definition);
            if let Err(error) = next
                .bump_definition_revision(true)
                .and_then(|_| next.validate_with_registry(&st.registry))
                .and_then(|_| next.save(&st.config_path()))
            {
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply_data(cb, id, true, None, json!({ "group_id": merged_group_id }));
        }

        Command::ListCommonCourses { .. } => unreachable!("handled asynchronously"),

        Command::SetTargetSchedule {
            target,
            expected_revision,
            schedule,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            if let Err(error) = schedule.validate() {
                return reply(cb, id, false, Some(error));
            }
            let mut next = st.config.clone();
            let found = match &target {
                crate::config::TargetId::Account { account_id } => next
                    .account_mut(account_id)
                    .filter(|account| !account.is_teacher)
                    .map(|account| account.schedule = schedule),
                crate::config::TargetId::Group { group_id } => next
                    .groups
                    .iter_mut()
                    .find(|group| &group.id == group_id)
                    .map(|group| group.schedule = schedule),
            };
            if found.is_none() {
                return reply(cb, id, false, Some("unknown_target".into()));
            }
            let prepared = match prepare_definition_change(st, HashSet::from([target.clone()])) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            if let Err(error) = next
                .bump_definition_revision(true)
                .and_then(|_| next.save(&st.config_path()))
            {
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::SetMonitoringPreferences {
            expected_revision,
            global_schedule,
            time_zone,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if expected_revision != st.config.config_revision {
                return reply_revision_conflict(cb, id, st);
            }
            if let Err(error) = global_schedule.validate() {
                return reply(cb, id, false, Some(error));
            }
            let affected: HashSet<TargetId> = st
                .supervisor
                .plan()
                .routes
                .iter()
                .flat_map(|route| route.source_targets.iter().cloned())
                .collect();
            let prepared = match prepare_definition_change(st, affected) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let mut next = st.config.clone();
            next.monitoring.global_schedule = global_schedule;
            next.monitoring.time_zone = time_zone;
            if let Err(error) = next
                .bump_definition_revision(true)
                .and_then(|_| next.save(&st.config_path()))
            {
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.config = next;
            st.supervisor.invalidate_clock();
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::ApplyScheduleClock {
            clock_revision,
            config_revision,
            schedule_revision,
            evaluated_at_utc,
            targets,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if let Err(error) = st.supervisor.apply_clock(
                &st.config,
                clock_revision,
                config_revision,
                schedule_revision,
                &evaluated_at_utc,
                targets,
            ) {
                return reply(cb, id, false, Some(error));
            }
            let previous = st.config.clone();
            if st.supervisor.reconcile(&mut st.config) {
                if let Err(error) = st.config.save(&st.config_path()) {
                    st.config = previous;
                    st.supervisor.invalidate_clock();
                    return reply(cb, id, false, Some(error));
                }
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::StartTarget { target, .. } => {
            apply_target_override_command(cb, id, guard.as_mut(), target, true);
        }

        Command::StopTarget { target, .. } => {
            apply_target_override_command(cb, id, guard.as_mut(), target, false);
        }

        Command::StopAllMonitoring { .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            let affected: HashSet<TargetId> = st
                .supervisor
                .plan()
                .routes
                .iter()
                .flat_map(|route| route.source_targets.iter().cloned())
                .collect();
            let prepared = match prepare_definition_change(st, affected) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let previous = st.config.clone();
            st.supervisor.stop_all(&mut st.config);
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous;
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::ResumeScheduledMonitoring { .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            let previous = st.config.clone();
            st.supervisor.resume_schedules(&mut st.config);
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous;
                return reply(cb, id, false, Some(error));
            }
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::AcknowledgeTemporaryMerge {
            component_id,
            plan_revision,
            ..
        } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if let Err(error) = st
                .supervisor
                .acknowledge_merge(&component_id, plan_revision)
            {
                return reply(cb, id, false, Some(error));
            }
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::SuspendForPlatformLimit { reason, .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            let affected: HashSet<TargetId> = st
                .supervisor
                .plan()
                .routes
                .iter()
                .flat_map(|route| route.source_targets.iter().cloned())
                .collect();
            let prepared = match prepare_definition_change(st, affected) {
                Ok(prepared) => prepared,
                Err(error) => return reply(cb, id, false, Some(error)),
            };
            let previous = st.config.clone();
            st.supervisor
                .suspend_for_platform_limit(&mut st.config, reason);
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous;
                rollback_definition_change(st, prepared);
                return reply(cb, id, false, Some(error));
            }
            st.supervisor.reconcile(&mut st.config);
            if let Err(error) = commit_definition_change(st, prepared) {
                return reply(cb, id, false, Some(error));
            }
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::ClearPlatformLimit { reason, .. } => {
            let Some(st) = guard.as_mut() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            if let Some(mode) = platform_wake_mode(&reason) {
                st.supervisor.set_wake_mode(mode);
                emit_monitoring_snapshot(cb, st);
                return reply(cb, id, true, None);
            }
            let previous = st.config.clone();
            if let Err(error) = st.supervisor.clear_platform_limit(&mut st.config, &reason) {
                return reply(cb, id, false, Some(error));
            }
            if let Err(error) = st.config.save(&st.config_path()) {
                st.config = previous;
                return reply(cb, id, false, Some(error));
            }
            st.supervisor.reconcile(&mut st.config);
            emit_monitoring_snapshot(cb, st);
            reply(cb, id, true, None);
        }

        Command::GetMonitoringSnapshot { .. } => {
            let Some(st) = guard.as_ref() else {
                return reply(cb, id, false, Some("not initialized".into()));
            };
            let snapshot = st.supervisor.snapshot(&st.config, &st.login_in_flight);
            reply_data(cb, id, true, None, json!({ "snapshot": snapshot }));
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
                    if txc.send(text).is_ok() {
                        reply(cb, id, true, None); // wakes the awaiting login task
                    } else {
                        // The awaiting login task is gone (panic/cancellation raced the submit): the
                        // challenge is withdrawn, so the submit cannot succeed.
                        reply(
                            cb,
                            id,
                            false,
                            Some("captcha challenge already withdrawn".into()),
                        );
                    }
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
            if let Err(error) = next.settings.validate() {
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
                st.pending_captcha.clear();
                st.login_in_flight.clear();
                if let Some(vault) = st.vault.as_mut() {
                    vault.lock();
                }
            }
            reply(cb, id, true, None);
        }

        Command::Login { .. } | Command::ImportCookies { .. } => {
            unreachable!("handled asynchronously")
        }
    }
}

fn prepare_definition_change(
    st: &CoreState,
    affected_sources: HashSet<TargetId>,
) -> Result<bool, String> {
    match &st.monitor {
        MonitorLifecycle::Running(running) => {
            running.handle.prepare_definition_change(affected_sources)?;
            Ok(true)
        }
        MonitorLifecycle::Idle | MonitorLifecycle::Starting(_) => Ok(false),
    }
}

fn commit_definition_change(st: &CoreState, prepared: bool) -> Result<(), String> {
    if !prepared {
        return Ok(());
    }
    match &st.monitor {
        MonitorLifecycle::Running(running) => running
            .handle
            .commit_definition_change(monitor_plan(st.supervisor.plan())),
        MonitorLifecycle::Idle | MonitorLifecycle::Starting(_) => Ok(()),
    }
}

fn rollback_definition_change(st: &CoreState, prepared: bool) {
    if prepared {
        if let MonitorLifecycle::Running(running) = &st.monitor {
            let _ = running.handle.rollback_definition_change();
        }
    }
}

fn reply_revision_conflict(cb: EventCb, id: u64, st: &CoreState) {
    let snapshot = st.supervisor.snapshot(&st.config, &st.login_in_flight);
    reply_data(
        cb,
        id,
        false,
        Some("revision_conflict".to_string()),
        json!({ "snapshot": snapshot }),
    );
}

fn group_definition(
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

fn apply_target_override_command(
    cb: EventCb,
    id: u64,
    state: Option<&mut CoreState>,
    target: TargetId,
    force_open: bool,
) {
    let Some(st) = state else {
        return reply(cb, id, false, Some("not initialized".into()));
    };
    if st.config.monitoring.all_suspended {
        return reply(cb, id, false, Some("all_monitoring_suspended".into()));
    }
    let prepared = if force_open {
        false
    } else {
        match prepare_definition_change(st, HashSet::from([target.clone()])) {
            Ok(prepared) => prepared,
            Err(error) => return reply(cb, id, false, Some(error)),
        }
    };
    let previous = st.config.clone();
    let persisted = match st
        .supervisor
        .set_target_override(&mut st.config, &target, force_open)
    {
        Ok(persisted) => persisted,
        Err(error) => {
            rollback_definition_change(st, prepared);
            return reply(cb, id, false, Some(error));
        }
    };
    if persisted {
        if let Err(error) = st.config.save(&st.config_path()) {
            st.config = previous;
            rollback_definition_change(st, prepared);
            return reply(cb, id, false, Some(error));
        }
    }
    st.supervisor.reconcile(&mut st.config);
    if let Err(error) = commit_definition_change(st, prepared) {
        return reply(cb, id, false, Some(error));
    }
    emit_monitoring_snapshot(cb, st);
    reply(cb, id, true, None);
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
            if starting.command_id != 0 {
                reply(
                    cb,
                    starting.command_id,
                    false,
                    Some(cancelled_reason.to_string()),
                );
            }
            if let Some(task) = starting.task.take() {
                task.abort();
            }
        }
        StoppedMonitor::Running(handle) => {
            handle.stop();
        }
    }
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
fn spawn_login(core: &Core, id: u64, account_id: String) {
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
    let captcha_answer_timeout = Duration::from_secs(180);

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
            &json!({ "id": null, "event": "LogLine", "level": "info",
                          "text": format!("login → {base_url}") }),
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
                emit(
                    cb,
                    &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": error }),
                );
                return;
            }
        };

        // Restore path: a cached session that still verifies skips the password login entirely.
        let result: Result<bool, String> = if !cached_cookies.is_empty()
            && login::verify_session(&client, &endpoints).await
        {
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
                        &json!({ "id": null, "event": "CaptchaChallenge",
                                      "account_id": account_id, "image_b64": login::encode_base64(&image_bytes) }),
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
                        emit(
                            cb,
                            &json!({ "id": id, "event": "LoginResult", "ok": true,
                            "detail": if from_cache { "session restored from cache" } else { "logged in" } }),
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
                        emit(
                            cb,
                            &json!({ "id": id, "event": "LoginResult", "ok": false,
                            "reason": "login succeeded but the account was deleted during login" }),
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
                        emit(
                            cb,
                            &json!({ "id": id, "event": "LoginResult", "ok": false,
                            "reason": format!("login succeeded but session persistence failed: {error}") }),
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
                emit(
                    cb,
                    &json!({ "id": id, "event": "LoginResult", "ok": false, "reason": e }),
                );
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
            // A captcha needs a human; automatic target activation cannot prompt. An interactive
            // Login caches the session before the supervisor retries the target.
            LoginOutcome::NeedCaptcha { .. } => {
                return Err("需要圖形驗證碼，請先用 Login 登入一次".into())
            }
        }
    }
    Ok((client, dump_cookies(&jar)))
}

/// The monitor's live settings snapshot. Built at actor startup and after every settings change.
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
    }
}

/// Push changed non-schedule settings into the long-lived actor and its pollers.
fn push_config(st: &CoreState) {
    if let Some(h) = st.monitor.running_handle() {
        let _ = h.tx.send(monitor::MonitorMsg::ConfigUpdated(Box::new(
            monitor_config(st),
        )));
    }
}

struct MonitorLoginInput {
    meta: AccountMeta,
    base_url: String,
    password: String,
    cookies: String,
}

#[derive(Clone, Copy)]
enum MonitorAuthMode {
    Start,
    Upsert,
}

fn monitor_plan(plan: &EffectivePlan) -> monitor::MonitorPlan {
    monitor::MonitorPlan {
        generation: plan.revision,
        routes: plan
            .routes
            .iter()
            .map(|route| monitor::MonitorRoute {
                source_targets: route.source_targets.clone(),
                detector_account_id: route.detector_account_id.clone(),
                participant_account_ids: route.participant_account_ids.clone(),
                course_ids: route.course_ids.clone(),
            })
            .collect(),
    }
}

fn required_monitor_accounts(st: &CoreState, plan: &EffectivePlan) -> HashSet<String> {
    let mut required: HashSet<String> = plan
        .routes
        .iter()
        .flat_map(|route| {
            route
                .participant_account_ids
                .iter()
                .chain(std::iter::once(&route.detector_account_id))
        })
        .filter(|account_id| !st.supervisor.account_is_terminal(account_id))
        .cloned()
        .collect();
    // Teacher accounts never poll. They are authenticated only as potential QR helpers.
    required.extend(
        st.config
            .accounts
            .iter()
            .filter(|account| account.is_teacher && !st.supervisor.account_is_terminal(&account.id))
            .map(|account| account.id.clone()),
    );
    required
}

fn monitor_login_inputs(st: &CoreState, account_ids: &HashSet<String>) -> Vec<MonitorLoginInput> {
    let Some(vault) = st.vault.as_ref() else {
        return Vec::new();
    };
    st.config
        .accounts
        .iter()
        .filter(|account| account_ids.contains(&account.id))
        .filter_map(|meta| {
            let base_url = st.registry.resolve(&meta.school_ref)?;
            let (password, cookies) = vault.get(&meta.id).unwrap_or_default().into_parts();
            Some(MonitorLoginInput {
                meta: meta.clone(),
                base_url,
                password,
                cookies,
            })
        })
        .collect()
}

fn reconcile_monitor(core: &Core, cancel_removed_pending: bool) {
    reconcile_monitor_with(
        core.rt.handle().clone(),
        core.state.clone(),
        core.cb,
        cancel_removed_pending,
    );
}

fn reconcile_monitor_with(
    runtime: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    cancel_removed_pending: bool,
) {
    let launch = {
        let mut guard = lock_state(&state);
        let Some(st) = guard.as_mut() else {
            return;
        };
        let runtime_changed = st.supervisor.reconcile(&mut st.config);
        if runtime_changed {
            if let Err(error) = st.config.save(&st.config_path()) {
                emit(
                    cb,
                    &json!({ "id": null, "event": "Error", "severity": "error",
                    "code": "runtime_persistence_failed", "message": error }),
                );
                return;
            }
        }
        let desired = st.supervisor.plan().clone();
        if desired.routes.is_empty() {
            match &mut st.monitor {
                MonitorLifecycle::Running(running) => {
                    let _ = running
                        .handle
                        .apply_plan(monitor_plan(&desired), cancel_removed_pending);
                }
                MonitorLifecycle::Starting(_) => {
                    stop_monitor(&mut st.monitor, cb, "no effective monitoring target");
                }
                MonitorLifecycle::Idle => {}
            }
            emit_monitoring_snapshot(cb, st);
            return;
        }
        if st.vault.is_none() {
            emit_monitoring_snapshot(cb, st);
            return;
        }
        let required = required_monitor_accounts(st, &desired);
        match &mut st.monitor {
            MonitorLifecycle::Starting(_) => return,
            MonitorLifecycle::Running(running) => {
                let _ = running
                    .handle
                    .apply_plan(monitor_plan(&desired), cancel_removed_pending);
                let missing: HashSet<String> = required
                    .difference(&running.loaded_account_ids)
                    .filter(|account_id| !running.loading_account_ids.contains(*account_id))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    return;
                }
                let inputs = monitor_login_inputs(st, &missing);
                let ids: HashSet<String> =
                    inputs.iter().map(|input| input.meta.id.clone()).collect();
                if let MonitorLifecycle::Running(running) = &mut st.monitor {
                    running.loading_account_ids.extend(ids.iter().cloned());
                }
                st.login_in_flight.extend(ids);
                emit_monitoring_snapshot(cb, st);
                Some((
                    MonitorAuthMode::Upsert,
                    running_generation(&st.monitor),
                    inputs,
                ))
            }
            MonitorLifecycle::Idle => {
                let inputs = monitor_login_inputs(st, &required);
                if inputs.is_empty() {
                    emit_monitoring_snapshot(cb, st);
                    return;
                }
                st.login_in_flight
                    .extend(inputs.iter().map(|input| input.meta.id.clone()));
                st.monitor_generation = st.monitor_generation.wrapping_add(1).max(1);
                let generation = st.monitor_generation;
                if st.monitor.begin_start(generation, 0).is_err() {
                    return;
                }
                emit_monitoring_snapshot(cb, st);
                Some((MonitorAuthMode::Start, generation, inputs))
            }
        }
    };
    if let Some((mode, generation, inputs)) = launch {
        spawn_monitor_auth(runtime, state, cb, generation, mode, inputs);
    }
}

fn running_generation(lifecycle: &MonitorLifecycle) -> u64 {
    match lifecycle {
        MonitorLifecycle::Running(running) => running.generation,
        MonitorLifecycle::Idle | MonitorLifecycle::Starting(_) => 0,
    }
}

fn spawn_monitor_auth(
    runtime: tokio::runtime::Handle,
    state: Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    generation: u64,
    mode: MonitorAuthMode,
    inputs: Vec<MonitorLoginInput>,
) {
    let task_state = state.clone();
    let attach_state = state;
    let restart_runtime = runtime.clone();
    let task = runtime.spawn(async move {
        let attempted_ids: HashSet<String> =
            inputs.iter().map(|input| input.meta.id.clone()).collect();
        let mut tasks = JoinSet::new();
        let mut task_accounts = HashMap::new();
        for input in inputs {
            let account_id = input.meta.id.clone();
            let abort = tasks.spawn(async move {
                let outcome = authed_client(
                    &input.base_url,
                    &input.meta.username,
                    &input.password,
                    &input.cookies,
                )
                .await;
                (
                    input.meta,
                    input.base_url,
                    crate::secrets::Secret::new(input.password),
                    outcome,
                )
            });
            task_accounts.insert(abort.id(), account_id);
        }

        let mut ready = Vec::new();
        let mut refreshed = Vec::new();
        let mut failures = Vec::new();
        while let Some(joined) = tasks.join_next_with_id().await {
            match joined {
                Ok((task_id, (meta, base_url, password, Ok((client, cookies))))) => {
                    task_accounts.remove(&task_id);
                    refreshed.push((meta.id.clone(), cookies));
                    ready.push(monitor::Account {
                        id: meta.id.clone(),
                        device_id: meta.device_id.clone(),
                        user_no: login::user_no_from_username(&meta.username),
                        is_teacher: meta.is_teacher,
                        course_id: meta.course_id.clone(),
                        base_url,
                        client,
                        username: meta.username,
                        password,
                    });
                }
                Ok((task_id, (meta, _base_url, _password, Err(error)))) => {
                    task_accounts.remove(&task_id);
                    failures.push((meta.id, error));
                }
                Err(error) => {
                    let account_id = task_accounts
                        .remove(&error.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    failures.push((account_id, INTERNAL_ERROR.to_string()));
                }
            }
        }

        {
            let mut guard = lock_state(&task_state);
            let Some(st) = guard.as_mut() else {
                return;
            };
            let current = match mode {
                MonitorAuthMode::Start => st.monitor.is_starting(generation),
                MonitorAuthMode::Upsert => {
                    matches!(&st.monitor, MonitorLifecycle::Running(running) if running.generation == generation)
                }
            };
            if !current {
                return;
            }
            for account_id in &attempted_ids {
                st.login_in_flight.remove(account_id);
            }
            if let Some(vault) = st.vault.as_mut() {
                for (account_id, cookies) in refreshed {
                    if let Some(mut secret) = vault.get(&account_id) {
                        secret.cookies = cookies;
                        if let Err(error) = vault.set(&account_id, secret) {
                            failures.push((account_id, format!("session persistence failed: {error}")));
                        }
                    }
                }
            }
            for account in &ready {
                st.supervisor
                    .set_login_state(account.id.clone(), LoginState::Online, None);
            }
            for (account_id, message) in failures {
                st.supervisor.set_login_state(
                    account_id,
                    LoginState::Error,
                    Some(WireError {
                        code: "login_failed".to_string(),
                        message,
                    }),
                );
            }
            if st.supervisor.reconcile(&mut st.config) {
                if let Err(error) = st.config.save(&st.config_path()) {
                    emit(
                        cb,
                        &json!({ "id": null, "event": "Error", "severity": "error",
                        "code": "runtime_persistence_failed", "message": error }),
                    );
                }
            }
            let desired = monitor_plan(st.supervisor.plan());
            match mode {
                MonitorAuthMode::Start => {
                    let loaded: HashSet<String> =
                        ready.iter().map(|account| account.id.clone()).collect();
                    let has_detector = desired
                        .routes
                        .iter()
                        .any(|route| loaded.contains(&route.detector_account_id));
                    if !has_detector {
                        st.monitor = MonitorLifecycle::Idle;
                    } else {
                        let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                        let (runtime_tx, runtime_rx) =
                            tokio::sync::mpsc::unbounded_channel::<monitor::RuntimeEvent>();
                        let handle = monitor::start(
                            cb,
                            ready,
                            desired,
                            monitor_config(st),
                            panic_tx,
                            runtime_tx,
                        );
                        st.monitor = MonitorLifecycle::Running(RunningMonitor {
                            generation,
                            handle,
                            loaded_account_ids: loaded,
                            loading_account_ids: HashSet::new(),
                        });
                        spawn_monitor_watchdog(
                            restart_runtime.clone(),
                            task_state.clone(),
                            cb,
                            generation,
                            panic_rx,
                        );
                        spawn_monitor_runtime_listener(
                            restart_runtime.clone(),
                            task_state.clone(),
                            cb,
                            generation,
                            runtime_rx,
                        );
                    }
                }
                MonitorAuthMode::Upsert => {
                    if let MonitorLifecycle::Running(running) = &mut st.monitor {
                        running
                            .loading_account_ids
                            .retain(|account_id| !attempted_ids.contains(account_id));
                        running
                            .loaded_account_ids
                            .extend(ready.iter().map(|account| account.id.clone()));
                        let _ = running.handle.upsert_accounts(ready);

                        let _ = running.handle.apply_plan(desired, false);
                    }
                }
            }
            emit_monitoring_snapshot(cb, st);
        }
        reconcile_monitor_with(restart_runtime, task_state, cb, false);
    });

    if matches!(mode, MonitorAuthMode::Start) {
        let mut guard = lock_state(&attach_state);
        match guard.as_mut() {
            Some(st) => st.monitor.attach_start_task(generation, task),
            None => task.abort(),
        }
    }
}
#[derive(Clone)]
struct CourseQueryFailure {
    account_id: String,
    code: String,
    message: String,
}

fn begin_course_query(
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

async fn query_common_courses(
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

fn course_failures_json(failures: &[CourseQueryFailure]) -> Value {
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

fn spawn_list_common_courses(core: &Core, id: u64, member_account_ids: Vec<String>) {
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

fn spawn_validated_group_command(core: &Core, command: Command) {
    let cb = core.cb;
    let state = core.state.clone();
    let runtime = core.rt.handle().clone();
    let (id, members, requested, cancel_removed_pending) = match &command {
        Command::CreateGroup { id, group, .. } => (
            *id,
            group.member_account_ids.clone(),
            group.course_ids.clone(),
            false,
        ),
        Command::UpdateGroup { id, group, .. } => (
            *id,
            group.member_account_ids.clone(),
            group.course_ids.clone(),
            true,
        ),
        Command::MergeGroups { id, group, .. } => (
            *id,
            group.member_account_ids.clone(),
            group.course_ids.clone(),
            true,
        ),
        _ => unreachable!("only bound group mutations are validated asynchronously"),
    };
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
/// Import supplied browser cookies, verify them, then persist them for one account.
fn spawn_import_cookies(core: &Core, id: u64, account_id: String, cookies_json: String) {
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

fn emit_providers(cb: EventCb, st: &CoreState) {
    emit(
        cb,
        &json!({ "id": null, "event": "Providers",
                      "default_key": st.registry.default_key,
                      "schools": st.registry.schools }),
    );
}

fn emit_monitoring_snapshot(cb: EventCb, st: &CoreState) {
    let snapshot = st.supervisor.snapshot(&st.config, &st.login_in_flight);
    emit(
        cb,
        &json!({ "id": null, "event": "MonitoringSnapshot", "snapshot": snapshot }),
    );
}

fn emit_caps(cb: EventCb) {
    emit(cb, &caps_payload());
}

fn caps_payload() -> Value {
    // Captcha is human-in-loop (no OCR), so `ocr_captcha` stays false. QR teacher-assist IS implemented
    // (monitor::spawn_qr_teacher_assist) — the build supports it; it just needs a teacher account added.
    json!({ "id": null, "event": "Caps", "caps": {
        // Background monitoring is only meaningful where the OS can keep the app alive while the
        // UI is not foregrounded (Android). The Windows/Mac desktop build has no background host,
        // so the capability is not advertised there — the UI must not offer a toggle it can't honor.
        "background_monitoring": cfg!(target_os = "android"),
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

fn publish_login_state(
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
    let has_llm_key = st.vault.as_ref().is_some_and(|v| v.has_llm_key());
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
    use std::path::Path;
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
        assert_eq!(
            caps["caps"]["background_monitoring"],
            cfg!(target_os = "android"),
            "background monitoring is only advertised where the OS keeps the app alive in background"
        );
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
            loaded_account_ids: HashSet::new(),
            loading_account_ids: HashSet::new(),
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
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut lifecycle = MonitorLifecycle::Idle;
        lifecycle.begin_start(1, 501).unwrap();

        stop_monitor(&mut lifecycle, collect_event, "cancelled by test");

        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                    schedule: ScheduleBinding::Disabled,
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
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let core = init(collect_event).unwrap();

        // A numeric secret-shaped payload in a sensitive field: serde fails on `key`, and the seam
        // must not reflect the value (the old message embedded the serde literal, echoing the number).
        send(&core, br#"{"id":77,"cmd":"SetLlmKey","key":123456789}"#);
        // Truly malformed JSON without a recoverable id → uncorrelated Error event, still no echo.
        send(&core, br#"{"cmd":"SetLlmKey","key":987654321"#);

        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
        events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let core = init(collect_event).unwrap();

        panic_reply(
            &core,
            br#"{"id":88,"cmd":"SetLlmKey","key":"super-secret-llm-key"}"#,
        );

        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            ("config.json", "CONFIG-SECRET-ECHO", "config_unavailable"),
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

    fn none_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if events()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(&pred)
            {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        true
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

    fn post(base_url: &str, path: &str, body: &str) -> String {
        use std::io::{Read, Write};
        let mut stream =
            std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
    }

    fn account_id_by_label(label: &str) -> Option<String> {
        let guard = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::test_support::account_id(&guard, label)
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
        let registry = Registry::load_or_seed(&data_dir.join("providers.json"))
            .unwrap()
            .registry;
        Arc::new(Mutex::new(Some(CoreState {
            data_dir,
            registry,
            config,
            vault,
            monitor: MonitorLifecycle::Idle,
            monitor_generation: 0,
            supervisor: TargetSupervisor::new(None),
            pending_captcha: HashMap::new(),
            login_in_flight: HashSet::new(),
        })))
    }

    #[test]
    fn active_target_only_blocks_its_accounts_and_legacy_switch_is_removed() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let base = start_fake();
        let dir = data_dir("target-account-guard");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        for (id, label) in [(2, "a"), (3, "b")] {
            send(
                &core,
                format!(
                    r#"{{"id":{id},"cmd":"AddAccount","label":"{label}","school":"{base}","username":"{label}","password":"secret"}}"#
                )
                .as_bytes(),
            );
            assert!(wait_for(
                |v| v["event"] == "Reply" && v["id"] == id && v["ok"] == true,
                5
            )
            .is_some());
        }
        let a = account_id_by_label("a").expect("account a");
        let b = account_id_by_label("b").expect("account b");
        let event_snapshot = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let definition = crate::test_support::latest_monitoring_snapshot(&event_snapshot).unwrap();
        send(
            &core,
            crate::test_support::apply_clock_command(4, definition).as_bytes(),
        );
        send(
            &core,
            crate::test_support::start_account_command(5, &a).as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == true,
            10
        )
        .is_some());
        assert!(wait_for(
            |v| crate::test_support::event_account_login_state(v, &a, "online"),
            10
        )
        .is_some());

        send(
            &core,
            format!(r#"{{"id":6,"cmd":"Login","account_id":"{b}"}}"#).as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "LoginResult" && v["id"] == 6 && v["ok"] == true,
                10
            )
            .is_some(),
            "unrelated account may be re-verified"
        );

        send(
            &core,
            format!(r#"{{"id":7,"cmd":"Login","account_id":"{a}"}}"#).as_bytes(),
        );
        let login_guard = wait_for(|v| v["event"] == "Reply" && v["id"] == 7, 5)
            .expect("active-account login guard");
        assert_eq!(login_guard["ok"], false);
        assert_eq!(login_guard["error"], "account_is_in_use");

        let current = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let revision = crate::test_support::latest_monitoring_snapshot(&current).unwrap()
            ["config_revision"]
            .as_u64()
            .unwrap();
        send(
            &core,
            format!(
                r#"{{"id":8,"cmd":"DeleteAccount","account_id":"{a}","expected_revision":{revision},"remove_from_groups":true}}"#
            )
            .as_bytes(),
        );
        let delete_guard = wait_for(|v| v["event"] == "Reply" && v["id"] == 8, 5)
            .expect("active-account delete guard");
        assert_eq!(delete_guard["ok"], false);
        assert_eq!(delete_guard["error"], "account_is_in_use");

        send(
            &core,
            format!(
                r#"{{"id":9,"cmd":"AddAccount","label":"c","school":"{base}","username":"c","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "Reply" && v["id"] == 9 && v["ok"] == true,
                5
            )
            .is_some(),
            "unrelated account remains manageable"
        );

        send(
            &core,
            format!(r#"{{"id":10,"cmd":"SwitchAccount","account_id":"{a}"}}"#).as_bytes(),
        );
        let removed =
            wait_for(|v| v["event"] == "Reply" && v["id"] == 10, 5).expect("removed command reply");
        assert_eq!(removed["ok"], false);
        assert_eq!(removed["error"], MALFORMED_COMMAND);
    }

    #[test]
    fn login_progress_is_published_only_through_monitoring_snapshots() {
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
        send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        send(
            &core,
            format!(
                r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"test","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
        assert!(wait_for(|v| crate::test_support::event_has_account(v, "dave"), 5).is_some());
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
        let logging_in = events.iter().position(|event| {
            crate::test_support::event_account_login_state(event, &acc, "logging_in")
        });
        let online = events.iter().position(|event| {
            crate::test_support::event_account_login_state(event, &acc, "online")
        });
        assert!(
            logging_in.is_some(),
            "login starts with a full snapshot containing logging_in"
        );
        assert!(
            online.is_some(),
            "login ends with a full snapshot containing online"
        );
        assert!(
            logging_in.unwrap() < online.unwrap(),
            "logging_in must precede online"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["event"].as_str(),
                Some("StateChanged" | "Accounts" | "AccountStatus")
            )),
            "removed account/global authority events must never return"
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
        // Real e2e against the fake server: the captcha challenge parks the Login task, and
        // DeleteAccount must cancel it — no injected state, no sleeps. The LoginResult is the
        // task's LAST event, so receiving it proves the completion (and any persist) is done.
        let base = start_fake();
        let dir = data_dir("captcha-cancel");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        post(
            &base,
            "/_test/captcha",
            r#"{"required":true,"expected":"A1B2"}"#,
        );
        send(
            &core,
            format!(
                r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
        assert!(wait_for(|v| crate::test_support::event_has_account(v, "dave"), 5).is_some());
        let acc = account_id_by_label("dave").expect("account id");

        send(
            &core,
            format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
                10
            )
            .is_some(),
            "login blocks on the captcha challenge"
        );
        send(
            &core,
            format!(r#"{{"id":5,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        let rejected = wait_for(
            |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == false,
            5,
        )
        .expect("second login rejected");
        assert!(
            rejected["error"]
                .as_str()
                .unwrap()
                .contains("already in progress"),
            "second login must be single-flight rejected"
        );

        // Delete cancels the pending captcha: the awaiting login task wakes and reports honestly.
        let current = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let revision = crate::test_support::latest_monitoring_snapshot(&current).unwrap()
            ["config_revision"]
            .as_u64()
            .unwrap();
        send(
            &core,
            format!(
                r#"{{"id":6,"cmd":"DeleteAccount","account_id":"{acc}","expected_revision":{revision},"remove_from_groups":true}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 6 && v["ok"] == true,
            5
        )
        .is_some());
        let cancelled = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 4, 10)
            .expect("cancelled login result");
        assert_eq!(cancelled["ok"], false);
        assert!(
            cancelled["reason"].as_str().unwrap().contains("cancelled"),
            "the deleted-account login must report cancellation: {}",
            cancelled["reason"]
        );

        // The stale async completion must not have written anything back: the vault has no entry
        // (LoginResult was the task's terminal event, so no straggler write can still land).
        let key = crate::secrets::load_or_create_device_key(
            &std::path::Path::new(&dir).join("device.key"),
        )
        .unwrap();
        let vault = crate::secrets::VaultFile::unlock_with_key(
            &std::path::Path::new(&dir).join("vault.bin"),
            key,
        )
        .unwrap();
        assert!(
            vault.get(&acc).is_none(),
            "deleted account's secret must not be resurrected"
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
            schedule: ScheduleBinding::Disabled,
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
            r#"{"autoanswer_types":["exam","exam"]}"#,
            r#"{"autoanswer_types":["exam","future_kind"]}"#,
            r#"{"radar_strategy":[]}"#,
            r#"{"radar_strategy":["empty_answer",""]}"#,
            r#"{"radar_strategy":["global_wgs84","global_wgs84"]}"#,
            r#"{"radar_strategy":["future_strategy"]}"#,
            r#"{"llm_endpoint":"not-a-url"}"#,
            r#"{"llm_endpoint":"ftp://example.com/v1"}"#,
            r#"{"llm_endpoint":""}"#,
            r#"{"llm_model":"   "}"#,
            r#"{"log_level":"verbose"}"#,
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
        // ...and a valid patch still applies afterwards. Because validation covers the complete clone,
        // this also proves none of the failed semantic patches leaked into memory. Zero max_tokens is
        // a supported persisted sentinel; llm::resolve_max_tokens keeps resolving it to 16384.
        send(
            &core,
            r#"{"id":100,"cmd":"UpdateConfig","patch":{"poll_idle_secs":7,"llm_max_tokens":0}}"#
                .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 100 && v["ok"] == true,
            5
        )
        .is_some());
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(on_disk["settings"]["poll_idle_secs"], 7);
        assert_eq!(on_disk["settings"]["llm_max_tokens"], 0);
        assert_eq!(
            on_disk["settings"]["llm_model"],
            Settings::default().llm_model
        );
        assert_eq!(
            on_disk["settings"]["autoanswer_types"],
            json!(Settings::default().autoanswer_types)
        );
        assert_eq!(
            on_disk["settings"]["radar_strategy"],
            json!(Settings::default().radar_strategy)
        );
        assert_eq!(
            on_disk["settings"]["log_level"],
            Settings::default().log_level
        );
    }

    // ===================== async panic backstop / monitor watchdog =====================

    #[test]
    fn async_panic_observer_turns_panic_into_backstop_and_ignores_cancel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // A panicking task triggers the backstop exactly once.
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let handle = spawn_observed(
            &rt,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            async { panic!("deliberate test panic") },
        );
        rt.block_on(handle).unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // A successful task does not trigger the backstop.
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let handle = spawn_observed(
            &rt,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            async {},
        );
        rt.block_on(handle).unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        // Aborting observed work is cancellation, not a panic; the aborting command owns completion.
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let handle = spawn_observed(
            &rt,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            async { tokio::time::sleep(Duration::from_secs(3600)).await },
        );
        // `sleep` must be constructed INSIDE the block_on future: building it outside would run
        // before the runtime context is entered (no reactor yet) and panic.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await }); // let the inner task start
        handle.abort();
        let joined = rt.block_on(handle);
        assert!(joined.is_err(), "aborting the observer cancels it");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "abort is not a panic");
    }

    #[test]
    fn login_panic_backstop_clears_pending_and_completes_with_fixed_error() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("panic-login");
        std::fs::create_dir_all(&dir).unwrap();
        let state = test_state(PathBuf::from(&dir), Config::default(), None);
        let (captcha_tx, _rx) = oneshot::channel::<String>();
        {
            let mut guard = lock_state(&state);
            let st = guard.as_mut().unwrap();
            st.pending_captcha.insert("acc".into(), captcha_tx);
            st.login_in_flight.insert("acc".into());
        }

        recover_login_panic(&state, collect_event, 77, "acc");

        {
            let guard = lock_state(&state);
            let st = guard.as_ref().unwrap();
            assert!(
                st.pending_captcha.is_empty(),
                "panic backstop must drop the stale captcha sender"
            );
            assert!(st.login_in_flight.is_empty());
        }
        let events = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = events
            .iter()
            .find(|e| e["event"] == "LoginResult" && e["id"] == 77)
            .expect("fixed LoginResult completes the awaiting command");
        assert_eq!(result["ok"], false);
        assert_eq!(result["reason"], INTERNAL_ERROR);
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "MonitoringSnapshot"),
            "panic must publish the authoritative monitoring snapshot"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"] == "LoginResult" && e["id"] == 77)
                .count(),
            1,
            "exactly one terminal event for the command"
        );
    }

    #[test]
    fn monitor_panic_watchdog_tears_down_only_the_matching_running_generation() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = data_dir("watchdog");
        std::fs::create_dir_all(&dir).unwrap();
        let state = test_state(PathBuf::from(&dir), Config::default(), None);
        let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut guard = lock_state(&state);
            guard.as_mut().unwrap().monitor = MonitorLifecycle::Running(RunningMonitor {
                generation: 7,
                handle: monitor::MonitorHandle::new(tx),
                loaded_account_ids: HashSet::new(),
                loading_account_ids: HashSet::new(),
            });
        }
        spawn_monitor_watchdog(
            rt.handle().clone(),
            state.clone(),
            collect_event,
            7,
            panic_rx,
        );

        // A panic ping for the matching generation tears the monitor down and emits one closed error.
        panic_tx.send(()).unwrap();
        assert!(
            wait_for(
                |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
                5
            )
            .is_some(),
            "the watchdog must publish monitor_panicked"
        );
        {
            let guard = lock_state(&state);
            assert!(
                matches!(guard.as_ref().unwrap().monitor, MonitorLifecycle::Idle),
                "the panicked monitor must be taken down to Idle"
            );
        }

        // A second ping is a no-op: already Idle, no duplicate events.
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        panic_tx.send(()).unwrap();
        assert!(
            none_for(
                |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
                1
            ),
            "a ping after teardown must not emit anything"
        );
    }

    #[test]
    fn monitor_panic_watchdog_ignores_stale_generation_and_sender_close() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = data_dir("watchdog-stale");
        std::fs::create_dir_all(&dir).unwrap();
        let state = test_state(PathBuf::from(&dir), Config::default(), None);
        let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut guard = lock_state(&state);
            guard.as_mut().unwrap().monitor = MonitorLifecycle::Running(RunningMonitor {
                generation: 9,
                handle: monitor::MonitorHandle::new(tx),
                loaded_account_ids: HashSet::new(),
                loading_account_ids: HashSet::new(),
            });
        }
        // Watchdog from an OLDER start (generation 8): its ping must not cancel generation 9.
        spawn_monitor_watchdog(
            rt.handle().clone(),
            state.clone(),
            collect_event,
            8,
            panic_rx,
        );
        panic_tx.send(()).unwrap();
        assert!(
            none_for(
                |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
                1
            ),
            "a stale watchdog must never cancel a newer session"
        );
        {
            let guard = lock_state(&state);
            assert!(
                matches!(
                    guard.as_ref().unwrap().monitor,
                    MonitorLifecycle::Running(_)
                ),
                "a stale watchdog must never cancel a newer session"
            );
        }

        // Normal close: all senders dropped (a plain stop drops the handle) → the watchdog exits
        // without touching the running monitor.
        drop(panic_tx);
        assert!(
            none_for(
                |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
                1
            ),
            "channel close (normal stop) must be a no-op"
        );
        {
            let guard = lock_state(&state);
            assert!(
                matches!(
                    guard.as_ref().unwrap().monitor,
                    MonitorLifecycle::Running(_)
                ),
                "channel close must not tear down a running monitor"
            );
        }
    }

    #[test]
    fn stopping_a_start_aborts_its_observed_task_and_child() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        use std::sync::atomic::{AtomicBool, Ordering};
        struct TrackDrop(Arc<AtomicBool>);
        impl Drop for TrackDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut lifecycle = MonitorLifecycle::Idle;
        lifecycle.begin_start(1, 501).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let track = TrackDrop(dropped.clone());
        let task = spawn_observed(&rt, || {}, async move {
            let _track = track; // held by the child future; drop proves the abort reached it
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await }); // child starts and holds _track
        lifecycle.attach_start_task(1, task);
        stop_monitor(&mut lifecycle, collect_event, "cancelled");
        assert!(matches!(lifecycle, MonitorLifecycle::Idle));

        // The abort propagated into the child: the tracked value must drop promptly (no 3600s linger).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "aborting the start task must abort its child"
            );
            rt.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        }
    }

    #[test]
    fn submit_captcha_to_a_dead_sender_fails_instead_of_succeeding() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("dead-captcha");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        // A challenge whose awaiting login task is already gone (the receiver side was dropped):
        // the entry still exists (the panic backstop raced the submit), but the send must fail.
        let (dead_tx, rx) = oneshot::channel::<String>();
        drop(rx); // `_rx` would stay alive to end of scope — drop it NOW so the sender is dead
        {
            let mut guard = lock_state(&core.state);
            guard
                .as_mut()
                .unwrap()
                .pending_captcha
                .insert("acc".into(), dead_tx);
        }
        send(
            &core,
            r#"{"id":2,"cmd":"SubmitCaptcha","account_id":"acc","text":"A1B2"}"#.as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).expect("submit reply");
        assert_eq!(reply["ok"], false, "a dead challenge cannot be submitted");
        assert!(
            reply["error"].as_str().unwrap().contains("withdrawn"),
            "error: {}",
            reply["error"]
        );
    }

    #[test]
    fn account_mutations_recover_a_stale_journal_before_mutating() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = data_dir("stale-journal");
        let core = init(collect_event).unwrap();
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        send(
            &core,
            r#"{"id":3,"cmd":"AddAccount","label":"dave","school":"thu","username":"dave","password":"secret"}"#
                .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
            5
        )
        .is_some());

        // Simulate a crash between begin and complete: a stale journal is now on disk.
        AccountJournal::begin(Path::new(&dir), AccountMutation::Add, "dave").unwrap();

        // AddAccount recovers it first: warn LogLine, and the mutation is NOT blocked.
        send(
            &core,
            r#"{"id":4,"cmd":"AddAccount","label":"eve","school":"thu","username":"eve","password":"secret"}"#
                .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 4 && v["ok"] == true,
            5
        )
        .is_some());
        {
            // Scoped so the guard never shadows the `events()` helper for later assertions.
            let events = events()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(
                events.iter().any(|e| {
                    e["event"] == "LogLine"
                        && e["level"] == "warn"
                        && e["text"].as_str().is_some_and(|t| t.contains("已恢復"))
                }),
                "a recovered stale journal warns"
            );
        }

        // A CORRUPT journal fails the mutation with the FIXED error (no file content echoed).
        std::fs::write(
            Path::new(&dir).join("account-transaction.json"),
            b"NOT-JSON-CONTENT-SECRET",
        )
        .unwrap();
        send(
            &core,
            r#"{"id":5,"cmd":"AddAccount","label":"frank","school":"thu","username":"frank","password":"secret"}"#
                .as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 5, 5)
            .expect("mutation must fail on a corrupt journal");
        assert_eq!(reply["ok"], false);
        assert_eq!(
            reply["error"],
            "account transaction journal unreadable; recovery skipped"
        );
        assert!(
            !events()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|e| e.to_string().contains("NOT-JSON-CONTENT-SECRET")),
            "the fixed error must never echo journal file content"
        );
    }

    #[test]
    fn captcha_timeout_reports_failure_and_allows_relogin() {
        let _g = LIFECYCLE_SEQ
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let base = start_fake();
        let dir = data_dir("captcha-timeout");
        // This core alone shortens its captcha answer window — every other core (and every other
        // module's e2e running in parallel) keeps the production 180 s.
        let mut core = init(collect_event).unwrap();
        core.captcha_answer_timeout = Duration::from_millis(200);
        send(
            &core,
            format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
        );
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
        send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
        assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
        post(
            &base,
            "/_test/captcha",
            r#"{"required":true,"expected":"A1B2"}"#,
        );
        send(
            &core,
            format!(
                r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
            5
        )
        .is_some());
        let acc = account_id_by_label("dave").expect("account id");

        // Park on the challenge and let the (test-shortened) answer window elapse.
        send(
            &core,
            format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
                10
            )
            .is_some(),
            "login blocks on the captcha challenge"
        );
        let timed_out = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 4, 15)
            .expect("timeout must produce a terminal LoginResult");
        assert_eq!(timed_out["ok"], false);
        assert!(
            timed_out["reason"].as_str().unwrap().contains("timed out"),
            "reason: {}",
            timed_out["reason"]
        );

        // The single-flight marker and stale pending entry were released: the SAME account can
        // log in again immediately, and answering the fresh challenge succeeds. Clear first so the
        // awaited CaptchaChallenge provably belongs to the NEW attempt, not the timed-out one.
        events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        send(
            &core,
            format!(r#"{{"id":5,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
        );
        assert!(
            wait_for(
                |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
                10
            )
            .is_some(),
            "after a timeout the account must be able to log in again"
        );
        send(
            &core,
            format!(r#"{{"id":6,"cmd":"SubmitCaptcha","account_id":"{acc}","text":"A1B2"}}"#)
                .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == 6 && v["ok"] == true,
            5
        )
        .is_some());
        let ok =
            wait_for(|v| v["event"] == "LoginResult" && v["id"] == 5, 10).expect("relogin result");
        assert_eq!(ok["ok"], true, "relogin with the fresh answer succeeds");
    }
}
