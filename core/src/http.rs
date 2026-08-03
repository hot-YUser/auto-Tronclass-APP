//! Shared HTTP response checks for TronClass API boundaries.
//!
//! Transport success is not application success: every mutating request must reject non-2xx
//! responses before its caller can emit a success event. Error bodies are deliberately omitted from
//! returned messages because tenant responses can contain account or course data.

use reqwest::{RequestBuilder, Response};
use serde_json::Value;

pub async fn send_checked(request: RequestBuilder, operation: &str) -> Result<Response, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{operation}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{operation}: HTTP {}", status.as_u16()));
    }
    Ok(response)
}

pub async fn json_checked(request: RequestBuilder, operation: &str) -> Result<Value, String> {
    send_checked(request, operation)
        .await?
        .json()
        .await
        .map_err(|error| format!("{operation}: invalid JSON response: {error}"))
}
