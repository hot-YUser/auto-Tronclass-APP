//! Structured, persisted config (docs 10). Holds account **metadata only** — never secrets.
//! Passwords and session cookies live in the vault (`secrets.rs`); the account just references
//! them by `id`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountMeta {
    pub id: String,
    pub label: String,
    /// A registry key/alias or a raw base_url (resolved via `providers::Registry::resolve`).
    pub school_ref: String,
    pub username: String,
    /// Stable random per-account device code sent on number/qr answers. `#[serde(default)]` keeps
    /// slice-1 config.json readable.
    #[serde(default)]
    pub device_id: String,
    /// A teacher account enables QR teacher-assist for its base_url.
    #[serde(default)]
    pub is_teacher: bool,
    /// Teacher accounts: the course under which to host the QR rollcall that sources `data`.
    #[serde(default)]
    pub course_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Reverse-window before auto-sign/submit (docs 20). Lowered in tests.
    #[serde(default = "default_countdown")]
    pub countdown_secs: u64,
    /// Anti-fake-rollcall threshold (docs 30 §15%).
    #[serde(default = "default_gate")]
    pub attendance_gate_percent: f64,
    /// LLM endpoint (chat-completions URL). Default NVIDIA NIM; the API key lives in the vault.
    #[serde(default = "default_llm_endpoint")]
    pub llm_endpoint: String,
    #[serde(default = "default_llm_model")]
    pub llm_model: String,
    /// Max tokens for the LLM answer. Reasoning models return empty/truncated `choices` if this is
    /// omitted or too small, so it is always sent. `0` = use the safe default (16384), resolved at
    /// the request-body layer (see `llm::resolve_max_tokens`).
    #[serde(default = "default_llm_max_tokens")]
    pub llm_max_tokens: u32,
    /// Resubmit the leaked-correct answers for full marks when the activity allows retake (docs 31).
    #[serde(default = "default_true")]
    pub resubmit_for_correct: bool,
    /// Max blank-answer re-asks before skipping a subject (docs 31).
    #[serde(default = "default_reask")]
    pub max_answer_reask: u32,
    /// R3c all-or-nothing gate: how long (seconds) to keep re-preparing a paper the LLM can't yet fully
    /// answer before giving up with an error. Minutes-scale so a briefly-stalled answer isn't abandoned.
    #[serde(default = "default_prepare_retry_budget")]
    pub prepare_retry_budget_secs: u64,
    /// R4 auto-answer allowlist: which activity families to detect + answer. Empty is treated as "all".
    #[serde(default = "default_autoanswer_types")]
    pub autoanswer_types: Vec<String>,
    /// R5: let the LLM call `search_course_materials` (course handouts + PDF text) when a question lacks context.
    #[serde(default = "default_true")]
    pub enable_llm_tools: bool,
    /// R5: how many tool ROUNDS the LLM may take before a final answer (the loop adds a reserved answer turn).
    #[serde(default = "default_tool_iterations")]
    pub max_tool_iterations: u32,
    /// Ordered radar sign-in strategy chain (docs 30). `empty_answer` = PUT `{}`; `global_wgs84` =
    /// probe distances and multilaterate on the WGS84 ellipsoid (`radar::solve`). Walked in order.
    #[serde(default = "default_radar_strategy")]
    pub radar_strategy: Vec<String>,
    /// number brute-force: concurrent in-flight code attempts (starts high, halves on throttling).
    #[serde(default = "default_number_concurrency")]
    pub number_concurrency: u32,
    /// number brute-force: floor concurrency it halves down to when throttled.
    #[serde(default = "default_number_min_concurrency")]
    pub number_min_concurrency: u32,
    /// number brute-force: cooldown sleep on a throttled/5xx batch, in milliseconds.
    #[serde(default = "default_number_cooldown_ms")]
    pub number_cooldown_ms: u64,
    /// number brute-force: give up after this many transient-cooldown rounds.
    #[serde(default = "default_number_max_cooldowns")]
    pub number_max_cooldowns: u32,
    /// Idle rollcall-poll interval (seconds). The active-countdown poll stays 1 s.
    #[serde(default = "default_poll_idle_secs")]
    pub poll_idle_secs: u64,
    /// Quiz-detection interval (seconds) — separate from rollcall polling, can be slower.
    #[serde(default = "default_quiz_detect_secs")]
    pub quiz_detect_secs: u64,
    /// Logging verbosity: `normal` (default) or `debug`. Debug log lines are dropped at `normal`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Operating-hours schedule. Empty (default) = always-on (no restriction).
    #[serde(default)]
    pub operating: Operating,
    /// Fixed local-time offset from UTC, in minutes (default +480 = UTC+8). Used to evaluate
    /// `operating` windows. No DST handling — DST regions adjust this twice a year (docs 90 minimal-deps).
    #[serde(default = "default_tz_offset")]
    pub tz_offset_minutes: i64,
}

fn default_countdown() -> u64 {
    15
}
fn default_gate() -> f64 {
    15.0
}
fn default_llm_endpoint() -> String {
    "https://integrate.api.nvidia.com/v1/chat/completions".to_string()
}
fn default_llm_model() -> String {
    "minimaxai/minimax-m3".to_string()
}
fn default_llm_max_tokens() -> u32 {
    16384
}
fn default_true() -> bool {
    true
}
fn default_reask() -> u32 {
    4
}
fn default_prepare_retry_budget() -> u64 {
    300
}
fn default_tool_iterations() -> u32 {
    3
}
fn default_autoanswer_types() -> Vec<String> {
    ["exam", "questionnaire", "homework", "vote", "classroom", "courseware"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_radar_strategy() -> Vec<String> {
    vec!["empty_answer".to_string(), "global_wgs84".to_string()]
}
fn default_number_concurrency() -> u32 {
    100
}
fn default_number_min_concurrency() -> u32 {
    5
}
fn default_number_cooldown_ms() -> u64 {
    5000
}
fn default_number_max_cooldowns() -> u32 {
    3
}
fn default_poll_idle_secs() -> u64 {
    5
}
fn default_quiz_detect_secs() -> u64 {
    45
}
fn default_log_level() -> String {
    "normal".to_string()
}
fn default_tz_offset() -> i64 {
    480
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            countdown_secs: default_countdown(),
            attendance_gate_percent: default_gate(),
            llm_endpoint: default_llm_endpoint(),
            llm_model: default_llm_model(),
            llm_max_tokens: default_llm_max_tokens(),
            resubmit_for_correct: default_true(),
            max_answer_reask: default_reask(),
            prepare_retry_budget_secs: default_prepare_retry_budget(),
            autoanswer_types: default_autoanswer_types(),
            enable_llm_tools: default_true(),
            max_tool_iterations: default_tool_iterations(),
            radar_strategy: default_radar_strategy(),
            number_concurrency: default_number_concurrency(),
            number_min_concurrency: default_number_min_concurrency(),
            number_cooldown_ms: default_number_cooldown_ms(),
            number_max_cooldowns: default_number_max_cooldowns(),
            poll_idle_secs: default_poll_idle_secs(),
            quiz_detect_secs: default_quiz_detect_secs(),
            log_level: default_log_level(),
            operating: Operating::default(),
            tz_offset_minutes: default_tz_offset(),
        }
    }
}

/// Operating-hours schedule (docs 20). Per-weekday enable + time windows; the monitor only polls
/// when the current local time falls inside an enabled window. An **empty** schedule means always-on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Operating {
    /// Per-weekday rules. Days not listed inherit the always-on default. `weekday`: 0=Mon .. 6=Sun.
    #[serde(default)]
    pub days: Vec<DaySchedule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaySchedule {
    /// 0=Mon .. 6=Sun.
    pub weekday: u8,
    #[serde(default)]
    pub enabled: bool,
    /// Open windows for this day. An enabled day with no windows is closed all day.
    #[serde(default)]
    pub windows: Vec<TimeWindow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeWindow {
    /// "HH:MM" local time.
    pub start: String,
    pub end: String,
}

impl Operating {
    /// Is monitoring open at `epoch_secs` (UTC seconds) given a fixed `tz_offset_minutes`? Pure —
    /// dependency-free integer arithmetic, so it is exhaustively testable with synthetic epochs.
    /// Empty schedule → always open. A listed-but-disabled weekday → closed. A weekday not listed →
    /// open (inherits the always-on default).
    pub fn is_open(&self, epoch_secs: i64, tz_offset_minutes: i64) -> bool {
        if self.days.is_empty() {
            return true;
        }
        let local = epoch_secs + tz_offset_minutes * 60;
        // 1970-01-01 was a Thursday (=3 when Mon=0), so shift the day index by 3.
        let weekday = (local.div_euclid(86400) + 3).rem_euclid(7) as u8;
        let minute = (local.rem_euclid(86400) / 60) as u32;
        match self.days.iter().find(|d| d.weekday == weekday) {
            Some(d) if d.enabled => d.windows.iter().any(|w| window_contains(&w.start, &w.end, minute)),
            Some(_) => false, // listed but disabled → closed
            // not listed → inherit always-on. ponytail: a future v1-config import (v1 has a per-day enable
            // gate where unlisted ≠ always-on) must materialize all 7 days (enabled=false for unlisted)
            // BEFORE storing, or unlisted days would wrongly read as open. No such import path exists yet.
            None => true,
        }
    }
}

/// Is `minute` (minute-of-day) inside the "HH:MM"–"HH:MM" window (v1 semantics)? `start==end` means ALL
/// DAY (the default 24/7 config is `00:00`–`00:00`); the end minute is INCLUSIVE (`start <= t <= end`); a
/// window whose end is < start wraps past midnight. Unparseable bounds → not contained (never falsely "open").
fn window_contains(start: &str, end: &str, minute: u32) -> bool {
    let (Some(s), Some(e)) = (parse_hhmm(start), parse_hhmm(end)) else {
        return false;
    };
    if s == e {
        return true; // start==end means ALL DAY (v1: `if start == end: return True`; the default 24/7 config)
    }
    if s < e {
        minute >= s && minute <= e // inclusive end (v1: start <= t <= end)
    } else {
        minute >= s || minute <= e // wraps past midnight
    }
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Map a v1-style 0=Sunday weekday to our internal 0=Monday..6=Sunday. Pure — apply at the boundary when
/// ingesting a v1 schedule (no such import path exists yet; kept ready). ponytail: unwired until needed.
#[allow(dead_code)] // wired only when a v1-config import path lands; exercised by slice4_test today
pub fn simple_weekday_to_internal(v1_weekday: u8) -> u8 {
    // v1: 0=Sun,1=Mon,..6=Sat → internal: {0:6, 1:0, 2:1, 3:2, 4:3, 5:4, 6:5}.
    [6, 0, 1, 2, 3, 4, 5].get(v1_weekday as usize).copied().unwrap_or(0)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub active_account: Option<String>,
    #[serde(default)]
    pub accounts: Vec<AccountMeta>,
    #[serde(default)]
    pub settings: Settings,
}

impl Config {
    pub fn load(path: &Path) -> Config {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, bytes).map_err(|e| e.to_string())
    }

    pub fn account(&self, id: &str) -> Option<&AccountMeta> {
        self.accounts.iter().find(|a| a.id == id)
    }
}

/// A random opaque account id (hex of 16 CSPRNG bytes) — avoids a uuid dependency.
pub fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("os rng");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
