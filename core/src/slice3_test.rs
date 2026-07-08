//! Slice-3 end-to-end (headless): detect an exam → prepare (fake LLM, streamed reasoning) → per-account
//! conflict → SetAnswer → countdown → auto-submit, over the FFI. Also proves LLM runs **once** per
//! activity (shared) even with two accounts, and that a blank/failed LLM subject is never submitted.

use crate::config::new_id;
use crate::fake;
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}
extern "C" fn collect(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    events().lock().unwrap().push(String::from_utf8_lossy(bytes).into_owned());
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
fn submitted(quiz_id: &str, account: &str) -> impl Fn(&Value) -> bool {
    let (q, a) = (quiz_id.to_string(), account.to_string());
    move |v| v["event"] == "QuizSubmitted" && v["quiz_id"] == q && v["account_id"].as_str() == Some(&a)
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

fn post(base_url: &str, path: &str, body: &str) -> String {
    let mut s = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}

#[test]
fn slice3_quiz_prepare_conflict_and_submit() {
    let base = start_fake();
    let data_dir = std::env::temp_dir().join(format!("tron-slice3-{}", new_id()));
    let data_dir = data_dir.to_string_lossy().replace('\\', "/");
    let h = crate::core_init(collect);
    let mut id = 0u64;
    let mut next = || {
        id += 1;
        id
    };

    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"Init","data_dir":"{data_dir}"}}"#));
    assert!(wait_for(reply_ok(i), 10).is_some());
    // fast countdown + quick quiz detection + point the LLM at the fake's endpoint
    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"UpdateConfig","patch":{{"countdown_secs":2,"quiz_detect_secs":1,"llm_endpoint":"{base}/v1/chat/completions"}}}}"#));
    wait_for(reply_ok(i), 5);
    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"CreateVault","master_password":"pw"}}"#));
    wait_for(reply_ok(i), 5);
    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"SetLlmKey","key":"fake-key"}}"#));
    assert!(wait_for(reply_ok(i), 5).unwrap()["ok"] == true, "SetLlmKey");

    for label in ["alice", "bob"] {
        let i = next();
        send(h, &format!(r#"{{"id":{i},"cmd":"AddAccount","label":"{label}","school":"{base}","username":"{label}","password":"secret"}}"#));
        wait_for(reply_ok(i), 5);
    }
    let alice = account_id("alice").unwrap();
    let bob = account_id("bob").unwrap();

    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"StartMonitoring"}}"#));
    assert!(wait_for(reply_ok(i), 15).is_some());
    wait_for(|v| v["event"] == "AccountStatus" && v["state"] == "online", 10);

    // Open an exam with two subjects: s1 (selection, LLM → option "o1"), s2 (short_answer, LLM → text).
    // bob has an EXISTING answer on s1 = "o2" → a per-account conflict (o2 ≠ LLM's o1). alice: none.
    let quiz = r#"{"activity_id":"EX1","course_id":"C1","course_name":"Math",
        "subjects":[{"id":"s1","type":"single_selection","content":"pick","options":[{"id":"o1","content":"A"},{"id":"o2","content":"B"}]},
                    {"id":"s2","type":"short_answer","content":"why"}],
        "existing":{"bob":{"s1":{"options":["o2"]}}}}"#;
    post(&base, "/_test/open_quiz", quiz);

    // Prepared with bob's conflict; reasoning streamed for the LLM subjects.
    let prepared = wait_for(|v| v["event"] == "QuizPrepared" && v["quiz_id"] == "EX1", 20).expect("QuizPrepared");
    assert!(prepared["conflict_count"].as_u64().unwrap_or(0) >= 1, "bob has a conflict on s1");
    assert!(wait_for(|v| v["event"] == "ReasoningChunk" && v["quiz_id"] == "EX1", 5).is_some(), "reasoning streamed");
    // alice (no conflict) should NOT auto-submit while bob's conflict is unresolved.
    assert!(wait_for(submitted("EX1", &alice), 4).is_none(), "no submit while a conflict is unresolved");

    // Resolve bob's conflict → countdown → both submit.
    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"SetAnswer","quiz_id":"EX1","account_id":"{bob}","subject_id":"s1","answer":{{"options":["o1"]}}}}"#));
    wait_for(reply_ok(i), 5);
    assert!(wait_for(submitted("EX1", &alice), 15).is_some(), "alice submits after resolution");
    assert!(wait_for(submitted("EX1", &bob), 15).is_some(), "bob submits after resolution");

    // LLM ran ONCE per pending subject (s1, s2), shared across both accounts → 2 calls, not 4.
    let calls: Value = serde_json::from_str(&fetch(&base, "/_test/llm_calls")).unwrap_or(Value::Null);
    assert_eq!(calls["count"].as_u64().unwrap_or(99), 2, "LLM shared: 2 calls for 2 subjects (not per-account)");

    let i = next();
    send(h, &format!(r#"{{"id":{i},"cmd":"StopMonitoring"}}"#));
    wait_for(reply_ok(i), 5);
    unsafe { crate::core_free(h) };
}

fn fetch(base_url: &str, path: &str) -> String {
    let mut s = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}
