//! The commands that run to completion under the state lock. Each handler returns what its
//! reply should carry; the reply itself is sent here, once.

use super::accounts::{self, NewAccount};
use super::monitoring::{emit_monitoring_snapshot, route_to_monitor, stop_monitor};
use super::state::{open_vault_auto, CoreState, MonitorLifecycle};
use super::{groups, lock_state, schedule, session, settings, Core};
use crate::config::Config;
use crate::monitor::MonitorMsg;
use crate::providers::Registry;
use crate::secrets::{decode_device_key_base64, Secret, VaultFile};
use crate::supervisor::TargetSupervisor;
use crate::wire::{emit, respond, Command, Event, EventCb, LogLevel, Notice, Problem};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// How a command ended successfully.
pub(super) enum Reply {
    Done,
    Data(Value),
    /// The monitor actor answers this command when it has dealt with it.
    Deferred,
}

/// Why a command failed, and what its reply still carries.
pub(super) struct Failure {
    pub error: String,
    pub data: Option<Value>,
}

impl<T: Into<String>> From<T> for Failure {
    fn from(error: T) -> Self {
        Self {
            error: error.into(),
            data: None,
        }
    }
}

pub(super) type Handled = Result<Reply, Failure>;

pub(super) fn handle_sync(core: &Core, command: Command) {
    handle_sync_state(&core.state, core.cb, command);
}

pub(super) fn handle_sync_state(
    state: &Arc<Mutex<Option<CoreState>>>,
    cb: EventCb,
    command: Command,
) {
    let id = command.id();
    let mut slot = lock_state(state);
    let handled = if let Command::Init {
        data_dir,
        device_key_b64,
        ..
    } = command
    {
        init(&mut slot, cb, Path::new(&data_dir), device_key_b64)
    } else {
        handle(slot.as_mut(), cb, id, command)
    };
    let reply = match handled {
        Ok(Reply::Deferred) => return,
        Ok(Reply::Done) => Event::reply(Ok(())),
        Ok(Reply::Data(data)) => Event::Reply {
            ok: true,
            error: None,
            data: Some(data),
        },
        Err(failure) => Event::Reply {
            ok: false,
            error: Some(failure.error),
            data: failure.data,
        },
    };
    respond(cb, id, reply);
}

fn handle(st: Option<&mut CoreState>, cb: EventCb, id: u64, command: Command) -> Handled {
    match (command, st) {
        (Command::Shutdown { .. }, st) => {
            if let Some(st) = st {
                shutdown(st, cb);
            }
            Ok(Reply::Done)
        }
        // The vault opens with the device key at Init. These remain only so an older host's
        // start-up sequence still completes.
        (Command::CreateVault { .. } | Command::Unlock { .. }, st) => {
            match st.is_some_and(|st| st.vault.is_some()) {
                true => Ok(Reply::Done),
                false => Err("vault not ready".into()),
            }
        }
        (command, Some(st)) => run(st, cb, id, command),
        (command, None) => Err(uninitialized(&command).into()),
    }
}

fn uninitialized(command: &Command) -> &'static str {
    match command {
        Command::SignNow { .. }
        | Command::DeferSignIn { .. }
        | Command::SubmitNow { .. }
        | Command::HoldAnswer { .. }
        | Command::DiscardAnswer { .. }
        | Command::SetAnswer { .. } => "not monitoring",
        _ => "not initialized",
    }
}

fn run(st: &mut CoreState, cb: EventCb, id: u64, command: Command) -> Handled {
    match command {
        Command::AddAccount {
            label,
            school,
            username,
            password,
            is_teacher,
            course_id,
            ..
        } => accounts::add(
            st,
            cb,
            NewAccount {
                label,
                school,
                username,
                password,
                is_teacher,
                course_id,
            },
        ),
        Command::DeleteAccount {
            account_id,
            expected_revision,
            remove_from_groups,
            ..
        } => accounts::delete(st, cb, &account_id, expected_revision, remove_from_groups),

        Command::CreateGroup {
            expected_revision,
            group,
            ..
        } => groups::create(st, cb, expected_revision, group),
        Command::UpdateGroup {
            group_id,
            expected_revision,
            group,
            ..
        } => groups::update(st, cb, group_id, expected_revision, group),
        Command::DeleteGroup {
            group_id,
            expected_revision,
            ..
        } => groups::delete(st, cb, &group_id, expected_revision),
        Command::MergeGroups {
            group_ids,
            expected_revision,
            group,
            ..
        } => groups::merge(st, cb, group_ids, expected_revision, group),

        Command::SetTargetSchedule {
            target,
            expected_revision,
            schedule,
            ..
        } => schedule::set_target_schedule(st, cb, target, expected_revision, schedule),
        Command::SetMonitoringPreferences {
            expected_revision,
            global_schedule,
            time_zone,
            ..
        } => schedule::set_preferences(st, cb, expected_revision, global_schedule, time_zone),
        Command::ApplyScheduleClock {
            clock_revision,
            config_revision,
            schedule_revision,
            evaluated_at_utc,
            targets,
            ..
        } => schedule::apply_clock(
            st,
            cb,
            clock_revision,
            config_revision,
            schedule_revision,
            &evaluated_at_utc,
            targets,
        ),
        Command::StartTarget { target, .. } => schedule::override_target(st, cb, target, true),
        Command::StopTarget { target, .. } => schedule::override_target(st, cb, target, false),
        Command::StopAllMonitoring { .. } => schedule::stop_all(st, cb),
        Command::ResumeScheduledMonitoring { .. } => schedule::resume(st, cb),
        Command::AcknowledgeTemporaryMerge {
            component_id,
            plan_revision,
            ..
        } => schedule::acknowledge_merge(st, cb, &component_id, plan_revision),
        Command::SuspendForPlatformLimit { reason, .. } => schedule::suspend(st, cb, reason),
        Command::ClearPlatformLimit { reason, .. } => schedule::clear_limit(st, cb, &reason),
        Command::GetMonitoringSnapshot { .. } => {
            Ok(Reply::Data(json!({ "snapshot": st.snapshot() })))
        }

        Command::SignNow { activity_token, .. } => route_to_monitor(
            st,
            MonitorMsg::SignNow {
                command_id: id,
                activity_token,
            },
        ),
        Command::DeferSignIn { activity_token, .. } => route_to_monitor(
            st,
            MonitorMsg::Defer {
                command_id: id,
                activity_token,
            },
        ),
        Command::SubmitNow { activity_token, .. } => route_to_monitor(
            st,
            MonitorMsg::QuizSubmitNow {
                command_id: id,
                activity_token,
            },
        ),
        Command::HoldAnswer { activity_token, .. } => route_to_monitor(
            st,
            MonitorMsg::QuizHold {
                command_id: id,
                activity_token,
            },
        ),
        Command::DiscardAnswer { activity_token, .. } => route_to_monitor(
            st,
            MonitorMsg::QuizDiscard {
                command_id: id,
                activity_token,
            },
        ),
        Command::SetAnswer {
            activity_token,
            account_id,
            subject_id,
            answer,
            ..
        } => route_to_monitor(
            st,
            MonitorMsg::QuizSetAnswer {
                command_id: id,
                activity_token,
                account_id,
                subject_id,
                answer,
            },
        ),

        Command::UpdateConfig { patch, .. } => settings::update(st, cb, &patch),
        Command::SetLlmKey { key, .. } => settings::set_llm_key(st, cb, &key),
        Command::SetQrRemoteKey { key, .. } => settings::set_qr_remote_key(st, cb, &key),
        Command::SubmitCaptcha {
            account_id, text, ..
        } => session::submit_captcha(st, &account_id, text),

        Command::Init { .. }
        | Command::Shutdown { .. }
        | Command::CreateVault { .. }
        | Command::Unlock { .. } => unreachable!("handled before the state is needed"),
        Command::Login { .. }
        | Command::ImportCookies { .. }
        | Command::ListCommonCourses { .. } => unreachable!("handled asynchronously"),
    }
}

/// Load everything under `dir` and install the state. The state is installed even when the vault
/// cannot be opened: the UI then shows its accounts and settings alongside the error.
fn init(
    slot: &mut Option<CoreState>,
    cb: EventCb,
    dir: &Path,
    device_key: Option<Secret>,
) -> Handled {
    // Not fatal by itself, but it is the root cause of whatever fails next: keep it to report.
    let dir_error = std::fs::create_dir_all(dir).err();
    let registry = load_registry(dir, cb, dir_error.as_ref())?;
    let (config, config_notice) = load_config(dir, cb)?;
    let vault = open_vault(dir, cb, &config, device_key);
    let (vault, vault_error) = match vault {
        Ok(vault) => (Some(vault), None),
        Err(error) => (None, Some(error)),
    };
    let unlocked = vault.is_some();

    let mut st = CoreState {
        data_dir: dir.to_path_buf(),
        registry,
        config,
        vault,
        monitor: MonitorLifecycle::Idle,
        monitor_generation: 0,
        supervisor: TargetSupervisor::new(config_notice),
        pending_captcha: HashMap::new(),
        login_in_flight: HashSet::new(),
    };
    st.supervisor.reconcile(&mut st.config);
    settings::emit_providers(cb, &st);
    settings::emit_settings(cb, &st);
    emit_monitoring_snapshot(cb, &st);
    emit(
        cb,
        Event::VaultState {
            exists: unlocked,
            unlocked,
        },
    );
    settings::emit_caps(cb);
    *slot = Some(st);
    match vault_error {
        None => Ok(Reply::Done),
        Some(error) => Err(error.into()),
    }
}

fn load_registry(
    dir: &Path,
    cb: EventCb,
    dir_error: Option<&std::io::Error>,
) -> Result<Registry, String> {
    match Registry::load_or_seed(&dir.join("providers.json")) {
        Ok(loaded) => {
            // The built-in list is in use either way; say so if it could not be written.
            if let Some(warning) = loaded.persist_warning {
                emit(
                    cb,
                    Problem::warning(
                        "providers_not_persisted",
                        format!(
                            "學校清單未能保存（{warning}）；本次使用內建清單，之後新增的學校不會被保存"
                        ),
                    ),
                );
            }
            Ok(loaded.registry)
        }
        Err(error) => {
            // `safe_detail` names the operation and the OS error only: a parse error could
            // otherwise quote the file.
            let mut message = format!("providers registry unavailable：{}", error.safe_detail());
            if let Some(dir_error) = dir_error {
                message.push_str(&format!("；資料目錄建立失敗（{:?}）", dir_error.kind()));
            }
            emit(cb, Problem::error("providers_unavailable", &message));
            Err(message)
        }
    }
}

fn load_config(dir: &Path, cb: EventCb) -> Result<(Config, Option<Notice>), String> {
    let initialized = Config::initialize(&dir.join("config.json")).map_err(|error| {
        let message = format!("設定檔無法載入：{error}");
        emit(cb, Problem::error("config_unavailable", &message));
        message
    })?;
    let notice = initialized.reset_notice.map(|notice| Notice {
        code: "config_reset".to_string(),
        message: "舊版設定已備份；帳號、群組與時間表已重設".to_string(),
        backup_path: Some(notice.backup_path.to_string_lossy().into_owned()),
    });
    Ok((initialized.config, notice))
}

/// Open the vault with the key the host recovered from the OS keystore, then finish or undo an
/// account change that a crash interrupted.
fn open_vault(
    dir: &Path,
    cb: EventCb,
    config: &Config,
    device_key: Option<Secret>,
) -> Result<VaultFile, String> {
    let opened = device_key
        .as_ref()
        .map(|key| decode_device_key_base64(key.expose()))
        .transpose()
        .and_then(|key| open_vault_auto(dir, key));
    let mut vault = opened.inspect_err(|error| {
        emit(cb, Problem::error("vault_open_failed", error));
    })?;
    match accounts::recover_account_transaction(dir, config, &mut vault) {
        Ok(Some(message)) => emit(cb, Event::log(LogLevel::Warn, message)),
        Ok(None) => {}
        // The journal loader's error can quote the file, so the message is fixed.
        Err(_) => emit(
            cb,
            Problem::error(
                "account_transaction_recovery_failed",
                "account transaction journal unreadable; recovery skipped",
            ),
        ),
    }
    Ok(vault)
}

fn shutdown(st: &mut CoreState, cb: EventCb) {
    stop_monitor(&mut st.monitor, cb, "shutdown cancelled monitor start");
    st.pending_captcha.clear();
    st.login_in_flight.clear();
    if let Some(vault) = st.vault.as_mut() {
        vault.lock();
    }
}
