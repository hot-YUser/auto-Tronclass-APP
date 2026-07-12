//! R3a: auto-answer submit/fetch contracts over the FFI against the real-contract fake, plus pure
//! per-type body-shape assertions. The e2e (red gate) proves the wire shapes end-to-end; the pure
//! `answer.rs`/`quiz.rs` unit tests assert every builder field-by-field. Serialized via `SEQ`.

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
fn last_submission(base: &str) -> Value {
    serde_json::from_str(&http(base, "GET /_test/last_submission HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")).unwrap_or(Value::Null)
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
/// Boot one account monitoring, LLM pointed at the fake; returns (harness, base, account_id).
fn boot(tag: &str, base: &str) -> (Harness, String) {
    events().lock().unwrap().clear();
    let mut hz = Harness { h: crate::core_init(collect), id: 0 };
    let dir = std::env::temp_dir().join(format!("tron-r3a-{tag}-{}", new_id())).to_string_lossy().replace('\\', "/");
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

/// A leaked fill_in_blank must submit as `answers:[{sort,content}]`, not a flat string array.
#[test]
fn fill_submits_sort_content_objects() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("fill", &base);
    let quiz = r#"{"activity_id":"FILL1","course_id":"C1","source":"exam",
        "subjects":[{"id":"s1","type":"fill_in_blank","content":"cap __ of __","correct_answers":[{"sort":0,"content":"Paris"},{"sort":1,"content":"France"}]}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "FILL1" && v["account_id"].as_str() == Some(&dave), 25).is_some(), "submitted");
    let sub = last_submission(&base);
    let ans = &sub["subjects"][0]["answers"][0];
    assert!(ans["sort"].is_number(), "fill answers must be [{{sort,content}}] objects, got {sub}");
    assert_eq!(ans["content"], "Paris");
    drop(hz);
}

/// A flattened matching sub carries `parent_id` and its choice `answer_option_ids` in the submit body.
#[test]
fn matching_flattens_and_submits_parent_id() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("match", &base);
    // container with 2 left items + 4 options; block option o3 leaks via is_answer.
    let quiz = r#"{"activity_id":"MAT1","course_id":"C1","source":"exam","subjects":[
        {"id":"m","type":"matching","sub_subjects":[{"id":"L1"},{"id":"L2"}],
         "options":[{"id":"o1"},{"id":"o2"},{"id":"o3","is_answer":true},{"id":"o4"}]}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "MAT1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let subs = last_submission(&base);
    let arr = subs["subjects"].as_array().expect("subjects");
    let l2 = arr.iter().find(|s| s["subject_id"] == "L2").expect("L2 submitted as its own subject");
    assert_eq!(l2["parent_id"], "m", "matching sub carries parent_id");
    assert_eq!(l2["answer_option_ids"], serde_json::json!(["o3"]), "L2's block is_answer leak replayed");
    drop(hz);
}

/// resubmit-for-correct: fires only under the gate, re-distributes for the FRESH instance, the review
/// value WINS (option_ids member-validated → cross-block dropped; blanks with the review's raw sort).
#[test]
fn resubmit_overlay_review_wins_and_gate() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("resub", &base);
    // s1 choice (no leak → first pass LLM picks o1); s2 fill (first pass LLM). allow_retake=true.
    // review: s1 correct is o2 (+ a cross-block GHOST id to be discarded); s2 correct blank sort 3.
    let quiz = r#"{"activity_id":"RS1","course_id":"C1","source":"exam","allow_retake":true,
        "subjects":[
            {"id":"s1","type":"single_selection","content":"pick","options":[{"id":"o1"},{"id":"o2"}]},
            {"id":"s2","type":"fill_in_blank","content":"__"}],
        "review":[
            {"subject_id":"s1","answer_option_ids":["o2","GHOST"]},
            {"subject_id":"s2","answers":[{"sort":3,"content":"RIGHT"}]}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "RS1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let sub = last_submission(&base); // the RESUBMIT body (awaited before QuizSubmitted)
    assert_eq!(sub["exam_paper_instance_id"], "inst-1-retake", "resubmit targets the fresh retake instance");
    let arr = sub["subjects"].as_array().unwrap();
    let s1 = arr.iter().find(|s| s["subject_id"] == "s1").unwrap();
    assert_eq!(s1["answer_option_ids"], serde_json::json!(["o2"]), "review wins; cross-block GHOST discarded");
    let s2 = arr.iter().find(|s| s["subject_id"] == "s2").unwrap();
    assert_eq!(s2["answers"], serde_json::json!([{"sort":3,"content":"RIGHT"}]), "review blank wins with raw sort");
    drop(hz);
}

/// The resubmit gate: a single-attempt exam (allow_retake_exam:false) must NOT resubmit (or it burns
/// its one graded attempt). Proven by the submitted paper staying on the original instance.
#[test]
fn no_resubmit_when_not_retakeable() {
    let _g = SEQ.lock().unwrap();
    let base = start_fake();
    let (hz, dave) = boot("noretake", &base);
    let quiz = r#"{"activity_id":"NR1","course_id":"C1","source":"exam","allow_retake":false,
        "subjects":[{"id":"s1","type":"single_selection","options":[{"id":"o1"},{"id":"o2"}]}],
        "review":[{"subject_id":"s1","answer_option_ids":["o2"]}]}"#;
    post(&base, "/_test/open_quiz", quiz);
    assert!(wait_for(|v| v["event"] == "QuizSubmitted" && v["quiz_id"] == "NR1" && v["account_id"].as_str() == Some(&dave), 25).is_some());
    let sub = last_submission(&base);
    assert_eq!(sub["exam_paper_instance_id"], "inst-1", "no retake instance → resubmit did NOT fire");
    drop(hz);
}
