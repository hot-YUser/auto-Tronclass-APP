//! Teacher-side QR rollcall source lifecycle.
//!
//! The teacher rollcall only supplies the rotating `data` value. Student submission always uses the
//! student's own client, endpoint set, rollcall id, and device id (see `rollcall::sign_qr_with_teacher_data`
//! and `monitor::sign_qr_student`). Contract ported from the proven Python v1 teacher flow
//! (`build_teacher_rollcall_payload(kind="qr")`); still needs a real teacher tenant to fully verify.

use crate::providers::Endpoints;
use crate::rollcall;
use reqwest::{Client, Response};
use serde_json::{json, Value};

/// The token lives only ~1–4 s, so re-source + re-send for this long before giving up on a student.
pub const CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(12);
/// Cadence between re-sourcing a fresh token and re-sending it.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
/// Cap the concurrent student PUTs per token so a big roster can't burst-hammer one tenant.
pub const FANOUT_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    AuthLost,
    Transient,
    Fatal,
}

#[derive(Debug)]
pub struct QrError {
    pub kind: FailureKind,
    operation: &'static str,
}

impl QrError {
    fn new(kind: FailureKind, operation: &'static str) -> Self {
        Self { kind, operation }
    }
}

impl std::fmt::Display for QrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "teacher QR {} failed ({:?})", self.operation, self.kind)
    }
}

pub struct Source {
    pub course_id: String,
    pub rollcall_id: String,
}

/// Python v1 `build_teacher_rollcall_payload(kind="qr")` contract. The old v2 placeholder body used
/// `latitude/longitude/altitude: 0.0`, `number_code: null`, `duration: 3600` — this aligns with the
/// values the working Python teacher flow actually sends.
pub fn build_create_payload(title: &str) -> Value {
    json!({
        "title": if title.trim().is_empty() { "Auto QR" } else { title.trim() },
        "status": "in_progress",
        "is_radar": false,
        "is_number": false,
        "type": "qr_rollcall",
        "number_code": "",
        "altitude": null,
        "latitude": null,
        "longitude": null,
        "use_beacon": false,
        "duration": 0,
        "student_rollcalls": []
    })
}

fn scalar_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn extract_teacher_rollcall_id(payload: &Value) -> Option<String> {
    let obj = payload.as_object()?;
    for key in ["id", "rollcall_id", "rollcallId"] {
        if let Some(id) = obj.get(key).and_then(scalar_id) {
            return Some(id);
        }
    }
    for key in ["rollcall", "data"] {
        if let Some(id) = obj.get(key).and_then(extract_teacher_rollcall_id) {
            return Some(id);
        }
    }
    None
}

pub fn extract_course_items(payload: &Value) -> Vec<Value> {
    if let Some(items) = payload.as_array() {
        return items.clone();
    }
    let Some(obj) = payload.as_object() else {
        return Vec::new();
    };
    for key in ["courses", "items"] {
        if let Some(items) = obj.get(key).and_then(Value::as_array) {
            return items.clone();
        }
    }
    match obj.get("data") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(data)) => ["courses", "items"]
            .iter()
            .find_map(|key| data.get(*key).and_then(Value::as_array).cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn extract_course_id(item: &Value) -> Option<String> {
    let obj = item.as_object()?;
    ["id", "course_id", "courseId"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(scalar_id))
}

pub fn extract_teacher_qr_data(payload: &Value) -> Option<String> {
    let obj = payload.as_object()?;
    if let Some(data) = obj.get("data").and_then(Value::as_str) {
        return (!data.is_empty()).then(|| data.to_string());
    }
    for outer in ["data", "result"] {
        if let Some(data) = obj
            .get(outer)
            .and_then(Value::as_object)
            .and_then(|v| v.get("data"))
            .and_then(Value::as_str)
        {
            if !data.is_empty() {
                return Some(data.to_string());
            }
        }
    }
    None
}

pub fn classify_http_response(
    status: u16,
    url: &reqwest::Url,
    body: &str,
) -> Result<(), FailureKind> {
    if rollcall::response_auth_lost(status, url, body) {
        return Err(FailureKind::AuthLost);
    }
    if matches!(status, 408 | 425 | 429) || (500..600).contains(&status) {
        return Err(FailureKind::Transient);
    }
    if !(200..300).contains(&status) {
        return Err(FailureKind::Fatal);
    }
    Ok(())
}

async fn response_body(
    resp: Result<Response, reqwest::Error>,
    operation: &'static str,
) -> Result<String, QrError> {
    let resp = resp.map_err(|_| QrError::new(FailureKind::Transient, operation))?;
    let status = resp.status().as_u16();
    let url = resp.url().clone();
    let body =
        match crate::http::read_bounded(resp, crate::http::MAX_MUTATION_BODY, operation).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => return Err(QrError::new(FailureKind::Transient, operation)),
        };
    classify_http_response(status, &url, &body).map_err(|kind| QrError::new(kind, operation))?;
    Ok(body)
}

async fn response_json(
    resp: Result<Response, reqwest::Error>,
    operation: &'static str,
) -> Result<Value, QrError> {
    let body = response_body(resp, operation).await?;
    serde_json::from_str(body.trim()).map_err(|_| QrError::new(FailureKind::Fatal, operation))
}

pub async fn resolve_course_id(
    client: &Client,
    ep: &Endpoints,
    configured: Option<&str>,
) -> Result<String, QrError> {
    if let Some(course) = configured
        .map(str::trim)
        .filter(|course| !course.is_empty())
    {
        return Ok(course.to_string());
    }
    let payload = response_json(client.get(ep.my_courses()).send().await, "list-courses").await?;
    extract_course_items(&payload)
        .iter()
        .find_map(extract_course_id)
        .ok_or_else(|| QrError::new(FailureKind::Fatal, "select-course"))
}

pub async fn create(client: &Client, ep: &Endpoints, course_id: &str) -> Result<Source, QrError> {
    let payload = response_json(
        client
            .post(ep.teacher_create_rollcall(course_id))
            .json(&build_create_payload("Auto QR"))
            .send()
            .await,
        "create",
    )
    .await?;
    let rollcall_id = extract_teacher_rollcall_id(&payload)
        .ok_or_else(|| QrError::new(FailureKind::Fatal, "create-id"))?;
    Ok(Source {
        course_id: course_id.to_string(),
        rollcall_id,
    })
}

pub async fn start(client: &Client, ep: &Endpoints, source: &Source) -> Result<(), QrError> {
    response_body(
        client
            .post(ep.teacher_start_rollcall(&source.rollcall_id))
            .send()
            .await,
        "start",
    )
    .await
    .map(|_| ())
}

pub async fn fetch_data(
    client: &Client,
    ep: &Endpoints,
    source: &Source,
) -> Result<String, QrError> {
    let payload = response_json(
        client
            .get(ep.teacher_qr_code(&source.course_id, &source.rollcall_id))
            .send()
            .await,
        "fetch-token",
    )
    .await?;
    extract_teacher_qr_data(&payload)
        .ok_or_else(|| QrError::new(FailureKind::Fatal, "fetch-token-data"))
}

pub async fn stop(client: &Client, ep: &Endpoints, source: &Source) -> Result<(), QrError> {
    response_body(
        client
            .put(ep.teacher_stop_qr(&source.rollcall_id))
            .send()
            .await,
        "stop",
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_payload_matches_python_contract() {
        let payload = build_create_payload("");
        assert_eq!(payload["type"], "qr_rollcall");
        assert_eq!(payload["is_radar"], false);
        assert_eq!(payload["is_number"], false);
        assert_eq!(payload["number_code"], "");
        assert!(
            payload["latitude"].is_null()
                && payload["longitude"].is_null()
                && payload["altitude"].is_null()
        );
        assert_eq!(payload["duration"], 0);
        assert_eq!(payload["student_rollcalls"], json!([]));
        assert!(!payload["title"].as_str().unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn oversized_bodies_fail_transient_not_buffered() {
        // A lying Content-Length (far above the 8 MiB mutation cap): every teacher response must
        // fail with the read-failure kind (Transient) instead of buffering the whole lie — the
        // same bounded-read policy as every other response.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n{\"courses\":[]}";
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let ep = Endpoints::derive(&format!("http://{address}"));
        let err = resolve_course_id(&Client::new(), &ep, None)
            .await
            .expect_err("an oversized body must not be buffered");
        assert_eq!(err.kind, FailureKind::Transient);
    }

    #[test]
    fn response_parsers_accept_observed_shapes() {
        assert_eq!(
            extract_teacher_rollcall_id(&json!({"id": 123})).as_deref(),
            Some("123")
        );
        assert_eq!(
            extract_teacher_rollcall_id(&json!({"data":{"rollcallId":456}})).as_deref(),
            Some("456")
        );
        assert_eq!(
            extract_teacher_rollcall_id(&json!({"rollcall":{"rollcall_id":"r"}})).as_deref(),
            Some("r")
        );
        assert_eq!(
            extract_course_items(&json!({"data":{"items":[{"courseId":9}]}})).len(),
            1
        );
        assert_eq!(
            extract_course_id(&json!({"course_id": 9})).as_deref(),
            Some("9")
        );
        assert_eq!(
            extract_teacher_qr_data(&json!({"result":{"data":"opaque"}})).as_deref(),
            Some("opaque")
        );
    }
}
