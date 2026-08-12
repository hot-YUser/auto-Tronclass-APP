//! Shared HTTP response checks for TronClass API boundaries.
//!
//! Transport success is not application success: every mutating request must reject non-2xx
//! responses before its caller can emit a success event. Error bodies are deliberately omitted from
//! returned messages because tenant responses can contain account or course data.
//!
//! Also the shared URL-safety vocabulary: what counts as a login-page redirect (one conservative
//! helper, never whole-URL substring scans) and which hosts are private/link-local/loopback (used by
//! the school client's redirect policy and the untrusted-attachment fetcher).

use reqwest::{RequestBuilder, Response};
use serde_json::Value;

/// Hard cap for general school-API JSON bodies (rollcall/quiz/radar payloads are KBs; a misbehaving
/// tenant must not be able to make the app buffer an unbounded response).
pub const MAX_API_JSON: usize = 32 * 1024 * 1024;
/// Hard cap for mutating-response bodies (submission receipts are tiny).
pub const MAX_MUTATION_BODY: usize = 8 * 1024 * 1024;

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
    let payload = read_bounded_json(response, MAX_MUTATION_BODY, operation).await?;
    if !payload.is_object() {
        return Err(format!("{operation}: response JSON was not an object"));
    }
    if explicit_business_error_value(&payload) {
        return Err(format!("{operation}: server rejected the request"));
    }
    Ok(payload)
}

/// One conservative login-redirect classifier: a path segment that IS a login/sso page (bare
/// `login`/`sso`, or a server-page filename like `Login.aspx`/`login.jsp`). The query string is
/// never inspected — an arbitrary `?next=/login` parameter is NOT a login redirect.
pub fn response_url_is_login(url: &reqwest::Url) -> bool {
    const LOGIN_EXTS: [&str; 7] = ["aspx", "jsp", "php", "do", "action", "htm", "html"];
    url.path().split('/').any(|segment| {
        let lower = segment.to_ascii_lowercase();
        if lower == "login" || lower == "sso" {
            return true;
        }
        let Some((name, ext)) = lower.rsplit_once('.') else {
            return false;
        };
        LOGIN_EXTS.contains(&ext) && (name == "login" || name == "sso")
    })
}

pub fn explicit_business_error(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|payload| explicit_business_error_value(&payload))
}

/// A JSON object that EXPLICITLY says the request failed. `error`/`error_code` values of 0, "0",
/// false, null and empty are SUCCESS (many tenants echo `"error":0` or `"error_code":""` on
/// success); non-zero numbers, true, and non-empty strings other than "0" are failures.
/// `success`/`ok`/`is_success` flags are read as booleans AND the string forms "true"/"false"/"1"/"0".
pub fn explicit_business_error_value(payload: &Value) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    for key in ["success", "ok", "is_success"] {
        if object.get(key).is_some_and(success_flag_is_false) {
            return true;
        }
    }
    if object.get("error_code").is_some_and(error_value_is_failure)
        || object.get("error").is_some_and(error_value_is_failure)
    {
        return true;
    }
    false
}

fn success_flag_is_false(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => !flag,
        Value::String(text) => {
            let text = text.trim().to_ascii_lowercase();
            text == "false" || text == "0"
        }
        _ => false,
    }
}

fn error_value_is_failure(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_i64()
            .map(|n| n != 0)
            .or_else(|| number.as_u64().map(|n| n != 0))
            .unwrap_or(true),
        Value::String(text) => {
            let text = text.trim();
            !text.is_empty() && text != "0"
        }
        _ => true,
    }
}

pub async fn json_checked(request: RequestBuilder, operation: &str) -> Result<Value, String> {
    let response = send_checked(request, operation).await?;
    read_bounded_json(response, MAX_API_JSON, operation).await
}

/// Read a response body with a hard byte cap: Content-Length is pre-checked (a lying/absent header
/// is caught by chunk accumulation). Errors are clear and never echo body content — tenant bodies
/// can carry account or course data.
pub async fn read_bounded(
    response: Response,
    max_bytes: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if let Some(length) = response.content_length() {
        if length > max_bytes as u64 {
            return Err(format!("{what}: response too large ({length} bytes)"));
        }
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > max_bytes {
                    return Err(format!("{what}: response exceeded {max_bytes} bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => return Err(format!("{what}: read failed: {error}")),
        }
    }
    Ok(body)
}

pub async fn read_bounded_json(
    response: Response,
    max_bytes: usize,
    what: &str,
) -> Result<Value, String> {
    let body = read_bounded(response, max_bytes, what).await?;
    serde_json::from_slice(&body).map_err(|_| format!("{what}: response was not JSON"))
}

/// Whether two URLs share scheme, host and effective port — the cross-origin test used by the
/// redirect policy and the untrusted-attachment fetcher.
pub fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// True when `host` (with optional port / IPv6 brackets) is localhost or a LITERAL private,
/// link-local, loopback, CGNAT or unspecified address. Hostnames are never resolved here — a name
/// like `intranet` is not a literal address, so it is not blocked by this check.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    let host = if let Some(bracketed) = host.strip_prefix('[') {
        bracketed
            .split_once(']')
            .map(|(address, _)| address)
            .unwrap_or(host)
    } else if host.parse::<std::net::IpAddr>().is_ok() {
        host
    } else {
        host.rsplit_once(':')
            .filter(|(address, port)| !address.contains(':') && port.parse::<u16>().is_ok())
            .map(|(address, _)| address)
            .unwrap_or(host)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host
        .rsplit_once('.')
        .is_some_and(|(_, last)| last.eq_ignore_ascii_case("localhost"))
    {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => is_private_v4(v4),
        Ok(std::net::IpAddr::V6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local() // fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
        Err(_) => false,
    }
}

fn is_private_v4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    match octets[0] {
        0 => true,                              // 0.0.0.0/8 — "this network" / unspecified
        10 => true,                             // RFC 1918
        100 => (64..=127).contains(&octets[1]), // 100.64.0.0/10 CGNAT
        127 => true,                            // loopback
        169 => octets[1] == 254,                // 169.254.0.0/16 link-local
        172 => (16..=31).contains(&octets[1]),  // RFC 1918
        192 => octets[1] == 168,                // RFC 1918
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::explicit_business_error;
    use super::is_private_host;
    use super::same_origin;

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
        assert!(!explicit_business_error(r#"{"error":0}"#));
        assert!(!explicit_business_error(r#"{"error":"0"}"#));
        assert!(!explicit_business_error(r#"{"error":false}"#));
        assert!(!explicit_business_error(r#"{"success":"true"}"#));
        assert!(explicit_business_error(r#"{"success":"false"}"#));
    }

    #[test]
    fn business_error_boolean_error_fields_follow_true_false() {
        // `error`/`error_code` as booleans: false is a zero-like success sentinel, true is failure.
        assert!(explicit_business_error(r#"{"error":true}"#));
        assert!(explicit_business_error(r#"{"error_code":true}"#));
        assert!(!explicit_business_error(r#"{"error_code":false}"#));
    }

    #[test]
    fn success_flags_parse_string_true_false_and_1_0() {
        assert!(!explicit_business_error(r#"{"success":"1"}"#));
        assert!(explicit_business_error(r#"{"success":"0"}"#));
        assert!(!explicit_business_error(r#"{"ok":"true"}"#));
        assert!(explicit_business_error(r#"{"ok":"false"}"#));
        assert!(!explicit_business_error(r#"{"is_success":true}"#));
        assert!(explicit_business_error(r#"{"is_success":false}"#));
    }

    #[test]
    fn login_redirect_paths_are_detected_case_insensitively() {
        for url in ["https://tenant/sso", "https://tenant/auth/Login?next=x"] {
            assert!(super::response_url_is_login(&url.parse().unwrap()));
        }
        assert!(!super::response_url_is_login(
            &"https://tenant/api/exams".parse().unwrap()
        ));
    }

    #[test]
    fn login_redirect_recognizes_server_pages_but_not_query_strings() {
        for url in [
            "https://tenant/auth/login.aspx",
            "https://tenant/Login.jsp?redirect=/home",
            "https://tenant/login.php",
            "https://tenant/sso/login.do",
        ] {
            assert!(super::response_url_is_login(&url.parse().unwrap()), "{url}");
        }
        // An arbitrary query containing "login" is NOT a login redirect; nor is a path segment that
        // merely STARTS with "login" (login-page, login-help).
        for url in [
            "https://tenant/api/exams?next=/login",
            "https://tenant/api/login-history",
            "https://tenant/login-page",
            "https://tenant/assets/login.js",
        ] {
            assert!(
                !super::response_url_is_login(&url.parse().unwrap()),
                "{url}"
            );
        }
    }

    #[test]
    fn private_host_detection_covers_literals_only() {
        for host in [
            "localhost",
            "localhost.",
            "127.0.0.1",
            "127.8.8.8",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "[::1]",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "127.0.0.1:8080",
            "[::1]:8080",
        ] {
            assert!(is_private_host(host), "{host}");
        }
        for host in [
            "example.com",
            "8.8.8.8",
            "140.112.1.1",
            "172.32.0.1",
            "192.169.1.1",
            "100.128.0.1",
            "2606:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(!is_private_host(host), "{host}");
        }
    }

    #[test]
    fn same_origin_compares_scheme_host_and_effective_port() {
        let a = "https://school.example/api/x".parse().unwrap();
        assert!(same_origin(
            &a,
            &"https://school.example/api/y".parse().unwrap()
        ));
        assert!(same_origin(
            &a,
            &"https://school.example:443/other".parse().unwrap()
        ));
        assert!(!same_origin(
            &a,
            &"http://school.example/api/y".parse().unwrap()
        ));
        assert!(!same_origin(
            &a,
            &"https://cdn.example/api/y".parse().unwrap()
        ));
        assert!(!same_origin(
            &a,
            &"https://school.example:8443/api/y".parse().unwrap()
        ));
    }

    #[tokio::test]
    async fn mutation_checked_rejects_explicit_business_failure_on_2xx() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 2xx transport, but the envelope EXPLICITLY says the mutation failed → must Err.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let body = r#"{"success":false,"error":"denied"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let result = super::mutation_checked(
            client
                .post(format!("http://{address}/submit"))
                .json(&serde_json::json!({})),
            "submit test",
        )
        .await;
        assert!(
            result.is_err(),
            "a 2xx with an explicit business error must never be a success"
        );

        // A 2xx receipt without any error signal IS a success ({"id":…} envelope).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let body = r#"{"submission_id": 42}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let result = super::mutation_checked(
            client
                .post(format!("http://{address}/submit"))
                .json(&serde_json::json!({})),
            "submit test",
        )
        .await;
        let payload =
            result.expect("a 2xx receipt without an explicit business error is a success");
        assert_eq!(payload["submission_id"], 42);
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_content_length_immediately() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            // Claims 100 MB but sends nothing — the precheck must reject before any body read.
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 104857600\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let error = super::read_bounded(response, 1024, "test")
            .await
            .unwrap_err();
        assert!(error.contains("too large"), "{error}");
    }

    #[tokio::test]
    async fn bounded_reader_rejects_chunked_overflow_and_accepts_small_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Chunked (no Content-Length): 10 KiB body against a 1 KiB cap → rejected mid-read.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            let body = "x".repeat(10 * 1024);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let error = super::read_bounded(response, 1024, "test")
            .await
            .unwrap_err();
        assert!(error.contains("exceeded"), "{error}");

        // A body within the cap round-trips intact.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            let body = r#"{"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let parsed = super::read_bounded_json(response, 1024, "test")
            .await
            .unwrap();
        assert_eq!(parsed, serde_json::json!({"ok": true}));
    }
}
