//! R5 e2e: the course-material tool (handout PDF text flows into the answer) + multimodal images
//! (login-gated `<img>` base64-inlined into the request). Drives everything over the FFI against the
//! real-contract fake. Serialized via `SEQ`.

use crate::config::new_id;
use crate::fake;
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static SEQ: Mutex<()> = Mutex::new(());
static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn collect(ptr: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(ptr, len) };
    events().lock().unwrap().push(String::from_utf8_lossy(b).into_owned());
}
fn snapshot() -> Vec<Value> {
    events().lock().unwrap().iter().filter_map(|s| serde_json::from_str(s).ok()).collect()
}
fn wait_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(v) = snapshot().into_iter().find(&pred) {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["event"] == "Reply" && v["id"] == id
}
fn submitted(quiz_id: &str) -> impl Fn(&Value) -> bool + '_ {
    move |v| v["event"] == "QuizSubmitted" && v["quiz_id"] == quiz_id
}
fn send(h: *mut std::ffi::c_void, json: &str) {
    unsafe { crate::core_send(h, json.as_ptr(), json.len()) };
}
fn account_id(label: &str) -> Option<String> {
    for ev in snapshot().iter().rev() {
        if ev["event"] == "Accounts" {
            if let Some(a) = ev["accounts"].as_array()?.iter().find(|a| a["label"] == label) {
                return a["id"].as_str().map(str::to_string);
            }
        }
    }
    None
}
fn start_fake() -> String {
    let (ptx, prx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (port, listener) = fake::bind_ephemeral().await;
            ptx.send(port).unwrap();
            fake::serve(listener).await;
        });
    });
    format!("http://127.0.0.1:{}", prx.recv().unwrap())
}
fn http(base: &str, req: &str) -> String {
    let mut s = std::net::TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}
fn post(base: &str, path: &str, body: &str) {
    http(base, &format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body));
}
fn get_json(base: &str, path: &str) -> Value {
    serde_json::from_str(&http(base, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"))).unwrap_or(Value::Null)
}
struct Harness {
    h: *mut std::ffi::c_void,
    id: u64,
}
impl Harness {
    fn next(&mut self) -> u64 {
        self.id += 1;
        self.id
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        unsafe { crate::core_free(self.h) };
    }
}
/// Boot one account monitoring, with LLM tools on/off.
fn boot(tag: &str, base: &str, enable_tools: bool) -> (Harness, String) {
    events().lock().unwrap().clear();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r5-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2,"quiz_detect_secs":1,"poll_idle_secs":1,"max_answer_reask":1,"enable_llm_tools":{enable_tools},"llm_endpoint":"{base}/v1/chat/completions"}}}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"SetLlmKey","key":"k"}}"#));
    wait_for(reply_ok(i), 5);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#));
    wait_for(reply_ok(i), 5);
    let dave = account_id("dave").unwrap();
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    wait_for(reply_ok(i), 15);
    wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10);
    (hz, dave)
}

const HANDOUT_QUIZ: &str = r#"{"activity_id":"EXH","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"Per the handout, what is the keyword?"}]}"#;
const HANDOUT_MATERIAL: &str = r#"{"id":"MATPDF","course_id":"C1","title":"Photosynthesis Handout","pdf_upload_id":"up1","pdf_sentinel":"PHOTOSYNTHESIS42"}"#;

/// The answer lives ONLY in a handout PDF. With tools on, the model calls `search_course_materials`,
/// the executor extracts the PDF's text, and the sentinel flows into the submitted answer.
#[test]
fn handout_pdf_answer_flows_via_tool() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("pdf", &base, true);
    post(&base, "/_test/open_material", HANDOUT_MATERIAL);
    post(&base, "/_test/open_quiz", HANDOUT_QUIZ);
    assert!(wait_for(submitted("EXH"), 30).is_some(), "handout question submits");
    let sub = get_json(&base, "/_test/last_submission");
    assert_eq!(sub["subjects"][0]["answer"], "PHOTOSYNTHESIS42", "the PDF sentinel is the submitted answer, got {sub}");
    drop(hz);
}

/// Red contrast: with tools OFF, the same handout question can't reach the PDF → the answer is the
/// model's blind guess, NOT the sentinel (proving the tool is load-bearing).
#[test]
fn without_tools_handout_answer_is_not_the_sentinel() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("notool", &base, false);
    post(&base, "/_test/open_material", HANDOUT_MATERIAL);
    post(&base, "/_test/open_quiz", HANDOUT_QUIZ);
    assert!(wait_for(submitted("EXH"), 30).is_some(), "still submits (its blind guess)");
    let sub = get_json(&base, "/_test/last_submission");
    assert_ne!(sub["subjects"][0]["answer"], "PHOTOSYNTHESIS42", "no tool → cannot know the PDF sentinel");
    drop(hz);
}

/// Multimodal: a subject whose stem carries an `<img>` → the request user content is a parts list whose
/// image is a base64 `data:` url (the login-gated image was fetched + inlined).
#[test]
fn subject_image_is_inlined_as_base64_data_url() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("img", &base, true);
    let quiz = r#"{"activity_id":"IMG","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"What is shown? <img src=\"/_test/image.png\">"}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(submitted("IMG"), 30).is_some(), "image subject submits");
    let req = get_json(&base, "/_test/last_llm_request");
    let content = &req["messages"][1]["content"];
    assert!(content.is_array(), "multimodal → parts-list content, got {content}");
    let img = content.as_array().unwrap().iter().find(|p| p["type"] == "image_url").expect("an image part");
    let url = img["image_url"]["url"].as_str().unwrap_or("");
    assert!(url.starts_with("data:image/png;base64,"), "the image is inlined as a base64 data url, got {url}");
    drop(hz);
}

/// Multimodal fetch-miss: an `<img>` that 404s → fall back to the raw (resolved) url, not a data url.
#[test]
fn image_fetch_miss_falls_back_to_raw_url() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, _dave) = boot("imgmiss", &base, true);
    let quiz = r#"{"activity_id":"IMG2","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"See <img src=\"/_test/missing.png\">"}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(submitted("IMG2"), 30).is_some(), "submits despite the missing image");
    let req = get_json(&base, "/_test/last_llm_request");
    let content = &req["messages"][1]["content"];
    let img = content.as_array().and_then(|a| a.iter().find(|p| p["type"] == "image_url")).expect("an image part");
    let url = img["image_url"]["url"].as_str().unwrap_or("");
    assert!(url.ends_with("/_test/missing.png") && !url.starts_with("data:"), "fetch miss → raw url fallback, got {url}");
    drop(hz);
}
