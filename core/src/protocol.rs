//! The wire schema across the FFI seam. Commands (UI → core) are parsed strictly — this is a
//! trust boundary, so a malformed command becomes an Error event, never a panic. Events (core →
//! UI) are emitted as free-form JSON at the call site (see `engine::emit`).

use crate::config::{
    DetectorSelection, PlatformBlock, ScheduleBinding, TargetId, TimeZoneSpec, WeeklySchedule,
};
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
        device_key_b64: Option<String>,
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
        password: String,
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
        key: String,
    },
    SetQrRemoteKey {
        id: u64,
        key: String,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub code: String,
    pub message: String,
    pub backup_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Starting,
    Running,
    Stopping,
    PlatformBlocked,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    ForegroundOnly,
    Exact,
    InexactUserActionRequired,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    Student,
    Teacher,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    Stored,
    LoggingIn,
    Online,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetRuntimeState {
    ScheduledOff,
    ManualOff,
    Starting,
    Monitoring,
    Stopping,
    SuppressedByGroup,
    PlatformBlocked,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountResultPhase {
    Idle,
    Pending,
    Authorized,
    Succeeded,
    Failed,
    UnknownAfterRestart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRef {
    pub target: TargetId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSnapshot {
    pub account_id: String,
    pub label: String,
    pub school_ref: String,
    pub username: String,
    pub role: AccountRole,
    pub teacher_course_id: Option<String>,
    pub login_state: LoginState,
    pub login_error: Option<WireError>,
    pub login_in_flight: bool,
    pub in_use_targets: Vec<TargetRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualOverrideSnapshot {
    pub force_open: bool,
    pub expires_at_utc: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorSnapshot {
    pub account_id: String,
    pub is_fallback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupDefinitionSnapshot {
    pub member_account_ids: Vec<String>,
    pub course_ids: Vec<String>,
    pub detector_selection: DetectorSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CourseSnapshot {
    pub course_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountResult {
    pub account_id: String,
    pub phase: AccountResultPhase,
    pub activity_kind: Option<String>,
    pub course_name: Option<String>,
    pub updated_at_utc: Option<String>,
    pub error: Option<WireError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSnapshot {
    pub target: TargetId,
    pub name: String,
    pub runtime_state: TargetRuntimeState,
    pub schedule: ScheduleBinding,
    pub schedule_open: bool,
    pub next_boundary_utc: Option<String>,
    pub manual_override: Option<ManualOverrideSnapshot>,
    pub detector: Option<DetectorSnapshot>,
    pub group_definition: Option<GroupDefinitionSnapshot>,
    pub courses: Vec<CourseSnapshot>,
    pub in_use_account_ids: Vec<String>,
    pub account_results: Vec<AccountResult>,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_edit_schedule: bool,
    pub disabled_reason: Option<String>,
    pub error: Option<WireError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeCoverage {
    SingleDetector,
    MultipleDetectorsRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergePrompt {
    pub component_id: String,
    pub group_ids: Vec<String>,
    pub coverage: MergeCoverage,
    pub detector_account_id: Option<String>,
    pub detector_count: u32,
    pub warning: Option<String>,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringSnapshot {
    pub schema_version: u8,
    pub config_revision: u64,
    pub schedule_revision: u64,
    pub plan_revision: u64,
    pub clock_revision: Option<u64>,
    pub session_state: SessionState,
    pub all_suspended: bool,
    pub platform_block: Option<PlatformBlock>,
    pub can_stop_all: bool,
    pub can_resume: bool,
    pub global_disabled_reason: Option<String>,
    pub global_schedule: WeeklySchedule,
    pub time_zone: TimeZoneSpec,
    pub wake_mode: WakeMode,
    pub accounts: Vec<AccountSnapshot>,
    pub targets: Vec<TargetSnapshot>,
    pub merge_prompts: Vec<MergePrompt>,
    pub config_notice: Option<Notice>,
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
            } if key.ends_with('=')
        ));
    }

    #[test]
    fn shared_quiz_prepared_fixture_uses_typed_answers() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("assets/quiz_prepared_v1.json")).unwrap();
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
    fn monitoring_snapshot_fixture_round_trips_every_closed_variant() {
        let fixture = include_str!("assets/monitoring_snapshot_v1.json");
        let snapshot: MonitoringSnapshot = serde_json::from_str(fixture).unwrap();
        assert_eq!(snapshot.schema_version, 1);
        assert!(snapshot
            .accounts
            .iter()
            .any(|account| account.role == AccountRole::Teacher));
        assert!(snapshot
            .targets
            .iter()
            .any(|target| { target.runtime_state == TargetRuntimeState::SuppressedByGroup }));
        assert!(snapshot.targets.iter().any(|target| target
            .detector
            .as_ref()
            .is_some_and(|detector| detector.is_fallback)));
        assert!(snapshot
            .merge_prompts
            .iter()
            .any(|prompt| prompt.coverage == MergeCoverage::SingleDetector));
        assert!(snapshot
            .merge_prompts
            .iter()
            .any(|prompt| prompt.coverage == MergeCoverage::MultipleDetectorsRequired));
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_slice::<MonitoringSnapshot>(&encoded).unwrap(),
            snapshot
        );
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
