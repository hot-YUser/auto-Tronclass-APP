//! A minimal fake TronClass server — the offline test rig mandated by docs 90 §10, and
//! the target the walking-skeleton UI logs into. Hand-rolled over tokio (no wiremock/hyper):
//! it answers exactly two routes and closes each connection, which is all the seam needs.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const GOOD_USER: &str = "test";
pub const GOOD_PASS: &str = "secret";
const COOKIE: &str = "session=sk-abc123";

/// Bind an OS-assigned port on loopback; returns (port, listener) for tests.
pub async fn bind_ephemeral() -> (u16, TcpListener) {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let port = l.local_addr().unwrap().port();
    (port, l)
}

/// Accept forever, one task per connection.
pub async fn serve(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle(stream));
            }
            Err(_) => continue,
        }
    }
}

async fn handle(mut stream: TcpStream) {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];

    // Read until end-of-headers, then until the full Content-Length body has arrived.
    let head_end = loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
        if let Some(p) = find(&data, b"\r\n\r\n") {
            break p + 4;
        }
        if data.len() > 64 * 1024 {
            return;
        }
    };
    let content_len = header_val(&data[..head_end], "content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < head_end + content_len {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }

    let text = String::from_utf8_lossy(&data);
    let request_line = text.lines().next().unwrap_or("");
    let response = route(request_line, &text);

    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    // Connection: close — drop the stream to end the connection.
}

fn route(request_line: &str, full: &str) -> String {
    if request_line.starts_with("POST /login") {
        let ok = full.contains(&format!("username={GOOD_USER}"))
            && full.contains(&format!("password={GOOD_PASS}"));
        return if ok {
            respond(200, "OK", &format!("Set-Cookie: {COOKIE}; Path=/\r\n"), r#"{"login":"ok"}"#)
        } else {
            respond(401, "Unauthorized", "", r#"{"error":"bad credentials"}"#)
        };
    }
    if request_line.starts_with("GET /api/current-semester-info") {
        return if full.to_lowercase().contains(COOKIE) {
            respond(200, "OK", "", r#"{"semester":"2026-fall","semester_id":42}"#)
        } else {
            respond(401, "Unauthorized", "", r#"{"error":"no session"}"#)
        };
    }
    respond(404, "Not Found", "", "{}")
}

fn respond(code: u16, reason: &str, extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         {extra}\r\n\
         {body}",
        len = body.len(),
        extra = extra_headers,
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Case-insensitive lookup of a single request header value.
fn header_val(head: &[u8], name: &str) -> Option<String> {
    let head = String::from_utf8_lossy(head);
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.to_string());
            }
        }
    }
    None
}
