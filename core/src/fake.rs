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

/// The form served AFTER a `/login` → `/sso/login-page` redirect: a RELATIVE action (`authenticate`),
/// so a correct urljoin posts to `/sso/authenticate` and a base-join wrongly posts to `/authenticate`.
const SSO_LOGIN_FORM: &str = concat!(
    "<!doctype html><html><body><h1>SSO Sign in</h1>",
    "<form action=\"authenticate\" method=\"post\">",
    "<input type=\"hidden\" name=\"csrf\" value=\"tok123\">",
    "<label>User <input type=\"text\" name=\"username\"></label>",
    "<label>Pass <input type=\"password\" name=\"password\"></label>",
    "<button type=\"submit\">Sign in</button>",
    "</form></body></html>"
);

/// Fixed bytes served at `/captcha.png` (ASCII so they fit the str response builder); the captcha
/// test base64-compares against these.
pub const CAPTCHA_IMAGE: &str = "FAKE-CAPTCHA-IMAGE-1234";

#[derive(Clone)]
struct Rollcall {
    id: String,
    kind: String, // number | radar | self_registration | qrcode
    course: String,
    number_code: Option<String>,
    attendance_rate: f64, // base class present-% (0..100); the roster is synthesized from it
    requires_coords: bool,
    location: Option<(f64, f64)>, // radar: the HIDDEN true target (never exposed via lite)
    signed: HashSet<String>,
    hide_code: bool,   // number_code null in the roster → forces a brute-force
    number_fatal: bool, // number answer returns 403 → the code must abort the whole round
    throttle: u32,     // first N number answers return 429 → transient/cooldown path
    use_beacon: bool,       // radar: lite advertises a beacon → answer must carry radarSignal
    beacon_nonce: String,   // radar: beacon nonce fed to the radarSignal md5
    scope_radius_m: f64,    // radar: a submitted coord within this of the target signs
}

struct QuizDef {
    activity_id: String,
    course_id: String,
    course_name: String,
    source: String,
    instance_id: String,
    subjects: Value,        // array of subject objects (real distribute shapes incl. leaks)
    existing: Value,        // { user: { subject_id: answer_value } } → per-user prior answer
    review: Value,          // correct_answers_data.correct_answers for the resubmit read
    stem: String,           // homework question stem (GET /api/activities/{id})
    vote_items: Value,      // vote: { "A": "text", ... } letter→text map
    vote_type: String,      // vote: "single" | "multiple"
    vote_students: Value,   // vote: [{ "user_no": "..." }] who already voted (skip if caller is in it)
    allow_retake: bool,
    reveal: bool,
    submitted: bool,        // once true, distribute mints a fresh (retake) instance id
    // --- R4 per-family detection gate fields (real list-endpoint shapes) ---
    is_started: bool,           // exam/questionnaire/courseware: activity has opened
    is_closed: bool,            // any: activity closed (past window)
    has_submitted: bool,        // exam/questionnaire/homework: caller already submitted → skip
    submit_times: i64,          // exam: max attempts (0 = unlimited)
    submission_count: i64,      // exam: attempts used → skip when submit_times>0 && count>=submit_times
    started_subjects_count: i64,// classroom: >=1 to be answerable (drops to 0 after 收答 closes)
    status: String,             // vote/classroom: "start" | "end"
    my_submission: bool,        // courseware: a non-empty my-submission → already answered, skip
    end_time: String,           // exam: ISO-8601 UTC window end (real tenants send a string, not epoch)
}

/// R5: a course material (handout) the `search_course_materials` tool can read; a `.pdf` attachment is
/// served on demand with `pdf_sentinel` as its text layer.
struct MaterialDef {
    id: String,
    course_id: String,
    course_name: String,
    title: String,
    description: String,
    pdf_upload_id: String, // "" = no PDF attachment
    pdf_sentinel: String,  // the served PDF's text
}

#[derive(Default)]
struct FakeState {
    rollcalls: Vec<Rollcall>,
    quizzes: Vec<QuizDef>,
    materials: Vec<MaterialDef>,
    llm_calls: u32,
    llm_fail_times: u32, // first N /v1/chat/completions calls return empty content (R3c retry test)
    last_submission: Value,
    last_llm_request: Value, // the last /v1/chat/completions request body (R3b assertion)
    captcha_required: bool,
    captcha_expected: String,
    sso_mode: bool,
    saw_radar_signal: bool, // set once a radar coord answer carried a radarSignal (beacon test)
    expired: bool,          // R4-D: authed requests serve a 200 login page until the account re-logins
    expire_mode: String,    // R4.1 #5: "login_page" (default) | "401" | "redirect" — how expiry manifests
    sso_redirect: bool,     // R4-C: GET /login 302-redirects to a DIFFERENT PATH with a RELATIVE form action
                            // (same-host; a true cross-host 302 would need a 2nd loopback port — not worth it)
    sign_expired: bool,     // R4.1 #2: ONLY the PUT answer_* (sign) routes expire; poll/detect stay healthy
    sign_expire_user: String, // R4.1 #2: expire only this user's signs (empty = all) → per-account double-sign test
    down: bool,             // R4.1 stale-offline: the poll canary gets a 503 (transient blip, NOT auth-lost)
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
        // SSO-redirect mode: 302 to a form on a different PATH whose action is relative (R4-C urljoin).
        if state.lock().unwrap().sso_redirect {
            return redirect("/sso/login-page");
        }
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
    if request_line.starts_with("GET /sso/login-page") {
        return html(200, &format!("Set-Cookie: csrftoken={CSRF}; Path=/\r\n"), SSO_LOGIN_FORM);
    }
    if request_line.starts_with("POST /login") || request_line.starts_with("POST /sso/authenticate") {
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
            // a successful (re-)login refreshes the session → clear every expiry flag.
            { let mut st = state.lock().unwrap(); st.expired = false; st.sign_expired = false; }
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
    if request_line.starts_with("POST /_test/down") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().down = v["enabled"].as_bool().unwrap_or(true);
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/expire_signs") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.sign_expired = v["enabled"].as_bool().unwrap_or(true);
        st.sign_expire_user = v["user"].as_str().unwrap_or("").to_string();
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/expire") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.expired = v["expired"].as_bool().unwrap_or(true);
        st.expire_mode = v["mode"].as_str().unwrap_or("login_page").to_string();
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/sso_redirect") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().sso_redirect = v["enabled"].as_bool().unwrap_or(true);
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/sso") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().sso_mode = v["enabled"].as_bool().unwrap_or(true);
        return json(200, r#"{"ok":true}"#);
    }

    // R4-D / R4.1 #5: session expiry. `login_page` (default) → treat a valid cookie as no session (authed
    // reads serve a login page); `401`/`redirect` → intercept authed `/api` reads so the poll canary sees
    // that specific failure. A (re-)login POST clears the flag above.
    let raw_user = session_user(full);
    let (expired, mode) = {
        let st = state.lock().unwrap();
        (st.expired && raw_user.is_some(), st.expire_mode.clone())
    };
    if expired && (request_line.starts_with("GET /api") || request_line.starts_with("PUT /api")) {
        match mode.as_str() {
            "401" => return response(401, "application/json", "", r#"{"error":"unauthorized"}"#),
            "redirect" => return redirect("/login"),
            _ => {} // login_page: fall through — user=None below makes routes serve a login page
        }
    }
    let user = if expired { None } else { raw_user };

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
            hide_code: v["hide_code"].as_bool().unwrap_or(false),
            number_fatal: v["number_fatal"].as_bool().unwrap_or(false),
            throttle: v["throttle"].as_u64().unwrap_or(0) as u32,
            use_beacon: v["use_beacon"].as_bool().unwrap_or(false),
            beacon_nonce: v["beacon_nonce"].as_str().unwrap_or("nonce-abc").to_string(),
            scope_radius_m: v["scope_radius"].as_f64().unwrap_or(100.0),
        });
        return json(200, r#"{"ok":true}"#);
    }

    // --- fake LLM + quiz test-control (no TronClass session needed) ---
    if request_line.starts_with("POST /v1/chat/completions") {
        let mut st = state.lock().unwrap();
        st.llm_calls += 1;
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        st.last_llm_request = v.clone();
        let msgs = v["messages"].as_array().cloned().unwrap_or_default();
        // The question is messages[1] (messages[0] is the system prompt); string OR a multimodal parts list.
        let user_text = llm_user_text(&msgs);
        let has_tool_result = msgs.iter().any(|m| m["role"] == "tool");

        // R5 non-streaming tools path: call search_course_materials, then answer from its result.
        if !v["stream"].as_bool().unwrap_or(true) {
            if has_tool_result {
                // Answer from the fetched material — the text after the PDF "內文）:" marker (the sentinel).
                let tc = msgs.iter().rev().find(|m| m["role"] == "tool").and_then(|m| m["content"].as_str()).unwrap_or("");
                let ans = tc.split("內文）:").nth(1).map(|s| s.trim().to_string()).unwrap_or_else(|| "unknown".to_string());
                return json(200, &llm_msg(&ans));
            }
            if user_text.contains("handout") {
                // Emit ONE tool call (the question relies on a handout we weren't given).
                return json(200, &json!({ "choices": [{ "message": { "role": "assistant", "content": null,
                    "tool_calls": [{ "id": "call_1", "type": "function",
                        "function": { "name": "search_course_materials", "arguments": "{\"query\":\"handout\"}" } }] } }] }).to_string());
            }
            // No tool needed → the SAME canned logic as the streaming path (existing tests stay green).
            return json(200, &llm_msg(&llm_canned(&user_text, st.llm_calls, st.llm_fail_times)));
        }

        // Streaming (no-tools) path — a tiny SSE stream: one reasoning delta, then the answer, then [DONE].
        let ans = llm_canned(&user_text, st.llm_calls, st.llm_fail_times);
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"thinking\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
            serde_json::to_string(&ans).unwrap()
        );
        return response(200, "text/event-stream", "", &sse);
    }
    if request_line.starts_with("GET /_test/llm_calls") {
        return json(200, &format!(r#"{{"count":{}}}"#, state.lock().unwrap().llm_calls));
    }
    if request_line.starts_with("POST /_test/llm_fail_times") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().llm_fail_times = v["times"].as_u64().unwrap_or(0) as u32;
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /_test/open_material") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        state.lock().unwrap().materials.push(MaterialDef {
            id: v["id"].as_str().unwrap_or("MAT").to_string(),
            course_id: v["course_id"].as_str().unwrap_or("C1").to_string(),
            course_name: v["course_name"].as_str().unwrap_or("Course").to_string(),
            title: v["title"].as_str().unwrap_or("Handout").to_string(),
            description: v["description"].as_str().unwrap_or("").to_string(),
            pdf_upload_id: v["pdf_upload_id"].as_str().unwrap_or("").to_string(),
            pdf_sentinel: v["pdf_sentinel"].as_str().unwrap_or("").to_string(),
        });
        return json(200, r#"{"ok":true}"#);
    }
    // R5: the executor GETs these (authed) — served here (no session gate needed). The document/url route
    // returns an ABSOLUTE url (built from the request Host) so the executor's reqwest GET can resolve it.
    if let Some(uid) = request_line.strip_prefix("GET /_test/pdf/").and_then(|r| r.split_whitespace().next()) {
        let st = state.lock().unwrap();
        let sentinel = st.materials.iter().find(|m| m.pdf_upload_id == uid).map(|m| m.pdf_sentinel.clone()).unwrap_or_default();
        let pdf = String::from_utf8(minimal_pdf(&sentinel)).unwrap_or_default(); // minimal_pdf is pure ASCII
        return response(200, "application/pdf", "", &pdf);
    }
    if request_line.starts_with("GET /_test/image.png") {
        return response(200, "image/png", "", "FAKE-PNG-BYTES"); // opaque bytes → base64 data-url (multimodal)
    }
    if request_line.starts_with("GET /_test/last_submission") {
        return json(200, &state.lock().unwrap().last_submission.to_string());
    }
    if request_line.starts_with("GET /_test/saw_radar_signal") {
        return json(200, &format!(r#"{{"saw":{}}}"#, state.lock().unwrap().saw_radar_signal));
    }
    if request_line.starts_with("GET /_test/last_llm_request") {
        return json(200, &state.lock().unwrap().last_llm_request.to_string());
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
            review: v["review"].clone(),
            stem: v["stem"].as_str().unwrap_or("").to_string(),
            vote_items: v["vote_items"].clone(),
            vote_type: v["vote_type"].as_str().unwrap_or("single").to_string(),
            vote_students: v["vote_students"].clone(),
            allow_retake: v["allow_retake"].as_bool().unwrap_or(false),
            reveal: v["reveal"].as_bool().unwrap_or(false),
            submitted: false,
            // detection gates default to "answerable" so a bare open_quiz is still detected.
            is_started: v["is_started"].as_bool().unwrap_or(true),
            is_closed: v["is_closed"].as_bool().unwrap_or(false),
            has_submitted: v["has_submitted"].as_bool().unwrap_or(false),
            submit_times: v["submit_times"].as_i64().unwrap_or(0),
            submission_count: v["submission_count"].as_i64().unwrap_or(0),
            started_subjects_count: v["started_subjects_count"].as_i64().unwrap_or(1),
            status: v["status"].as_str().unwrap_or("start").to_string(),
            my_submission: v["my_submission"].as_bool().unwrap_or(false),
            end_time: v["end_time"].as_str().unwrap_or("").to_string(),
        });
        return json(200, r#"{"ok":true}"#);
    }

    // Everything below needs a session.
    let Some(user) = user else { return html(200, "", LOGIN_PAGE) };

    if request_line.starts_with("GET /api/radar/rollcalls") {
        let st = state.lock().unwrap();
        if st.down {
            return response(503, "application/json", "", r#"{"error":"service unavailable"}"#); // transient blip
        }
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
    // --- per-family detection lists (real endpoints/keys/gate fields; R4) ---
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/exam-list") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source == "exam")
            .map(|q| json!({ "id": q.activity_id, "is_started": q.is_started, "is_closed": q.is_closed,
                "is_in_progress": !q.is_closed, "has_submitted": q.has_submitted,
                "submit_times": q.submit_times, "submission_count": q.submission_count,
                "end_time": q.end_time, "course_name": q.course_name })).collect();
        return json(200, &json!({ "exams": items }).to_string());
    }
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/questionnaire-list") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source == "questionnaire")
            .map(|q| json!({ "id": q.activity_id, "is_started": q.is_started, "is_closed": q.is_closed,
                "has_submitted": q.has_submitted, "course_name": q.course_name })).collect();
        return json(200, &json!({ "questionnaires": items }).to_string());
    }
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/homework-activities") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source == "homework")
            .map(|q| json!({ "id": q.activity_id, "is_closed": q.is_closed, "has_submitted": q.has_submitted,
                "description": q.stem, "course_name": q.course_name })).collect();
        return json(200, &json!({ "homework_activities": items }).to_string());
    }
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/interactions") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source == "vote")
            .map(|q| json!({ "id": q.activity_id, "type": "vote",
                "status": if q.is_closed { "end" } else { q.status.as_str() }, "course_name": q.course_name })).collect();
        return json(200, &json!({ "interactions": items }).to_string());
    }
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/classroom-list") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source.starts_with("classroom"))
            .map(|q| json!({ "id": q.activity_id, "status": if q.is_closed { "end" } else { q.status.as_str() },
                "started_subjects_count": q.started_subjects_count, "course_name": q.course_name })).collect();
        return json(200, &json!({ "classrooms": items }).to_string());
    }
    // courseware: material activities in the generic list, then a per-material quizzes chain. Also serves
    // R5 course materials (the `search_course_materials` tool GETs this same list).
    if request_line.starts_with("GET /api/courses/") && request_line.contains("/activities") {
        let cid = course_id_from(request_line);
        let st = state.lock().unwrap();
        let mut items: Vec<Value> = st.quizzes.iter().filter(|q| q.course_id == cid && q.source == "courseware-quiz")
            .map(|q| json!({ "id": format!("mat-{}", q.activity_id), "type": "material", "course_name": q.course_name })).collect();
        items.extend(st.materials.iter().filter(|m| m.course_id == cid)
            .map(|m| json!({ "id": m.id, "type": "material", "title": m.title, "course_name": m.course_name })));
        return json(200, &json!({ "activities": items }).to_string());
    }
    if let Some(aid) = api_suffix_id(request_line, "GET", "courseware-quiz/activity", "quizzes") {
        let qid = aid.strip_prefix("mat-").unwrap_or(&aid).to_string();
        let st = state.lock().unwrap();
        let items: Vec<Value> = st.quizzes.iter().filter(|q| q.activity_id == qid && q.source == "courseware-quiz")
            .map(|q| json!({ "id": q.activity_id, "is_started": q.is_started, "is_closed": q.is_closed })).collect();
        return json(200, &json!({ "quizzes": items }).to_string());
    }
    if let Some(qid) = api_suffix_id(request_line, "GET", "courseware-quiz/quiz", "my-submission") {
        let st = state.lock().unwrap();
        let done = st.quizzes.iter().any(|q| q.activity_id == qid && q.my_submission);
        return json(200, &if done { json!({ "id": "sub-1" }) } else { Value::Null }.to_string());
    }
    if request_line.contains("/check-exam-qualification") {
        return json(200, r#"{"ok":true}"#);
    }
    // --- paper fetch: distribute (exam | questionnaire | classroom), courseware has its own ---
    for seg in ["exams", "questionnaire", "classroom"] {
        if let Some(id) = api_suffix_id(request_line, "GET", seg, "distribute") {
            let st = state.lock().unwrap();
            let Some(q) = st.quizzes.iter().find(|q| q.activity_id == id) else { return json(404, "{}") };
            let subjects = merge_existing(&q.subjects, &q.existing, &user);
            // A retake mints a fresh paper instance (the original is closed after a submit).
            let inst = if q.submitted { format!("{}-retake", q.instance_id) } else { q.instance_id.clone() };
            return json(200, &json!({ "exam_paper_instance_id": inst, "subjects": subjects,
                "allow_retake_exam": q.allow_retake,
                "announce_answer": if q.reveal { "immediate" } else { "never" } }).to_string());
        }
    }
    if let Some(id) = api_suffix_id(request_line, "GET", "courseware-quiz/quiz", "subjects") {
        let st = state.lock().unwrap();
        let Some(q) = st.quizzes.iter().find(|q| q.activity_id == id) else { return json(404, "{}") };
        return json(200, &json!({ "exam_paper_instance_id": q.instance_id, "subjects": q.subjects.clone() }).to_string());
    }
    if let Some(id) = api_id(request_line, "GET", "votes") {
        let st = state.lock().unwrap();
        let (items, vtype, students) = st.quizzes.iter().find(|q| q.activity_id == id)
            .map(|q| (q.vote_items.clone(), q.vote_type.clone(), q.vote_students.clone()))
            .unwrap_or((Value::Null, "single".to_string(), Value::Null));
        // `students` = who already voted; detection skips when the caller's user_no is present.
        return json(200, &json!({ "students": students,
            "interaction": { "data": { "vote_option_items": items, "vote_type": vtype } } }).to_string());
    }
    // R5 material chain: attachments of a material, then a preview url for an upload id.
    if let Some(aid) = api_suffix_id(request_line, "GET", "activities", "upload_references") {
        let st = state.lock().unwrap();
        let refs: Vec<Value> = st.materials.iter().find(|m| m.id == aid).filter(|m| !m.pdf_upload_id.is_empty())
            .map(|m| vec![json!({ "name": "handout.pdf", "upload_id": m.pdf_upload_id })])
            .unwrap_or_default();
        return json(200, &json!({ "upload_references": refs }).to_string());
    }
    if let Some(uid) = api_suffix_id(request_line, "GET", "uploads/document", "url") {
        return json(200, &json!({ "url": format!("http://{}/_test/pdf/{uid}", host_of(full)) }).to_string());
    }
    if let Some(id) = api_id(request_line, "GET", "activities") {
        let st = state.lock().unwrap();
        if let Some(m) = st.materials.iter().find(|m| m.id == id) {
            return json(200, &json!({ "title": m.title, "description": m.description }).to_string());
        }
        let stem = st.quizzes.iter().find(|q| q.activity_id == id).map(|q| q.stem.clone()).unwrap_or_default();
        return json(200, &json!({ "description": stem }).to_string());
    }
    // review (exam submissions/{sid}) → correct_answers_data.correct_answers
    if request_line.starts_with("GET /api/exams/") && request_line.contains("/submissions/") {
        let id = path_seg(request_line, "/api/exams/");
        let st = state.lock().unwrap();
        let arr = st.quizzes.iter().find(|q| q.activity_id == id)
            .map(|q| if q.review.is_array() { q.review.clone() } else { json!([]) }).unwrap_or(json!([]));
        return json(200, &json!({ "correct_answers_data": { "correct_answers": arr } }).to_string());
    }
    // --- submit endpoints (record the last body; classroom rejects a flat body) ---
    if let Some(id) = api_suffix_id(request_line, "POST", "exams", "submissions") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.last_submission = v;
        let (retake, sid) = st.quizzes.iter_mut().find(|q| q.activity_id == id)
            .map(|q| { q.submitted = true; (q.allow_retake, format!("sub-{user}")) })
            .unwrap_or((false, format!("sub-{user}")));
        return json(200, &json!({ "submission_id": sid, "allow_retake_exam": retake }).to_string());
    }
    if api_suffix_id(request_line, "POST", "questionnaire", "submissions").is_some()
        || api_suffix_id(request_line, "POST", "courseware-quiz/quiz", "submissions").is_some()
        || api_suffix_id(request_line, "POST", "course/activities", "submissions").is_some()
        || api_id(request_line, "POST", "votes").map(|_| request_line.contains("/vote")).unwrap_or(false)
    {
        state.lock().unwrap().last_submission = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        return json(200, r#"{"ok":true}"#);
    }
    if request_line.starts_with("POST /api/classroom/") && request_line.contains("/submit/") {
        let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
        // Flat body (no exam wrapper) → 400; the wrapper is required per subject.
        if v.get("exam_paper_instance_id").is_none() || !v.get("subjects").map(|s| s.is_array()).unwrap_or(false) {
            return json(400, r#"{"error":"classroom flat body rejected"}"#);
        }
        state.lock().unwrap().last_submission = v;
        return json(200, r#"{"ok":true}"#);
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

    // --- current user (for user_no capture) ---
    if request_line.starts_with("GET /api/user") {
        return json(200, &format!(r#"{{"user_no":"{user}","user_id":"{user}"}}"#));
    }

    // --- per-rollcall reads/answers (real contract: object roster of strings) ---
    if let Some(id) = rollcall_route(request_line, "GET", "student_rollcalls") {
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id) {
            Some(r) => json(200, &student_rollcalls_body(r, &user)),
            None => json(404, "{}"),
        };
    }
    if let Some(id) = rollcall_route(request_line, "GET", "answers") {
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id) {
            Some(_) => json(200, r#"{"answers":[],"last_timestamp":0}"#),
            None => json(404, "{}"),
        };
    }
    if let Some(id) = rollcall_route(request_line, "GET", "lite") {
        // Real lite carries NO target coordinate (docs/70 §1) — only beacon metadata.
        let st = state.lock().unwrap();
        return match st.rollcalls.iter().find(|r| r.id == id) {
            Some(r) => json(200, &format!(
                r#"{{"rollcall_id":"{}","use_beacon":{},"beacon_nonce":"{}"}}"#,
                r.id, r.use_beacon, r.beacon_nonce
            )),
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
            // R4.1 #2: signs-only expiry — the sign PUT alone serves a login page (poll/detect stay
            // healthy), so the SIGN path is what discovers the dead session. Per-user for the double-sign test.
            {
                let st = state.lock().unwrap();
                if st.sign_expired && (st.sign_expire_user.is_empty() || st.sign_expire_user == user) {
                    return html(200, "", LOGIN_PAGE);
                }
            }
            return answer(state, &id, kind, &user, body);
        }
    }

    json(404, "{}")
}

/// Build the real `student_rollcalls` object: a synthetic class roster (sized from `attendance_rate`)
/// plus the caller's own entry, and a top-level status that is `on_call_fine` only when present==total.
fn student_rollcalls_body(r: &Rollcall, user: &str) -> String {
    let present_syn = (r.attendance_rate.round() as i64).clamp(0, 100);
    let code_json = if r.hide_code { "null".to_string() } else { json_str_or_null(&r.number_code) };
    let mut entries: Vec<String> = (0..100)
        .map(|i| {
            let status = if (i as i64) < present_syn { "on_call_fine" } else { "in_progress" };
            format!(r#"{{"user_no":"c{i}","rollcall_status":"{status}","number_code":{code_json}}}"#)
        })
        .collect();
    // The caller's OWN entry — reflects their sign, so my_present is testable apart from top-level.
    let me_fine = r.signed.contains(user);
    let me_status = if me_fine { "on_call_fine" } else { "in_progress" };
    entries.push(format!(r#"{{"user_no":"{user}","rollcall_status":"{me_status}","number_code":{code_json}}}"#));
    let present = present_syn as usize + if me_fine { 1 } else { 0 };
    let total = entries.len(); // 101
    let top = if present == total { "on_call_fine" } else { "in_progress" };
    format!(r#"{{"status":"{top}","rollcallStatus":"{top}","student_rollcalls":[{}]}}"#, entries.join(","))
}

/// Apply a student answer; sign the user when the answer is correct for the type.
fn answer(state: &Arc<Mutex<FakeState>>, id: &str, kind: &str, user: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    let mut st = state.lock().unwrap();
    if kind == "radar" && body.contains("radarSignal") {
        st.saw_radar_signal = true; // beacon test: the answer carried a radarSignal
    }
    let Some(r) = st.rollcalls.iter_mut().find(|r| r.id == id) else { return json(404, "{}") };

    // number: real servers return distinguishable codes + a body success flag (docs 30 classifier).
    if kind == "number" {
        if r.number_fatal {
            return json(403, r#"{"error":"forbidden"}"#); // session invalid → fatal, abort the round
        }
        if r.throttle > 0 {
            r.throttle -= 1;
            return json(429, r#"{"error":"slow down"}"#); // transient → cooldown
        }
        return if v["numberCode"].as_str() == r.number_code.as_deref() {
            r.signed.insert(user.to_string());
            json(200, r#"{"success":true}"#)
        } else {
            json(400, r#"{"success":false}"#) // wrong code
        };
    }

    // radar: the solver must reverse-locate from distances; lite carries no target (docs/70 §1).
    if kind == "radar" {
        let has_coords = v.get("latitude").is_some();
        if !has_coords {
            // empty {} main path: passes unless the rollcall requires coordinates.
            if !r.requires_coords {
                r.signed.insert(user.to_string());
            }
            return json(200, r#"{"ok":true}"#);
        }
        if let (Some(lat), Some(lon), Some((tlat, tlng))) =
            (v["latitude"].as_f64(), v["longitude"].as_f64(), r.location)
        {
            let dist = crate::radar::haversine(
                crate::radar::GeoPoint { lat, lon },
                crate::radar::GeoPoint { lat: tlat, lon: tlng },
            );
            if dist <= r.scope_radius_m {
                r.signed.insert(user.to_string());
                return json(200, r#"{"success":true}"#);
            }
            // Out of scope → HTTP 200 + nested error envelope (docs/70 §1: 200-with-error_code).
            return json(200, &format!(
                r#"{{"error_code":"radar_out_of_rollcall_scope","data":{{"distance":{dist}}}}}"#
            ));
        }
        return json(200, r#"{"ok":true}"#);
    }

    let signed_now = match kind {
        "self_registration" => true,
        "qrcode" => v["data"].as_str() == Some(QR_TOKEN), // teacher-sourced portable data
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

/// `METHOD /api/{seg}/{id}/{suffix}[?..]` → id. `seg` may contain slashes (e.g. `courseware-quiz/quiz`).
fn api_suffix_id(rl: &str, method: &str, seg: &str, suffix: &str) -> Option<String> {
    let mut parts = rl.split_whitespace();
    if parts.next()? != method {
        return None;
    }
    let rest = parts.next()?.strip_prefix(&format!("/api/{seg}/"))?;
    let (id, tail) = rest.split_once('/')?;
    (tail.split('?').next()? == suffix).then(|| id.to_string())
}

/// `METHOD /api/{seg}/{id}[/..|?..]` → id (the first path segment after `seg`).
fn api_id(rl: &str, method: &str, seg: &str) -> Option<String> {
    let mut parts = rl.split_whitespace();
    if parts.next()? != method {
        return None;
    }
    let rest = parts.next()?.strip_prefix(&format!("/api/{seg}/"))?;
    let id = rest.split(['/', '?']).next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// First path segment after `prefix`.
fn path_seg(rl: &str, prefix: &str) -> String {
    rl.split_whitespace()
        .nth(1)
        .and_then(|p| p.strip_prefix(prefix))
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
        .to_string()
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


/// A minimal, VALID single-page PDF whose text layer is `text` (correct byte-offset xref so `pdf-extract`
/// parses it). `text` must not contain `(`/`)` (unescaped PDF string delimiters). Used to prove the R5
/// executor→pdf-extract→answer chain end-to-end.
pub fn minimal_pdf(text: &str) -> Vec<u8> {
    let stream = format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET");
    let objs = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>".to_string(),
        format!("<</Length {}>>stream\n{stream}\nendstream", stream.len()),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let xref_pos = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
    for off in &offsets {
        pdf.push_str(&format!("{off:010} 00000 n \n"));
    }
    pdf.push_str(&format!("trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_pos}\n%%EOF", objs.len() + 1));
    pdf.into_bytes()
}

/// The user question text from `messages[1]` — a plain string, or the concatenated text parts of a
/// multimodal parts list (R5).
fn llm_user_text(msgs: &[Value]) -> String {
    match msgs.get(1).map(|m| &m["content"]) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// The canned answer shared by the streaming + non-streaming fake-LLM paths (keeps existing tests green).
fn llm_canned(user_text: &str, calls: u32, fail_times: u32) -> String {
    if calls <= fail_times {
        String::new() // R3c: first N calls empty → subject unanswered → re-prepare
    } else if user_text.contains("\nA. ") {
        "A,B".to_string()
    } else if user_text.contains("' ||| '") {
        "aa ||| bb".to_string()
    } else {
        "canned answer".to_string()
    }
}

/// A non-streaming chat-completions message body carrying `content` (+ reasoning, so the tool loop emits
/// a ReasoningChunk per turn just like the streaming path).
fn llm_msg(content: &str) -> String {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content, "reasoning_content": "thinking" } }] }).to_string()
}

/// The request's `Host` header (so a served absolute url points back at this fake's port).
fn host_of(full: &str) -> String {
    full.lines()
        .find_map(|l| l.strip_prefix("Host:").or_else(|| l.strip_prefix("host:")))
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// A 302 redirect (reqwest follows it; `page.url()` then reflects `location`) — models an SSO handoff.
fn redirect(location: &str) -> String {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}
fn json(code: u16, body: &str) -> String {
    response(code, "application/json", "", body)
}
fn html(code: u16, extra_headers: &str, body: &str) -> String {
    response(code, "text/html; charset=utf-8", extra_headers, body)
}
fn response(code: u16, content_type: &str, extra_headers: &str, body: &str) -> String {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Not Found",
    };
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
