//! The four rollcall types (rollcall.rs). `classify` is pure and unit-tested; the per-type answer +
//! `on_call_fine` recheck are async over a single account's session. Each account signs for itself
//! with its own device id; shared computation (number code, radar solve) is done once by the caller.

use crate::providers::Endpoints;
use crate::radar::{self, GeoPoint, Observation};
use reqwest::Client;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollcallKind {
    Number,
    Radar,
    SelfRegistration,
    Qr,
    Unknown,
}

impl RollcallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RollcallKind::Number => "number",
            RollcallKind::Radar => "radar",
            RollcallKind::SelfRegistration => "self_registration",
            RollcallKind::Qr => "qrcode",
            RollcallKind::Unknown => "unknown",
        }
    }
}

/// Classify a rollcall by its status flags — each rollcall is exactly one type (rollcall.rs table).
pub fn classify(rc: &Value) -> RollcallKind {
    let flag = |k: &str| rc.get(k).and_then(Value::as_bool) == Some(true);
    if flag("is_number") {
        RollcallKind::Number
    } else if flag("is_radar") {
        RollcallKind::Radar
    } else if flag("is_self_registration") {
        RollcallKind::SelfRegistration
    } else if flag("unsupported_qrcode") {
        RollcallKind::Qr
    } else {
        RollcallKind::Unknown
    }
}

/// Result of a successful sign; `discovered_code` lets a brute-force number sign share its find.
#[derive(Clone, Debug, Default)]
pub struct SignOutcome {
    pub method: String,
    pub discovered_code: Option<String>,
}

// --- `student_rollcalls` object roster helpers (rollcall.rs real contract) ---

/// A roster entry's status (any of the three real field names) is the present state `on_call_fine`.
fn entry_fine(e: &Value) -> bool {
    ["rollcall_status", "student_rollcall_status", "status"]
        .iter()
        .any(|k| e.get(*k).and_then(Value::as_str) == Some("on_call_fine"))
}

/// The top-level rollcall status (`status` or `rollcallStatus`) is `on_call_fine`.
fn top_fine(v: &Value) -> bool {
    ["status", "rollcallStatus"]
        .iter()
        .any(|k| v.get(*k).and_then(Value::as_str) == Some("on_call_fine"))
}

/// (present, total) over the `student_rollcalls` roster — present = entries in an `on_call_fine` state.
fn roster_stats(v: &Value) -> (usize, usize) {
    match v.get("student_rollcalls").and_then(Value::as_array) {
        Some(a) => (a.iter().filter(|e| entry_fine(e)).count(), a.len()),
        None => (0, 0),
    }
}

/// The caller's OWN roster entry (matched by `user_no`/`user_id`) is present.
fn my_present(v: &Value, user_no: &str) -> bool {
    if user_no.is_empty() {
        return false;
    }
    v.get("student_rollcalls")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().any(|e| {
                let uid = e
                    .get("user_no")
                    .or_else(|| e.get("user_id"))
                    .and_then(Value::as_str);
                uid.map(|u| u.eq_ignore_ascii_case(user_no))
                    .unwrap_or(false)
                    && entry_fine(e)
            })
        })
        .unwrap_or(false)
}

/// Whole-class attendance rate (percent) for the 15% gate — computed from the roster (rollcall.rs).
/// The body is read with the shared bounded reader (an oversized Content-Length → None, never an
/// unbounded `.json()` buffer).
pub async fn attendance_rate(client: &Client, ep: &Endpoints, id: &str) -> Option<f64> {
    let resp = client.get(ep.student_rollcalls(id)).send().await.ok()?;
    let v = crate::http::read_bounded_json(resp, crate::http::MAX_API_JSON, "rollcall roster")
        .await
        .ok()?;
    let (present, total) = roster_stats(&v);
    (total > 0).then(|| present as f64 / total as f64 * 100.0)
}

/// Read the shared number code once from the roster. None → caller brute-forces.
pub async fn read_number_code(client: &Client, ep: &Endpoints, id: &str) -> Option<String> {
    let resp = client.get(ep.student_rollcalls(id)).send().await.ok()?;
    let v = crate::http::read_bounded_json(resp, crate::http::MAX_API_JSON, "rollcall roster")
        .await
        .ok()?;
    number_code_from_payload(&v)
}

/// Coerce one JSON value into a 4-digit code (v1 `coerce_number_code`). The real server exposes
/// `number_code` as a STRING **or an INT** — an int `123` is the code `"0123"` — so reading it only as
/// a string (the old code) returned None on every int tenant and fell to a needless brute-force.
fn coerce_number_code(v: &Value) -> Option<String> {
    let text = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) if (0..=9999).contains(&i) => format!("{i:04}"),
            Some(i) => i.to_string(),
            None => return None,
        },
        _ => return None,
    };
    (text.len() == 4 && text.bytes().all(|b| b.is_ascii_digit())).then_some(text)
}

/// Pull a 4-digit `number_code` out of a student_rollcalls-style payload (v1 `parse_number_code_payload`).
/// Robust to the observed shapes: top-level `{number_code}`, `{data:{number_code}}`, a
/// `student_rollcalls`/`data` array of student items, or a bare list of them.
fn number_code_from_payload(payload: &Value) -> Option<String> {
    let in_item = |item: &Value| {
        item.as_object()?
            .get("number_code")
            .and_then(coerce_number_code)
    };
    if let Some(obj) = payload.as_object() {
        if let Some(c) = obj.get("number_code").and_then(coerce_number_code) {
            return Some(c);
        }
        if let Some(c) = obj
            .get("data")
            .and_then(|d| d.get("number_code"))
            .and_then(coerce_number_code)
        {
            return Some(c);
        }
        for key in ["student_rollcalls", "data"] {
            if let Some(c) = obj
                .get(key)
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find_map(in_item))
            {
                return Some(c);
            }
        }
        None
    } else {
        payload.as_array()?.iter().find_map(in_item)
    }
}

/// Confirm the account is actually marked present after a sign (v1 `confirmed_present`). Confirmed iff
/// **my_present** (the caller's own `user_no` entry is present), **or** the whole class is present
/// (present==total), **or** the top-level status is present. NEVER "any entry" — that would mask the
/// caller's own sign failure whenever a classmate is present. Empty `user_no` skips my_present.
pub async fn recheck_on_call_fine(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
) -> bool {
    let Ok(resp) = client.get(ep.student_rollcalls(id)).send().await else {
        return false;
    };
    let Ok(v) =
        crate::http::read_bounded_json(resp, crate::http::MAX_API_JSON, "rollcall roster").await
    else {
        return false;
    };
    let (present, total) = roster_stats(&v);
    my_present(&v, user_no) || (total > 0 && present == total) || top_fine(&v)
}

/// Brute-force tuning (rollcall.rs). Kept for backward compatibility — the
/// runtime clamps effective concurrency to 1 per rollcall/profile (unknown-safe
/// sequential; see `brute_force_number`).
#[derive(Clone, Copy)]
pub struct NumberCfg {
    pub concurrency: u32,
    pub min_concurrency: u32,
    pub cooldown_ms: u64,
    pub max_cooldowns: u32,
}

impl NumberCfg {
    /// Retained knobs are clamped to sequential: at most one unresolved number
    /// mutation per rollcall/profile (ambiguous responses may be successes).
    pub fn effective_concurrency(self) -> u32 {
        1
    }
    pub fn was_clamped(self) -> bool {
        self.concurrency.clamp(1, 256) != 1 || self.min_concurrency.clamp(1, 256) != 1
    }
}

/// Classification of a single number-answer response — the real server distinguishes these and the
/// old code (recheck-only, 429-only) could neither stop on a fatal session nor tell wrong-vs-throttled.
/// `Ambiguous` is the submitted-unconfirmed outcome: a 2xx that carries no explicit success or wrong
/// marker (empty/204, non-JSON, non-object, or an unknown JSON object). Callers must not try another
/// code; they must run the bounded `on_call_fine` recheck and, if still unconfirmed, surface an
/// ambiguous result without retrying the mutation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeResult {
    Success,
    Wrong,
    Transient,
    Fatal,
    Ambiguous,
}

/// Classify a number-answer by HTTP status first, then the body for a 2xx (rollcall.rs).
pub fn classify_response(status: u16, body: &str) -> CodeResult {
    if status == 401 || status == 403 || (300..400).contains(&status) {
        return CodeResult::Fatal; // auth lost / redirect to login → aborting is the only safe move
    }
    if matches!(status, 408 | 425 | 429) || (500..600).contains(&status) {
        return CodeResult::Transient; // throttled / server hiccup → cool down and retry
    }
    if matches!(status, 400 | 409 | 422) {
        return CodeResult::Wrong; // this code is wrong, others may be right
    }
    if (200..300).contains(&status) {
        return classify_number_2xx(body);
    }
    CodeResult::Ambiguous
}

// Number-answer body markers (v1 `number_rollcall`), matched case-insensitively against body+message.
const UNAUTHORIZED_MARKERS: &[&str] = &[
    "unauthorized",
    "forbidden",
    "login",
    "sign in",
    "session expired",
    "未登入",
    "請登入",
    "登入逾時",
    "權限",
];

const WRONG_CODE_MARKERS: &[&str] = &[
    "wrong",
    "incorrect",
    "invalid number",
    "invalid code",
    "not match",
    "mismatch",
    "錯誤",
    "錯碼",
    "不正確",
    "失敗",
    "不存在",
    "過期",
];

/// Classify a 2xx number-answer body. Confirmed live (2026-07): a real accept is
/// `{"id":…,"status":"on_call"}` with no success bool. An empty/204 body or any 2xx without an
/// explicit positive or explicit wrong marker is `Ambiguous` (submitted-unconfirmed): callers must
/// stop trying further codes, verify via the bounded recheck, and surface unconfirmed if not proven.
fn classify_number_2xx(body: &str) -> CodeResult {
    let text = body.trim();
    if text.is_empty() {
        return CodeResult::Ambiguous;
    }
    if has_marker(&text.to_lowercase(), UNAUTHORIZED_MARKERS) {
        return CodeResult::Fatal;
    }
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        // Non-JSON text: treat explicit wrong text as Wrong (ARTT parity), otherwise Ambiguous.
        if has_marker(&text.to_lowercase(), WRONG_CODE_MARKERS) {
            return CodeResult::Wrong;
        }
        return CodeResult::Ambiguous;
    };
    let Some(object) = payload.as_object() else {
        return CodeResult::Ambiguous;
    };
    let message = payload_message(&payload);
    let combined = format!("{text} {message}").to_lowercase();
    if has_marker(&combined, UNAUTHORIZED_MARKERS) {
        return CodeResult::Fatal;
    }
    let success_flag = payload_bool(&payload, &["success", "ok", "is_success"]);
    if success_flag == Some(true) {
        return CodeResult::Success;
    }
    if has_marker(&combined, WRONG_CODE_MARKERS) {
        return CodeResult::Wrong;
    }
    if success_flag == Some(false) {
        return CodeResult::Wrong;
    }
    if crate::http::explicit_business_error_value(&payload) {
        return CodeResult::Wrong;
    }
    let status = object.get("status").and_then(Value::as_str).unwrap_or("");
    if matches!(
        status,
        "on_call" | "on_call_fine" | "accepted" | "completed"
    ) {
        return CodeResult::Success;
    }
    // No explicit success and no explicit wrong → submitted-unconfirmed.
    CodeResult::Ambiguous
}

/// The human message a payload carries (v1 `_payload_message`) — the first non-empty of these keys.
fn payload_message(p: &Value) -> String {
    [
        "message",
        "msg",
        "error",
        "error_description",
        "detail",
        "status",
    ]
    .iter()
    .find_map(|k| match p.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
    .unwrap_or_default()
}

fn payload_bool(p: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| p.get(*k).and_then(Value::as_bool))
}

fn has_marker(text_lower: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| text_lower.contains(m))
}

/// The token every sign type's auth-lost `Err` carries, so `is_auth_lost` recognises it (R4.1 #2).
pub const SESSION_INVALID: &str = "session invalid";

/// The URL half of the auth-lost decision: a final URL that landed on a login page. ONE shared
/// conservative classifier (`http::response_url_is_login`): path segments `login`/`sso` or server
/// pages like `Login.aspx`/`login.jsp` — never a whole-URL substring scan, so an arbitrary
/// `?next=/login` query parameter can't false-positive a healthy API response as a dead session.
pub fn response_url_is_login(url: &reqwest::Url) -> bool {
    crate::http::response_url_is_login(url)
}

/// Is this response a dead session? The status+body half (`classify_response == Fatal`) OR the url half.
/// Used by radar/self_registration/number so ALL sign types recover a session lost mid-sign, not only number.
pub fn response_auth_lost(status: u16, url: &reqwest::Url, body: &str) -> bool {
    classify_response(status, body) == CodeResult::Fatal || response_url_is_login(url)
}

/// Every mutating sign request must prove HTTP success before a roster recheck can confirm this
/// attempt. Otherwise a pre-existing present row could turn an HTTP 400/500 into a false success.
fn validate_sign_response(
    operation: &str,
    status: u16,
    url: &reqwest::Url,
    body: &str,
) -> Result<(), String> {
    if response_auth_lost(status, url, body) {
        return Err(format!("{operation}: {SESSION_INVALID}"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("{operation}: HTTP {status}"));
    }
    if crate::http::explicit_business_error(body) {
        return Err(format!("{operation}: server rejected the request"));
    }
    Ok(())
}

/// Does a sign error signal a lost session (→ re-login + re-sign)? Keyed on the shared `SESSION_INVALID`.
pub fn is_auth_lost(err: &str) -> bool {
    err.contains(SESSION_INVALID)
}

/// number: submit the shared code once (classified), or brute-force it. Success is the response
/// success flag — not just a recheck (rollcall.rs §3). Returns the winning code so it can be shared.
/// An `Ambiguous` (submitted-unconfirmed) 2xx never tries another code: it runs the bounded
/// `on_call_fine` recheck once and, if still unconfirmed, surfaces `submitted_unconfirmed` without
/// retrying the mutation. Explicit `Wrong` still advances; explicit `Success` still requires confirmation.
pub async fn sign_number(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
    device_id: &str,
    code: Option<&str>,
    cfg: NumberCfg,
) -> Result<SignOutcome, String> {
    let url = ep.answer_number(id);
    let outcome = if let Some(code) = code {
        match submit_number_code(client, &url, device_id, code).await {
            CodeResult::Success => {
                if !recheck_on_call_fine(client, ep, id, user_no).await {
                    return Err("number: submission was not confirmed by the roster".into());
                }
                Ok(SignOutcome {
                    method: "number".into(),
                    discovered_code: Some(code.to_string()),
                })
            }
            CodeResult::Ambiguous => {
                if recheck_on_call_fine(client, ep, id, user_no).await {
                    Ok(SignOutcome {
                        method: "number(ambiguous-confirmed)".into(),
                        discovered_code: Some(code.to_string()),
                    })
                } else {
                    Err(format!(
                        "number: ambiguous response for code {code} (submitted_unconfirmed) — roster did not confirm on_call_fine, not retrying"
                    ))
                }
            }
            CodeResult::Fatal => Err(format!("number: fatal response ({SESSION_INVALID})")),
            CodeResult::Transient => Err("number: transient error submitting shared code".into()),
            CodeResult::Wrong => Err("number: shared code rejected".into()),
        }
    } else {
        brute_force_number(client, ep, id, user_no, &url, device_id, cfg).await
    }?;
    Ok(outcome)
}

async fn submit_number_code(client: &Client, url: &str, device_id: &str, code: &str) -> CodeResult {
    match client
        .put(url)
        .json(&json!({ "deviceId": device_id, "numberCode": code }))
        .send()
        .await
    {
        Err(_) => CodeResult::Transient, // transport error → treat as transient, cool down
        Ok(resp) => {
            let status = resp.status().as_u16();
            let rurl = resp.url().clone();
            let Ok(body) =
                crate::http::read_bounded(resp, crate::http::MAX_MUTATION_BODY, "number sign")
                    .await
            else {
                return CodeResult::Transient;
            };
            let body = String::from_utf8_lossy(&body);
            let r = classify_response(status, &body);
            // OR-in the url half so a redirect-to-login (2xx body) is Fatal too (shared auth-lost truth).
            if r == CodeResult::Fatal || response_url_is_login(&rurl) {
                CodeResult::Fatal
            } else {
                r
            }
        }
    }
}

// Unknown-safe sequential brute-force: at most ONE unresolved number mutation in
// flight per rollcall/profile at a time. An ambiguous 2xx can be a successful
// submit whose confirmation has not yet landed on the roster; a concurrent
// wave (the old width≈100 JoinSet) would let other in-flight candidate PUTs
// arrive after that success, mutating the rollcall again with an unverified
// side-effect (the endpoint semantics do not prove wrong-code requests harmless
// post-attendance). Strict single-flight makes cancellation irrelevant — there
// is never a second unresolved mutation to cancel — and an already-handed-off
// request cannot be retracted by aborting a task. Explicit Wrong advances
// immediately; explicit/ambiguous Success runs the bounded on_call_fine recheck
// then stops (confirmed/unconfirmed) without trying another code; transport
// failure follows the bounded transient policy without blind duplicate.
async fn brute_force_number(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
    url: &str,
    device_id: &str,
    cfg: NumberCfg,
) -> Result<SignOutcome, String> {
    let floor = cfg.cooldown_ms.max(1);
    let mut cooldowns = 0u32;
    let mut n: u32 = 0;
    while n <= 9999 {
        let code = format!("{n:04}");
        match submit_number_code(client, url, device_id, &code).await {
            CodeResult::Fatal => {
                return Err(format!(
                    "number: fatal response ({SESSION_INVALID} / login page) — aborting the round"
                ))
            }
            CodeResult::Success => {
                if !recheck_on_call_fine(client, ep, id, user_no).await {
                    return Err("number: submission was not confirmed by the roster".into());
                }
                return Ok(SignOutcome {
                    method: "number(brute)".into(),
                    discovered_code: Some(code),
                });
            }
            CodeResult::Ambiguous => {
                if recheck_on_call_fine(client, ep, id, user_no).await {
                    return Ok(SignOutcome {
                        method: "number(brute-ambiguous-confirmed)".into(),
                        discovered_code: Some(code),
                    });
                }
                return Err(format!(
                    "number: ambiguous response for code {code} (submitted_unconfirmed) — roster did not confirm on_call_fine, not retrying"
                ));
            }
            CodeResult::Transient => {
                cooldowns += 1;
                if cooldowns > cfg.max_cooldowns {
                    return Err("number: too many transient errors, giving up".into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(floor)).await;
                // retry the same code without advancing — sequential retry of the throttled code.
                continue;
            }
            CodeResult::Wrong => {
                n += 1;
            }
        }
    }
    Err("number code not found in 0000–9999".into())
}

/// radar: walk the configured strategy chain in order, rechecking `on_call_fine` after each (rollcall.rs).
/// `empty_answer` = PUT `{}` (main path); `global_wgs84` = probe distances and multilaterate on the
/// WGS84 ellipsoid, then resubmit the solved point. Default chain is `[empty_answer, global_wgs84]`.
pub async fn sign_radar(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    strategies: &[String],
    user_no: &str,
    device_id: &str,
) -> Result<SignOutcome, String> {
    let mut last_err = String::from("radar: no strategy in chain succeeded");
    for strat in strategies {
        match strat.as_str() {
            "empty_answer" => {
                // Empty main path (rollcall.rs / radar.rs §1): plain `{}`, no api_version, no beacon.
                let resp = client
                    .put(ep.answer_radar(id))
                    .json(&json!({}))
                    .send()
                    .await
                    .map_err(|e| format!("radar: {e}"))?;
                let (status, rurl) = (resp.status().as_u16(), resp.url().clone());
                let body =
                    crate::http::read_bounded(resp, crate::http::MAX_MUTATION_BODY, "radar sign")
                        .await?;
                validate_sign_response("radar", status, &rurl, &String::from_utf8_lossy(&body))?;
                if recheck_on_call_fine(client, ep, id, user_no).await {
                    return Ok(SignOutcome {
                        method: "radar(empty)".into(),
                        ..Default::default()
                    });
                }
                last_err = "radar empty answer did not mark present".into();
            }
            "global_wgs84" => {
                match radar_solve_and_sign(client, ep, id, user_no, device_id).await {
                    Ok(outcome) => return Ok(outcome),
                    // A dead session must abort the whole chain instead of falling through to the next
                    // strategy: the shared SESSION_INVALID marker routes the monitor to re-login + re-sign.
                    Err(e) if is_auth_lost(&e) => return Err(e),
                    Err(e) => last_err = e,
                }
            }
            other => last_err = format!("radar: unknown strategy '{other}' skipped"),
        }
    }
    Err(last_err)
}

/// `global_wgs84` = the radar.rs §11 driver (steps 1-5): `lite` (no coords) → probe the 12 earth-scale
/// anchors → `solve_global_radar` → standard sampling rings → refined estimate. Any submit that lands
/// in scope (HTTP 2xx, no error_code) is a hit → recheck → sign. ponytail: steps 6-7 (supplement
/// rings / unbounded chessboard grid / rate-limit cooldown) + concurrent anchor probing are R2.5.
async fn radar_solve_and_sign(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
    device_id: &str,
) -> Result<SignOutcome, String> {
    let (use_beacon, beacon_nonce) = radar_lite(client, ep, id).await;
    let beacon = if use_beacon {
        Some((beacon_nonce.as_str(), user_no))
    } else {
        None
    };

    let mut obs: Vec<Observation> = Vec::new();
    // (1) probe the 12 global anchors; a direct in-scope hit signs immediately. A dead session aborts
    // the WHOLE chain right here — running every anchor first would turn an auth loss into a generic
    // "not within scope" failure and skip the shared re-login + re-sign path.
    for point in radar::global_anchor_points(12) {
        match radar_probe(client, ep, id, point, device_id, beacon).await {
            ProbeOutcome::AuthLost => return Err(format!("radar probe: {SESSION_INVALID}")),
            ProbeOutcome::InRange => return radar_confirm(client, ep, id, user_no).await,
            ProbeOutcome::Distance(d) => obs.push(Observation { point, distance: d }),
            ProbeOutcome::NoInfo => {}
        }
    }
    // (2) need ≥3 distances to solve.
    if obs.len() < 3 {
        return Err(format!(
            "radar: only {} anchor distances (need ≥3)",
            obs.len()
        ));
    }
    // (3) coarse global solve.
    let est = radar::solve_global_radar(&obs, None)
        .ok_or("radar: solver failed")?
        .point;
    // (4) standard sampling rings around the estimate.
    for point in radar::standard_sample_points(est) {
        match radar_probe(client, ep, id, point, device_id, beacon).await {
            ProbeOutcome::AuthLost => return Err(format!("radar probe: {SESSION_INVALID}")),
            ProbeOutcome::InRange => return radar_confirm(client, ep, id, user_no).await,
            ProbeOutcome::Distance(d) => obs.push(Observation { point, distance: d }),
            ProbeOutcome::NoInfo => {}
        }
    }
    // (5) refine with the fuller observation set and submit the estimate itself.
    let est2 = radar::solve_global_radar(&obs, Some(est))
        .ok_or("radar: refine failed")?
        .point;
    match radar_probe(client, ep, id, est2, device_id, beacon).await {
        ProbeOutcome::AuthLost => Err(format!("radar probe: {SESSION_INVALID}")),
        ProbeOutcome::InRange => radar_confirm(client, ep, id, user_no).await,
        _ => Err("radar: estimate not within scope (supplement/grid deferred to R2.5)".into()),
    }
}

async fn radar_confirm(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
) -> Result<SignOutcome, String> {
    if recheck_on_call_fine(client, ep, id, user_no).await {
        Ok(SignOutcome {
            method: "radar(solved)".into(),
            ..Default::default()
        })
    } else {
        Err("radar: hit but on_call_fine not set".into())
    }
}

/// `lite` (radar.rs §1): carries NO target coordinate — only `use_beacon` + `beacon_nonce`.
async fn radar_lite(client: &Client, ep: &Endpoints, id: &str) -> (bool, String) {
    let v: Value = match client.get(ep.lite(id)).send().await {
        Ok(r) => crate::http::read_bounded_json(r, crate::http::MAX_API_JSON, "radar lite")
            .await
            .unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    (
        v.get("use_beacon")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        v.get("beacon_nonce")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

enum ProbeOutcome {
    /// The probe response proves the session is dead (401/403/redirect-to-login/200 login page).
    AuthLost,
    InRange,
    Distance(f64),
    NoInfo,
}

/// Submit one coordinate answer (radar.rs §1 body) and classify the response.
async fn radar_probe(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    point: GeoPoint,
    device_id: &str,
    beacon: Option<(&str, &str)>,
) -> ProbeOutcome {
    let mut body = json!({
        "deviceId": device_id, "latitude": point.lat, "longitude": point.lon,
        "accuracy": 60, "speed": null, "heading": null, "altitude": 0, "altitudeAccuracy": null
    });
    if let Some((nonce, uid)) = beacon {
        body["radarSignal"] = Value::String(radar_signature(nonce, device_id, uid, now_unix()));
    }
    let resp = match client
        .put(ep.answer_radar_coord(id))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return ProbeOutcome::NoInfo,
    };
    let status = resp.status().as_u16();
    let rurl = resp.url().clone();
    let Ok(text) =
        crate::http::read_bounded(resp, crate::http::MAX_MUTATION_BODY, "radar probe").await
    else {
        return ProbeOutcome::NoInfo;
    };
    let text = String::from_utf8_lossy(&text);
    // A dead session (401/403/redirect-to-login/200 login HTML) is the shared auth marker, NOT an
    // out-of-scope miss: classify it FIRST so the radar chain aborts early with SESSION_INVALID and
    // the monitor's re-login + re-sign path runs instead of a generic failure after all anchors.
    if response_auth_lost(status, &rurl, &text) {
        return ProbeOutcome::AuthLost;
    }
    parse_scope(status, &text)
}

/// radar.rs §1 distance extraction — **error_code first, regardless of status** (a real server returns
/// out-of-scope as `200 + error_code`; a status-first check would misread every off-target anchor as
/// in-range → no distances → the solver never runs → radar fully dead; exact status is a §12 unknown).
fn parse_scope(status: u16, body: &str) -> ProbeOutcome {
    let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    if has_scope_error(&v) {
        return match extract_distance(&v) {
            Some(d) if d >= 0.0 => ProbeOutcome::Distance(d),
            _ => ProbeOutcome::NoInfo, // scope error but no usable distance
        };
    }
    if (200..300).contains(&status) {
        return ProbeOutcome::InRange; // in scope → hit
    }
    ProbeOutcome::NoInfo
}

/// Nested walk (radar.rs §1): body; if dict, descend keys `data,result,error,errors,scope,rollcall`;
/// if list, first 3 elements. Collect the dicts to inspect (bounded depth).
fn walk_dicts<'a>(v: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>, depth: u32) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Object(m) => {
            out.push(m);
            for key in ["data", "result", "error", "errors", "scope", "rollcall"] {
                if let Some(child) = m.get(key) {
                    walk_dicts(child, out, depth + 1);
                }
            }
        }
        Value::Array(a) => a
            .iter()
            .take(3)
            .for_each(|item| walk_dicts(item, out, depth + 1)),
        _ => {}
    }
}
fn has_scope_error(v: &Value) -> bool {
    let mut dicts = Vec::new();
    walk_dicts(v, &mut dicts, 0);
    dicts
        .iter()
        .any(|m| m.get("error_code").and_then(Value::as_str) == Some("radar_out_of_rollcall_scope"))
}
fn extract_distance(v: &Value) -> Option<f64> {
    let mut dicts = Vec::new();
    walk_dicts(v, &mut dicts, 0);
    for m in dicts {
        for key in [
            "distance",
            "scope_distance",
            "distance_meters",
            "distanceMeters",
        ] {
            if let Some(d) = m.get(key).and_then(Value::as_f64) {
                return Some(d);
            }
        }
    }
    None
}

/// beacon signature (radar.rs §1): `md5(nonce+deviceId+userId+ts) + "," + ts`. ts = unix seconds.
fn radar_signature(nonce: &str, device_id: &str, user_no: &str, ts: u64) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(format!("{nonce}{device_id}{user_no}{ts}"));
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex},{ts}")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// self_registration: empty body, the simplest type.
pub async fn sign_self_registration(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    user_no: &str,
) -> Result<SignOutcome, String> {
    let resp = client
        .put(ep.answer_self_registration(id))
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("self_registration: {e}"))?;
    let (status, rurl) = (resp.status().as_u16(), resp.url().clone());
    let body = crate::http::read_bounded(
        resp,
        crate::http::MAX_MUTATION_BODY,
        "self-registration sign",
    )
    .await?;
    validate_sign_response(
        "self_registration",
        status,
        &rurl,
        &String::from_utf8_lossy(&body),
    )?;
    if recheck_on_call_fine(client, ep, id, user_no).await {
        Ok(SignOutcome {
            method: "self_registration".into(),
            ..Default::default()
        })
    } else {
        Err("self_registration submitted but on_call_fine not set".into())
    }
}

/// qr teacher-assist: `data` sourced from the TEACHER's own qr rollcall is submitted to the
/// STUDENT's real `student_rollcall_id` (docs 32 — the token is portable; teacher rollcall is only
/// the data source, never the sign target).
pub async fn sign_qr_with_teacher_data(
    student: &Client,
    ep: &Endpoints,
    student_rollcall_id: &str,
    device_id: &str,
    data: &str,
    user_no: &str,
) -> Result<SignOutcome, String> {
    let resp = student
        .put(ep.answer_qr(student_rollcall_id))
        .json(&json!({ "deviceId": device_id, "data": data }))
        .send()
        .await
        .map_err(|e| format!("qr: {e}"))?;
    let (status, rurl) = (resp.status().as_u16(), resp.url().clone());
    let body = crate::http::read_bounded(resp, crate::http::MAX_MUTATION_BODY, "QR sign").await?;
    validate_sign_response("qr", status, &rurl, &String::from_utf8_lossy(&body))?;
    if recheck_on_call_fine(student, ep, student_rollcall_id, user_no).await {
        Ok(SignOutcome {
            method: "qr(teacher-assist)".into(),
            ..Default::default()
        })
    } else {
        Err("qr submitted but on_call_fine not set".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutating_sign_response_rejects_non_success_before_roster_recheck() {
        let answer = reqwest::Url::parse("https://tenant.example/api/rollcalls/1/answer").unwrap();
        let login = reqwest::Url::parse("https://tenant.example/login").unwrap();
        assert!(validate_sign_response("radar", 204, &answer, "").is_ok());
        assert!(
            validate_sign_response("radar", 200, &answer, r#"{"success":false}"#)
                .unwrap_err()
                .contains("server rejected")
        );
        assert!(
            validate_sign_response("qr", 200, &answer, r#"{"error_code":"qr_invalid"}"#)
                .unwrap_err()
                .contains("server rejected")
        );
        assert!(
            validate_sign_response("radar", 400, &answer, r#"{"ok":false}"#)
                .unwrap_err()
                .contains("HTTP 400")
        );
        assert!(validate_sign_response("qr", 500, &answer, "server error")
            .unwrap_err()
            .contains("HTTP 500"));
        assert!(validate_sign_response("self_registration", 200, &login, "")
            .unwrap_err()
            .contains(SESSION_INVALID));
    }

    #[test]
    fn number_code_read_from_every_observed_shape() {
        // The real 55379 roster: the code sits at the TOP LEVEL, NOT inside student_rollcalls[] — the
        // old code only looked in the array and so returned None on the live server (this bug).
        let live = json!({"is_number": true, "number_code": "1234", "status": "in_progress",
                          "student_rollcalls": [{"user_no": "a@b", "rollcall_status": "absent"}]});
        assert_eq!(number_code_from_payload(&live).as_deref(), Some("1234"));

        // int number_code → zero-padded 4-digit (a common contract variant).
        assert_eq!(
            number_code_from_payload(&json!({"number_code": 123})).as_deref(),
            Some("0123")
        );
        // data-wrapped, array container, and a bare list of student items.
        assert_eq!(
            number_code_from_payload(&json!({"data": {"number_code": "0007"}})).as_deref(),
            Some("0007")
        );
        assert_eq!(
            number_code_from_payload(&json!({"student_rollcalls": [{"number_code": "4321"}]}))
                .as_deref(),
            Some("4321")
        );
        assert_eq!(
            number_code_from_payload(&json!([{"number_code": 42}])).as_deref(),
            Some("0042")
        );
    }

    #[test]
    fn number_code_rejects_non_codes() {
        assert_eq!(
            number_code_from_payload(&json!({"status": "in_progress"})),
            None
        ); // no code field
        assert_eq!(coerce_number_code(&json!("not-a-code")), None);
        assert_eq!(coerce_number_code(&json!(true)), None); // bool is not a code
        assert_eq!(coerce_number_code(&json!("12")), None); // must be 4 digits
        assert_eq!(coerce_number_code(&json!("１２３４")), None); // full-width digits are not ascii
    }

    #[test]
    fn classify_number_2xx_requires_structured_success() {
        assert_eq!(
            classify_response(200, r#"{"id":1,"status":"on_call"}"#),
            CodeResult::Success
        );
        assert_eq!(
            classify_response(200, r#"{"success":true}"#),
            CodeResult::Success
        );
        // Explicit wrong / fatal still keep their lanes (incl. ARTT markers: wrong/invalid/錯誤 etc.).
        assert_eq!(
            classify_response(200, r#"{"success":false}"#),
            CodeResult::Wrong
        );
        assert_eq!(
            classify_response(200, r#"{"error_code":"denied"}"#),
            CodeResult::Wrong
        );
        assert_eq!(
            classify_response(200, r#"{"error":"wrong"}"#),
            CodeResult::Wrong
        );
        assert_eq!(
            classify_response(200, r#"{"message":"wrong number code"}"#),
            CodeResult::Wrong
        );
        assert_eq!(
            classify_response(200, r#"{"message":"invalid code"}"#),
            CodeResult::Wrong
        );
        assert_eq!(classify_response(200, "wrong code"), CodeResult::Wrong);
        assert_eq!(
            classify_response(200, "<html>login</html>"),
            CodeResult::Fatal
        );
        // Ambiguous 2xx (submitted-unconfirmed): empty/204, non-JSON, non-object, or an
        // unknown JSON object that carries no explicit success or wrong marker. These must
        // never classify Wrong — callers stop trying further codes and verify instead.
        for ambiguous in [
            "",
            "   ",
            "unsuccessful",
            "not ok",
            r#"{}"#,
            r#"{"id":1}"#,
            r#"{"foo":1,"bar":"baz"}"#,
            r#"["a","b"]"#,
            "204",
        ] {
            assert_eq!(
                classify_response(200, ambiguous),
                CodeResult::Ambiguous,
                "{ambiguous}"
            );
            assert_eq!(
                classify_response(204, ambiguous),
                CodeResult::Ambiguous,
                "204 {ambiguous}"
            );
        }
        // Plain non-JSON that is NOT a wrong marker is also ambiguous (not wrong).
        assert_eq!(
            classify_response(200, "just text no flag"),
            CodeResult::Ambiguous
        );
        assert_eq!(
            classify_response(204, "just text no flag"),
            CodeResult::Ambiguous
        );
        assert_eq!(classify_response(400, "wrong code"), CodeResult::Wrong);
        assert_eq!(classify_response(403, ""), CodeResult::Fatal);
        assert_eq!(classify_response(429, ""), CodeResult::Transient);
    }

    #[tokio::test]
    async fn ambiguous_number_does_not_emit_second_mutation_and_reports_unconfirmed_without_retry()
    {
        // One-shot faux TronClass: first number PUT is an ambiguous empty 2xx (but the roster
        // is marked present for the caller), roster confirms via my_present, second bogus 2xx
        // must never be hit; without the roster mark the same first ambiguous must surface the
        // submitted_unconfirmed error without a second mutation.
        use crate::providers::Endpoints;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn serve(
            listener: tokio::net::TcpListener,
            counter: Arc<AtomicUsize>,
            roster_present: bool,
        ) {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let cnt = counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let is_put = text.starts_with("PUT /api/rollcall/");
                    let resp = if is_put {
                        let idx = cnt.fetch_add(1, Ordering::SeqCst);
                        if idx == 0 {
                            // First code: ambiguous empty 2xx (no success/wrong marker).
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_string()
                        } else {
                            // Second attempt would be a different code; fail the test if hit.
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"success\":true}".to_string()
                        }
                    } else if text.contains("student_rollcalls") {
                        let status = if roster_present {
                            "on_call_fine"
                        } else {
                            "in_progress"
                        };
                        let body = format!(
                            r#"{{"status":"{status}","student_rollcalls":[{{"user_no":"u1","rollcall_status":"{status}"}}]}}"#
                        );
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        }

        // Delayed verification can confirm: ambiguous PUT + roster now present → success.
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let counter = Arc::new(AtomicUsize::new(0));
            let c2 = counter.clone();
            tokio::spawn(serve(listener, c2, true));
            let base = format!("http://{addr}");
            let ep = Endpoints::derive(&base);
            let client = reqwest::Client::new();
            let cfg = NumberCfg {
                concurrency: 1,
                min_concurrency: 1,
                cooldown_ms: 1,
                max_cooldowns: 0,
            };
            let outcome = sign_number(&client, &ep, "RC1", "u1", "dev-1", Some("0000"), cfg)
                .await
                .expect("ambiguous + later roster present must confirm");
            assert_eq!(outcome.discovered_code.as_deref(), Some("0000"));
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "must not try a second code"
            );
        }

        // Exhausted verification returns unconfirmed without retrying the mutation.
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let counter = Arc::new(AtomicUsize::new(0));
            let c2 = counter.clone();
            tokio::spawn(serve(listener, c2, false));
            let base = format!("http://{addr}");
            let ep = Endpoints::derive(&base);
            let client = reqwest::Client::new();
            let cfg = NumberCfg {
                concurrency: 1,
                min_concurrency: 1,
                cooldown_ms: 1,
                max_cooldowns: 0,
            };
            let err = sign_number(&client, &ep, "RC1", "u1", "dev-1", Some("0000"), cfg)
                .await
                .unwrap_err();
            assert!(
                err.contains("submitted_unconfirmed"),
                "exhausted ambiguous must surface unconfirmed, got: {err}"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "must not retry the mutation"
            );
        }

        // Explicit wrong still advances: Wrong 2xx does not become Ambiguous.
        assert_eq!(
            classify_response(200, r#"{"message":"wrong number code"}"#),
            CodeResult::Wrong
        );

        // ARTT-equivalent fixtures produce the same semantic category (empty/unknown→ambiguous, explicit preserved).
        assert_eq!(classify_response(200, ""), CodeResult::Ambiguous);
        assert_eq!(classify_response(204, ""), CodeResult::Ambiguous);
        assert_eq!(classify_response(200, r#"{"id":1}"#), CodeResult::Ambiguous);
        assert_eq!(
            classify_response(200, r#"{"success":true}"#),
            CodeResult::Success
        );
        assert_eq!(
            classify_response(200, r#"{"success":false}"#),
            CodeResult::Wrong
        );
        assert_eq!(classify_response(401, ""), CodeResult::Fatal);
        assert_eq!(classify_response(429, ""), CodeResult::Transient);
    }

    /// A one-shot roster server: `headers` is spliced verbatim after the status line (for a lying
    /// Content-Length), then `body`.
    async fn roster_server(headers: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}Connection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn oversized_roster_bodies_are_rejected_not_buffered() {
        // A Content-Length far above the 32 MiB API cap: the shared bounded read must reject it
        // UP FRONT — an unbounded `.json()` (the old code) would try to buffer the whole lie.
        // Both roster readers keep their Option semantics (None on any failure).
        let base = roster_server(
            "Content-Length: 999999999\r\n",
            "{\"number_code\":\"1234\"}",
        )
        .await;
        assert_eq!(
            attendance_rate(&Client::new(), &Endpoints::derive(&base), "1").await,
            None
        );
        let base = roster_server(
            "Content-Length: 999999999\r\n",
            "{\"number_code\":\"1234\"}",
        )
        .await;
        assert_eq!(
            read_number_code(&Client::new(), &Endpoints::derive(&base), "1").await,
            None
        );
    }

    #[tokio::test]
    async fn normal_roster_bodies_still_read_through_the_bounded_path() {
        // The bounded read must not disturb the success path: a small roster yields the rate and
        // the code exactly as before. `roster_server` serves ONE request, so each reader gets its
        // own server.
        let body = "{\"student_rollcalls\":[{\"user_no\":\"a\",\"rollcall_status\":\"on_call_fine\"},{\"user_no\":\"b\",\"rollcall_status\":\"absent\"}],\"number_code\":\"4321\"}";
        let base = roster_server("", body).await;
        assert_eq!(
            attendance_rate(&Client::new(), &Endpoints::derive(&base), "1").await,
            Some(50.0)
        );
        let base = roster_server("", body).await;
        assert_eq!(
            read_number_code(&Client::new(), &Endpoints::derive(&base), "1")
                .await
                .as_deref(),
            Some("4321")
        );
    }

    #[test]
    fn response_auth_lost_recognizes_every_session_death_shape() {
        let api = reqwest::Url::parse("https://tenant.example/api/rollcalls/1/answer").unwrap();
        let login = reqwest::Url::parse("https://tenant.example/login").unwrap();
        // 401 / 403 → fatal by status (radar probe aborts the chain, monitor re-logins + re-signs).
        assert!(response_auth_lost(401, &api, ""));
        assert!(response_auth_lost(403, &api, ""));
        // Redirected to a login URL — any status, incl. a 2xx after the redirect was followed.
        assert!(response_auth_lost(200, &login, ""));
        assert!(response_auth_lost(302, &login, ""));
        // A 200 login page served as HTML (the body carries the marker even without a login URL).
        assert!(response_auth_lost(
            200,
            &api,
            "<html><title>login</title></html>"
        ));
        // Genuine responses are NOT session loss.
        assert!(!response_auth_lost(200, &api, r#"{"status":"on_call"}"#));
        assert!(!response_auth_lost(400, &api, r#"{"error":"wrong"}"#));
    }

    #[test]
    fn number_cfg_clamps_effective_concurrency_to_one() {
        let cfg = NumberCfg {
            concurrency: 100,
            min_concurrency: 5,
            cooldown_ms: 5000,
            max_cooldowns: 3,
        };
        assert!(cfg.was_clamped());
        assert_eq!(cfg.effective_concurrency(), 1);
        let unitary = NumberCfg {
            concurrency: 1,
            min_concurrency: 1,
            cooldown_ms: 1,
            max_cooldowns: 0,
        };
        assert!(!unitary.was_clamped());
        assert_eq!(unitary.effective_concurrency(), 1);
    }

    #[tokio::test]
    async fn brute_force_number_is_sequential_with_barrier_proof() {
        use crate::providers::Endpoints;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn barrier_server(
            expected_concurrency_cfg: u32,
            handler: impl Fn(String, usize, Arc<AtomicUsize>, Arc<AtomicUsize>) -> String
                + Send
                + Sync
                + 'static,
        ) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            // Only mutation PUTs contend — roster GETs are sequential rechecks, not mutations.
            let mut_concurrent = Arc::new(AtomicUsize::new(0));
            let mut_max = Arc::new(AtomicUsize::new(0));
            let mut_requests = Arc::new(AtomicUsize::new(0));
            let handler = Arc::new(handler);
            let c1 = mut_concurrent.clone();
            let m1 = mut_max.clone();
            let r1 = mut_requests.clone();
            tokio::spawn(async move {
                let _ = expected_concurrency_cfg;
                loop {
                    let Ok((mut s, _)) = listener.accept().await else {
                        break;
                    };
                    let h = handler.clone();
                    let c = c1.clone();
                    let m = m1.clone();
                    let r = r1.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        let is_put = text.starts_with("PUT /api/rollcall/")
                            && text.contains("answer_number_rollcall");
                        let resp = if is_put {
                            let cur = c.fetch_add(1, Ordering::SeqCst) + 1;
                            m.fetch_max(cur, Ordering::SeqCst);
                            let idx = r.fetch_add(1, Ordering::SeqCst);
                            // Artificial delay to expose concurrency if caller were parallel.
                            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                            let resp = h(text, idx, c.clone(), m.clone());
                            c.fetch_sub(1, Ordering::SeqCst);
                            resp
                        } else if text.contains("student_rollcalls") {
                            let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(), body
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                        };
                        let _ = s.write_all(resp.as_bytes()).await;
                    });
                }
            });
            (
                format!("http://{addr}"),
                mut_concurrent,
                mut_max,
                mut_requests,
            )
        }

        // 1) Configured concurrency 100 must still max concurrent == 1, ambiguous first emits exactly 1 request.
        {
            let (base, _cur, max_c, reqs) = barrier_server(100, |_, idx, _, _| {
                assert_eq!(idx, 0, "ambiguous first must be the only PUT");
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            })
            .await;
            let ep = Endpoints::derive(&base);
            let client = Client::new();
            let cfg = NumberCfg {
                concurrency: 100,
                min_concurrency: 5,
                cooldown_ms: 1,
                max_cooldowns: 0,
            };
            let out = brute_force_number(
                &client,
                &ep,
                "RC1",
                "u1",
                &ep.answer_number("RC1"),
                "dev-1",
                cfg,
            )
            .await
            .expect("ambiguous confirmed");
            assert_eq!(out.discovered_code.as_deref(), Some("0000"));
            assert_eq!(
                max_c.load(Ordering::SeqCst),
                1,
                "configured 100 must still be sequential"
            );
            assert_eq!(
                reqs.load(Ordering::SeqCst),
                1,
                "ambiguous first emits exactly one request"
            );
        }

        // 2) Explicit wrong then next emits exactly two PUTs sequentially, second succeeds (confirmed).
        {
            let (base, _cur, max_c, reqs) = barrier_server(100, |text, idx, _, _| {
                if idx == 0 {
                    assert!(text.contains("\"0000\""), "first must be 0000");
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"message\":\"wrong number code\"}".to_string()
                } else if idx == 1 {
                    assert!(text.contains("\"0001\""), "second must be 0001");
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"success\":true}".to_string()
                } else {
                    panic!("must not emit more than two PUTs, idx={idx}");
                }
            })
            .await;
            let ep = Endpoints::derive(&base);
            let client = Client::new();
            let cfg = NumberCfg {
                concurrency: 64,
                min_concurrency: 4,
                cooldown_ms: 1,
                max_cooldowns: 0,
            };
            let out = brute_force_number(
                &client,
                &ep,
                "RC1",
                "u1",
                &ep.answer_number("RC1"),
                "dev-1",
                cfg,
            )
            .await
            .expect("wrong then success");
            assert_eq!(out.discovered_code.as_deref(), Some("0001"));
            assert_eq!(max_c.load(Ordering::SeqCst), 1);
            assert_eq!(
                reqs.load(Ordering::SeqCst),
                2,
                "wrong advances then next emits exactly two sequentially"
            );
        }

        // 3) Transport hang: cancellation/timeout aborts the round without a second mutation.
        // The server never responds to the first PUT; the client has a short timeout and must not emit a second PUT.
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let reqs = Arc::new(AtomicUsize::new(0));
            let r2 = reqs.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut s, _)) = listener.accept().await else {
                        break;
                    };
                    let r = r2.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        if text.starts_with("PUT /api/rollcall/") {
                            r.fetch_add(1, Ordering::SeqCst);
                            // Hang: never write a response, holding the connection.
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        } else if text.contains("student_rollcalls") {
                            let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                            let _ = s.write_all(resp.as_bytes()).await;
                        } else {
                            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        }
                    });
                }
            });
            let base = format!("http://{addr}");
            let ep = Endpoints::derive(&base);
            let url = ep.answer_number("RC1");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(200))
                .build()
                .unwrap();
            let cfg = NumberCfg {
                concurrency: 50,
                min_concurrency: 5,
                cooldown_ms: 50,
                max_cooldowns: 0,
            };
            let fut = brute_force_number(&client, &ep, "RC1", "u1", &url, "dev-1", cfg);
            let res = tokio::time::timeout(std::time::Duration::from_secs(2), fut).await;
            // Either the round times out or it fails after its transient budget — but it must have emitted exactly one PUT and never a second.
            assert!(
                res.is_err() || res.unwrap().is_err(),
                "hang must not succeed"
            );
            // Give the hanging handler a moment to be counted before asserting (it counts on accept).
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_eq!(
                reqs.load(Ordering::SeqCst),
                1,
                "timeout/cancel must not emit a second mutation"
            );
        }

        // 4) Group scope: two independent rollcall ids each run sequential; barriers isolated per rollcall/profile.
        {
            for rid in ["G1", "G2"] {
                let (base, _cur, max_c, reqs) = barrier_server(100, |text, idx, _, _| {
                    if idx == 0 {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"message\":\"wrong number code\"}".to_string()
                    } else {
                        assert!(text.contains("\"0001\""));
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"success\":true}".to_string()
                    }
                })
                .await;
                let ep = Endpoints::derive(&base);
                let client = Client::new();
                let cfg = NumberCfg {
                    concurrency: 32,
                    min_concurrency: 2,
                    cooldown_ms: 1,
                    max_cooldowns: 0,
                };
                let out = brute_force_number(
                    &client,
                    &ep,
                    rid,
                    "u1",
                    &ep.answer_number(rid),
                    "dev-1",
                    cfg,
                )
                .await
                .unwrap();
                assert_eq!(out.discovered_code.as_deref(), Some("0001"));
                assert_eq!(
                    max_c.load(Ordering::SeqCst),
                    1,
                    "each rollcall is sequential"
                );
                assert_eq!(reqs.load(Ordering::SeqCst), 2);
            }
        }
    }
}
