//! The four rollcall types (docs 30). `classify` is pure and unit-tested; the per-type answer +
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

/// Classify a rollcall by its status flags — each rollcall is exactly one type (docs 30 table).
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

// --- `student_rollcalls` object roster helpers (docs 30 real contract) ---

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
                let uid = e.get("user_no").or_else(|| e.get("user_id")).and_then(Value::as_str);
                uid.map(|u| u.eq_ignore_ascii_case(user_no)).unwrap_or(false) && entry_fine(e)
            })
        })
        .unwrap_or(false)
}

/// Whole-class attendance rate (percent) for the 15% gate — computed from the roster (docs 30).
pub async fn attendance_rate(client: &Client, ep: &Endpoints, id: &str) -> Option<f64> {
    let v: Value = client.get(ep.student_rollcalls(id)).send().await.ok()?.json().await.ok()?;
    let (present, total) = roster_stats(&v);
    (total > 0).then(|| present as f64 / total as f64 * 100.0)
}

/// Read the shared number code once from the roster. None → caller brute-forces.
pub async fn read_number_code(client: &Client, ep: &Endpoints, id: &str) -> Option<String> {
    let v: Value = client.get(ep.student_rollcalls(id)).send().await.ok()?.json().await.ok()?;
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
    let in_item = |item: &Value| item.as_object()?.get("number_code").and_then(coerce_number_code);
    if let Some(obj) = payload.as_object() {
        if let Some(c) = obj.get("number_code").and_then(coerce_number_code) {
            return Some(c);
        }
        if let Some(c) = obj.get("data").and_then(|d| d.get("number_code")).and_then(coerce_number_code) {
            return Some(c);
        }
        for key in ["student_rollcalls", "data"] {
            if let Some(c) = obj.get(key).and_then(Value::as_array).and_then(|a| a.iter().find_map(in_item)) {
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
pub async fn recheck_on_call_fine(client: &Client, ep: &Endpoints, id: &str, user_no: &str) -> bool {
    let Ok(resp) = client.get(ep.student_rollcalls(id)).send().await else { return false };
    let Ok(v) = resp.json::<Value>().await else { return false };
    let (present, total) = roster_stats(&v);
    my_present(&v, user_no) || (total > 0 && present == total) || top_fine(&v)
}

/// Brute-force tuning (docs 30). Concurrency starts high and halves toward `min_concurrency` on
/// throttling; `cooldown_ms` is the backoff sleep; give up after `max_cooldowns` transient rounds.
#[derive(Clone, Copy)]
pub struct NumberCfg {
    pub concurrency: u32,
    pub min_concurrency: u32,
    pub cooldown_ms: u64,
    pub max_cooldowns: u32,
}

/// Classification of a single number-answer response — the real server distinguishes these and the
/// old code (recheck-only, 429-only) could neither stop on a fatal session nor tell wrong-vs-throttled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeResult {
    Success,
    Wrong,
    Transient,
    Fatal,
}

/// Classify a number-answer by HTTP status first, then the body for a 2xx (docs 30).
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
    CodeResult::Wrong
}

// Number-answer body markers (v1 `number_rollcall`), matched case-insensitively against body+message.
const SUCCESS_MARKERS: &[&str] =
    &["success", "ok", "on_call", "on_call_fine", "accepted", "completed", "已完成", "成功", "點名成功", "簽到成功"];
const WRONG_CODE_MARKERS: &[&str] =
    &["wrong", "incorrect", "invalid number", "invalid code", "not match", "mismatch", "錯誤", "錯碼", "不正確", "失敗", "不存在", "過期"];
const UNAUTHORIZED_MARKERS: &[&str] =
    &["unauthorized", "forbidden", "login", "sign in", "session expired", "未登入", "請登入", "登入逾時", "權限"];

/// Classify a 2xx number-answer body — faithful port of v1 `classify_number_response`'s 2xx branch.
/// Confirmed live (2026-07): a real accept is `{"id":…,"status":"on_call"}` with NO success bool, so a
/// 2xx **defaults to Success** (the old v2 default of Wrong silently rejected every real sign). Empty →
/// Success; auth markers → Fatal; explicit `success` bool wins; wrong markers / `success:false` → Wrong.
fn classify_number_2xx(body: &str) -> CodeResult {
    let text = body.trim();
    if text.is_empty() {
        return CodeResult::Success;
    }
    let payload: Option<Value> = serde_json::from_str::<Value>(text).ok().filter(Value::is_object);
    let message = payload.as_ref().map(payload_message).unwrap_or_default();
    let combined = format!("{text} {message}").to_lowercase();

    if has_marker(&combined, UNAUTHORIZED_MARKERS) {
        return CodeResult::Fatal; // a 2xx that is really the login page / an auth error = session expired
    }
    let success_flag = payload.as_ref().and_then(|p| payload_bool(p, &["success", "ok", "is_success"]));
    if success_flag == Some(true) {
        return CodeResult::Success;
    }
    if has_marker(&combined, WRONG_CODE_MARKERS) {
        return CodeResult::Wrong;
    }
    if success_flag == Some(false) {
        return CodeResult::Wrong;
    }
    let marker_text = if payload.is_some() { message.to_lowercase() } else { combined };
    if has_marker(&marker_text, SUCCESS_MARKERS) {
        return CodeResult::Success;
    }
    CodeResult::Success // v1's 2xx default
}

/// The human message a payload carries (v1 `_payload_message`) — the first non-empty of these keys.
fn payload_message(p: &Value) -> String {
    ["message", "msg", "error", "error_description", "detail", "status"]
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

/// The URL half of the auth-lost decision: a final URL that landed on a login page (whole URL, lowercased
/// — v1 `"login" in str(resp.url).lower()`). Shared by every sign type and the poll canary (single truth).
pub fn response_url_is_login(url: &reqwest::Url) -> bool {
    url.as_str().to_lowercase().contains("login")
}

/// Is this response a dead session? The status+body half (`classify_response == Fatal`) OR the url half.
/// Used by radar/self_registration/number so ALL sign types recover a session lost mid-sign, not only number.
pub fn response_auth_lost(status: u16, url: &reqwest::Url, body: &str) -> bool {
    classify_response(status, body) == CodeResult::Fatal || response_url_is_login(url)
}

/// Does a sign error signal a lost session (→ re-login + re-sign)? Keyed on the shared `SESSION_INVALID`.
pub fn is_auth_lost(err: &str) -> bool {
    err.contains(SESSION_INVALID)
}

/// number: submit the shared code once (classified), or brute-force it. Success is the response
/// success flag — not just a recheck (docs 30 §3). Returns the winning code so it can be shared.
pub async fn sign_number(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    device_id: &str,
    code: Option<&str>,
    cfg: NumberCfg,
) -> Result<SignOutcome, String> {
    let url = ep.answer_number(id);
    if let Some(code) = code {
        return match submit_number_code(client, &url, device_id, code).await {
            CodeResult::Success => Ok(SignOutcome { method: "number".into(), discovered_code: Some(code.to_string()) }),
            CodeResult::Fatal => Err(format!("number: fatal response ({SESSION_INVALID})")),
            CodeResult::Transient => Err("number: transient error submitting shared code".into()),
            CodeResult::Wrong => Err("number: shared code rejected".into()),
        };
    }
    brute_force_number(client, &url, device_id, cfg).await
}

async fn submit_number_code(client: &Client, url: &str, device_id: &str, code: &str) -> CodeResult {
    match client.put(url).json(&json!({ "deviceId": device_id, "numberCode": code })).send().await {
        Err(_) => CodeResult::Transient, // transport error → treat as transient, cool down
        Ok(resp) => {
            let status = resp.status().as_u16();
            let rurl = resp.url().clone();
            let body = resp.text().await.unwrap_or_default();
            let r = classify_response(status, &body);
            // OR-in the url half so a redirect-to-login (2xx body) is Fatal too (shared auth-lost truth).
            if r == CodeResult::Fatal || response_url_is_login(&rurl) { CodeResult::Fatal } else { r }
        }
    }
}

// ponytail: bounded 0000–9999 in batches of `width` (starts at `concurrency`≈100). Any Fatal aborts
// the whole round immediately; any Success wins; a throttled batch halves `width` toward the min and
// retries after a cooldown, giving up after `max_cooldowns`. Widen/tune via Settings if a real tenant
// needs it.
async fn brute_force_number(client: &Client, url: &str, device_id: &str, cfg: NumberCfg) -> Result<SignOutcome, String> {
    let floor = cfg.cooldown_ms.max(1);
    let min_w = cfg.min_concurrency.clamp(1, 256);
    let mut width = cfg.concurrency.clamp(min_w, 256);
    let mut cooldowns = 0u32;
    let mut n: u32 = 0;
    while n <= 9999 {
        let batch_start = n;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..width {
            if n > 9999 {
                break;
            }
            let code = format!("{n:04}");
            n += 1;
            let (client, url, device_id) = (client.clone(), url.to_string(), device_id.to_string());
            set.spawn(async move {
                let r = submit_number_code(&client, &url, &device_id, &code).await;
                (code, r)
            });
        }
        let (mut fatal, mut transient, mut success) = (false, false, None);
        while let Some(res) = set.join_next().await {
            if let Ok((code, r)) = res {
                match r {
                    CodeResult::Fatal => fatal = true,
                    CodeResult::Success => success = Some(code),
                    CodeResult::Transient => transient = true,
                    CodeResult::Wrong => {}
                }
            }
        }
        if fatal {
            return Err(format!("number: fatal response ({SESSION_INVALID} / login page) — aborting the round"));
        }
        if let Some(code) = success {
            return Ok(SignOutcome { method: "number(brute)".into(), discovered_code: Some(code) });
        }
        if transient {
            cooldowns += 1;
            if cooldowns > cfg.max_cooldowns {
                return Err("number: too many transient errors, giving up".into());
            }
            width = (width / 2).max(min_w); // adaptive: halve toward the floor concurrency
            tokio::time::sleep(std::time::Duration::from_millis(floor)).await;
            n = batch_start; // retry the throttled batch
        }
        // all wrong → n already advanced to the next batch
    }
    Err("number code not found in 0000–9999".into())
}

/// radar: walk the configured strategy chain in order, rechecking `on_call_fine` after each (docs 30).
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
                // Empty main path (docs 30 / docs/70 §1): plain `{}`, no api_version, no beacon.
                let resp = client
                    .put(ep.answer_radar(id))
                    .json(&json!({}))
                    .send()
                    .await
                    .map_err(|e| format!("radar: {e}"))?;
                // Session lost mid-sign → abort with the shared marker so the monitor re-logins + re-signs.
                let (status, rurl) = (resp.status().as_u16(), resp.url().clone());
                let body = resp.text().await.unwrap_or_default();
                if response_auth_lost(status, &rurl, &body) {
                    return Err(format!("radar: {SESSION_INVALID}"));
                }
                if recheck_on_call_fine(client, ep, id, user_no).await {
                    return Ok(SignOutcome { method: "radar(empty)".into(), ..Default::default() });
                }
                last_err = "radar empty answer did not mark present".into();
            }
            "global_wgs84" => match radar_solve_and_sign(client, ep, id, user_no, device_id).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) => last_err = e,
            },
            other => last_err = format!("radar: unknown strategy '{other}' skipped"),
        }
    }
    Err(last_err)
}

/// `global_wgs84` = the docs/70 §11 driver (steps 1-5): `lite` (no coords) → probe the 12 earth-scale
/// anchors → `solve_global_radar` → standard sampling rings → refined estimate. Any submit that lands
/// in scope (HTTP 2xx, no error_code) is a hit → recheck → sign. ponytail: steps 6-7 (supplement
/// rings / unbounded chessboard grid / rate-limit cooldown) + concurrent anchor probing are R2.5.
async fn radar_solve_and_sign(client: &Client, ep: &Endpoints, id: &str, user_no: &str, device_id: &str) -> Result<SignOutcome, String> {
    let (use_beacon, beacon_nonce) = radar_lite(client, ep, id).await;
    let beacon = if use_beacon { Some((beacon_nonce.as_str(), user_no)) } else { None };

    let mut obs: Vec<Observation> = Vec::new();
    // (1) probe the 12 global anchors; a direct in-scope hit signs immediately.
    for point in radar::global_anchor_points(12) {
        match radar_probe(client, ep, id, point, device_id, beacon).await {
            ProbeOutcome::InRange => return radar_confirm(client, ep, id, user_no).await,
            ProbeOutcome::Distance(d) => obs.push(Observation { point, distance: d }),
            ProbeOutcome::NoInfo => {}
        }
    }
    // (2) need ≥3 distances to solve.
    if obs.len() < 3 {
        return Err(format!("radar: only {} anchor distances (need ≥3)", obs.len()));
    }
    // (3) coarse global solve.
    let est = radar::solve_global_radar(&obs, None).ok_or("radar: solver failed")?.point;
    // (4) standard sampling rings around the estimate.
    for point in radar::standard_sample_points(est) {
        match radar_probe(client, ep, id, point, device_id, beacon).await {
            ProbeOutcome::InRange => return radar_confirm(client, ep, id, user_no).await,
            ProbeOutcome::Distance(d) => obs.push(Observation { point, distance: d }),
            ProbeOutcome::NoInfo => {}
        }
    }
    // (5) refine with the fuller observation set and submit the estimate itself.
    let est2 = radar::solve_global_radar(&obs, Some(est)).ok_or("radar: refine failed")?.point;
    match radar_probe(client, ep, id, est2, device_id, beacon).await {
        ProbeOutcome::InRange => radar_confirm(client, ep, id, user_no).await,
        _ => Err("radar: estimate not within scope (supplement/grid deferred to R2.5)".into()),
    }
}

async fn radar_confirm(client: &Client, ep: &Endpoints, id: &str, user_no: &str) -> Result<SignOutcome, String> {
    if recheck_on_call_fine(client, ep, id, user_no).await {
        Ok(SignOutcome { method: "radar(solved)".into(), ..Default::default() })
    } else {
        Err("radar: hit but on_call_fine not set".into())
    }
}

/// `lite` (docs/70 §1): carries NO target coordinate — only `use_beacon` + `beacon_nonce`.
async fn radar_lite(client: &Client, ep: &Endpoints, id: &str) -> (bool, String) {
    let v: Value = match client.get(ep.lite(id)).send().await {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    (
        v.get("use_beacon").and_then(Value::as_bool).unwrap_or(false),
        v.get("beacon_nonce").and_then(Value::as_str).unwrap_or("").to_string(),
    )
}

enum ProbeOutcome {
    InRange,
    Distance(f64),
    NoInfo,
}

/// Submit one coordinate answer (docs/70 §1 body) and classify the response.
async fn radar_probe(client: &Client, ep: &Endpoints, id: &str, point: GeoPoint, device_id: &str, beacon: Option<(&str, &str)>) -> ProbeOutcome {
    let mut body = json!({
        "deviceId": device_id, "latitude": point.lat, "longitude": point.lon,
        "accuracy": 60, "speed": null, "heading": null, "altitude": 0, "altitudeAccuracy": null
    });
    if let Some((nonce, uid)) = beacon {
        body["radarSignal"] = Value::String(radar_signature(nonce, device_id, uid, now_unix()));
    }
    let resp = match client.put(ep.answer_radar_coord(id)).json(&body).send().await {
        Ok(r) => r,
        Err(_) => return ProbeOutcome::NoInfo,
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_scope(status, &text)
}

/// docs/70 §1 distance extraction — **error_code first, regardless of status** (a real server returns
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

/// Nested walk (docs/70 §1): body; if dict, descend keys `data,result,error,errors,scope,rollcall`;
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
        Value::Array(a) => a.iter().take(3).for_each(|item| walk_dicts(item, out, depth + 1)),
        _ => {}
    }
}
fn has_scope_error(v: &Value) -> bool {
    let mut dicts = Vec::new();
    walk_dicts(v, &mut dicts, 0);
    dicts.iter().any(|m| m.get("error_code").and_then(Value::as_str) == Some("radar_out_of_rollcall_scope"))
}
fn extract_distance(v: &Value) -> Option<f64> {
    let mut dicts = Vec::new();
    walk_dicts(v, &mut dicts, 0);
    for m in dicts {
        for key in ["distance", "scope_distance", "distance_meters", "distanceMeters"] {
            if let Some(d) = m.get(key).and_then(Value::as_f64) {
                return Some(d);
            }
        }
    }
    None
}

/// beacon signature (docs/70 §1): `md5(nonce+deviceId+userId+ts) + "," + ts`. ts = unix seconds.
fn radar_signature(nonce: &str, device_id: &str, user_no: &str, ts: u64) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(format!("{nonce}{device_id}{user_no}{ts}"));
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex},{ts}")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// self_registration: empty body, the simplest type.
pub async fn sign_self_registration(client: &Client, ep: &Endpoints, id: &str, user_no: &str) -> Result<SignOutcome, String> {
    let resp = client
        .put(ep.answer_self_registration(id))
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("self_registration: {e}"))?;
    let (status, rurl) = (resp.status().as_u16(), resp.url().clone());
    let body = resp.text().await.unwrap_or_default();
    if response_auth_lost(status, &rurl, &body) {
        return Err(format!("self_registration: {SESSION_INVALID}"));
    }
    if recheck_on_call_fine(client, ep, id, user_no).await {
        Ok(SignOutcome { method: "self_registration".into(), ..Default::default() })
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
    student
        .put(ep.answer_qr(student_rollcall_id))
        .json(&json!({ "deviceId": device_id, "data": data }))
        .send()
        .await
        .map_err(|e| format!("qr: {e}"))?;
    if recheck_on_call_fine(student, ep, student_rollcall_id, user_no).await {
        Ok(SignOutcome { method: "qr(teacher-assist)".into(), ..Default::default() })
    } else {
        Err("qr submitted but on_call_fine not set".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_code_read_from_every_observed_shape() {
        // The real 55379 roster: the code sits at the TOP LEVEL, NOT inside student_rollcalls[] — the
        // old code only looked in the array and so returned None on the live server (this bug).
        let live = json!({"is_number": true, "number_code": "1234", "status": "in_progress",
                          "student_rollcalls": [{"user_no": "a@b", "rollcall_status": "absent"}]});
        assert_eq!(number_code_from_payload(&live).as_deref(), Some("1234"));

        // int number_code → zero-padded 4-digit (a common contract variant).
        assert_eq!(number_code_from_payload(&json!({"number_code": 123})).as_deref(), Some("0123"));
        // data-wrapped, array container, and a bare list of student items.
        assert_eq!(number_code_from_payload(&json!({"data": {"number_code": "0007"}})).as_deref(), Some("0007"));
        assert_eq!(number_code_from_payload(&json!({"student_rollcalls": [{"number_code": "4321"}]})).as_deref(), Some("4321"));
        assert_eq!(number_code_from_payload(&json!([{"number_code": 42}])).as_deref(), Some("0042"));
    }

    #[test]
    fn number_code_rejects_non_codes() {
        assert_eq!(number_code_from_payload(&json!({"status": "in_progress"})), None); // no code field
        assert_eq!(coerce_number_code(&json!("not-a-code")), None);
        assert_eq!(coerce_number_code(&json!(true)), None); // bool is not a code
        assert_eq!(coerce_number_code(&json!("12")), None); // must be 4 digits
        assert_eq!(coerce_number_code(&json!("１２３４")), None); // full-width digits are not ascii
    }

    #[test]
    fn classify_number_2xx_defaults_to_success() {
        // A real accept is `{"status":"on_call"}` with no success bool → Success (docs 30).
        assert_eq!(classify_response(200, r#"{"id":1,"status":"on_call"}"#), CodeResult::Success);
        assert_eq!(classify_response(200, ""), CodeResult::Success);
        assert_eq!(classify_response(400, "wrong code"), CodeResult::Wrong);
        assert_eq!(classify_response(403, ""), CodeResult::Fatal);
        assert_eq!(classify_response(429, ""), CodeResult::Transient);
    }
}

/// Teacher side: read the rotating `data` from the teacher's OWN qr rollcall (the data source).
pub async fn teacher_source_qr_data(
    teacher: &Client,
    ep: &Endpoints,
    course_id: &str,
    teacher_rollcall_id: &str,
) -> Option<String> {
    let v: Value = teacher
        .get(ep.teacher_qr_code(course_id, teacher_rollcall_id))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("data").and_then(|d| d.as_str().map(str::to_string))
}
