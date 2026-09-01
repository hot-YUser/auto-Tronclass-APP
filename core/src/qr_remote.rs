//! QR remote data source (VPS `/token`). Read-only fetch before any student mutation.
//! Dedicated client: redirects disabled, no cookies, no proxy, 8 s timeout, 64 KiB cap.

use reqwest::{redirect::Policy, Client};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use zeroize::Zeroizing;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(12);
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const RESPONSE_CAP: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Unauthorized,
    RateLimited(Duration),
    Redirect,
    Transient(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "qr remote unauthorized"),
            Self::RateLimited(d) => write!(f, "qr remote rate limited {}s", d.as_secs()),
            Self::Redirect => write!(f, "qr remote redirect rejected"),
            Self::Transient(msg) => write!(f, "qr remote transient: {msg}"),
        }
    }
}

pub fn is_valid_token(data: &str) -> bool {
    if data.len() != 42 {
        return false;
    }
    let bytes = data.as_bytes();
    if !bytes[..10].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if !bytes[10..]
        .iter()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return false;
    }
    data.is_ascii()
}

static QR_CLIENT: OnceLock<Client> = OnceLock::new();

pub fn qr_client() -> &'static Client {
    QR_CLIENT.get_or_init(|| {
        Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .cookie_store(false)
            .timeout(REQUEST_TIMEOUT)
            .connection_verbose(false)
            .build()
            .expect("qr remote client")
    })
}

#[cfg_attr(not(test), expect(dead_code))]
pub fn build_client() -> Result<Client, String> {
    Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .cookie_store(false)
        .timeout(REQUEST_TIMEOUT)
        .connection_verbose(false)
        .build()
        .map_err(|e| format!("qr remote client: {e}"))
}

pub fn token_url(normalized_base: &str) -> String {
    format!("{normalized_base}/token")
}

pub(crate) fn parse_retry_after(value: Option<&str>) -> Duration {
    let Some(text) = value else {
        return Duration::from_secs(1);
    };
    let trimmed = text.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        let capped = secs.clamp(1, 120);
        return Duration::from_secs(capped);
    }
    Duration::from_secs(1)
}

pub async fn fetch_token(
    client: &Client,
    normalized_base: &str,
    api_key: &str,
) -> Result<String, FetchError> {
    let url = token_url(normalized_base);
    // Avoid long-lived plain key copy: transient Bearer buffer is zeroizing and erased promptly.
    let mut auth = Zeroizing::new(format!("Bearer {api_key}"));
    // HeaderValue copies the bytes; reqwest then copies into the request — we drop our buffer ASAP.
    let header_val = reqwest::header::HeaderValue::from_str(auth.as_str())
        .map_err(|_| FetchError::Transient("invalid key shape".into()))?;
    // Erase our Bearer buffer before the request is sent; reqwest holds its own copy in the header.
    {
        use zeroize::Zeroize;
        auth.zeroize();
    }
    let _ = auth;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", header_val)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                FetchError::Transient("timeout".into())
            } else if e.is_connect() {
                FetchError::Transient(format!("connect: {e}"))
            } else {
                FetchError::Transient(format!("send: {e}"))
            }
        })?;

    let status = resp.status().as_u16();
    if (300..400).contains(&status) {
        return Err(FetchError::Redirect);
    }
    if status == 401 || status == 403 {
        return Err(FetchError::Unauthorized);
    }
    if status == 429 {
        let retry = resp
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok());
        let delay = parse_retry_after(retry);
        return Err(FetchError::RateLimited(delay));
    }
    if status == 503 {
        let retry = resp
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok());
        let delay = parse_retry_after(retry);
        let body = read_capped(resp).await?;
        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
            if let Some(err) = value.get("error").and_then(Value::as_str) {
                let norm = err.trim().to_ascii_lowercase();
                if norm == "busy" {
                    return Err(FetchError::RateLimited(delay));
                }
                if norm == "stale" || norm == "no_data" {
                    return Err(FetchError::Transient(norm));
                }
            }
            return Err(FetchError::Transient("503 transient".into()));
        }
        return Err(FetchError::Transient("503 transient".into()));
    }
    if status != 200 {
        let _ = read_capped(resp).await;
        return Err(FetchError::Transient(format!("http {status}")));
    }

    let body = read_capped(resp).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| FetchError::Transient("malformed json".into()))?;
    if !value.is_object() {
        return Err(FetchError::Transient("non-object".into()));
    }
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        return Err(FetchError::Transient("ok false".into()));
    }
    let data = value
        .get("data")
        .and_then(Value::as_str)
        .ok_or(FetchError::Transient("missing data".into()))?;
    if !is_valid_token(data) {
        return Err(FetchError::Transient("invalid token shape".into()));
    }
    Ok(data.to_string())
}

async fn read_capped(resp: reqwest::Response) -> Result<Vec<u8>, FetchError> {
    crate::http::read_bounded(resp, RESPONSE_CAP, "qr remote")
        .await
        .map_err(FetchError::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape_strict() {
        let good = format!("{}{}", "0123456789", "a".repeat(32));
        assert_eq!(good.len(), 42);
        assert!(is_valid_token(&good));
        assert!(!is_valid_token("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_valid_token("0123456789abcdef0123456789abcde"));
        assert!(!is_valid_token("0123456789abcdef0123456789abcdef012345678"));
        assert!(!is_valid_token("abcdefghijabcdef0123456789abcdef01234567"));
        assert!(!is_valid_token("0123456789abcdef0123456789abcdef0123456G"));
    }

    #[test]
    fn token_url_no_double_slash() {
        assert_eq!(
            token_url("https://example.com"),
            "https://example.com/token"
        );
        assert_eq!(
            token_url("https://example.com/api"),
            "https://example.com/api/token"
        );
    }

    #[test]
    fn retry_after_clamp() {
        assert_eq!(parse_retry_after(Some("5")).as_secs(), 5);
        assert_eq!(parse_retry_after(Some("0")).as_secs(), 1);
        assert_eq!(parse_retry_after(Some("1")).as_secs(), 1);
        assert_eq!(parse_retry_after(Some("120")).as_secs(), 120);
        assert_eq!(parse_retry_after(Some("999")).as_secs(), 120);
        assert_eq!(parse_retry_after(Some("abc")).as_secs(), 1);
        assert_eq!(parse_retry_after(None).as_secs(), 1);
    }

    fn valid_token() -> String {
        format!("{}{}", "0123456789", "a".repeat(32))
    }

    #[tokio::test]
    async fn fetch_success_and_strict_shape() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token = valid_token();
        let t2 = token.clone();
        tokio::spawn(async move {
            for _ in 0..1 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf)
                    .await
                    .unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(text
                    .to_ascii_lowercase()
                    .contains("accept: application/json"));
                assert!(text.to_ascii_lowercase().contains("authorization: bearer"));
                assert!(!text.to_ascii_lowercase().contains("cookie:"));
                let body = format!(r#"{{"ok":true,"data":"{t2}"}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let client = build_client().unwrap();
        let base = format!("http://{addr}");
        let got = fetch_token(&client, &base, "test-key").await.unwrap();
        assert_eq!(got, token);
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener2.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let body = r#"{"ok":true,"data":"0123456789ABCDEF0123456789ABCDEF01234567"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                .await
                .unwrap();
        });
        let base2 = format!("http://{addr2}");
        let err = fetch_token(&client, &base2, "k").await.unwrap_err();
        assert!(matches!(err, FetchError::Transient(_)));
    }

    #[tokio::test]
    async fn fetch_401_403_terminal_and_redirect_rejected() {
        for code in [401u16, 403u16] {
            let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a1 = l1.local_addr().unwrap();
            let c = code;
            tokio::spawn(async move {
                let (mut s, _) = l1.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let resp =
                    format!("HTTP/1.1 {c} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            });
            let client = build_client().unwrap();
            assert!(
                matches!(
                    fetch_token(&client, &format!("http://{a1}"), "k")
                        .await
                        .unwrap_err(),
                    FetchError::Unauthorized
                ),
                "code {code} must be terminal"
            );
        }
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l2.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                .await
                .unwrap();
        });
        let client = build_client().unwrap();
        assert!(matches!(
            fetch_token(&client, &format!("http://{a2}"), "k")
                .await
                .unwrap_err(),
            FetchError::Redirect
        ));
    }

    #[tokio::test]
    async fn fetch_429_503_exact_with_cap_and_malformed_oversize() {
        let client = build_client().unwrap();
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a1 = l1.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l1.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 999\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                .await
                .unwrap();
        });
        match fetch_token(&client, &format!("http://{a1}"), "k")
            .await
            .unwrap_err()
        {
            FetchError::RateLimited(d) => assert_eq!(d.as_secs(), 120),
            other => panic!("429 must be rate_limited, got {other:?}"),
        }
        assert_eq!(parse_retry_after(Some("0")).as_secs(), 1);
        for (err_str, expect_rate) in [("busy", true), ("stale", false), ("no_data", false)] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            let body = format!(r#"{{"error":"{err_str}"}}"#);
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let resp = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            });
            let err = fetch_token(&client, &format!("http://{a}"), "k")
                .await
                .unwrap_err();
            if expect_rate {
                assert!(
                    matches!(err, FetchError::RateLimited(_)),
                    "busy must be rate_limited, got {err:?} for {err_str}"
                );
            } else {
                assert!(
                    matches!(err, FetchError::Transient(_)),
                    "{err_str} must be transient, got {err:?}"
                );
            }
        }
        for bad in ["busy extra", "my busy", "stale_data", "no_data_extra"] {
            if bad.trim().eq_ignore_ascii_case("busy") {
                continue;
            }
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            let body = format!(r#"{{"error":"{bad}"}}"#);
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let resp = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            });
            let err = fetch_token(&client, &format!("http://{a}"), "k")
                .await
                .unwrap_err();
            assert!(
                matches!(err, FetchError::Transient(_)),
                "contains prefix {bad} must NOT be rate_limited, got {err:?}"
            );
        }
        for good in [" busy ", "BUSY", " Busy "] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            let body = format!(r#"{{"error":"{good}"}}"#);
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let resp = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            });
            let err = fetch_token(&client, &format!("http://{a}"), "k")
                .await
                .unwrap_err();
            assert!(
                matches!(err, FetchError::RateLimited(_)),
                "trimmed case-insensitive busy '{good}' must be rate_limited"
            );
        }
        for body in [
            r#" not json "#,
            r#"["array"]"#,
            r#"{"ok":false,"data":"x"}"#,
        ] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            let body_owned = body.to_string();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_owned.len(), body_owned
                );
                tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                    .await
                    .unwrap();
            });
            let err = fetch_token(&client, &format!("http://{a}"), "k")
                .await
                .unwrap_err();
            assert!(
                matches!(err, FetchError::Transient(_)),
                "must be transient for body {}",
                body
            );
        }
        let big = "x".repeat(70 * 1024);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                big.len(), big
            );
            tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                .await
                .unwrap();
        });
        assert!(matches!(
            fetch_token(&client, &format!("http://{a}"), "k")
                .await
                .unwrap_err(),
            FetchError::Transient(_)
        ));
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l2.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 999999\r\nConnection: close\r\n\r\n";
            tokio::io::AsyncWriteExt::write_all(&mut s, resp.as_bytes())
                .await
                .unwrap();
        });
        assert!(matches!(
            fetch_token(&client, &format!("http://{a2}"), "k")
                .await
                .unwrap_err(),
            FetchError::Transient(_)
        ));
    }
}
