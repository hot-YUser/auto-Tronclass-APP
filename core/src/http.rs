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

pub async fn mutation_checked(request: RequestBuilder, operation: &str) -> Result<(), String> {
    let response = send_checked(request, operation).await?;
    let body = response.text().await.map_err(|error| format!("{operation}: invalid response: {error}"))?;
    if explicit_business_error(&body) {
        return Err(format!("{operation}: server rejected the request"));
    }
    Ok(())
}

pub fn explicit_business_error(body: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let Some(object) = payload.as_object() else {
        return false;
    };
    if ["success", "ok", "is_success"]
        .iter()
        .any(|key| object.get(*key).and_then(Value::as_bool) == Some(false))
    {
        return true;
    }
    object.get("error_code").is_some_and(|value| match value {
        Value::Null => false,
        Value::String(code) => !code.trim().is_empty(),
        _ => true,
    })
}

pub async fn json_checked(request: RequestBuilder, operation: &str) -> Result<Value, String> {
    send_checked(request, operation)
        .await?
        .json()
        .await
        .map_err(|error| format!("{operation}: invalid JSON response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::explicit_business_error;

    #[test]
    fn explicit_business_error_rejects_known_error_envelopes() {
        assert!(explicit_business_error(r#"{"success":false}"#));
        assert!(explicit_business_error(r#"{"ok":false}"#));
        assert!(explicit_business_error(r#"{"error_code":"denied"}"#));
        assert!(explicit_business_error(r#"{"error_code":403}"#));
    }

    #[test]
    fn explicit_business_error_allows_success_and_unstructured_bodies() {
        assert!(!explicit_business_error(""));
        assert!(!explicit_business_error("OK"));
        assert!(!explicit_business_error(r#"{"success":true}"#));
        assert!(!explicit_business_error(r#"{"id":"submission-1"}"#));
        assert!(!explicit_business_error(r#"{"error_code":""}"#));
        assert!(!explicit_business_error(r#"{"error_code":null}"#));
    }
}
