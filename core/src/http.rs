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

pub async fn mutation_checked(request: RequestBuilder, operation: &str) -> Result<Value, String> {
    let response = send_checked(request, operation).await?;
    if response_url_is_login(response.url()) {
        return Err(format!("{operation}: session redirected to login"));
    }
    let body = response.text().await.map_err(|error| format!("{operation}: invalid response: {error}"))?;
    let payload = serde_json::from_str::<Value>(&body)
        .map_err(|_| format!("{operation}: response was not JSON"))?;
    if !payload.is_object() {
        return Err(format!("{operation}: response JSON was not an object"));
    }
    if explicit_business_error_value(&payload) {
        return Err(format!("{operation}: server rejected the request"));
    }
    Ok(payload)
}

pub fn response_url_is_login(url: &reqwest::Url) -> bool {
    url.path().split('/').any(|segment| segment.eq_ignore_ascii_case("login") || segment.eq_ignore_ascii_case("sso"))
}

pub fn explicit_business_error(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|payload| explicit_business_error_value(&payload))
}

pub fn explicit_business_error_value(payload: &Value) -> bool {
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
        Value::Number(number) => number.as_i64() != Some(0) && number.as_u64() != Some(0),
        Value::String(code) => !code.trim().is_empty() && code.trim() != "0",
        _ => true,
    }) || object.get("error").is_some_and(|value| match value {
        Value::Null => false,
        Value::String(message) => !message.trim().is_empty(),
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
        assert!(explicit_business_error(r#"{"error":"denied"}"#));
    }

    #[test]
    fn explicit_business_error_allows_explicit_success_and_zero_error_codes() {
        assert!(!explicit_business_error(r#"{"success":true}"#));
        assert!(!explicit_business_error(r#"{"id":"submission-1"}"#));
        assert!(!explicit_business_error(r#"{"error_code":""}"#));
        assert!(!explicit_business_error(r#"{"error_code":"0"}"#));
        assert!(!explicit_business_error(r#"{"error_code":0}"#));
        assert!(!explicit_business_error(r#"{"error_code":null}"#));
    }

    #[test]
    fn login_redirect_paths_are_detected_case_insensitively() {
        for url in ["https://tenant/sso", "https://tenant/auth/Login?next=x"] {
            assert!(super::response_url_is_login(&url.parse().unwrap()));
        }
        assert!(!super::response_url_is_login(&"https://tenant/api/exams".parse().unwrap()));
    }
}
