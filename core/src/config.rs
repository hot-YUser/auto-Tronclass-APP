//! Versioned persisted definitions and runtime state. Secrets never enter this file.

use crate::providers::Registry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_SCHEMA_VERSION: u8 = 2;
pub(crate) const AUTOANSWER_TYPES: &[&str] = &[
    "exam",
    "questionnaire",
    "homework",
    "vote",
    "classroom",
    "courseware",
];
pub(crate) const RADAR_STRATEGIES: &[&str] = &["empty_answer", "global_wgs84"];
const MINUTES_PER_DAY: u16 = 1_440;
const MINUTES_PER_WEEK: u32 = 7 * MINUTES_PER_DAY as u32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountMeta {
    pub id: String,
    pub label: String,
    /// Registry key/alias or raw base URL.
    pub school_ref: String,
    pub username: String,
    pub device_id: String,
    pub is_teacher: bool,
    pub course_id: Option<String>,
    pub schedule: ScheduleBinding,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub countdown_secs: u64,
    pub attendance_gate_percent: f64,
    pub llm_endpoint: String,
    pub llm_model: String,
    pub llm_max_tokens: u32,
    pub resubmit_for_correct: bool,
    pub max_answer_reask: u32,
    pub prepare_retry_budget_secs: u64,
    pub autoanswer_types: Vec<String>,
    pub enable_llm_tools: bool,
    pub max_tool_iterations: u32,
    pub radar_strategy: Vec<String>,
    pub number_cooldown_ms: u64,
    pub number_max_cooldowns: u32,
    pub poll_idle_secs: u64,
    pub quiz_detect_secs: u64,
    pub qr_remote_enabled: bool,
    pub qr_remote_base_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        let all = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
        Self {
            countdown_secs: 15,
            attendance_gate_percent: 15.0,
            llm_endpoint: "https://integrate.api.nvidia.com/v1/chat/completions".to_string(),
            llm_model: "minimaxai/minimax-m3".to_string(),
            llm_max_tokens: 16_384,
            resubmit_for_correct: true,
            max_answer_reask: 4,
            prepare_retry_budget_secs: 300,
            autoanswer_types: all(AUTOANSWER_TYPES),
            enable_llm_tools: true,
            max_tool_iterations: 3,
            radar_strategy: all(RADAR_STRATEGIES),
            number_cooldown_ms: 5_000,
            number_max_cooldowns: 3,
            poll_idle_secs: 5,
            quiz_detect_secs: 45,
            qr_remote_enabled: false,
            qr_remote_base_url: "https://api.hlp.qzz.io".to_string(),
        }
    }
}

impl Settings {
    /// Validate the complete persisted settings object. This boundary is shared by startup, saves,
    /// and runtime patches so editing config.json cannot bypass the command-side validator.
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_range("countdown_secs", self.countdown_secs, 1, 86_400)?;
        if !self.attendance_gate_percent.is_finite()
            || !(0.0..=100.0).contains(&self.attendance_gate_percent)
        {
            return Err("attendance_gate_percent 必須介於 0 與 100".to_string());
        }
        if !valid_http_url(&self.llm_endpoint) {
            return Err("llm_endpoint 必須是有效的 http(s) URL".to_string());
        }
        if self.llm_model.trim().is_empty() {
            return Err("llm_model 不得為空".to_string());
        }
        validate_range(
            "llm_max_tokens",
            u64::from(self.llm_max_tokens),
            0,
            1_000_000,
        )?;
        validate_closed_unique(
            "autoanswer_types",
            &self.autoanswer_types,
            AUTOANSWER_TYPES,
            true,
        )?;
        validate_closed_unique(
            "radar_strategy",
            &self.radar_strategy,
            RADAR_STRATEGIES,
            false,
        )?;
        validate_range("max_answer_reask", u64::from(self.max_answer_reask), 1, 100)?;
        validate_range(
            "prepare_retry_budget_secs",
            self.prepare_retry_budget_secs,
            1,
            86_400,
        )?;
        validate_range(
            "max_tool_iterations",
            u64::from(self.max_tool_iterations),
            0,
            100,
        )?;
        validate_range("number_cooldown_ms", self.number_cooldown_ms, 1, 3_600_000)?;
        validate_range(
            "number_max_cooldowns",
            u64::from(self.number_max_cooldowns),
            0,
            1_000,
        )?;
        validate_range("poll_idle_secs", self.poll_idle_secs, 1, 86_400)?;
        validate_range("quiz_detect_secs", self.quiz_detect_secs, 1, 86_400)?;
        validate_qr_remote(&self.qr_remote_enabled, &self.qr_remote_base_url)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetId {
    Account { account_id: String },
    Group { group_id: String },
}

impl TargetId {
    pub fn account(account_id: impl Into<String>) -> Self {
        Self::Account {
            account_id: account_id.into(),
        }
    }

    pub fn group(group_id: impl Into<String>) -> Self {
        Self::Group {
            group_id: group_id.into(),
        }
    }

    pub fn stable_key(&self) -> String {
        match self {
            Self::Account { account_id } => format!("account:{account_id}"),
            Self::Group { group_id } => format!("group:{group_id}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DetectorSelection {
    Auto,
    Preferred { account_id: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleBinding {
    #[default]
    Disabled,
    InheritGlobal,
    Custom {
        weekly: WeeklySchedule,
    },
}

impl ScheduleBinding {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Custom { weekly } => weekly.validate(),
            Self::Disabled | Self::InheritGlobal => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklySchedule {
    pub monday: Vec<TimeWindow>,
    pub tuesday: Vec<TimeWindow>,
    pub wednesday: Vec<TimeWindow>,
    pub thursday: Vec<TimeWindow>,
    pub friday: Vec<TimeWindow>,
    pub saturday: Vec<TimeWindow>,
    pub sunday: Vec<TimeWindow>,
}

impl WeeklySchedule {
    pub fn days(&self) -> [&[TimeWindow]; 7] {
        [
            &self.monday,
            &self.tuesday,
            &self.wednesday,
            &self.thursday,
            &self.friday,
            &self.saturday,
            &self.sunday,
        ]
    }

    pub fn is_empty(&self) -> bool {
        self.days().iter().all(|day| day.is_empty())
    }

    /// Validates minute ranges and overlap on a cyclic week. Adjacent half-open ranges are legal.
    pub fn validate(&self) -> Result<(), String> {
        let mut intervals = Vec::<(u32, u32)>::new();
        for (day, windows) in self.days().into_iter().enumerate() {
            for window in windows {
                window.validate()?;
                let start =
                    day as u32 * u32::from(MINUTES_PER_DAY) + u32::from(window.start_minute);
                let mut end =
                    day as u32 * u32::from(MINUTES_PER_DAY) + u32::from(window.end_minute);
                if window.start_minute > window.end_minute {
                    end += u32::from(MINUTES_PER_DAY);
                }
                if end <= MINUTES_PER_WEEK {
                    intervals.push((start, end));
                } else {
                    intervals.push((start, MINUTES_PER_WEEK));
                    intervals.push((0, end - MINUTES_PER_WEEK));
                }
            }
        }
        intervals.sort_unstable_by_key(|range| range.0);
        for pair in intervals.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err("weekly schedule windows overlap".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeWindow {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl TimeWindow {
    pub fn validate(&self) -> Result<(), String> {
        if self.start_minute >= MINUTES_PER_DAY {
            return Err("start_minute must be between 0 and 1439".to_string());
        }
        if self.end_minute > MINUTES_PER_DAY {
            return Err("end_minute must be between 0 and 1440".to_string());
        }
        if self.start_minute == self.end_minute {
            return Err("schedule window cannot be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimeZoneSpec {
    #[default]
    Device,
    Named {
        iana_id: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringPreferences {
    pub global_schedule: WeeklySchedule,
    pub time_zone: TimeZoneSpec,
    pub all_suspended: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringRuntime {
    pub manual_overrides: Vec<ManualOverride>,
    pub group_rotation: BTreeMap<String, GroupRotation>,
    pub platform_block: Option<PlatformBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualOverride {
    pub target: TargetId,
    pub force_open: bool,
    pub expires_at_utc: String,
    pub schedule_revision: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRotation {
    pub next_index: usize,
    pub last_window_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformBlock {
    pub reason: String,
    pub observed_at_utc: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountGroup {
    pub id: String,
    pub name: String,
    pub tenant: String,
    pub member_account_ids: Vec<String>,
    pub course_ids: Vec<String>,
    pub detector: DetectorSelection,
    pub schedule: ScheduleBinding,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u8,
    pub config_revision: u64,
    pub schedule_revision: u64,
    pub accounts: Vec<AccountMeta>,
    pub settings: Settings,
    pub groups: Vec<AccountGroup>,
    pub monitoring: MonitoringPreferences,
    pub runtime: MonitoringRuntime,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            config_revision: 0,
            schedule_revision: 0,
            accounts: Vec::new(),
            settings: Settings::default(),
            groups: Vec::new(),
            monitoring: MonitoringPreferences::default(),
            runtime: MonitoringRuntime::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigResetNotice {
    pub backup_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct InitializedConfig {
    pub config: Config,
    pub reset_notice: Option<ConfigResetNotice>,
}

#[derive(Debug)]
pub enum ConfigLoadError {
    Read(String),
    InvalidJson,
    UnsupportedSchema,
    UnsupportedOrCorrupt,
    Backup(String),
    Write(String),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "read config: {error}"),
            Self::InvalidJson => write!(formatter, "invalid config JSON"),
            Self::UnsupportedSchema => write!(formatter, "unsupported config schema"),
            Self::UnsupportedOrCorrupt => write!(formatter, "unsupported or corrupt config"),
            Self::Backup(error) => write!(formatter, "backup older config: {error}"),
            Self::Write(error) => write!(formatter, "write config: {error}"),
        }
    }
}

impl Config {
    /// Read a config of the current schema; a missing file reads as the empty config.
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Self, ConfigLoadError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(ConfigLoadError::Read(error.to_string())),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| ConfigLoadError::InvalidJson)?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(CONFIG_SCHEMA_VERSION))
        {
            return Err(ConfigLoadError::UnsupportedSchema);
        }
        Self::parse(value)
    }

    /// Load the config for this run, creating it on first start. A file written by an older build
    /// is kept as a backup and replaced by an empty config; the caller is told so through
    /// `reset_notice`. A file that is not JSON, or that a newer build wrote, is left exactly as
    /// it is and reported as an error.
    pub fn initialize(path: &Path) -> Result<InitializedConfig, ConfigLoadError> {
        Self::initialize_at(path, crate::time::now())
    }

    fn initialize_at(path: &Path, unix_seconds: i64) -> Result<InitializedConfig, ConfigLoadError> {
        let fresh = |reset_notice| {
            let config = Self::default();
            config.save(path).map_err(ConfigLoadError::Write)?;
            Ok(InitializedConfig {
                config,
                reset_notice,
            })
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fresh(None),
            Err(error) => return Err(ConfigLoadError::Read(error.to_string())),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| ConfigLoadError::InvalidJson)?;
        if !value.is_object() {
            return Err(ConfigLoadError::UnsupportedOrCorrupt);
        }
        let current = u64::from(CONFIG_SCHEMA_VERSION);
        match value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
        {
            Some(version) if version == current => Ok(InitializedConfig {
                config: Self::parse(value)?,
                reset_notice: None,
            }),
            Some(version) if version > current => Err(ConfigLoadError::UnsupportedSchema),
            // An older build's file, versioned or not.
            _ => {
                let file_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("config.json");
                let backup_path = path.with_file_name(format!(
                    "{file_name}.pre-v{CONFIG_SCHEMA_VERSION}-{unix_seconds}.bak"
                ));
                crate::atomic_file::create_new(&backup_path, &bytes)
                    .map_err(|error| ConfigLoadError::Backup(error.to_string()))?;
                fresh(Some(ConfigResetNotice { backup_path }))
            }
        }
    }

    /// A config of the current schema, strictly: unknown fields and invalid values are errors.
    fn parse(value: serde_json::Value) -> Result<Self, ConfigLoadError> {
        let config: Self =
            serde_json::from_value(value).map_err(|_| ConfigLoadError::UnsupportedOrCorrupt)?;
        config
            .validate_static()
            .map_err(|_| ConfigLoadError::UnsupportedOrCorrupt)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate_static()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        crate::atomic_file::replace(path, &bytes).map_err(|error| format!("write config: {error}"))
    }

    pub fn account(&self, id: &str) -> Option<&AccountMeta> {
        self.accounts.iter().find(|account| account.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut AccountMeta> {
        self.accounts.iter_mut().find(|account| account.id == id)
    }

    pub fn group(&self, id: &str) -> Option<&AccountGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn bump_definition_revision(&mut self, schedule_changed: bool) -> Result<(), String> {
        self.config_revision = self
            .config_revision
            .checked_add(1)
            .ok_or_else(|| "config revision exhausted".to_string())?;
        if schedule_changed {
            self.schedule_revision = self
                .schedule_revision
                .checked_add(1)
                .ok_or_else(|| "schedule revision exhausted".to_string())?;
            self.runtime
                .manual_overrides
                .retain(|entry| entry.schedule_revision == self.schedule_revision);
        }
        Ok(())
    }

    pub fn validate_static(&self) -> Result<(), String> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err("unsupported config schema".to_string());
        }
        self.settings.validate()?;
        self.monitoring.global_schedule.validate()?;
        let mut account_ids = HashSet::new();
        for account in &self.accounts {
            if account.id.trim().is_empty() || !account_ids.insert(account.id.as_str()) {
                return Err("account ids must be non-empty and unique".to_string());
            }
            account.schedule.validate()?;
            if account.is_teacher && !matches!(account.schedule, ScheduleBinding::Disabled) {
                return Err("teacher schedule must be disabled".to_string());
            }
        }
        let mut group_ids = HashSet::new();
        for group in &self.groups {
            if group.id.trim().is_empty() || !group_ids.insert(group.id.as_str()) {
                return Err("group ids must be non-empty and unique".to_string());
            }
            if group.name.trim().is_empty() {
                return Err("group name must not be empty".to_string());
            }
            if group.tenant.trim().is_empty() {
                return Err("group tenant must not be empty".to_string());
            }
            if group.member_account_ids.is_empty() {
                return Err("group must retain at least one member".to_string());
            }
            if has_duplicates(&group.member_account_ids) || has_duplicates(&group.course_ids) {
                return Err("group members and courses must be unique".to_string());
            }
            group.schedule.validate()?;
            if let DetectorSelection::Preferred { account_id } = &group.detector {
                if !group.member_account_ids.contains(account_id) {
                    return Err("preferred detector must be a group member".to_string());
                }
            }
        }
        if self
            .runtime
            .manual_overrides
            .iter()
            .any(|entry| entry.schedule_revision != self.schedule_revision)
        {
            return Err("manual override has stale schedule revision".to_string());
        }
        Ok(())
    }

    pub fn validate_with_registry(&self, registry: &Registry) -> Result<(), String> {
        self.validate_static()?;
        for group in &self.groups {
            let mut tenant = None::<String>;
            for member_id in &group.member_account_ids {
                let account = self
                    .account(member_id)
                    .ok_or_else(|| format!("unknown group member: {member_id}"))?;
                if account.is_teacher {
                    return Err("teacher cannot be a group member".to_string());
                }
                let canonical = canonical_tenant(registry, &account.school_ref)?;
                match &tenant {
                    Some(expected) if expected != &canonical => {
                        return Err("group members must use the same tenant".to_string())
                    }
                    None => tenant = Some(canonical),
                    _ => {}
                }
            }
            if tenant.as_deref() != Some(group.tenant.as_str()) {
                return Err("group tenant does not match its members".to_string());
            }
        }
        Ok(())
    }
}

pub fn canonical_tenant(registry: &Registry, school_ref: &str) -> Result<String, String> {
    let resolved = registry
        .resolve(school_ref.trim())
        .ok_or_else(|| "school reference cannot be resolved".to_string())?;
    let mut url = reqwest::Url::parse(&resolved).map_err(|_| "invalid school URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("school URL must be an HTTP(S) base URL".to_string());
    }
    url.set_query(None);
    url.set_fragment(None);
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Ok(if path.is_empty() {
        origin
    } else {
        format!("{origin}{path}")
    })
}

pub fn new_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return fallback_id();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn new_group_id() -> String {
    new_id()
}

fn fallback_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:016x}{:016x}", nanos ^ counter.rotate_left(32), counter)
}

fn has_duplicates(values: &[String]) -> bool {
    let mut seen = HashSet::with_capacity(values.len());
    values.iter().any(|value| !seen.insert(value.as_str()))
}

fn validate_range(name: &str, value: u64, min: u64, max: u64) -> Result<(), String> {
    if !(min..=max).contains(&value) {
        return Err(format!("{name} 必須介於 {min} 與 {max}"));
    }
    Ok(())
}

fn validate_closed_unique(
    name: &str,
    values: &[String],
    allowed: &[&str],
    allow_empty: bool,
) -> Result<(), String> {
    if values.is_empty() && !allow_empty {
        return Err(format!("{name} 不得為空"));
    }
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if value.is_empty() {
            return Err(format!("{name} 不得包含空值"));
        }
        if !allowed.contains(&value.as_str()) {
            return Err(format!("{name} 包含未實作值: {value}"));
        }
        if !seen.insert(value.as_str()) {
            return Err(format!("{name} 不得包含重複值: {value}"));
        }
    }
    Ok(())
}

fn valid_http_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

pub fn normalize_qr_remote_base_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("qr_remote_base_url 不得為空".to_string());
    }
    // No backtracking: empty string before/after? already checked
    let url = reqwest::Url::parse(trimmed)
        .map_err(|_| "qr_remote_base_url 必須是有效的 http(s) URL".to_string())?;
    let scheme = url.scheme().to_string();
    if scheme != "http" && scheme != "https" {
        return Err("qr_remote_base_url 僅支援 http/https".to_string());
    }
    if url.host_str().is_none_or(|host| host.trim().is_empty()) {
        return Err("qr_remote_base_url 必須包含 host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("qr_remote_base_url 不得包含帳號資訊".to_string());
    }
    if url.query().is_some() {
        return Err("qr_remote_base_url 不得包含 query".to_string());
    }
    if url.fragment().is_some() {
        return Err("qr_remote_base_url 不得包含 fragment".to_string());
    }
    if scheme == "http" && !is_qr_remote_allowed_http_host(url.host_str().unwrap_or("")) {
        return Err("http 僅允許 localhost 或 127.0.0.0/8、::1".to_string());
    }
    // Normalize: remove trailing slash unless it's bare origin with path "/"
    // Store without trailing slash for consistent /token join; url crate keeps path "/".
    // Single-parse construction: origin() keeps scheme/host/port only — userinfo, query and
    // fragment are dropped and default ports canonicalized by construction — and the appended
    // path slice comes from url.path(), which cannot contain '?' or '#'. The http loopback
    // rule was already enforced on this same parsed value above (the host helper also accepts
    // bracketed IPv6), so no second parse is needed.
    let path = url.path().to_string();
    let normalized = if path == "/" {
        // origin only: keep origin (e.g. https://api.example)
        let origin = url.origin().ascii_serialization();
        // preserve non-default port already in origin
        origin
    } else {
        // Keep origin + path without trailing slash
        let origin = url.origin().ascii_serialization();
        let trimmed_path = path.trim_end_matches('/');
        if trimmed_path.is_empty() {
            origin
        } else {
            format!("{}{}", origin, trimmed_path)
        }
    };
    Ok(normalized)
}

pub fn is_qr_remote_allowed_http_host(host: &str) -> bool {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "localhost" || lower == "localhost." {
        return true;
    }
    if let Ok(ip) = lower
        .trim_matches(|character| character == '[' || character == ']')
        .parse::<std::net::IpAddr>()
    {
        return is_qr_remote_loopback_ip(ip);
    }
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return is_qr_remote_loopback_ip(ip);
    }
    false
}

fn is_qr_remote_loopback_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.octets()[0] == 127,
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn validate_qr_remote(enabled: &bool, base_url: &str) -> Result<(), String> {
    let trimmed = base_url.trim();
    if !enabled {
        if trimmed.is_empty() {
            return Ok(());
        }
        // Even when disabled the stored URL must be a syntactically valid allowed URL when non-empty.
        normalize_qr_remote_base_url(base_url).map(|_| ())?;
        return Ok(());
    }
    if trimmed.is_empty() {
        return Err("qr_remote_enabled 為 true 時 qr_remote_base_url 不得為空".to_string());
    }
    normalize_qr_remote_base_url(base_url).map(|_| ())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("autotronclass-{name}-{}", new_id()))
    }

    fn legacy_json() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "active_account": null,
            "accounts": [],
            "settings": {
                "countdown_secs": 15,
                "attendance_gate_percent": 15.0,
                "llm_endpoint": "https://example.test",
                "llm_model": "model",
                "llm_max_tokens": 16384,
                "resubmit_for_correct": true,
                "max_answer_reask": 4,
                "prepare_retry_budget_secs": 300,
                "autoanswer_types": [],
                "enable_llm_tools": true,
                "max_tool_iterations": 3,
                "radar_strategy": [],
                "number_concurrency": 100,
                "number_min_concurrency": 5,
                "number_cooldown_ms": 5000,
                "number_max_cooldowns": 3,
                "poll_idle_secs": 5,
                "quiz_detect_secs": 45,
                "log_level": "normal",
                "operating": { "days": [] },
                "tz_offset_minutes": 480
            }
        }))
        .unwrap()
    }

    #[test]
    fn weekly_schedule_rejects_same_day_cross_day_and_cross_week_overlap() {
        let mut weekly = WeeklySchedule {
            monday: vec![
                TimeWindow {
                    start_minute: 60,
                    end_minute: 120,
                },
                TimeWindow {
                    start_minute: 120,
                    end_minute: 180,
                },
            ],
            ..WeeklySchedule::default()
        };
        assert!(weekly.validate().is_ok(), "adjacent windows are legal");
        weekly.monday.push(TimeWindow {
            start_minute: 100,
            end_minute: 130,
        });
        assert!(weekly.validate().is_err());

        let mut cross_day = WeeklySchedule::default();
        cross_day.monday.push(TimeWindow {
            start_minute: 1_380,
            end_minute: 60,
        });
        cross_day.tuesday.push(TimeWindow {
            start_minute: 30,
            end_minute: 90,
        });
        assert!(cross_day.validate().is_err());

        let mut cross_week = WeeklySchedule::default();
        cross_week.sunday.push(TimeWindow {
            start_minute: 1_380,
            end_minute: 60,
        });
        cross_week.monday.push(TimeWindow {
            start_minute: 30,
            end_minute: 90,
        });
        assert!(cross_week.validate().is_err());
    }

    #[test]
    fn a_missing_config_is_created_with_the_current_schema() {
        let path = temporary_path("missing-config");
        let initialized = Config::initialize_at(&path, 1).unwrap();
        assert_eq!(initialized.config, Config::default());
        assert!(initialized.reset_notice.is_none());
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn an_older_config_is_backed_up_before_it_is_reset() {
        let schema_one = serde_json::to_vec(
            &json!({ "schema_version": CONFIG_SCHEMA_VERSION - 1, "accounts": [] }),
        )
        .unwrap();
        for (name, bytes) in [("unversioned", legacy_json()), ("schema-one", schema_one)] {
            let path = temporary_path(name);
            fs::write(&path, &bytes).unwrap();
            let initialized = Config::initialize_at(&path, 42).unwrap();
            let backup = initialized.reset_notice.unwrap().backup_path;
            assert_eq!(fs::read(&backup).unwrap(), bytes);
            assert_eq!(Config::load(&path).unwrap(), Config::default());
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(backup);
        }
    }

    #[test]
    fn a_failed_backup_leaves_the_older_config_untouched() {
        let path = temporary_path("legacy-backup-failure");
        let bytes = legacy_json();
        fs::write(&path, &bytes).unwrap();
        let file_name = path.file_name().unwrap().to_string_lossy();
        let backup =
            path.with_file_name(format!("{file_name}.pre-v{CONFIG_SCHEMA_VERSION}-42.bak"));
        fs::write(&backup, b"occupied").unwrap();
        assert!(matches!(
            Config::initialize_at(&path, 42),
            Err(ConfigLoadError::Backup(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup);
    }

    #[test]
    fn a_corrupt_config_and_a_newer_one_are_left_untouched() {
        for (name, bytes) in [
            ("corrupt", b"{broken".to_vec()),
            (
                "future",
                serde_json::to_vec(&json!({ "schema_version": CONFIG_SCHEMA_VERSION + 1 }))
                    .unwrap(),
            ),
        ] {
            let path = temporary_path(name);
            fs::write(&path, &bytes).unwrap();
            assert!(Config::initialize_at(&path, 7).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn persisted_settings_share_the_runtime_validation_boundary() {
        let path = temporary_path("settings-validation");
        let mut config = Config::default();
        config.settings.llm_max_tokens = 0;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().settings.llm_max_tokens, 0);

        let mut invalid = config;
        invalid.settings.countdown_secs = 0;
        let invalid_bytes = serde_json::to_vec_pretty(&invalid).unwrap();
        fs::write(&path, &invalid_bytes).unwrap();
        assert!(matches!(
            Config::initialize_at(&path, 7),
            Err(ConfigLoadError::UnsupportedOrCorrupt)
        ));
        assert_eq!(fs::read(&path).unwrap(), invalid_bytes);

        invalid.settings.countdown_secs = 15;
        invalid.settings.llm_endpoint = "https://".into();
        assert!(invalid.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid_bytes);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_settings_reject_unknown_fields_without_silent_rewrite() {
        let path = temporary_path("settings-unknown-reject");
        let config = Config::default();
        let mut value = serde_json::to_value(&config).unwrap();
        value["settings"]["bogus_future_field"] = json!("unexpected");
        let invalid_bytes = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&path, &invalid_bytes).unwrap();
        // Startup/load must reject unknown settings fields, not silently accept and rewrite.
        assert!(matches!(
            Config::initialize_at(&path, 7),
            Err(ConfigLoadError::UnsupportedOrCorrupt)
        ));
        assert_eq!(fs::read(&path).unwrap(), invalid_bytes);
        // Unknown fields must stay rejected on direct load as well.
        assert!(Config::load(&path).is_err());
        let _ = fs::remove_file(&path);
        // Direct parse of Settings with deny_unknown_fields.
        let known: Settings = serde_json::from_value(json!({
            "countdown_secs": 30,
            "attendance_gate_percent": 12.5,
            "llm_endpoint": "https://example.test",
            "llm_model": "m",
            "llm_max_tokens": 0,
        }))
        .unwrap();
        assert_eq!(known.countdown_secs, 30);
        assert!(serde_json::from_value::<Settings>(json!({
            "countdown_secs": 30,
            "unknown_key": 1,
        }))
        .is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_settings_known_schemas_accepted_save_failure_does_not_alter_disk() {
        let path = temporary_path("settings-known-and-save-fail");
        // Known current schema (partial with defaults) must be accepted at load.
        let partial = json!({
            "schema_version": CONFIG_SCHEMA_VERSION,
            "config_revision": 0,
            "schedule_revision": 0,
            "accounts": [],
            "settings": { "countdown_secs": 30 },
            "groups": [],
            "monitoring": { "global_schedule": { "monday": [], "tuesday": [], "wednesday": [], "thursday": [], "friday": [], "saturday": [], "sunday": [] }, "time_zone": { "kind": "device" }, "all_suspended": false },
            "runtime": { "manual_overrides": [], "group_rotation": {}, "platform_block": null }
        });
        fs::write(&path, serde_json::to_vec_pretty(&partial).unwrap()).unwrap();
        assert!(Config::initialize_at(&path, 7).is_ok());
        // UpdateConfig/save failure must not alter memory/disk: patch with invalid range.
        let before = fs::read(&path).unwrap();
        let mut loaded = Config::load(&path).unwrap();
        loaded.settings.countdown_secs = 0; // invalid
        assert!(loaded.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn target_and_sum_type_wire_names_are_stable() {
        assert_eq!(
            serde_json::to_value(TargetId::group("g")).unwrap(),
            json!({ "kind": "group", "group_id": "g" })
        );
        assert_eq!(
            serde_json::to_value(DetectorSelection::Preferred {
                account_id: "a".into()
            })
            .unwrap(),
            json!({ "kind": "preferred", "account_id": "a" })
        );
        assert_eq!(
            serde_json::to_value(ScheduleBinding::Disabled).unwrap(),
            json!({ "kind": "disabled" })
        );
    }

    #[test]
    fn canonical_tenant_normalizes_alias_default_port_and_base_path() {
        let registry = Registry {
            default_key: Some("school".into()),
            schools: vec![crate::providers::School {
                key: "school".into(),
                label: "School".into(),
                base_url: "https://EXAMPLE.edu:443/root/".into(),
                aliases: vec!["alias".into()],
                notes: String::new(),
            }],
        };
        assert_eq!(
            canonical_tenant(&registry, "ALIAS").unwrap(),
            canonical_tenant(&registry, "https://example.edu/root").unwrap()
        );

        let mut config = Config::default();
        for (id, school_ref) in [("a", "alias"), ("b", "https://example.edu/root/")] {
            config.accounts.push(AccountMeta {
                id: id.into(),
                label: id.into(),
                school_ref: school_ref.into(),
                username: id.into(),
                device_id: format!("device-{id}"),
                is_teacher: false,
                course_id: None,
                schedule: ScheduleBinding::Disabled,
            });
        }
        config.groups.push(AccountGroup {
            id: "g".into(),
            name: "group".into(),
            tenant: canonical_tenant(&registry, "alias").unwrap(),
            member_account_ids: vec!["a".into(), "b".into()],
            course_ids: Vec::new(),
            detector: DetectorSelection::Preferred {
                account_id: "a".into(),
            },
            schedule: ScheduleBinding::Disabled,
        });
        assert!(config.validate_with_registry(&registry).is_ok());
        config.accounts[1].is_teacher = true;
        assert!(config.validate_with_registry(&registry).is_err());
    }

    #[test]
    fn runtime_rotation_does_not_change_definition_revisions() {
        let mut config = Config::default();
        config.runtime.manual_overrides.push(ManualOverride {
            target: TargetId::account("a"),
            force_open: true,
            expires_at_utc: "2026-01-01T00:00:00Z".into(),
            schedule_revision: 0,
        });
        config.runtime.group_rotation.insert(
            "g".into(),
            GroupRotation {
                next_index: 1,
                last_window_key: Some("window".into()),
            },
        );
        assert_eq!((config.config_revision, config.schedule_revision), (0, 0));

        config.bump_definition_revision(true).unwrap();
        assert_eq!((config.config_revision, config.schedule_revision), (1, 1));
        assert!(config.runtime.manual_overrides.is_empty());
    }

    #[test]
    fn new_ids_are_unique_lowercase_hex() {
        let account = new_id();
        let group = new_group_id();
        assert_eq!(account.len(), 32);
        assert_eq!(group.len(), 32);
        assert!(account
            .chars()
            .chain(group.chars())
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)));
        assert_ne!(account, group);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn qr_remote_old_json_defaults_and_validation() {
        // Old schema v1 config without qr fields must load with defaults (enabled false, default base).
        let old = json!({
            "schema_version": CONFIG_SCHEMA_VERSION,
            "config_revision": 0,
            "schedule_revision": 0,
            "accounts": [],
            "settings": { "countdown_secs": 30 },
            "groups": [],
            "monitoring": { "global_schedule": { "monday": [], "tuesday": [], "wednesday": [], "thursday": [], "friday": [], "saturday": [], "sunday": [] }, "time_zone": { "kind": "device" }, "all_suspended": false },
            "runtime": { "manual_overrides": [], "group_rotation": {}, "platform_block": null }
        });
        let config: Config = serde_json::from_value(old).unwrap();
        assert!(!config.settings.qr_remote_enabled);
        assert_eq!(config.settings.qr_remote_base_url, "https://api.hlp.qzz.io");
        assert!(config.settings.validate().is_ok());

        // Valid HTTPS and loopback HTTP accepted
        for url in [
            "https://api.example.com",
            "https://api.example.com:8443/path",
            "http://localhost:8080",
            "http://127.0.0.1:9000",
            "http://[::1]:9000",
        ] {
            assert!(normalize_qr_remote_base_url(url).is_ok(), "{url}");
        }
        // Invalid: scheme, userinfo, query, fragment, non-loopback http
        for url in [
            "ftp://example.com",
            "https://user:pass@example.com",
            "https://example.com?x=1",
            "https://example.com#frag",
            "http://example.com",
            "http://192.168.1.1:8080",
            "http://10.0.0.1",
            "https://",
        ] {
            assert!(
                normalize_qr_remote_base_url(url).is_err(),
                "{url} must be invalid"
            );
        }
        // Non-loopback HTTP rejected even with path
        assert!(normalize_qr_remote_base_url("http://8.8.8.8/token").is_err());

        // Strict patch typing is enforced via apply_config_patch; verify normalize rejects wrong types via Settings validate boundary
        // Direct validation of wrong-typed patch is exercised via engine::tests::update_config_rejects_malformed; here verify URL normalization errors

        // Validation/save failure leaves disk unchanged is covered by existing persisted_settings tests; here verify enabled true with empty URL fails validation
        let mut s = Settings::default();
        s.qr_remote_enabled = true;
        s.qr_remote_base_url = "".to_string();
        assert!(s.validate().is_err());
        s.qr_remote_base_url = "https://api.hlp.qzz.io".to_string();
        assert!(s.validate().is_ok());
        // Disabled may retain default/valid URL
        let mut s2 = Settings::default();
        s2.qr_remote_enabled = false;
        s2.qr_remote_base_url = "https://api.hlp.qzz.io".to_string();
        assert!(s2.validate().is_ok());
        #[allow(clippy::field_reassign_with_default)]
        {
            let mut s3 = Settings::default();
            s3.qr_remote_enabled = true;
            s3.qr_remote_base_url = "http://127.0.0.1:8080".to_string();
            assert!(s3.validate().is_ok());
        }
    }

    #[test]
    fn qr_remote_normalization_trims_and_strips_trailing_slash() {
        // Exact canonical outputs — single-parse construction must hold every row.
        for (raw, canonical) in [
            (" https://example.com/ ", "https://example.com"),
            ("https://example.com///", "https://example.com"),
            ("https://example.com/path/", "https://example.com/path"),
            ("https://example.com/a//b///", "https://example.com/a//b"),
            ("http://localhost:8080/", "http://localhost:8080"),
            ("http://127.0.0.1:8080/base/", "http://127.0.0.1:8080/base"),
            ("http://[::1]:9000/", "http://[::1]:9000"),
            ("https://example.com:443/", "https://example.com"),
            ("https://example.com:8443/", "https://example.com:8443"),
            ("http://localhost:80/", "http://localhost"),
        ] {
            assert_eq!(
                normalize_qr_remote_base_url(raw).unwrap(),
                canonical,
                "raw {raw:?}"
            );
        }
        assert_eq!(
            normalize_qr_remote_base_url(" https://example.com/ ").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_qr_remote_base_url("https://example.com///").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_qr_remote_base_url("https://example.com/path/").unwrap(),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_qr_remote_base_url("http://localhost:8080/").unwrap(),
            "http://localhost:8080"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn qr_remote_disabled_empty_symmetry_and_enabled_empty_rejects() {
        // Disabled + trimmed empty base is valid through load/save/validate
        let mut disabled_empty = Settings::default();
        disabled_empty.qr_remote_enabled = false;
        disabled_empty.qr_remote_base_url = "".to_string();
        assert!(disabled_empty.validate().is_ok());
        disabled_empty.qr_remote_base_url = "   ".to_string();
        assert!(disabled_empty.validate().is_ok());

        // Enabled + empty (or whitespace) is rejected
        let mut enabled_empty = Settings::default();
        enabled_empty.qr_remote_enabled = true;
        enabled_empty.qr_remote_base_url = "".to_string();
        assert!(enabled_empty.validate().is_err());
        enabled_empty.qr_remote_base_url = "   ".to_string();
        assert!(enabled_empty.validate().is_err());

        // Old JSON without qr fields defaults correctly and validates
        let old = serde_json::json!({
            "schema_version": CONFIG_SCHEMA_VERSION, "config_revision": 0, "schedule_revision": 0,
            "accounts": [],
            "settings": { "countdown_secs": 30 },
            "groups": [], "monitoring": { "global_schedule": { "monday": [], "tuesday": [], "wednesday": [], "thursday": [], "friday": [], "saturday": [], "sunday": [] }, "time_zone": { "kind": "device" }, "all_suspended": false },
            "runtime": { "manual_overrides": [], "group_rotation": {}, "platform_block": null }
        });
        let c: Config = serde_json::from_value(old).unwrap();
        assert!(!c.settings.qr_remote_enabled);
        assert_eq!(c.settings.qr_remote_base_url, "https://api.hlp.qzz.io");

        // Disabled+empty persists through save/load symmetry; enabled+empty rejected at save
        {
            let path = temporary_path("qr-disabled-empty-sym");
            let mut cfg = Config::default();
            cfg.settings.qr_remote_enabled = false;
            cfg.settings.qr_remote_base_url = "".to_string();
            cfg.save(&path).unwrap();
            let loaded = Config::load(&path).unwrap();
            assert_eq!(loaded.settings.qr_remote_base_url, "");
            assert!(loaded.settings.validate().is_ok());
            let mut cfg2 = Config::default();
            cfg2.settings.qr_remote_enabled = true;
            cfg2.settings.qr_remote_base_url = "".to_string();
            assert!(cfg2.save(&path).is_err());
            let mut cfg3 = Config::default();
            cfg3.settings.qr_remote_enabled = true;
            cfg3.settings.qr_remote_base_url = "   ".to_string();
            assert!(cfg3.save(&path).is_err());
            let _ = fs::remove_file(&path);
        }

        // Disabled+empty patch accepted; enabled+empty patch rejected at final validate (smoke via validate)
        // Nonempty core normalization shape — core strips trailing slash, not UI
        assert!(normalize_qr_remote_base_url("https://example.com/path/")
            .unwrap()
            .ends_with("/path"));
        assert_eq!(
            normalize_qr_remote_base_url("https://example.com:443/").unwrap(),
            "https://example.com"
        );
    }
}
