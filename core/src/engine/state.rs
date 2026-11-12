//! What the core holds between commands.

use crate::monitor;
use crate::store::config::Config;
use crate::store::providers::Registry;
use crate::store::secrets::VaultFile;
use crate::supervisor::TargetSupervisor;
use crate::wire::MonitoringSnapshot;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub(super) struct CoreState {
    pub(super) data_dir: PathBuf,
    pub(super) registry: Registry,
    pub(super) config: Config,
    pub(super) vault: Option<VaultFile>, // Some(..) once unlocked
    pub(super) monitor: MonitorLifecycle,
    pub(super) monitor_generation: u64,
    pub(super) supervisor: TargetSupervisor,
    /// In-flight captcha logins: account_id → the channel that delivers the user's typed answer.
    pub(super) pending_captcha: HashMap<String, oneshot::Sender<String>>,
    /// Same-account login single-flight: account ids with a Login task currently in flight. A second
    /// Login for the same account is rejected until the first finishes; every terminal path removes it.
    pub(super) login_in_flight: HashSet<String>,
}

pub(super) struct StartingMonitor {
    pub(super) generation: u64,
    pub(super) command_id: u64,
    pub(super) task: Option<JoinHandle<()>>,
}

pub(super) struct RunningMonitor {
    pub(super) generation: u64,
    pub(super) handle: monitor::MonitorHandle,
    pub(super) loaded_account_ids: HashSet<String>,
    pub(super) loading_account_ids: HashSet<String>,
}

pub(super) enum MonitorLifecycle {
    Idle,
    Starting(StartingMonitor),
    Running(RunningMonitor),
}

pub(super) enum StoppedMonitor {
    Idle,
    Starting(StartingMonitor),
    Running(monitor::MonitorHandle),
}

impl MonitorLifecycle {
    pub(super) fn begin_start(
        &mut self,
        generation: u64,
        command_id: u64,
    ) -> Result<(), &'static str> {
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

    pub(super) fn is_starting(&self, generation: u64) -> bool {
        matches!(self, Self::Starting(starting) if starting.generation == generation)
    }

    pub(super) fn attach_start_task(&mut self, generation: u64, task: JoinHandle<()>) {
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

    pub(super) fn running_handle(&self) -> Option<&monitor::MonitorHandle> {
        match self {
            Self::Running(running) => Some(&running.handle),
            _ => None,
        }
    }

    pub(super) fn take_for_stop(&mut self) -> StoppedMonitor {
        match std::mem::replace(self, Self::Idle) {
            Self::Idle => StoppedMonitor::Idle,
            Self::Starting(starting) => StoppedMonitor::Starting(starting),
            Self::Running(running) => StoppedMonitor::Running(running.handle),
        }
    }
}

impl CoreState {
    pub(super) fn vault_mut(&mut self) -> Result<&mut VaultFile, String> {
        self.vault
            .as_mut()
            .ok_or_else(|| "vault is locked".to_string())
    }

    /// The complete state the UI renders.
    pub(super) fn snapshot(&self) -> MonitoringSnapshot {
        self.supervisor
            .snapshot(&self.config, &self.login_in_flight)
    }

    pub(super) fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
}

/// Open (or first-run create) the vault with the persistent device key — auto-unlock, no master
/// password. Only a genuinely missing vault may be created. Every failure opening an existing vault
/// is returned without mutating either file so recovery remains possible.
pub(super) fn open_vault_auto(
    dir: &std::path::Path,
    host_key: Option<[u8; 32]>,
) -> Result<VaultFile, String> {
    let key = match host_key {
        Some(key) => key,
        None => crate::store::secrets::load_or_create_device_key(&dir.join("device.key"))?,
    };
    let vault_path = dir.join("vault.bin");
    if VaultFile::exists(&vault_path) {
        VaultFile::unlock_with_key(&vault_path, key)
    } else {
        VaultFile::create_with_key(&vault_path, key)
    }
}
