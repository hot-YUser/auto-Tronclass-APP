//! R5 course-material tool + image fetcher (docs 31). The answerer calls `search_course_materials` to
//! pull the student's OWN course handouts (incl. PDF text) when a question lacks context; `<img>` images
//! embedded in a subject are fetched (authed) and base64-inlined for the multimodal path. All GETs use the
//! account's authed client; every failure degrades to a string / `None` — an error never enters the answer
//! flow (the model must still get its best shot).

use crate::providers::Endpoints;
use crate::quiz::clean_html;
use reqwest::{Client, Response};
use serde_json::{json, Value};

const MATERIAL_TYPES: [&str; 5] = ["material", "page", "online_video", "web_link", "scorm"];
const MAX_MATERIALS: usize = 60; // listed materials scanned
const MAX_READ: usize = 12000; // total concatenated tool-result text
const MAX_PDF: usize = 8000; // per-PDF extracted text

/// Bounded response bodies for the course-material tool: JSON envelopes are KBs; a handout PDF can
/// legitimately be large but must still have a cap (we only extract its text layer).
const MAX_MATERIAL_JSON: usize = 8 * 1024 * 1024;
const MAX_PDF_BYTES: usize = 20 * 1024 * 1024;
/// Images are base64-inlined into the prompt; 5 MiB is far beyond any real subject image and keeps
/// a runaway/lying server from ballooning the request.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The single tool the model may call (the SYSTEM_PROMPT has instructed it since R3b).
pub fn tool_spec() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "search_course_materials",
            "description": "Search this course's materials/handouts (including PDF text) for the information needed to answer the question, and return the relevant material text. Call this whenever the question relies on a passage, figure, dataset, or handout you were not given.",
            "parameters": {
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Keywords describing what to find in the course materials." } },
                "required": ["query"]
            }
        }
    })
}

/// Fetch the course materials relevant to `query` and return their text. NEVER errors → `""` on any miss.
pub async fn search_course_materials(
    client: &Client,
    base_url: &str,
    course_id: &str,
    query: &str,
) -> String {
    let ep = Endpoints::derive(base_url);
    let list: Value = match client.get(ep.course_activities(course_id)).send().await {
        Ok(r) => crate::http::read_bounded_json(r, MAX_MATERIAL_JSON, "course activities")
            .await
            .unwrap_or(Value::Null),
        Err(_) => return String::new(),
    };
    let items = list
        .get("activities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let materials: Vec<&Value> = items
        .iter()
        .filter(|a| {
            a.get("type")
                .and_then(Value::as_str)
                .map(|t| MATERIAL_TYPES.contains(&t))
                .unwrap_or(false)
        })
        .take(MAX_MATERIALS)
        .collect();

    // Pick materials whose title contains a query token (≥2 chars, lowercased); else the first 3.
    let tokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(str::to_string)
        .collect();
    let matched: Vec<&Value> = materials
        .iter()
        .copied()
        .filter(|a| {
            let title = a
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            tokens.iter().any(|t| title.contains(t))
        })
        .collect();
    let picked = if matched.is_empty() {
        materials
    } else {
        matched
    };

    let mut out = String::new();
    for a in picked.into_iter().take(3) {
        let Some(aid) = a.get("id").and_then(id_str) else {
            continue;
        };
        let chunk = read_material(client, &ep, &aid).await;
        if !chunk.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n=====\n\n");
            }
            out.push_str(&chunk);
        }
        if out.len() >= MAX_READ {
            break;
        }
    }
    truncate_safe(&mut out, MAX_READ);
    out
}

/// One material: its title + html-stripped description, plus each file attachment (PDF text inlined).
async fn read_material(client: &Client, ep: &Endpoints, aid: &str) -> String {
    let detail: Value = match client.get(ep.activity_detail(aid)).send().await {
        Ok(r) => crate::http::read_bounded_json(r, MAX_MATERIAL_JSON, "material detail")
            .await
            .unwrap_or(Value::Null),
        Err(_) => return String::new(),
    };
    let title = detail.get("title").and_then(Value::as_str).unwrap_or("");
    let desc = detail
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| detail.get("content").and_then(Value::as_str))
        .unwrap_or("");
    let mut s = format!("教材：{title}\n{}", clean_html(desc));

    let refs: Value = match client.get(ep.upload_references(aid)).send().await {
        Ok(r) => crate::http::read_bounded_json(r, MAX_MATERIAL_JSON, "upload references")
            .await
            .unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let arr = refs
        .get("upload_references")
        .or_else(|| refs.get("references"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for rf in &arr {
        s.push_str("\n\n");
        s.push_str(&read_reference(client, ep, rf).await);
    }
    s
}

/// One attachment: PDF → inline its text (or a "no text" note); image → a note; else just its name.
async fn read_reference(client: &Client, ep: &Endpoints, rf: &Value) -> String {
    let name = rf
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| rf.get("title").and_then(Value::as_str))
        .or_else(|| rf.pointer("/upload/name").and_then(Value::as_str))
        .unwrap_or("attachment")
        .to_string();
    let mut url = rf
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| rf.get("download_url").and_then(Value::as_str))
        .or_else(|| rf.get("preview_url").and_then(Value::as_str))
        .or_else(|| rf.pointer("/upload/url").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let upload_id = rf
        .get("upload_id")
        .and_then(id_str)
        .or_else(|| rf.pointer("/upload/id").and_then(id_str))
        .or_else(|| rf.get("id").and_then(id_str))
        .unwrap_or_default();

    // Server-provided direct attachment URLs are untrusted. Resolve relative values against the
    // school origin; cross-origin URLs must be HTTPS and may not name a literal private address.
    // The school client's redirect policy applies the same check to subsequent hops.
    if !url.is_empty() {
        url = resolve_external_url(ep.base_url(), &url).unwrap_or_default();
    }
    let lower = name.to_lowercase();
    if lower.ends_with(".pdf") {
        if url.is_empty() && !upload_id.is_empty() {
            if let Ok(r) = client.get(ep.upload_document_url(&upload_id)).send().await {
                if let Ok(v) =
                    crate::http::read_bounded_json(r, MAX_MATERIAL_JSON, "upload url").await
                {
                    url = v
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|value| resolve_external_url(ep.base_url(), value))
                        .unwrap_or_default();
                }
            }
        }
        if !url.is_empty() {
            if let Ok(r) = client.get(&url).send().await {
                if let Ok(bytes) = crate::http::read_bounded(r, MAX_PDF_BYTES, "course PDF").await {
                    let text = pdf_text(&bytes);
                    if !text.is_empty() {
                        return format!("附件 {name}（PDF 內文）:\n{text}");
                    }
                }
            }
        }
        return format!("附件 {name}（PDF，無法取得內文）");
    }
    if is_image_name(&lower) {
        return format!("附件 {name}（圖片，未載入）");
    }
    format!("附件 {name}")
}

fn resolve_external_url(base_url: &str, raw: &str) -> Option<String> {
    let base = reqwest::Url::parse(base_url).ok()?;
    let url = base.join(raw).ok()?;
    if !crate::http::same_origin(&base, &url)
        && (url.scheme() != "https" || crate::http::is_private_host(url.host_str().unwrap_or("")))
    {
        return None;
    }
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// Extract a PDF's text layer (`pdf-extract`, pure Rust). Empty on a scanned/no-text PDF → the caller's
/// graceful "（無法取得內文）" note. `catch_unwind` guards a malformed-PDF panic (never crash the answer flow).
pub(crate) fn pdf_text(bytes: &[u8]) -> String {
    let mut t =
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes).unwrap_or_default())
            .unwrap_or_default();
    truncate_safe(&mut t, MAX_PDF);
    t.trim().to_string()
}

/// Authed GET an image → `(bytes, mime)`. Body capped at 5 MiB; content-type must be `image/*`
/// (or a missing/octet-stream type with an image-looking URL path — some CDNs don't set it).
/// A non-image response (e.g. a soft-404 HTML page) is rejected, never inlined as image data.
#[cfg(test)]
pub async fn fetch_image(client: &Client, url: &str) -> Option<(Vec<u8>, String)> {
    let resp = client.get(url).send().await.ok()?;
    fetch_image_response(resp).await
}

/// Bounded image read from an already-sent response; see `fetch_image`.
pub(crate) async fn fetch_image_response(resp: Response) -> Option<(Vec<u8>, String)> {
    if !resp.status().is_success() {
        return None;
    }
    // Clone the URL before `read_bounded` consumes the response; the final URL is needed
    // for the image-path checks and for the subtype fallback (redirects may change it).
    let url = resp.url().clone();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let is_image_ct = ct.starts_with("image/");
    let ext_image = is_image_name(&url.path().to_lowercase());
    if !is_image_ct && !(ext_image && (ct.is_empty() || ct == "application/octet-stream")) {
        return None;
    }
    let bytes = crate::http::read_bounded(resp, MAX_IMAGE_BYTES, "image")
        .await
        .ok()?;
    let mime = if is_image_ct {
        ct
    } else {
        format!("image/{}", ext_to_subtype(url.as_str()))
    };
    Some((bytes, mime))
}

/// Image subtype from a url extension (`jpg`→`jpeg`); default `png`.
pub fn ext_to_subtype(url: &str) -> &'static str {
    let u = url.to_lowercase();
    let u = u.split('?').next().unwrap_or("");
    if u.ends_with(".jpg") || u.ends_with(".jpeg") {
        "jpeg"
    } else if u.ends_with(".gif") {
        "gif"
    } else if u.ends_with(".webp") {
        "webp"
    } else {
        "png"
    }
}

fn is_image_name(lower: &str) -> bool {
    [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"]
        .iter()
        .any(|e| lower.ends_with(e))
}

fn id_str(x: &Value) -> Option<String> {
    x.as_str()
        .map(str::to_string)
        .or_else(|| x.as_i64().map(|n| n.to_string()))
}

/// Truncate `s` to at most `max` BYTES, backing off to the nearest char boundary (never mid-codepoint).
fn truncate_safe(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_and_truncate_helpers() {
        assert_eq!(ext_to_subtype("http://x/a.JPG?t=1"), "jpeg");
        assert_eq!(ext_to_subtype("http://x/a.webp"), "webp");
        assert_eq!(ext_to_subtype("http://x/a"), "png");
        let mut s = "héllo".to_string(); // 'é' is 2 bytes → truncating to 2 must not split it
        truncate_safe(&mut s, 2);
        assert_eq!(s, "h");
        assert!(is_image_name("photo.png") && !is_image_name("notes.pdf"));
    }

    #[tokio::test]
    async fn fetch_image_rejects_non_images_and_oversized_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // text/html (a soft-404 page) must NOT be inlined as image data.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let body = "<html>not an image</html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let url = format!("http://{address}/img.png");
        assert!(
            fetch_image(&client, &url).await.is_none(),
            "non-image content-type must be rejected"
        );

        // A real image round-trips with its content-type.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let body = [0x89, b'P', b'N', b'G'];
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let client = reqwest::Client::new();
        let url = format!("http://{address}/img");
        let (bytes, mime) = fetch_image(&client, &url)
            .await
            .expect("image/png accepted");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes.len(), 4);

        // An oversized Content-Length is rejected by the bounded reader.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 104857600\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let url = format!("http://{address}/big.png");
        assert!(
            fetch_image(&client, &url).await.is_none(),
            "oversized image must be rejected"
        );
    }

    #[test]
    fn pdf_text_extracts_sentinel_and_degrades() {
        // pdf-extract must read the fake's hand-built PDF text layer; a non-PDF degrades to "".
        let pdf = crate::fake::minimal_pdf("PHOTOSYNTHESIS42");
        assert!(
            pdf_text(&pdf).contains("PHOTOSYNTHESIS42"),
            "pdf-extract reads the sentinel"
        );
        assert!(
            pdf_text(b"not a pdf at all").is_empty(),
            "a non-PDF degrades to empty"
        );
    }
}
