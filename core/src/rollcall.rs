//! The four rollcall types (docs 30). `classify` is pure and unit-tested; the per-type answer +
//! `on_call_fine` recheck are async over a single account's session. Each account signs for itself
//! with its own device id; shared computation (number code, radar solve) is done once by the caller.

use crate::providers::Endpoints;
use crate::radar::{self, Observation};
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

/// Whole-class attendance rate (percent) for the 15% gate — `GET /api/rollcall/{id}/answers`.
pub async fn attendance_rate(client: &Client, ep: &Endpoints, id: &str) -> Option<f64> {
    let v: Value = client.get(ep.answers(id)).send().await.ok()?.json().await.ok()?;
    v.get("attendance_rate")
        .and_then(Value::as_f64)
        .or_else(|| v.pointer("/attendance/rate").and_then(Value::as_f64))
}

/// Read the shared number code once (`GET student_rollcalls`). None → caller brute-forces.
pub async fn read_number_code(client: &Client, ep: &Endpoints, id: &str) -> Option<String> {
    let v: Value = client.get(ep.student_rollcalls(id)).send().await.ok()?.json().await.ok()?;
    v.get("number_code")
        .and_then(|c| c.as_str().map(str::to_string))
        .or_else(|| v.pointer("/0/number_code").and_then(|c| c.as_str().map(str::to_string)))
}

/// Confirm the account is actually marked present (`on_call_fine`) — the rule after every sign.
pub async fn recheck_on_call_fine(client: &Client, ep: &Endpoints, id: &str) -> bool {
    let Ok(resp) = client.get(ep.student_rollcalls(id)).send().await else { return false };
    let Ok(v) = resp.json::<Value>().await else { return false };
    if let Some(b) = v.get("on_call_fine").and_then(Value::as_bool) {
        return b;
    }
    v.as_array()
        .map(|a| a.iter().any(|e| e.get("on_call_fine").and_then(Value::as_bool) == Some(true)))
        .unwrap_or(false)
}

/// number: submit the code (shared) or brute-force it. Returns the code it used so it can be shared.
/// `concurrency`/`cooldown_ms` tune the brute-force fallback (docs 30 tuning knobs).
pub async fn sign_number(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    device_id: &str,
    code: Option<&str>,
    concurrency: u32,
    cooldown_ms: u64,
) -> Result<SignOutcome, String> {
    if let Some(code) = code {
        submit_number(client, ep, id, device_id, code).await?;
        if recheck_on_call_fine(client, ep, id).await {
            return Ok(SignOutcome { method: "number".into(), discovered_code: Some(code.to_string()) });
        }
        return Err("number submitted but on_call_fine not set".into());
    }
    brute_force_number(client, ep, id, device_id, concurrency, cooldown_ms).await
}

async fn submit_number(client: &Client, ep: &Endpoints, id: &str, device_id: &str, code: &str) -> Result<(), String> {
    client
        .put(ep.answer_number(id))
        .json(&json!({ "deviceId": device_id, "numberCode": code }))
        .send()
        .await
        .map_err(|e| format!("number: {e}"))?;
    Ok(())
}

// ponytail: bounded 0000–9999, `concurrency` codes in flight per batch, exponential 429 backoff with
// a configurable floor. `concurrency == 1` (default) is the sequential path with a reliable
// `discovered_code` to share; widen only if a real tenant makes it too slow. `cooldown_ms` is the
// backoff floor. A transport error on one code is swallowed (the sweep continues) — a rare fallback.
async fn brute_force_number(
    client: &Client,
    ep: &Endpoints,
    id: &str,
    device_id: &str,
    concurrency: u32,
    cooldown_ms: u64,
) -> Result<SignOutcome, String> {
    let floor = cooldown_ms.max(1);
    let width = concurrency.clamp(1, 64);
    let mut delay_ms = 0u64;
    let mut n: u32 = 0;
    while n <= 9999 {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let batch_start = n;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..width {
            if n > 9999 {
                break;
            }
            let code = format!("{n:04}");
            n += 1;
            let (client, url, device_id) = (client.clone(), ep.answer_number(id), device_id.to_string());
            set.spawn(async move {
                client
                    .put(&url)
                    .json(&json!({ "deviceId": device_id, "numberCode": code }))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
                    .unwrap_or(0) // transport error → treat as non-429, keep sweeping
            });
        }
        let mut throttled = false;
        while let Some(res) = set.join_next().await {
            if matches!(res, Ok(429)) {
                throttled = true;
            }
        }
        if throttled {
            delay_ms = (delay_ms * 2).clamp(floor, 5000); // back off, don't give up
            n = batch_start; // retry the throttled batch
            continue;
        }
        delay_ms = 0;
        if recheck_on_call_fine(client, ep, id).await {
            // Only the sequential path can attribute the winning code to share it.
            let (method, discovered) = if width == 1 {
                ("number(brute)", Some(format!("{batch_start:04}")))
            } else {
                ("number(brute-parallel)", None)
            };
            return Ok(SignOutcome { method: method.into(), discovered_code: discovered });
        }
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
) -> Result<SignOutcome, String> {
    let mut last_err = String::from("radar: no strategy in chain succeeded");
    for strat in strategies {
        match strat.as_str() {
            "empty_answer" => {
                client
                    .put(ep.answer_radar(id))
                    .json(&json!({}))
                    .send()
                    .await
                    .map_err(|e| format!("radar: {e}"))?;
                if recheck_on_call_fine(client, ep, id).await {
                    return Ok(SignOutcome { method: "radar(empty)".into(), ..Default::default() });
                }
                last_err = "radar empty answer did not mark present".into();
            }
            "global_wgs84" => match radar_multilaterate(client, ep, id).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) => last_err = e,
            },
            other => last_err = format!("radar: unknown strategy '{other}' skipped"),
        }
    }
    Err(last_err)
}

/// `global_wgs84`: probe a few spread coordinates, read the returned distance, solve on the WGS84
/// ellipsoid (`radar::solve`), resubmit the solved point, and confirm.
async fn radar_multilaterate(client: &Client, ep: &Endpoints, id: &str) -> Result<SignOutcome, String> {
    let center = radar_center(client, ep, id).await.unwrap_or((25.0, 121.5));
    let probes = spread(center);
    let mut obs = Vec::new();
    for (lat, lon) in probes {
        if let Some(dist) = probe_distance(client, ep, id, lat, lon).await {
            obs.push(Observation { lat, lon, dist_m: dist });
        }
    }
    let (tlat, tlon) = radar::solve(&obs).ok_or("radar: could not multilaterate")?;
    client
        .put(ep.answer_radar(id))
        .json(&json!({ "lat": tlat, "lng": tlon }))
        .send()
        .await
        .map_err(|e| format!("radar resubmit: {e}"))?;
    if recheck_on_call_fine(client, ep, id).await {
        Ok(SignOutcome { method: "radar(solved)".into(), ..Default::default() })
    } else {
        Err("radar solved but on_call_fine not set".into())
    }
}

async fn radar_center(client: &Client, ep: &Endpoints, id: &str) -> Option<(f64, f64)> {
    let v: Value = client.get(ep.lite(id)).send().await.ok()?.json().await.ok()?;
    Some((v.get("lat")?.as_f64()?, v.get("lng").or(v.get("lon"))?.as_f64()?))
}

/// Four spread points ~200 m around a center — non-collinear so the solver is well-posed.
fn spread((lat, lon): (f64, f64)) -> [(f64, f64); 4] {
    let d = 0.002; // ~200 m
    [(lat + d, lon), (lat - d, lon + d), (lat, lon - d), (lat + d, lon + d)]
}

async fn probe_distance(client: &Client, ep: &Endpoints, id: &str, lat: f64, lon: f64) -> Option<f64> {
    let v: Value = client
        .put(ep.answer_radar(id))
        .json(&json!({ "lat": lat, "lng": lon }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("distance").and_then(Value::as_f64)
}

/// self_registration: empty body, the simplest type.
pub async fn sign_self_registration(client: &Client, ep: &Endpoints, id: &str) -> Result<SignOutcome, String> {
    client
        .put(ep.answer_self_registration(id))
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("self_registration: {e}"))?;
    if recheck_on_call_fine(client, ep, id).await {
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
) -> Result<SignOutcome, String> {
    student
        .put(ep.answer_qr(student_rollcall_id))
        .json(&json!({ "deviceId": device_id, "data": data }))
        .send()
        .await
        .map_err(|e| format!("qr: {e}"))?;
    if recheck_on_call_fine(student, ep, student_rollcall_id).await {
        Ok(SignOutcome { method: "qr(teacher-assist)".into(), ..Default::default() })
    } else {
        Err("qr submitted but on_call_fine not set".into())
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
