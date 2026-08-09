//! The wire schema across the FFI seam. Commands (UI → core) are parsed strictly — this is a
//! trust boundary, so a malformed command becomes an Error event, never a panic. Events (core →
//! UI) are emitted as free-form JSON at the call site (see `engine::emit`).

use crate::quiz::Answer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerWire {
    Options { option_ids: Vec<String> },
    Blanks { values: Vec<String> },
    Text { value: String },
    Vote { letters: Vec<String> },
}

impl AnswerWire {
    pub fn into_answer(self) -> Result<Answer, String> {
        match self {
            Self::Options { option_ids }
                if !option_ids.is_empty() && option_ids.iter().all(|id| !id.trim().is_empty()) =>
            {
                Ok(Answer::Options(option_ids))
            }
            Self::Blanks { values }
                if !values.is_empty() && values.iter().all(|value| !value.trim().is_empty()) =>
            {
                Ok(Answer::Blanks(values))
            }
            Self::Text { value } if !value.trim().is_empty() => Ok(Answer::Text(value)),
            Self::Vote { letters }
                if !letters.is_empty() && letters.iter().all(|letter| !letter.trim().is_empty()) =>
            {
                Ok(Answer::Vote(letters))
            }
            _ => Err("answer payload is empty or malformed".to_string()),
        }
    }

    pub fn from_answer(answer: &Answer) -> Self {
        match answer {
            Answer::Options(option_ids) => Self::Options {
                option_ids: option_ids.clone(),
            },
            Answer::Blanks(values) => Self::Blanks {
                values: values.clone(),
            },
            Answer::Text(value) => Self::Text {
                value: value.clone(),
            },
            Answer::Vote(letters) => Self::Vote {
                letters: letters.clone(),
            },
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Options { option_ids } => option_ids.join(", "),
            Self::Blanks { values } => values.join(" ||| "),
            Self::Text { value } => value.clone(),
            Self::Vote { letters } => letters.join(", "),
        }
    }
}

/// UI → core. Internally tagged by `cmd`; every variant carries the correlation `id` the caller
/// assigned, which the core echoes back on the matching reply event.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    /// Load registry + config from `data_dir`; the vault auto-unlocks with the device key here, then
    /// emit Providers/Accounts/VaultState/Caps.
    Init {
        id: u64,
        data_dir: String,
        /// UI hosts pass a raw vault key recovered through DPAPI / Android Keystore. None retains
        /// the headless/test compatibility path that owns `device.key` itself.
        #[serde(default)]
        device_key_b64: Option<String>,
    },
    /// Idempotent no-ops: the vault auto-unlocks at Init (no master password). Kept for wire back-compat.
    CreateVault { id: u64 },
    Unlock { id: u64 },
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
    SignNow { id: u64, activity_token: String },
    DeferSignIn { id: u64, activity_token: String },

    /// User decisions on an in-flight quiz (docs 20 flow A). Submit/hold/discard are per merged
    /// activity; SetAnswer resolves one account's one subject (conflicts are per-account).
    SubmitNow { id: u64, activity_token: String },
    HoldAnswer { id: u64, activity_token: String },
    DiscardAnswer { id: u64, activity_token: String },
    SetAnswer {
        id: u64,
        activity_token: String,
        account_id: String,
        subject_id: String,
        answer: AnswerWire,
    },

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
            | Command::CreateVault { id }
            | Command::Unlock { id }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_commands_require_token_and_typed_answer() {
        let command: Command = serde_json::from_str(
            r#"{"id":7,"cmd":"SetAnswer","activity_token":"opaque","account_id":"a","subject_id":"s","answer":{"kind":"options","option_ids":["12"]}}"#,
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::SetAnswer {
                activity_token,
                answer: AnswerWire::Options { option_ids },
                ..
            } if activity_token == "opaque" && option_ids == ["12"]
        ));

        assert!(serde_json::from_str::<Command>(
            r#"{"id":8,"cmd":"SubmitNow","quiz_id":"ambiguous"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Command>(
            r#"{"id":9,"cmd":"SetAnswer","activity_token":"opaque","account_id":"a","subject_id":"s","answer":"free text"}"#
        )
        .is_err());
    }

    #[test]
    fn empty_answer_wire_is_rejected() {
        assert!(AnswerWire::Text {
            value: "  ".into()
        }
        .into_answer()
        .is_err());
        assert!(AnswerWire::Options { option_ids: vec![] }
            .into_answer()
            .is_err());
    }

    #[test]
    fn init_accepts_an_os_protected_device_key_from_the_host() {
        let command: Command = serde_json::from_str(
            r#"{"id":1,"cmd":"Init","data_dir":"data","device_key_b64":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="}"#,
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::Init {
                device_key_b64: Some(key),
                ..
            } if key.ends_with('=')
        ));
    }

    #[test]
    fn shared_quiz_prepared_fixture_uses_typed_answers() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../protocol/fixtures/quiz_prepared_v1.json"
        ))
        .unwrap();
        assert_eq!(fixture["event"], "QuizPrepared");
        assert_eq!(fixture["schema_version"], 1);
        assert!(fixture["activity_token"]
            .as_str()
            .is_some_and(|token| !token.is_empty()));

        for account in fixture["per_account"].as_array().unwrap() {
            assert!(account["instance_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()));
            for question in account["questions"].as_array().unwrap() {
                let answer: AnswerWire =
                    serde_json::from_value(question["answer"].clone()).unwrap();
                answer.into_answer().unwrap();
            }
        }
    }
}
