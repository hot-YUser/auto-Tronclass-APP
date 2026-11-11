//! Core → UI events. Every event the core can emit is a variant here, so the wire shape is defined
//! in one file instead of at each call site. Fields borrow from the caller: an event is serialized
//! and gone before `emit` returns.

use super::{AnswerWire, MonitoringSnapshot};
use crate::providers::School;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event")]
pub enum Event<'a> {
    /// Completes a command. Sent with the command's id.
    Reply {
        ok: bool,
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// Completes a `Login` or `ImportCookies` command. Sent with the command's id.
    LoginResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Error(Problem<'a>),
    LogLine {
        level: LogLevel,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        activity_token: Option<&'a str>,
    },

    VaultState {
        exists: bool,
        unlocked: bool,
    },
    Providers {
        default_key: Option<&'a str>,
        schools: &'a [School],
    },
    Caps {
        caps: Caps,
    },
    Settings {
        settings: SettingsView<'a>,
    },
    MonitoringSnapshot {
        snapshot: &'a MonitoringSnapshot,
    },
    CaptchaChallenge {
        account_id: &'a str,
        image_b64: String,
    },

    RollcallDetected {
        rollcall_id: &'a str,
        activity_token: &'a str,
        base_url: &'a str,
        kind: &'a str,
        course: &'a str,
        attendance_rate: Option<f64>,
        accounts: Vec<&'a str>,
    },
    RollcallGate {
        rollcall_id: &'a str,
        activity_token: &'a str,
        rate: Option<f64>,
        gate_percent: f64,
        holding: bool,
    },
    PendingSignIn {
        rollcall_id: &'a str,
        activity_token: &'a str,
    },
    SignedIn {
        rollcall_id: &'a str,
        activity_token: &'a str,
        account_id: &'a str,
        course: &'a str,
        method: &'a str,
    },
    Countdown {
        scope: CountdownScope,
        activity_token: &'a str,
        external_id: &'a str,
        remaining_secs: u64,
    },

    QuizPrepared(QuizPrepared<'a>),
    ReasoningChunk {
        activity_token: &'a str,
        account_id: &'a str,
        subject_id: &'a str,
        text: &'a str,
    },
    AnswerUpdated {
        quiz_id: &'a str,
        activity_token: &'a str,
        account_id: &'a str,
        subject_id: &'a str,
        answer: AnswerWire,
        display_answer: String,
        source: &'a str,
        conflict: bool,
    },
    QuizSubmitted {
        quiz_id: &'a str,
        activity_token: &'a str,
        account_id: &'a str,
        result: &'a str,
    },
}

impl<'a> Event<'a> {
    pub fn reply(result: Result<(), String>) -> Self {
        Self::Reply {
            ok: result.is_ok(),
            error: result.err(),
            data: None,
        }
    }

    pub fn logged_in(detail: &'a str) -> Self {
        Self::LoginResult {
            ok: true,
            detail: Some(detail),
            reason: None,
        }
    }

    pub fn login_failed(reason: impl Into<String>) -> Self {
        Self::LoginResult {
            ok: false,
            detail: None,
            reason: Some(reason.into()),
        }
    }

    pub fn log(level: LogLevel, text: impl Into<String>) -> Self {
        Self::LogLine {
            level,
            text: text.into(),
            activity_token: None,
        }
    }
}

/// Something went wrong that the user should see. `code` is stable and machine-readable;
/// `message` is the text shown.
#[derive(Clone, Debug, Serialize)]
pub struct Problem<'a> {
    pub severity: Severity,
    pub code: &'a str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<&'a str>,
}

impl<'a> Problem<'a> {
    pub fn error(code: &'a str, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: &'a str, message: impl Into<String>) -> Self {
        Self::new(Severity::Warn, code, message)
    }

    fn new(severity: Severity, code: &'a str, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            activity_token: None,
            account_id: None,
        }
    }

    pub fn activity(mut self, token: &'a str) -> Self {
        self.activity_token = Some(token);
        self
    }

    pub fn account(mut self, id: &'a str) -> Self {
        self.account_id = Some(id);
        self
    }
}

impl<'a> From<Problem<'a>> for Event<'a> {
    fn from(problem: Problem<'a>) -> Self {
        Self::Error(problem)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CountdownScope {
    Rollcall,
    Quiz,
}

/// What this build can do, so the UI never offers a control the platform cannot honor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Caps {
    pub background_monitoring: bool,
    pub self_update: bool,
    pub qr_teacher_assist: bool,
    pub ocr_captcha: bool,
}

/// The settings the UI may display. Keys never cross the seam; only whether one is stored.
#[derive(Clone, Debug, Serialize)]
pub struct SettingsView<'a> {
    pub countdown_secs: u64,
    pub attendance_gate_percent: f64,
    pub llm_endpoint: &'a str,
    pub llm_model: &'a str,
    pub llm_max_tokens: u32,
    pub resubmit_for_correct: bool,
    pub enable_llm_tools: bool,
    pub has_llm_key: bool,
    pub has_qr_remote_key: bool,
    pub qr_remote_enabled: bool,
    pub qr_remote_base_url: &'a str,
}

/// A quiz and every account's prepared answers, re-sent whole whenever any of it changes.
#[derive(Clone, Debug, Serialize)]
pub struct QuizPrepared<'a> {
    pub schema_version: u8,
    pub activity_token: &'a str,
    pub quiz_id: &'a str,
    pub activity: QuizActivityRef<'a>,
    pub course: &'a str,
    /// Accounts whose paper is ready to show.
    pub per_account: Vec<PreparedAccount<'a>>,
    /// Every account the quiz applies to, including those still preparing.
    pub expected_accounts: Vec<ExpectedAccount<'a>>,
    pub conflict_count: usize,
}

impl<'a> From<QuizPrepared<'a>> for Event<'a> {
    fn from(prepared: QuizPrepared<'a>) -> Self {
        Self::QuizPrepared(prepared)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct QuizActivityRef<'a> {
    pub external_id: &'a str,
    pub source: &'a str,
    pub course_id: &'a str,
    pub course: &'a str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExpectedAccount<'a> {
    pub account_id: &'a str,
    pub state: AttemptState,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreparedAccount<'a> {
    pub account_id: &'a str,
    pub instance_id: &'a str,
    pub state: AttemptState,
    pub questions: Vec<PreparedQuestion<'a>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreparedQuestion<'a> {
    pub subject_id: String,
    pub parent_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub answer_type: &'a str,
    pub stem: &'a str,
    pub options: Vec<QuestionOption<'a>>,
    pub answer: Option<AnswerWire>,
    pub existing_answer: Option<AnswerWire>,
    pub display_answer: String,
    pub source: &'a str,
    pub conflict: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct QuestionOption<'a> {
    pub id: String,
    pub text: &'a str,
}

/// Where one account stands with one quiz.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Waiting,
    Preparing,
    Ready,
    Submitting,
    Submitted,
    Gone,
    Failed,
}
