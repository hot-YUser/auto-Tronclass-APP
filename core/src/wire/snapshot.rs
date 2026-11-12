//! The monitoring snapshot: the complete, authoritative state the UI renders. The core re-emits it
//! whole after every change, so the UI never has to reconcile deltas.

use crate::store::config::{
    DetectorSelection, PlatformBlock, ScheduleBinding, TargetId, TimeZoneSpec, WeeklySchedule,
};
use serde::{Deserialize, Serialize};

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
    fn monitoring_snapshot_fixture_round_trips_every_closed_variant() {
        let fixture = include_str!("../assets/monitoring_snapshot_v1.json");
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
}
