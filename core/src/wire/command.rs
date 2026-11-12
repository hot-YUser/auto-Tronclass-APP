//! UI → core commands. This is a trust boundary: parsing is strict, and a malformed command
//! becomes an error reply, never a panic.

use crate::config::{DetectorSelection, ScheduleBinding, TargetId, TimeZoneSpec, WeeklySchedule};
use crate::quiz::Answer;
use crate::secrets::Secret;
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
                if !letters.is_empty()
                    && letters.iter().all(|letter| !letter.trim().is_empty()) =>
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupInput {
    pub name: String,
    pub member_account_ids: Vec<String>,
    pub course_ids: Vec<String>,
    pub detector: DetectorSelection,
    pub schedule: ScheduleBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleClockEntry {
    pub target: TargetId,
    pub is_open: bool,
    pub window_key: Option<String>,
    pub current_window_start_utc: Option<String>,
    pub next_boundary_utc: Option<String>,
    pub next_is_open: Option<bool>,
    pub clock_error: Option<String>,
}

/// UI → core. The outer command variant remains PascalCase; nested sum types use snake_case `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", deny_unknown_fields)]
pub enum Command {
    Init {
        id: u64,
        data_dir: String,
        #[serde(default)]
        device_key_b64: Option<Secret>,
    },
    CreateVault {
        id: u64,
    },
    Unlock {
        id: u64,
    },
    AddAccount {
        id: u64,
        label: String,
        school: String,
        username: String,
        password: Secret,
        #[serde(default)]
        is_teacher: bool,
        #[serde(default)]
        course_id: Option<String>,
    },
    DeleteAccount {
        id: u64,
        account_id: String,
        expected_revision: u64,
        remove_from_groups: bool,
    },
    Login {
        id: u64,
        account_id: String,
    },
    ImportCookies {
        id: u64,
        account_id: String,
        cookies_json: String,
    },
    SubmitCaptcha {
        id: u64,
        account_id: String,
        text: String,
    },
    CreateGroup {
        id: u64,
        expected_revision: u64,
        group: GroupInput,
    },
    UpdateGroup {
        id: u64,
        group_id: String,
        expected_revision: u64,
        group: GroupInput,
    },
    DeleteGroup {
        id: u64,
        group_id: String,
        expected_revision: u64,
    },
    MergeGroups {
        id: u64,
        group_ids: Vec<String>,
        expected_revision: u64,
        group: GroupInput,
    },
    ListCommonCourses {
        id: u64,
        member_account_ids: Vec<String>,
    },
    SetTargetSchedule {
        id: u64,
        target: TargetId,
        expected_revision: u64,
        schedule: ScheduleBinding,
    },
    SetMonitoringPreferences {
        id: u64,
        expected_revision: u64,
        global_schedule: WeeklySchedule,
        time_zone: TimeZoneSpec,
    },
    ApplyScheduleClock {
        id: u64,
        clock_revision: u64,
        config_revision: u64,
        schedule_revision: u64,
        evaluated_at_utc: String,
        targets: Vec<ScheduleClockEntry>,
    },
    StartTarget {
        id: u64,
        target: TargetId,
    },
    StopTarget {
        id: u64,
        target: TargetId,
    },
    StopAllMonitoring {
        id: u64,
    },
    ResumeScheduledMonitoring {
        id: u64,
    },
    AcknowledgeTemporaryMerge {
        id: u64,
        component_id: String,
        plan_revision: u64,
    },
    SuspendForPlatformLimit {
        id: u64,
        reason: String,
    },
    ClearPlatformLimit {
        id: u64,
        reason: String,
    },
    GetMonitoringSnapshot {
        id: u64,
    },
    SignNow {
        id: u64,
        activity_token: String,
    },
    DeferSignIn {
        id: u64,
        activity_token: String,
    },
    SubmitNow {
        id: u64,
        activity_token: String,
    },
    HoldAnswer {
        id: u64,
        activity_token: String,
    },
    DiscardAnswer {
        id: u64,
        activity_token: String,
    },
    SetAnswer {
        id: u64,
        activity_token: String,
        account_id: String,
        subject_id: String,
        answer: AnswerWire,
    },
    SetLlmKey {
        id: u64,
        key: Secret,
    },
    SetQrRemoteKey {
        id: u64,
        key: Secret,
    },
    UpdateConfig {
        id: u64,
        patch: serde_json::Value,
    },
    Shutdown {
        id: u64,
    },
}

impl Command {
    pub fn id(&self) -> u64 {
        match self {
            Self::Init { id, .. }
            | Self::CreateVault { id }
            | Self::Unlock { id }
            | Self::AddAccount { id, .. }
            | Self::DeleteAccount { id, .. }
            | Self::Login { id, .. }
            | Self::ImportCookies { id, .. }
            | Self::SubmitCaptcha { id, .. }
            | Self::CreateGroup { id, .. }
            | Self::UpdateGroup { id, .. }
            | Self::DeleteGroup { id, .. }
            | Self::MergeGroups { id, .. }
            | Self::ListCommonCourses { id, .. }
            | Self::SetTargetSchedule { id, .. }
            | Self::SetMonitoringPreferences { id, .. }
            | Self::ApplyScheduleClock { id, .. }
            | Self::StartTarget { id, .. }
            | Self::StopTarget { id, .. }
            | Self::StopAllMonitoring { id }
            | Self::ResumeScheduledMonitoring { id }
            | Self::AcknowledgeTemporaryMerge { id, .. }
            | Self::SuspendForPlatformLimit { id, .. }
            | Self::ClearPlatformLimit { id, .. }
            | Self::GetMonitoringSnapshot { id }
            | Self::SignNow { id, .. }
            | Self::DeferSignIn { id, .. }
            | Self::SubmitNow { id, .. }
            | Self::HoldAnswer { id, .. }
            | Self::DiscardAnswer { id, .. }
            | Self::SetAnswer { id, .. }
            | Self::SetLlmKey { id, .. }
            | Self::SetQrRemoteKey { id, .. }
            | Self::UpdateConfig { id, .. }
            | Self::Shutdown { id } => *id,
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
        assert!(AnswerWire::Text { value: "  ".into() }
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
            } if key.expose().ends_with('=')
        ));
    }

    #[test]
    fn shared_quiz_prepared_fixture_uses_typed_answers() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../assets/quiz_prepared_v1.json")).unwrap();
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
    #[test]
    fn clean_cutover_rejects_legacy_monitor_commands_and_missing_revisions() {
        for legacy in [
            r#"{"id":1,"cmd":"StartMonitoring"}"#,
            r#"{"id":1,"cmd":"StopMonitoring"}"#,
            r#"{"id":1,"cmd":"SwitchAccount","account_id":"a"}"#,
            r#"{"id":1,"cmd":"DeleteAccount","account_id":"a"}"#,
        ] {
            assert!(serde_json::from_str::<Command>(legacy).is_err(), "{legacy}");
        }
        assert!(serde_json::from_str::<Command>(
            r#"{"id":1,"cmd":"DeleteAccount","account_id":"a","expected_revision":4,"remove_from_groups":true}"#
        )
        .is_ok());
    }
}
