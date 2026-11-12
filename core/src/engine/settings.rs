//! Settings, and the static facts the UI asks for once.

use super::commands::{Handled, Reply};
use super::monitoring::push_config;
use super::state::CoreState;
use crate::config::Settings;
use crate::secrets::Secret;
use crate::wire::{emit, Caps, Event, EventCb, SettingsView};
use serde_json::Value;

/// Apply a settings patch. The patch is applied to a copy, which is saved (saving validates the
/// whole result) and only then adopted: a rejected patch changes nothing in memory or on disk.
pub(super) fn update(st: &mut CoreState, cb: EventCb, patch: &Value) -> Handled {
    let mut next = st.config.clone();
    apply_config_patch(&mut next.settings, patch)?;
    next.save(&st.config_path())?;
    st.config = next;
    adopted(st, cb);
    Ok(Reply::Done)
}

pub(super) fn set_llm_key(st: &mut CoreState, cb: EventCb, key: &Secret) -> Handled {
    st.vault_mut()?.set_llm_key(key.expose().to_string())?;
    adopted(st, cb);
    Ok(Reply::Done)
}

/// A blank key clears the stored one.
pub(super) fn set_qr_remote_key(st: &mut CoreState, cb: EventCb, key: &Secret) -> Handled {
    let key = match key.expose().trim().is_empty() {
        true => String::new(),
        false => key.expose().to_string(),
    };
    st.vault_mut()?.set_qr_remote_key(key)?;
    adopted(st, cb);
    Ok(Reply::Done)
}

/// New settings are in force: a running session follows them at once, and the UI shows them.
fn adopted(st: &CoreState, cb: EventCb) {
    push_config(st);
    emit_settings(cb, st);
}

pub(super) fn emit_providers(cb: EventCb, st: &CoreState) {
    emit(
        cb,
        Event::Providers {
            default_key: st.registry.default_key.as_deref(),
            schools: &st.registry.schools,
        },
    );
}

pub(super) fn emit_caps(cb: EventCb) {
    emit(cb, Event::Caps { caps: CAPS });
}

pub(super) const CAPS: Caps = Caps {
    // Only Android can keep the app alive while its UI is not in the foreground.
    background_monitoring: cfg!(target_os = "android"),
    // This repository ships no updater.
    self_update: false,
    qr_teacher_assist: true,
    // Captchas are answered by the user.
    ocr_captcha: false,
};

pub(super) fn emit_settings(cb: EventCb, st: &CoreState) {
    let s = &st.config.settings;
    let has_llm_key = st.vault.as_ref().is_some_and(|v| v.has_llm_key());
    let has_qr_remote_key = st.vault.as_ref().is_some_and(|v| v.has_qr_remote_key());
    emit(
        cb,
        Event::Settings {
            settings: SettingsView {
                countdown_secs: s.countdown_secs,
                attendance_gate_percent: s.attendance_gate_percent,
                llm_endpoint: &s.llm_endpoint,
                llm_model: &s.llm_model,
                llm_max_tokens: s.llm_max_tokens,
                resubmit_for_correct: s.resubmit_for_correct,
                enable_llm_tools: s.enable_llm_tools,
                has_llm_key,
                has_qr_remote_key,
                qr_remote_enabled: s.qr_remote_enabled,
                qr_remote_base_url: &s.qr_remote_base_url,
            },
        },
    );
}

/// Apply a settings patch to `settings` (a caller-owned clone). Strict per-field typing: a
/// wrong-typed value or an unknown key is an error, never a silent ignore; `operating` parses
/// strictly so a malformed schedule fails the whole patch.
pub(super) fn apply_config_patch(settings: &mut Settings, patch: &Value) -> Result<(), String> {
    let patch = patch
        .as_object()
        .ok_or_else(|| "config patch 必須是物件".to_string())?;
    for (key, value) in patch {
        match key.as_str() {
            "countdown_secs" => settings.countdown_secs = u64_field(key, value)?,
            "attendance_gate_percent" => settings.attendance_gate_percent = f64_field(key, value)?,
            "llm_endpoint" => settings.llm_endpoint = str_field(key, value)?,
            "llm_model" => settings.llm_model = str_field(key, value)?,
            "llm_max_tokens" => settings.llm_max_tokens = u32_field(key, value)?,
            "resubmit_for_correct" => settings.resubmit_for_correct = bool_field(key, value)?,
            "max_answer_reask" => settings.max_answer_reask = u32_field(key, value)?,
            "prepare_retry_budget_secs" => {
                settings.prepare_retry_budget_secs = u64_field(key, value)?
            }
            "autoanswer_types" => settings.autoanswer_types = str_vec_field(key, value)?,
            "enable_llm_tools" => settings.enable_llm_tools = bool_field(key, value)?,
            "max_tool_iterations" => settings.max_tool_iterations = u32_field(key, value)?,
            "radar_strategy" => settings.radar_strategy = str_vec_field(key, value)?,
            "number_cooldown_ms" => settings.number_cooldown_ms = u64_field(key, value)?,
            "number_max_cooldowns" => settings.number_max_cooldowns = u32_field(key, value)?,
            "poll_idle_secs" => settings.poll_idle_secs = u64_field(key, value)?,
            "quiz_detect_secs" => settings.quiz_detect_secs = u64_field(key, value)?,
            "qr_remote_enabled" => settings.qr_remote_enabled = bool_field(key, value)?,
            "qr_remote_base_url" => {
                let raw = str_field(key, value)?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    settings.qr_remote_base_url = String::new();
                } else {
                    settings.qr_remote_base_url =
                        crate::config::normalize_qr_remote_base_url(&raw)?;
                }
            }

            _ => return Err(format!("unknown config field: {key}")),
        }
    }
    Ok(())
}

pub(super) fn u64_field(key: &str, value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{key} 必須是非負整數"))
}

pub(super) fn u32_field(key: &str, value: &Value) -> Result<u32, String> {
    u32::try_from(u64_field(key, value)?).map_err(|_| format!("{key} 超出範圍"))
}

pub(super) fn f64_field(key: &str, value: &Value) -> Result<f64, String> {
    value.as_f64().ok_or_else(|| format!("{key} 必須是數字"))
}

pub(super) fn bool_field(key: &str, value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| format!("{key} 必須是布林"))
}

pub(super) fn str_field(key: &str, value: &Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{key} 必須是字串"))
}

pub(super) fn str_vec_field(key: &str, value: &Value) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("{key} 必須是字串陣列"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key} 必須全是字串"))
        })
        .collect()
}
