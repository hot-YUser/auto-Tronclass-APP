//! R3b: the LLM answerer contract (request body, option-letter rendering, reply parsing, correction
//! re-ask, strip_think) + the two R3a acceptance bugs (numeric matching sort; single-vote). Pure
//! parsing units live in quiz.rs/llm.rs; the e2e here drive the request/parse end-to-end over the FFI.

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
fn boot(tag: &str, base: &str) -> (Harness, String) {
    events().lock().unwrap().clear();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r3b-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/");
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{dir}"}}"#));
    wait_for(reply_ok(i), 10);
    let i = hz.next();
    send(hz.h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2,"quiz_detect_secs":1,"llm_endpoint":"{base}/v1/chat/completions"}}}}"#));
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

/// The LLM request must carry `chat_template_kwargs.thinking_mode == "enabled"` (string) and a system
/// message — else a reasoning model returns HTTP 200 + empty choices ("m3 returns nothing").
#[test]
fn llm_request_body_contract() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("body", &base);
    // A pending short_answer (no leak) forces an LLM call.
    let quiz = r#"{"activity_id":"Q1","course_id":"C1","source":"exam","subjects":[{"id":"s1","type":"short_answer","content":"why is the sky blue?"}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let req = get_json(&base, "/_test/last_llm_request");
    assert_eq!(req["chat_template_kwargs"]["thinking_mode"], "enabled", "thinking_mode must be the string \"enabled\", got {req}");
    assert_eq!(req["messages"][0]["role"], "system", "first message must be the system prompt");
    drop(hz);
}

/// End-to-end: a leak-free multiple_selection renders lettered options, the fake replies letters
/// ("A,B"), and those letters map back to the correct option IDS in the submit body (order preserved,
/// unmatched options excluded). Proves option-letter rendering + reply→id parsing over the wire.
#[test]
fn mc_letters_map_to_option_ids() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("mc", &base);
    // 3 options, no leak → the LLM answers; the fake replies "A,B" for a lettered prompt.
    let quiz = r#"{"activity_id":"MC1","course_id":"C1","source":"exam","subjects":[
        {"id":"s1","type":"multiple_selection","content":"pick two",
         "options":[{"id":"o1","content":"alpha"},{"id":"o2","content":"beta"},{"id":"o3","content":"gamma"}]}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "MC1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let sub = get_json(&base, "/_test/last_submission");
    assert_eq!(sub["subjects"][0]["answer_option_ids"], serde_json::json!(["o1", "o2"]), "A,B → first two option ids, got {sub}");
    drop(hz);
}

/// End-to-end: a >1-blank fill (no leak) appends the ' ||| ' hint, the fake replies "aa ||| bb", and
/// that splits into per-blank `answers:[{sort,content}]`. Proves the multi-blank hint + split path.
#[test]
fn multi_blank_fill_splits_to_answers() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("blanks", &base);
    // answer_number:2 → blank_count 2 → the hint carries ' ||| ' → the fake replies "aa ||| bb".
    let quiz = r#"{"activity_id":"BL1","course_id":"C1","source":"exam","subjects":[
        {"id":"s1","type":"fill_in_blank","answer_number":2,"content":"__ and __"}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "BL1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let sub = get_json(&base, "/_test/last_submission");
    assert_eq!(sub["subjects"][0]["answers"], serde_json::json!([{"sort":0,"content":"aa"},{"sort":1,"content":"bb"}]), "multi-blank split, got {sub}");
    drop(hz);
}

/// Bug B end-to-end: a SINGLE-choice vote must cast exactly ONE letter even when the LLM replies
/// several ("A,B"). `vote_type:"single"` → single_selection → capped to the first letter.
#[test]
fn single_vote_casts_one_letter() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("vote", &base);
    let quiz = r#"{"activity_id":"VT1","course_id":"C1","source":"vote","vote_type":"single",
        "vote_items":{"A":"apple","B":"banana"}}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "VT1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let sub = get_json(&base, "/_test/last_submission");
    let votes = sub["votes"].as_array().expect("votes array");
    assert_eq!(votes.len(), 1, "single vote casts exactly one letter, got {sub}");
    assert_eq!(votes[0], "A", "capped to the first letter");
    drop(hz);
}
