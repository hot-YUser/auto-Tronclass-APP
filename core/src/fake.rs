//! A stateful fake TronClass for headless tests (docs 90 §10). Multiple accounts (any username,
//! password = "secret") get their own `session=sk-<user>` cookie; a `_test` control endpoint opens
//! rollcalls; the four student answer endpoints record a per-user sign confirmed via `on_call_fine`;
//! and the teacher QR endpoints model docs 32 — the teacher's own rollcall only *sources* the
//! portable `data`, which a student then applies to *their own* rollcall id.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const GOOD_USER: &str = "test";
pub const GOOD_PASS: &str = "secret";
const CSRF: &str = "tok123";
/// Rotating QR `data` the teacher sources; portable across rollcalls (docs 32).
const QR_TOKEN: &str = "QRDATA-XYZ";

const LOGIN_PAGE: &str = concat!(
    "<!doctype html><html><body><h1>Sign in</h1>",
    "<form action=\"/login\" method=\"post\">",
    "<input type=\"hidden\" name=\"csrf\" value=\"tok123\">",
    "<label>User <input type=\"text\" name=\"username\"></label>",
    "<label>Pass <input type=\"password\" name=\"password\"></label>",
    "<button type=\"submit\">Sign in</button>",
    "</form></body></html>"
);

/// Captcha-guarded variant of the login page (image + captcha input); served when captcha mode is on.
const CAPTCHA_PAGE: &str = concat!(
    "<!doctype html><html><body><h1>Sign in</h1>",
    "<form action=\"/login\" method=\"post\">",
    "<input type=\"hidden\" name=\"csrf\" value=\"tok123\">",
    "<label>User <input type=\"text\" name=\"username\"></label>",
    "<label>Pass <input type=\"password\" name=\"password\"></label>",
    "<img src=\"/captcha.png\" alt=\"captcha\">",
    "<label>Code <input type=\"text\" name=\"captcha\"></label>",
    "<button type=\"submit\">Sign in</button>",
    "</form></body></html>"
);

/// An enterprise-SSO page (SAML/NetIQ NAM) — detected as `SsoRedirect`, routed to the cookie fallback.
const SSO_PAGE: &str = "<!doctype html><html><body>redirecting to nidp SAML single sign-on</body></html>";

/// Fixed bytes served at `/captcha.png` (ASCII so they fit the str response builder); the captcha
/// test base64-compares against these.
pub const CAPTCHA_IMAGE: &str = "FAKE-CAPTCHA-IMAGE-1234";

#[derive(Clone)]
struct Rollcall {
    id: String,
    kind: String, // number | radar | self_registration | qrcode
    course: String,
    number_code: Option<String>,
    attendance_rate: f64,
    requires_coords: bool,
    location: Option<(f64, f64)>,
    signed: HashSet<String>,
}

struct QuizDef {
    activity_id: String,
    course_id: String,
    course_name: String,
    source: String,
    instance_id: String,
    subjects: Value,        // array of subject objects (distribute view, no leak)
    existing: Value,        // { user: { subject_id: answer_value } }
    allow_retake: bool,
    reveal: bool,
}

#[derive(Default)]
struct FakeState {
    rollcalls: Vec<Rollcall>,
    quizzes: Vec<QuizDef>,
    llm_calls: u32,
    last_submission: Value,
    captcha_required: bool,
    captcha_expected: String,
    sso_mode: bool,
}

pub async fn bind_ephemeral() -> (u16, TcpListener) {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let port = l.local_addr().unwrap().port();
    (port, l)
}

pub async fn serve(listener: TcpListener) {
    let state = Arc::new(Mutex::new(FakeState::default()));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle(stream, state.clone()));
            }
            Err(_) => continue,
        }
    }
}

async fn handle(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
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
    let request_line = text.lines().next().unwrap_or("").to_string();
    let full = text.to_string();
    let body = text.get(head_end..).unwrap_or("").to_string();

    let response = route(&request_line, &full, &body, &state);
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn route(request_line: &str, full: &str, body: &str, state: &Arc<Mutex<FakeState>>) -> String {
    // --- auth ---
    if request_line.starts_with("GET /captcha.png") {
        return response(200, "image/png", "", CAPTCHA_IMAGE);
    }
    if request_line.starts_with("GET /login") {
        let page = {
            let st = state.lock().unwrap();
            if st.captcha_required {
                CAPTCHA_PAGE
            } else if st.sso_mode {
                SSO_PAGE
            } else {
                LOGIN_PAGE
            }
        };
        return html(200, &format!("Set-Cookie: csrftoken={CSRF}; Path=/\r\n"), page);
    }
    if request_line.starts_with("POST /login") {
        let captcha_ok = {
            let st = state.lock().unwrap();
            !st.captcha_required || form_field(body, "captcha") == st.captcha_expected
        };
        let username = form_field(body, "username");
        let ok = form_field(body, "password") == GOOD_PASS
            && body.contains(&format!("csrf={CSRF}"))
            && !username.is_empty()
            && captcha_ok;
        return if ok {
            html(200, &format!("Set-Cookie: session=sk-{username}; Path=/\r\n"), "<html>ok</html>")
        } else {
            html(200, "", LOGIN_PAGE) // 200-with-login-page (the false-positive trap)
        };
    }
    if request_line.starts_with("POST /_test/captcha") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.captcha_required = v["required"].as_bool().unwrap_or(true);
        st.captcha_expected = v["expected"].as_str().unwrap_or("A1B2").to_string();
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/sso") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().sso_mode = v["enabled"].as_bool().unwrap_or(true);
        return json(200, r#"{"ok":true}"#);
    }

    let user = session_user(full);

    if request_line.starts_with("GET /api/current-semester-info") {
        return match &user {
            Some(_) => json(200, r#"{"semester":"2026-fall","semester_id":42}"#),
            None => html(200, "", LOGIN_PAGE),
        };
    }

    // --- dev-only test control ---
    if request_line.starts_with("POST /_test/open_rollcall") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.rollcalls.push(Rollcall {
            id: v["id"].as_str().unwrap_or("RC").to_string(),
            kind: v["kind"].as_str().unwrap_or("self_registration").to_string(),
            course: v["course"].as_str().unwrap_or("Course").to_string(),
            number_code: v["number_code"].as_str().map(str::to_string),
            attendance_rate: v["attendance_rate"].as_f64().unwrap_or(100.0),
            requires_coords: v["requires_coords"].as_bool().unwrap_or(false),
            location: match (v["lat"].as_f64(), v["lng"].as_f64()) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            },
            signed: HashSet::new(),
        });
        return json(200, r#"{"ok":true}"#);
    }

    // --- fake LLM + quiz test-control (no TronClass session needed) ---
    if request_line.starts_with("POST /v1/chat/completions") {
        let mut st = state.lock().unwrap();
        st.llm_calls += 1;
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let prompt = v.pointer("/messages/0/content").and_then(Value::as_str).unwrap_or("");
        let ans = first_option_id(prompt).unwrap_or_else(|| "canned answer".to_string());
        // A tiny SSE stream: one reasoning delta, then the answer, then [DONE].
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"thinking\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
            serde_json::to_string(&ans).unwrap()
        );
        return response(200, "text/event-stream", "", &sse);
    }
    if request_line.starts_with("GET /_test/llm_calls") {
        return json(200, &format!(r#"{{"count":{}}}"#, state.lock().unwrap().llm_calls));
    }
    if request_line.starts_with("GET /_test/last_submission") {
        return json(200, &state.lock().unwrap().last_submission.to_string());
    }
    if request_line.starts_with("POST /_test/open_quiz") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.quizzes.push(QuizDef {
            activity_id: v["activity_id"].as_str().unwrap_or("EX").to_string(),
            course_id: v["course_id"].as_str().unwrap_or("C1").to_string(),
            course_name: v["course_name"].as_str().unwrap_or("Course").to_string(),
            source: v["source"].as_str().unwrap_or("exam").to_string(),
            instance_id: v["instance_id"].as_str().unwrap_or("inst-1").to_string(),
            subjects: v["subjects"].clone(),
            existing: v["existing"].clone(),
            allow_retake: v["allow_retake"].as_bool().unwrap_or(false),
            reveal: v["reveal"].as_bool().unwrap_or(false),
        });
        return json(200, r#"{"ok":true}"#);
    }

    // Everything below needs a session.
    let Some(user) = user else { return html(200, "", LOGIN_PAGE) };

    if request_line.starts_with("GET /api/radar/rollcalls") {
        let st = state.lock().unwrap();
        let items: Vec<String> = st.rollcalls.iter().map(rollcall_json).collect();
        return json(200, &format!(r#"{{"rollcalls":[{}]}}"#, items.join(",")));
    }

    // --- quiz detection (per-course) + exam contract ---
    if request_line.starts_with("GET /api/my-courses") {
        let st = state.lock().unwrap();
        let mut seen = HashSet::new();
        let items: Vec<String> = st
            .quizzes
            .iter()
            .filter(|q| seen.insert(q.course_id.clone()))
            .map(|q| format!(r#"{{"id":"{}","name":"{}"}}"#, q.course_id, q.course_name))
            .collect();
        return json(200, &format!(r#"{{"courses":[{}]}}"#, items.join(",")));
    }
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/activities") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<String> = st
            .quizzes
            .iter()
            .filter(|q| q.course_id == cid)
            .map(|q| format!(r#"{{"id":"{}","type":"{}","is_in_progress":true,"course_name":"{}"}}"#, q.activity_id, q.source, q.course_name))
            .collect();
        return json(200, &format!(r#"{{"activities":[{}]}}"#, items.join(",")));
    }
    if request_line.starts_with("GET /api/courses/") {
        return json(200, r#"{"activities":[]}"#); // /exams, /homework-activities empty in the fake
    }
    if request_line.contains("/check-exam-qualification") {
        return json(200, r#"{"ok":true}"#);
    }
    if let Some(id) = exam_route(request_line, "GET", "distribute") {
        let st = state.lock().unwrap();
        let Some(q) = st.quizzes.iter().find(|q| q.activity_id == id) else { return json(404, "{}") };
        let subjects = merge_existing(&q.subjects, &q.existing, &user);
        let body = json!({ "exam_paper_instance_id": q.instance_id, "subjects": subjects,
                           "allow_retake_exam": q.allow_retake,
                           "announce_answer": if q.reveal { "immediate" } else { "never" } });
        return json(200, &body.to_string());
    }
    if request_line.contains("/submissions/") {
        return json(200, r#"{"subjects_data":{"subjects":[]}}"#); // review: no extra leak in the fake
    }
    if exam_route(request_line, "POST", "submissions").is_some() {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.last_submission = v;
        return json(200, &format!(r#"{{"submission_id":"sub-{user}","allow_retake_exam":false}}"#));
    }

    // --- teacher QR: source the portable data ---
    if request_line.starts_with("POST /api/course/") && request_line.contains("/rollcall") && !request_line.contains("qr_code") {
        return json(200, r#"{"rollcall_id":"teacher-qr-1"}"#);
    }
    if request_line.contains("/qr_code") {
        return json(200, &format!(r#"{{"data":"{QR_TOKEN}"}}"#));
    }
    if request_line.contains("/start-rollcall") || request_line.contains("/stop_qr_rollcall") {
        return json(200, r#"{"ok":true}"#);
    }

    // --- per-rollcall reads/answers ---
    if let Some(id) = rollcall_route(request_line, "GET", "student_rollcalls") {
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id) {
            Some(r) => json(200, &format!(r#"{{"number_code":{},"on_call_fine":{}}}"#,
                json_str_or_null(&r.number_code), r.signed.contains(&user))),
            None => json(404, "{}"),
        };
    }
    if let Some(id) = rollcall_route(request_line, "GET", "answers") {
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id) {
            Some(r) => json(200, &format!(r#"{{"attendance_rate":{}}}"#, r.attendance_rate)),
            None => json(404, "{}"),
        };
    }
    if let Some(id) = rollcall_route(request_line, "GET", "lite") {
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id).and_then(|r| r.location) {
            Some((lat, lng)) => json(200, &format!(r#"{{"lat":{lat},"lng":{lng}}}"#)),
            None => json(200, "{}"),
        };
    }

    for (suffix, kind) in [
        ("answer_number_rollcall", "number"),
        ("answer_self_registration_rollcall", "self_registration"),
        ("answer_qr_rollcall", "qrcode"),
        ("answer", "radar"),
    ] {
        if let Some(id) = rollcall_route(request_line, "PUT", suffix) {
            return answer(state, &id, kind, &user, body);
        }
    }

    json(404, "{}")
}

/// Apply a student answer; sign the user (set on_call_fine) when the answer is correct for the type.
fn answer(state: &Arc<Mutex<FakeState>>, id: &str, kind: &str, user: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    let mut st = state.lock().unwrap();
    let Some(r) = st.rollcalls.iter_mut().find(|r| r.id == id) else { return json(404, "{}") };

    let signed_now = match kind {
        "number" => v["numberCode"].as_str() == r.number_code.as_deref(),
        "self_registration" => true,
        "qrcode" => v["data"].as_str() == Some(QR_TOKEN), // teacher-sourced portable data
        "radar" => {
            let has_coords = v.get("lat").is_some();
            if !has_coords {
                !r.requires_coords // empty {} passes unless coords are required
            } else if let (Some(lat), Some(lng), Some((tlat, tlng))) =
                (v["lat"].as_f64(), v["lng"].as_f64(), r.location)
            {
                let dist = approx_dist_m(lat, lng, tlat, tlng);
                if dist > 50.0 {
                    // Wrong spot → return the distance feedback (drives the WGS84 fallback).
                    return json(200, &format!(r#"{{"distance":{dist}}}"#));
                }
                true
            } else {
                false
            }
        }
        _ => false,
    };
    if signed_now {
        r.signed.insert(user.to_string());
    }
    json(200, r#"{"ok":true}"#)
}

fn rollcall_json(r: &Rollcall) -> String {
    let f = |k: &str| if r.kind == k { "true" } else { "false" };
    format!(
        r#"{{"rollcall_id":"{}","id":"{}","course_name":"{}","is_number":{},"is_radar":{},"is_self_registration":{},"unsupported_qrcode":{}}}"#,
        r.id, r.id, r.course, f("number"), f("radar"), f("self_registration"), f("qrcode")
    )
}

/// `/api/exams/{id}/{suffix}` → id.
fn exam_route(request_line: &str, method: &str, suffix: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    if parts.next()? != method {
        return None;
    }
    let rest = parts.next()?.strip_prefix("/api/exams/")?;
    let (id, tail) = rest.split_once('/')?;
    (tail.split('?').next()? == suffix).then(|| id.to_string())
}

fn course_id_from(request_line: &str) -> String {
    request_line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.strip_prefix("/api/courses/"))
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
        .to_string()
}

/// Merge a user's existing answers into the distribute subjects (drives conflict detection).
fn merge_existing(subjects: &Value, existing: &Value, user: &str) -> Value {
    let mut arr = subjects.as_array().cloned().unwrap_or_default();
    if let Some(uex) = existing.get(user) {
        for s in arr.iter_mut() {
            let sid = s.get("subject_id").or_else(|| s.get("id")).and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(ans) = uex.get(&sid) {
                if let Some(opts) = ans.get("options") {
                    s["student_answer_option_ids"] = opts.clone();
                } else if let Some(t) = ans.get("text") {
                    s["student_answer"] = t.clone();
                }
            }
        }
    }
    Value::Array(arr)
}

/// First `[id]` bracket in the prompt (the fake LLM "answers" selection questions with option 1).
fn first_option_id(prompt: &str) -> Option<String> {
    let a = prompt.find('[')?;
    let b = prompt[a + 1..].find(']')?;
    Some(prompt[a + 1..a + 1 + b].to_string())
}

/// `/api/rollcall/{id}/{suffix}` → id (tolerates a trailing query string).
fn rollcall_route(request_line: &str, method: &str, suffix: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    if parts.next()? != method {
        return None;
    }
    let path = parts.next()?;
    let rest = path.strip_prefix("/api/rollcall/")?;
    let (id, tail) = rest.split_once('/')?;
    if tail.split('?').next()? == suffix {
        Some(id.to_string())
    } else {
        None
    }
}

fn session_user(full: &str) -> Option<String> {
    // find "session=sk-<user>" in the Cookie header
    let lower = full.to_lowercase();
    let idx = lower.find("session=sk-")?;
    let after = &full[idx + "session=sk-".len()..];
    let user: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
    if user.is_empty() {
        None
    } else {
        Some(user)
    }
}

fn form_field(body: &str, key: &str) -> String {
    body.split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap_or("")
        .to_string()
}

fn json_str_or_null(s: &Option<String>) -> String {
    match s {
        Some(v) => format!("\"{v}\""),
        None => "null".to_string(),
    }
}

fn approx_dist_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians() * lat1.to_radians().cos();
    (dlat * dlat + dlon * dlon).sqrt() * R
}

fn json(code: u16, body: &str) -> String {
    response(code, "application/json", "", body)
}
fn html(code: u16, extra_headers: &str, body: &str) -> String {
    response(code, "text/html; charset=utf-8", extra_headers, body)
}
fn response(code: u16, content_type: &str, extra_headers: &str, body: &str) -> String {
    let reason = if code == 200 { "OK" } else { "Not Found" };
    format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n{extra}\r\n{body}",
        len = body.len(),
        extra = extra_headers,
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
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
