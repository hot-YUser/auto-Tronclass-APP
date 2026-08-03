//! Headless teacher-QR parity tests over two independent fake TronClass endpoints.

use crate::config::new_id;
use crate::fake;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn events() -> &'static Mutex<Vec<String>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

extern "C" fn collect(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    events()
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(bytes).into_owned());
}

fn snapshot() -> Vec<Value> {
    events()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| serde_json::from_str(event).ok())
        .collect()
}

fn wait_for<F: Fn(&Value) -> bool>(predicate: F, seconds: u64) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(event) = snapshot().into_iter().find(&predicate) {
            return Some(event);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn send(handle: *mut std::ffi::c_void, body: &str) {
    unsafe { crate::core_send(handle, body.as_ptr(), body.len()) };
}

fn reply_ok(id: u64) -> impl Fn(&Value) -> bool {
    move |event| event["event"] == "Reply" && event["id"] == id && event["ok"] == true
}

fn account_id(label: &str) -> String {
    snapshot()
        .iter()
        .rev()
        .find(|event| event["event"] == "Accounts")
        .and_then(|event| event["accounts"].as_array())
        .and_then(|accounts| accounts.iter().find(|account| account["label"] == label))
        .and_then(|account| account["id"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn start_fake() -> String {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (port, listener) = fake::bind_ephemeral().await;
            port_tx.send(port).unwrap();
            fake::serve(listener).await;
        });
    });
    format!("http://127.0.0.1:{}", port_rx.recv().unwrap())
}

fn request(base_url: &str, method: &str, path: &str, body: &str) -> Value {
    let mut stream = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    serde_json::from_str(response.rsplit("\r\n\r\n").next().unwrap_or("{}")).unwrap_or(Value::Null)
}

fn post(base_url: &str, path: &str, body: &str) {
    let _ = request(base_url, "POST", path, body);
}

fn stats(base_url: &str) -> Value {
    request(base_url, "GET", "/_test/qr_stats", "")
}

fn wait_stats<F: Fn(&Value) -> bool>(base_url: &str, predicate: F, seconds: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let value = stats(base_url);
        if predicate(&value) {
            return value;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stats(base_url)
}

fn open_qr(base_url: &str, id: &str) {
    post(
        base_url,
        "/_test/open_rollcall",
        &json!({"id": id, "kind": "qrcode", "attendance_rate": 100.0}).to_string(),
    );
}

#[test]
fn teacher_qr_cross_endpoint_auth_recovery_confirmation_and_cleanup() {
    events().lock().unwrap().clear();
    let teacher_base = start_fake();
    let student_base = start_fake();
    let data_dir = std::env::temp_dir().join(format!("tron-teacher-qr-{}", new_id()));
    let data_dir = data_dir.to_string_lossy().replace('\\', "/");
    let handle = crate::core_init(collect);
    let mut next_id = 0u64;
    let mut command = |body: String| {
        next_id += 1;
        let id = next_id;
        send(handle, &format!(r#"{{"id":{id},{body}}}"#));
        assert!(
            wait_for(reply_ok(id), 15).is_some(),
            "command {id} succeeds; events={:?}",
            snapshot()
        );
    };

    command(format!(r#""cmd":"Init","data_dir":"{data_dir}""#));
    command(r#""cmd":"UpdateConfig","patch":{"countdown_secs":0,"poll_idle_secs":1}"#.to_string());
    command(r#""cmd":"CreateVault","master_password":"pw""#.to_string());
    command(format!(
        r#""cmd":"AddAccount","label":"teacher","school":"{teacher_base}","username":"teacher","password":"secret","is_teacher":true,"course_id":"C1""#
    ));
    command(format!(
        r#""cmd":"AddAccount","label":"student","school":"{student_base}","username":"student","password":"secret""#
    ));
    let student_id = account_id("student");
    assert!(!student_id.is_empty());
    command(r#""cmd":"StartMonitoring""#.to_string());

    // Force the first teacher API call and first student QR PUT through independent login recovery.
    post(
        &teacher_base,
        "/_test/expire",
        r#"{"expired":true,"mode":"login_page"}"#,
    );
    post(
        &student_base,
        "/_test/expire_signs",
        r#"{"enabled":true,"user":"student"}"#,
    );
    open_qr(&student_base, "QR-X1");
    assert!(
        wait_for(
            |event| event["event"] == "SignedIn"
                && event["rollcall_id"] == "QR-X1"
                && event["account_id"] == student_id,
            25,
        )
        .is_some(),
        "student recovers, signs on its own endpoint, and is confirmed present"
    );

    // Three consecutive activities each get a fresh source and a matching cleanup.
    for id in ["QR-X2", "QR-X3"] {
        open_qr(&student_base, id);
        assert!(wait_for(
            |event| event["event"] == "SignedIn" && event["rollcall_id"] == id,
            20
        )
        .is_some());
    }
    let teacher_stats = wait_stats(
        &teacher_base,
        |value| value["teacher_stops"].as_u64().unwrap_or(0) >= 3,
        10,
    );
    let student_stats = stats(&student_base);
    assert_eq!(
        teacher_stats["teacher_create_payloads"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    assert_eq!(teacher_stats["teacher_starts"], 3);
    assert_eq!(teacher_stats["teacher_stops"], 3);
    assert!(student_stats["qr_answers"].as_u64().unwrap_or(0) >= 3);

    let payload = &teacher_stats["teacher_create_payloads"][0];
    assert_eq!(payload["type"], "qr_rollcall");
    assert_eq!(payload["number_code"], "");
    assert!(
        payload["latitude"].is_null()
            && payload["longitude"].is_null()
            && payload["altitude"].is_null()
    );
    assert_eq!(payload["duration"], 0);
    assert_eq!(payload["student_rollcalls"], json!([]));
    assert!(
        teacher_stats["rollcall_polls"].get("teacher").is_none(),
        "teacher accounts are not student pollers"
    );

    // A successful HTTP submission without roster confirmation must never emit SignedIn.
    post(
        &student_base,
        "/_test/qr_deny_confirmation",
        r#"{"enabled":true}"#,
    );
    open_qr(&student_base, "QR-UNCONFIRMED");
    assert!(wait_for(
        |event| event["event"] == "RollcallDetected" && event["rollcall_id"] == "QR-UNCONFIRMED",
        10
    )
    .is_some());
    assert!(wait_for(
        |event| event["event"] == "SignedIn" && event["rollcall_id"] == "QR-UNCONFIRMED",
        15
    )
    .is_none());
    let teacher_stats = wait_stats(
        &teacher_base,
        |value| value["teacher_stops"].as_u64().unwrap_or(0) >= 4,
        5,
    );
    assert_eq!(
        teacher_stats["teacher_create_payloads"]
            .as_array()
            .map(Vec::len),
        Some(4),
        "cooldown prevents immediate source churn"
    );

    // Hold token fetches transiently, then stop monitoring: cancellation still performs one cleanup.
    post(
        &student_base,
        "/_test/qr_deny_confirmation",
        r#"{"enabled":false}"#,
    );
    post(
        &teacher_base,
        "/_test/teacher_token_status",
        r#"{"status":503}"#,
    );
    open_qr(&student_base, "QR-CANCEL");
    let before_stop = wait_stats(
        &teacher_base,
        |value| {
            value["teacher_create_payloads"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
                >= 5
        },
        10,
    );
    assert_eq!(before_stop["teacher_stops"], 4);
    command(r#""cmd":"StopMonitoring""#.to_string());
    let after_stop = wait_stats(
        &teacher_base,
        |value| value["teacher_stops"].as_u64().unwrap_or(0) >= 5,
        5,
    );
    assert_eq!(
        after_stop["teacher_stops"], 5,
        "cancelled monitor closes the active teacher source"
    );

    let serialized_events = events().lock().unwrap().join("\n");
    assert!(
        !serialized_events.contains("QRDATA-XYZ"),
        "QR data never crosses logs or events"
    );
    assert!(
        !serialized_events.to_lowercase().contains("session=sk-"),
        "session cookies never cross logs or events"
    );
    unsafe { crate::core_free(handle) };
}
