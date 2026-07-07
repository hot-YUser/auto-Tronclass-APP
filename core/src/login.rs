//! Minimal async login handshake. Its only job in the skeleton is to make the seam do a
//! real, cookie-carrying HTTP round-trip on the tokio runtime — proving async-over-FFI.
//!
//! ponytail: this is a single plain-form login path against a cooperating server. Real
//! TronClass login is feature-routed (captcha / SSO / NetIQ / email-SPA) and belongs to
//! slice 1 — do NOT grow it here.

use reqwest::Client;

pub async fn login(base_url: &str, username: &str, password: &str) -> Result<String, String> {
    let base = base_url.trim_end_matches('/');

    let client = Client::builder()
        .cookie_store(true) // carry the session cookie from /login to the verify GET
        .build()
        .map_err(|e| format!("client: {e}"))?;

    let resp = client
        .post(format!("{base}/login"))
        .form(&[("username", username), ("password", password)])
        .send()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("login rejected: HTTP {}", resp.status().as_u16()));
    }

    // Confirm the cookie actually authenticates us against a real endpoint shape.
    let vr = client
        .get(format!("{base}/api/current-semester-info"))
        .send()
        .await
        .map_err(|e| format!("verify: {e}"))?;
    if !vr.status().is_success() {
        return Err(format!("session not valid: HTTP {}", vr.status().as_u16()));
    }

    let body = vr.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(80).collect();
    Ok(format!("session ok; {snippet}"))
}
