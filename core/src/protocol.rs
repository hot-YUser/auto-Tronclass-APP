//! The wire schema across the FFI seam. Commands (UI → core) are parsed strictly — this is a
//! trust boundary, so a malformed command becomes an Error event, never a panic. Events (core →
//! UI) are emitted as free-form JSON at the call site (see `engine::emit`).

use serde::Deserialize;

/// UI → core. Internally tagged by `cmd`; every variant carries the correlation `id` the caller
/// assigned, which the core echoes back on the matching reply event.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    /// Load registry + config from `data_dir`; emit Providers/Accounts/VaultState/Caps.
    Init { id: u64, data_dir: String },
    /// First run: create the encrypted vault under this master password.
    CreateVault { id: u64, master_password: String },
    /// Unlock an existing vault with the master password.
    Unlock { id: u64, master_password: String },
    /// Unlock the vault with the platform keystore's stored key (passwordless/biometric). Fails if
    /// no key is stored (the UI then falls back to a master-password `Unlock`).
    UnlockWithKeystore { id: u64 },
    /// Lock the vault: zeroize the in-memory key and drop it (the keystore's stored copy remains).
    LockVault { id: u64 },
    /// Add an account; its password goes straight into the vault, never the config.
    AddAccount {
        id: u64,
        label: String,
        school: String,
        username: String,
        password: String,
        #[serde(default)]
        is_teacher: bool,
        #[serde(default)]
        course_id: Option<String>,
    },
    SwitchAccount { id: u64, account_id: String },
    DeleteAccount { id: u64, account_id: String },
    /// Real login for the given account (resolves school → endpoints, reads secret from vault).
    Login { id: u64, account_id: String },

    /// Import a browser-exported cookie set for an account → vault → verify (no capture UI here).
    ImportCookies { id: u64, account_id: String, cookies_json: String },

    /// Answer a captcha challenge for an in-flight login (paired with the `CaptchaChallenge` event).
    SubmitCaptcha { id: u64, account_id: String, text: String },

    /// Begin/stop concurrent per-account rollcall monitoring.
    StartMonitoring { id: u64 },
    StopMonitoring { id: u64 },

    /// User decisions on an in-flight rollcall (per-activity: all participating accounts).
    SignNow { id: u64, rollcall_id: String },
    DeferSignIn { id: u64, rollcall_id: String },

    /// User decisions on an in-flight quiz (docs 20 flow A). Submit/hold/discard are per merged
    /// activity; SetAnswer resolves one account's one subject (conflicts are per-account).
    SubmitNow { id: u64, quiz_id: String },
    HoldAnswer { id: u64, quiz_id: String },
    DiscardAnswer { id: u64, quiz_id: String },
    SetAnswer { id: u64, quiz_id: String, account_id: String, subject_id: String, answer: serde_json::Value },

    /// Store the LLM API key in the vault (never in config/logs).
    SetLlmKey { id: u64, key: String },

    /// Patch typed settings (e.g. countdown_secs). `patch` is a JSON object merged into Settings.
    UpdateConfig { id: u64, patch: serde_json::Value },

    Shutdown { id: u64 },
}

impl Command {
    /// The correlation id, so the dispatcher can always reply even on early failure.
    pub fn id(&self) -> u64 {
        match self {
            Command::Init { id, .. }
            | Command::CreateVault { id, .. }
            | Command::Unlock { id, .. }
            | Command::UnlockWithKeystore { id }
            | Command::LockVault { id }
            | Command::AddAccount { id, .. }
            | Command::SwitchAccount { id, .. }
            | Command::DeleteAccount { id, .. }
            | Command::Login { id, .. }
            | Command::ImportCookies { id, .. }
            | Command::SubmitCaptcha { id, .. }
            | Command::StartMonitoring { id }
            | Command::StopMonitoring { id }
            | Command::SignNow { id, .. }
            | Command::DeferSignIn { id, .. }
            | Command::SubmitNow { id, .. }
            | Command::HoldAnswer { id, .. }
            | Command::DiscardAnswer { id, .. }
            | Command::SetAnswer { id, .. }
            | Command::SetLlmKey { id, .. }
            | Command::UpdateConfig { id, .. }
            | Command::Shutdown { id } => *id,
        }
    }
}
