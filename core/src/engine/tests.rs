use super::accounts::*;
use super::monitoring::*;
use super::session::*;
use super::state::*;
use super::*;
use crate::config::{new_id, AccountMeta, Config, ScheduleBinding, Settings};
use crate::monitor;
use crate::persistence::{AccountJournal, AccountMutation};
use crate::providers::Registry;
use crate::secrets::{AccountSecret, VaultFile};
use crate::supervisor::TargetSupervisor;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::oneshot;

static TEST_EVENTS: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();

extern "C" fn collect_event(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    if let Ok(value) = serde_json::from_slice(bytes) {
        TEST_EVENTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(value);
    }
}

#[test]
fn monitor_lifecycle_rejects_duplicate_start_while_starting() {
    let mut lifecycle = MonitorLifecycle::Idle;

    assert!(lifecycle.begin_start(1, 101).is_ok());
    assert_eq!(
        lifecycle.begin_start(2, 102),
        Err("monitor is already starting")
    );
    assert!(lifecycle.is_starting(1));
}

#[tokio::test]
async fn school_client_refuses_cross_origin_private_redirects_but_follows_same_origin() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A public school must never be able to redirect the authed client into a private host:
    // 302 → http://127.0.0.1:<other-port> is cross-origin AND a loopback literal → refused.
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    let (hit_tx, hit_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let accepted = target.accept().await.is_ok();
        let _ = hit_tx.send(accepted);
    });
    let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_addr = source.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = source.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await;
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let (client, _jar) = build_client("").unwrap();
    let result = client
        .get(format!("http://{source_addr}/start"))
        .send()
        .await;
    assert!(
        result.is_err(),
        "a cross-origin redirect to a loopback literal must be refused"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), hit_rx)
            .await
            .is_err(),
        "the private redirect target must never be contacted"
    );

    // Same-origin redirects (an intranet school redirecting within itself) keep working.
    let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_addr = source.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = source.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await;
        let response = "HTTP/1.1 302 Found\r\nLocation: /landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(response.as_bytes()).await.unwrap();
        let (mut stream2, _) = source.accept().await.unwrap();
        let mut request2 = [0_u8; 4096];
        let _ = stream2.read(&mut request2).await;
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        stream2.write_all(ok.as_bytes()).await.unwrap();
    });
    let (client, _jar) = build_client("").unwrap();
    let response = client
        .get(format!("http://{source_addr}/start"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        200,
        "same-origin redirects are followed"
    );
}

#[test]
fn cancelling_start_invalidates_stale_generation() {
    let mut lifecycle = MonitorLifecycle::Idle;
    lifecycle.begin_start(1, 101).unwrap();

    match lifecycle.take_for_stop() {
        StoppedMonitor::Starting(starting) => {
            assert_eq!(starting.command_id, 101);
            assert!(starting.task.is_none());
        }
        _ => panic!("starting lifecycle must be cancelled as Starting"),
    }

    lifecycle.begin_start(2, 102).unwrap();
    assert!(
        !lifecycle.is_starting(1),
        "generation 1 completion is stale"
    );
    assert!(
        lifecycle.is_starting(2),
        "generation 2 remains authoritative"
    );
}

#[test]
fn running_handle_is_removed_exactly_once() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut lifecycle = MonitorLifecycle::Running(RunningMonitor {
        generation: 7,
        handle: monitor::MonitorHandle::new(tx),
        loaded_account_ids: HashSet::new(),
        loading_account_ids: HashSet::new(),
    });

    assert!(lifecycle.running_handle().is_some());
    assert!(matches!(
        lifecycle.take_for_stop(),
        StoppedMonitor::Running(_)
    ));
    assert!(matches!(lifecycle.take_for_stop(), StoppedMonitor::Idle));
}

#[test]
fn stopping_during_start_completes_pending_command() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let mut lifecycle = MonitorLifecycle::Idle;
    lifecycle.begin_start(1, 501).unwrap();

    stop_monitor(&mut lifecycle, collect_event, "cancelled by test");

    let events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(events.iter().any(|event| {
        event["event"] == "Reply"
            && event["id"] == 501
            && event["ok"] == false
            && event["error"] == "cancelled by test"
    }));
    assert!(matches!(lifecycle, MonitorLifecycle::Idle));
}

#[test]
fn corrupt_vault_is_preserved_instead_of_recreated() {
    let dir = std::env::temp_dir().join(format!("tron-corrupt-vault-{}", crate::config::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();
    drop(VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap());
    let corrupt = b"existing vault evidence".to_vec();
    std::fs::write(dir.join("vault.bin"), &corrupt).unwrap();

    assert!(open_vault_auto(&dir, None).is_err());
    assert_eq!(
        std::fs::read(dir.join("vault.bin")).unwrap(),
        corrupt,
        "opening a corrupt vault must never replace its bytes"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn supplied_os_key_never_creates_a_plaintext_device_key_file() {
    let dir = std::env::temp_dir().join(format!("tron-os-key-vault-{}", crate::config::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = std::array::from_fn::<_, 32, _>(|index| index as u8);

    drop(open_vault_auto(&dir, Some(key)).unwrap());
    assert!(!dir.join("device.key").exists());
    drop(open_vault_auto(&dir, Some(key)).unwrap());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_os_key_fails_init_instead_of_reporting_boot_success() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    const COMMAND_ID: u64 = 9_223_372_036_854_700_001;
    let dir = std::env::temp_dir().join(format!(
        "tron-invalid-os-key-init-{}",
        crate::config::new_id()
    ));
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(
            r#"{{"id":{COMMAND_ID},"cmd":"Init","data_dir":{},"device_key_b64":"invalid"}}"#,
            serde_json::to_string(&dir).unwrap()
        )
        .as_bytes(),
    );

    let events = TEST_EVENTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(events.iter().any(|event| {
        event["event"] == "Reply"
            && event["id"] == COMMAND_ID
            && event["ok"] == false
            && event["error"]
                .as_str()
                .is_some_and(|error| error.contains("device key"))
    }));
    assert!(!dir.join("device.key").exists());
    drop(events);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn account_transaction_recovery_is_idempotent_for_add_and_delete() {
    for (mutation, account_exists, expect_secret) in [
        (AccountMutation::Add, false, false),
        (AccountMutation::Add, true, true),
        (AccountMutation::Delete, false, false),
        (AccountMutation::Delete, true, true),
    ] {
        let dir =
            std::env::temp_dir().join(format!("tron-account-recovery-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        vault
            .set(
                "acc",
                AccountSecret {
                    password: "secret".into(),
                    cookies: String::new(),
                },
            )
            .unwrap();
        let mut config = Config::default();
        if account_exists {
            config.accounts.push(AccountMeta {
                id: "acc".into(),
                label: "account".into(),
                school_ref: "school".into(),
                username: "user".into(),
                device_id: "device".into(),
                is_teacher: false,
                course_id: None,
                schedule: ScheduleBinding::Disabled,
            });
        }
        AccountJournal::begin(&dir, mutation, "acc").unwrap();

        assert!(recover_account_transaction(&dir, &config, &mut vault)
            .unwrap()
            .is_some());
        assert_eq!(vault.get("acc").is_some(), expect_secret);
        assert!(AccountJournal::load(&dir).unwrap().is_none());
        assert!(recover_account_transaction(&dir, &config, &mut vault)
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn core_init_with_null_callback_returns_null_handle() {
    let handle = crate::core_init(None);
    assert!(
        handle.is_null(),
        "null callback must yield a null handle, never a live core"
    );
}

#[test]
fn poisoned_state_lock_recovers_instead_of_wedging_the_core() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let core = init(collect_event).unwrap();

    // A panic while holding the state lock poisons it exactly like a caught seam panic would.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = core.state.lock().unwrap();
        panic!("deliberate test poison");
    }))
    .ok();

    const ID: u64 = 424_242;
    send(
        &core,
        format!(r#"{{"id":{ID},"cmd":"CreateVault"}}"#).as_bytes(),
    );
    let events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reply = events
        .iter()
        .find(|e| e["event"] == "Reply" && e["id"] == ID)
        .expect("a command must complete despite a poisoned lock");
    assert_eq!(reply["ok"], false);
    assert_eq!(
        reply["error"], "vault not ready",
        "the command must run normally (poison recovered), not fail with the seam panic error"
    );
}

#[test]
fn malformed_command_errors_never_echo_input_payloads() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let core = init(collect_event).unwrap();

    // A numeric secret-shaped payload in a sensitive field: serde fails on `key`, and the seam
    // must not reflect the value (the old message embedded the serde literal, echoing the number).
    send(&core, br#"{"id":77,"cmd":"SetLlmKey","key":123456789}"#);
    // Truly malformed JSON without a recoverable id → uncorrelated Error event, still no echo.
    send(&core, br#"{"cmd":"SetLlmKey","key":987654321"#);

    let events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reply = events
        .iter()
        .find(|e| e["event"] == "Reply" && e["id"] == 77)
        .expect("correlated reply for the id-recoverable command");
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], MALFORMED_COMMAND);
    let serialized = events.iter().map(|e| e.to_string()).collect::<String>();
    assert!(
        !serialized.contains("123456789"),
        "reply must not echo the numeric secret payload"
    );
    assert!(
        !serialized.contains("987654321"),
        "error event must not echo the numeric secret payload"
    );
}

#[test]
fn panic_reply_completes_command_with_fixed_error_and_never_echoes_input() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let events = TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let core = init(collect_event).unwrap();

    panic_reply(
        &core,
        br#"{"id":88,"cmd":"SetLlmKey","key":"super-secret-llm-key"}"#,
    );

    let events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reply = events
        .iter()
        .find(|e| e["event"] == "Reply" && e["id"] == 88)
        .expect("correlated reply completes the awaiting command");
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], INTERNAL_ERROR);
    assert!(
        !events
            .iter()
            .any(|e| e.to_string().contains("super-secret-llm-key")),
        "panic reply must never echo the input"
    );
}

#[test]
fn init_parse_errors_never_echo_file_content() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Each corrupt file gets its own Init: a providers error short-circuits the arm before the
    // config is even loaded. Both files hold a bare string — serde's error literal would echo it
    // verbatim if it crossed the seam.
    for (file, marker, code) in [
        (
            "providers.json",
            "PROVIDER-SECRET-ECHO",
            "providers_unavailable",
        ),
        ("config.json", "CONFIG-SECRET-ECHO", "config_unavailable"),
    ] {
        TEST_EVENTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let dir = std::env::temp_dir().join(format!(
            "tron-init-echo-{}-{}",
            file,
            crate::config::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), format!("\"{marker}\"")).unwrap();

        let core = init(collect_event).unwrap();
        const ID: u64 = 424_243;
        send(
            &core,
            format!(
                r#"{{"id":{ID},"cmd":"Init","data_dir":{}}}"#,
                serde_json::to_string(&dir).unwrap()
            )
            .as_bytes(),
        );

        let events = TEST_EVENTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = events
            .iter()
            .find(|e| e["code"] == code)
            .unwrap_or_else(|| panic!("corrupt {file} surfaces a {code} event"));
        assert!(
            !event["message"].as_str().unwrap_or("").contains(marker),
            "{code} must not echo file content"
        );
        drop(events);
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ===================== lifecycle / config contract tests =====================
// The e2e tests below share the global TEST_EVENTS sink, so they serialize among themselves.
static LIFECYCLE_SEQ: Mutex<()> = Mutex::new(());

fn events() -> &'static Mutex<Vec<Value>> {
    TEST_EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn wait_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> Option<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if let Some(v) = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|v| pred(v))
            .cloned()
        {
            return Some(v);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

fn none_for<F: Fn(&Value) -> bool>(pred: F, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(&pred)
        {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    true
}

fn start_fake() -> String {
    let (ptx, prx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let (port, listener) = crate::fake::bind_ephemeral().await;
            ptx.send(port).unwrap();
            crate::fake::serve(listener).await;
        });
    });
    format!("http://127.0.0.1:{}", prx.recv().unwrap())
}

fn post(base_url: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(base_url.trim_start_matches("http://")).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf.rsplit("\r\n\r\n").next().unwrap_or("").to_string()
}

fn account_id_by_label(label: &str) -> Option<String> {
    let guard = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::test_support::account_id(&guard, label)
}

fn data_dir(tag: &str) -> String {
    std::env::temp_dir()
        .join(format!("tron-engine-{tag}-{}", new_id()))
        .to_string_lossy()
        .replace('\\', "/")
}

fn test_state(
    data_dir: PathBuf,
    config: Config,
    vault: Option<VaultFile>,
) -> Arc<Mutex<Option<CoreState>>> {
    let registry = Registry::load_or_seed(&data_dir.join("providers.json"))
        .unwrap()
        .registry;
    Arc::new(Mutex::new(Some(CoreState {
        data_dir,
        registry,
        config,
        vault,
        monitor: MonitorLifecycle::Idle,
        monitor_generation: 0,
        supervisor: TargetSupervisor::new(None),
        pending_captcha: HashMap::new(),
        login_in_flight: HashSet::new(),
    })))
}

#[test]
fn active_target_only_blocks_its_accounts_and_legacy_switch_is_removed() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let base = start_fake();
    let dir = data_dir("target-account-guard");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    for (id, label) in [(2, "a"), (3, "b")] {
        send(
            &core,
            format!(
                r#"{{"id":{id},"cmd":"AddAccount","label":"{label}","school":"{base}","username":"{label}","password":"secret"}}"#
            )
            .as_bytes(),
        );
        assert!(wait_for(
            |v| v["event"] == "Reply" && v["id"] == id && v["ok"] == true,
            5
        )
        .is_some());
    }
    let a = account_id_by_label("a").expect("account a");
    let b = account_id_by_label("b").expect("account b");
    let event_snapshot = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let definition = crate::test_support::latest_monitoring_snapshot(&event_snapshot).unwrap();
    send(
        &core,
        crate::test_support::apply_clock_command(4, definition).as_bytes(),
    );
    send(
        &core,
        crate::test_support::start_account_command(5, &a).as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == true,
        10
    )
    .is_some());
    assert!(wait_for(
        |v| crate::test_support::event_account_login_state(v, &a, "online"),
        10
    )
    .is_some());

    send(
        &core,
        format!(r#"{{"id":6,"cmd":"Login","account_id":"{b}"}}"#).as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "LoginResult" && v["id"] == 6 && v["ok"] == true,
            10
        )
        .is_some(),
        "unrelated account may be re-verified"
    );

    send(
        &core,
        format!(r#"{{"id":7,"cmd":"Login","account_id":"{a}"}}"#).as_bytes(),
    );
    let login_guard =
        wait_for(|v| v["event"] == "Reply" && v["id"] == 7, 5).expect("active-account login guard");
    assert_eq!(login_guard["ok"], false);
    assert_eq!(login_guard["error"], "account_is_in_use");

    let current = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let revision = crate::test_support::latest_monitoring_snapshot(&current).unwrap()
        ["config_revision"]
        .as_u64()
        .unwrap();
    send(
        &core,
        format!(
            r#"{{"id":8,"cmd":"DeleteAccount","account_id":"{a}","expected_revision":{revision},"remove_from_groups":true}}"#
        )
        .as_bytes(),
    );
    let delete_guard = wait_for(|v| v["event"] == "Reply" && v["id"] == 8, 5)
        .expect("active-account delete guard");
    assert_eq!(delete_guard["ok"], false);
    assert_eq!(delete_guard["error"], "account_is_in_use");

    send(
        &core,
        format!(
            r#"{{"id":9,"cmd":"AddAccount","label":"c","school":"{base}","username":"c","password":"secret"}}"#
        )
        .as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "Reply" && v["id"] == 9 && v["ok"] == true,
            5
        )
        .is_some(),
        "unrelated account remains manageable"
    );

    send(
        &core,
        format!(r#"{{"id":10,"cmd":"SwitchAccount","account_id":"{a}"}}"#).as_bytes(),
    );
    let removed =
        wait_for(|v| v["event"] == "Reply" && v["id"] == 10, 5).expect("removed command reply");
    assert_eq!(removed["ok"], false);
    assert_eq!(removed["error"], MALFORMED_COMMAND);
}

#[test]
fn login_progress_is_published_only_through_monitoring_snapshots() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let base = start_fake();
    let dir = data_dir("login-status");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
    send(
        &core,
        format!(
            r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"test","password":"secret"}}"#
        )
        .as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
    assert!(wait_for(|v| crate::test_support::event_has_account(v, "dave"), 5).is_some());
    let acc = account_id_by_label("dave").expect("account id");

    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    send(
        &core,
        format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "LoginResult" && v["id"] == 4 && v["ok"] == true,
            15
        )
        .is_some(),
        "login succeeds against the fake"
    );

    let events = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let logging_in = events.iter().position(|event| {
        crate::test_support::event_account_login_state(event, &acc, "logging_in")
    });
    let online = events
        .iter()
        .position(|event| crate::test_support::event_account_login_state(event, &acc, "online"));
    assert!(
        logging_in.is_some(),
        "login starts with a full snapshot containing logging_in"
    );
    assert!(
        online.is_some(),
        "login ends with a full snapshot containing online"
    );
    assert!(
        logging_in.unwrap() < online.unwrap(),
        "logging_in must precede online"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event["event"].as_str(),
            Some("StateChanged" | "Accounts" | "AccountStatus")
        )),
        "removed account/global authority events must never return"
    );
    drop(events);
}

#[test]
fn deleting_an_account_cancels_its_pending_captcha_login_without_resurrecting_the_secret() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    // Real e2e against the fake server: the captcha challenge parks the Login task, and
    // DeleteAccount must cancel it — no injected state, no sleeps. The LoginResult is the
    // task's LAST event, so receiving it proves the completion (and any persist) is done.
    let base = start_fake();
    let dir = data_dir("captcha-cancel");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
    post(
        &base,
        "/_test/captcha",
        r#"{"required":true,"expected":"A1B2"}"#,
    );
    send(
        &core,
        format!(
            r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#
        )
        .as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 3, 5).is_some());
    assert!(wait_for(|v| crate::test_support::event_has_account(v, "dave"), 5).is_some());
    let acc = account_id_by_label("dave").expect("account id");

    send(
        &core,
        format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
            10
        )
        .is_some(),
        "login blocks on the captcha challenge"
    );
    send(
        &core,
        format!(r#"{{"id":5,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
    );
    let rejected = wait_for(
        |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == false,
        5,
    )
    .expect("second login rejected");
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("already in progress"),
        "second login must be single-flight rejected"
    );

    // Delete cancels the pending captcha: the awaiting login task wakes and reports honestly.
    let current = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let revision = crate::test_support::latest_monitoring_snapshot(&current).unwrap()
        ["config_revision"]
        .as_u64()
        .unwrap();
    send(
        &core,
        format!(
            r#"{{"id":6,"cmd":"DeleteAccount","account_id":"{acc}","expected_revision":{revision},"remove_from_groups":true}}"#
        )
        .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 6 && v["ok"] == true,
        5
    )
    .is_some());
    let cancelled = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 4, 10)
        .expect("cancelled login result");
    assert_eq!(cancelled["ok"], false);
    assert!(
        cancelled["reason"].as_str().unwrap().contains("cancelled"),
        "the deleted-account login must report cancellation: {}",
        cancelled["reason"]
    );

    // The stale async completion must not have written anything back: the vault has no entry
    // (LoginResult was the task's terminal event, so no straggler write can still land).
    let key =
        crate::secrets::load_or_create_device_key(&std::path::Path::new(&dir).join("device.key"))
            .unwrap();
    let vault = crate::secrets::VaultFile::unlock_with_key(
        &std::path::Path::new(&dir).join("vault.bin"),
        key,
    )
    .unwrap();
    assert!(
        vault.get(&acc).is_none(),
        "deleted account's secret must not be resurrected"
    );
    {
        let guard = lock_state(&core.state);
        let state = guard.as_ref().expect("initialized state");
        assert!(!state.login_in_flight.contains(&acc));
        assert!(!state
            .config
            .accounts
            .iter()
            .any(|account| account.id == acc));
    }
}

#[test]
fn stale_login_completion_never_resurrects_a_deleted_account() {
    let dir = std::env::temp_dir().join(format!("tron-stale-persist-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = crate::secrets::load_or_create_device_key(&dir.join("device.key")).unwrap();

    // Account present → the async completion persists and updates the cached session.
    let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
    vault
        .set(
            "acc",
            AccountSecret {
                password: "pw".into(),
                cookies: String::new(),
            },
        )
        .unwrap();
    let mut config = Config::default();
    config.accounts.push(AccountMeta {
        id: "acc".into(),
        label: "account".into(),
        school_ref: "school".into(),
        username: "user".into(),
        device_id: "device".into(),
        is_teacher: false,
        course_id: None,
        schedule: ScheduleBinding::Disabled,
    });
    let state = test_state(dir.clone(), config, Some(vault));
    assert!(matches!(
        persist_login_session(
            &state,
            "acc",
            AccountSecret {
                password: "pw".into(),
                cookies: "session=NEW".into()
            }
        ),
        Ok(PersistOutcome::Saved)
    ));
    {
        let guard = state.lock().unwrap();
        let st = guard.as_ref().unwrap();
        assert_eq!(
            st.vault.as_ref().unwrap().get("acc").unwrap().cookies,
            "session=NEW"
        );
    }

    // Deleted account (gone from config AND vault): the same stale completion must be refused
    // and must not write anything back.
    let gone_dir = dir.join("gone");
    std::fs::create_dir_all(&gone_dir).unwrap();
    let gone = test_state(
        gone_dir.clone(),
        Config::default(),
        Some(VaultFile::create_with_key(&gone_dir.join("vault.bin"), key).unwrap()),
    );
    assert!(matches!(
        persist_login_session(
            &gone,
            "acc",
            AccountSecret {
                password: "pw".into(),
                cookies: "session=STALE".into()
            }
        ),
        Ok(PersistOutcome::AccountGone)
    ));
    let guard = gone.lock().unwrap();
    assert!(
        guard
            .as_ref()
            .unwrap()
            .vault
            .as_ref()
            .unwrap()
            .get("acc")
            .is_none(),
        "stale completion must not write the deleted account's secret"
    );
}

#[test]
fn single_field_patch_cannot_break_cross_field_invariants() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("xfield");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    // Materialize config.json (Init does not write when the file is missing).
    send(
        &core,
        r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2}}"#.as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
        5
    )
    .is_some());

    // With the remote address cleared, switching the remote on is incoherent. The patch that does
    // so names one field only, so the rule must be checked against the value already stored.
    send(
        &core,
        r#"{"id":3,"cmd":"UpdateConfig","patch":{"qr_remote_base_url":""}}"#.as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
        5
    )
    .is_some());
    send(
        &core,
        r#"{"id":4,"cmd":"UpdateConfig","patch":{"qr_remote_enabled":true}}"#.as_bytes(),
    );
    let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 4, 5).expect("reply");
    assert_eq!(reply["ok"], false);
    assert!(reply["error"]
        .as_str()
        .unwrap()
        .contains("qr_remote_base_url"));

    // The refusal left the disk untouched.
    let config_path = std::path::Path::new(&dir).join("config.json");
    let on_disk = || -> Value {
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap()
    };
    assert_eq!(on_disk()["settings"]["qr_remote_enabled"], false);

    // A coherent pair still applies.
    send(
        &core,
        r#"{"id":5,"cmd":"UpdateConfig","patch":{"qr_remote_enabled":true,"qr_remote_base_url":"https://qr.example"}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 5 && v["ok"] == true,
        5
    )
    .is_some());
    assert_eq!(on_disk()["settings"]["qr_remote_enabled"], true);
    assert_eq!(
        on_disk()["settings"]["qr_remote_base_url"],
        "https://qr.example"
    );
}

#[test]
fn update_config_rejects_malformed_or_unknown_fields_without_touching_disk() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("badpatch");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    // Materialize config.json, then freeze its bytes as the untouched baseline.
    send(
        &core,
        r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2}}"#.as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
        5
    )
    .is_some());
    let config_path = std::path::Path::new(&dir).join("config.json");
    let before = std::fs::read(&config_path).unwrap();

    let bad_patches = [
        r#"{"operating":5}"#,
        r#"{"operating":{"days":[{"weekday":"monday"}]}}"#,
        r#"{"countdown_secs":"fast"}"#,
        r#"{"countdown_secs":true}"#,
        r#"{"autoanswer_types":["exam",5]}"#,
        r#"{"autoanswer_types":["exam","exam"]}"#,
        r#"{"autoanswer_types":["exam","future_kind"]}"#,
        r#"{"radar_strategy":[]}"#,
        r#"{"radar_strategy":["empty_answer",""]}"#,
        r#"{"radar_strategy":["global_wgs84","global_wgs84"]}"#,
        r#"{"radar_strategy":["future_strategy"]}"#,
        r#"{"llm_endpoint":"not-a-url"}"#,
        r#"{"llm_endpoint":"ftp://example.com/v1"}"#,
        r#"{"llm_endpoint":""}"#,
        r#"{"llm_model":"   "}"#,
        r#"{"log_level":"verbose"}"#,
        r#"{"attendance_gate_percent":150}"#,
    ];
    for (index, patch) in bad_patches.iter().enumerate() {
        let id = 3 + index as u64;
        send(
            &core,
            format!(r#"{{"id":{id},"cmd":"UpdateConfig","patch":{patch}}}"#).as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == id, 5)
            .unwrap_or_else(|| panic!("no reply for patch {patch}"));
        assert_eq!(reply["ok"], false, "patch {patch} must be rejected");
    }
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "failed patches must not touch the file"
    );

    // An unknown key is rejected and names the key...
    send(
        &core,
        r#"{"id":99,"cmd":"UpdateConfig","patch":{"bogus_field":1}}"#.as_bytes(),
    );
    let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 99, 5).expect("unknown key reply");
    assert_eq!(reply["ok"], false);
    assert!(
        reply["error"].as_str().unwrap().contains("bogus_field"),
        "unknown key must be named: {}",
        reply["error"]
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "unknown key must not touch the file"
    );
    // ...and a valid patch still applies afterwards. Because validation covers the complete clone,
    // this also proves none of the failed semantic patches leaked into memory. Zero max_tokens is
    // a supported persisted sentinel; llm::resolve_max_tokens keeps resolving it to 16384.
    send(
        &core,
        r#"{"id":100,"cmd":"UpdateConfig","patch":{"poll_idle_secs":7,"llm_max_tokens":0}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 100 && v["ok"] == true,
        5
    )
    .is_some());
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(on_disk["settings"]["poll_idle_secs"], 7);
    assert_eq!(on_disk["settings"]["llm_max_tokens"], 0);
    assert_eq!(
        on_disk["settings"]["llm_model"],
        Settings::default().llm_model
    );
    assert_eq!(
        on_disk["settings"]["autoanswer_types"],
        json!(Settings::default().autoanswer_types)
    );
    assert_eq!(
        on_disk["settings"]["radar_strategy"],
        json!(Settings::default().radar_strategy)
    );
}

// ===================== async panic backstop / monitor watchdog =====================

#[test]
fn async_panic_observer_turns_panic_into_backstop_and_ignores_cancel() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // A panicking task triggers the backstop exactly once.
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let handle = spawn_observed(
        &rt,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        },
        async { panic!("deliberate test panic") },
    );
    rt.block_on(handle).unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // A successful task does not trigger the backstop.
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let handle = spawn_observed(
        &rt,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        },
        async {},
    );
    rt.block_on(handle).unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // Aborting observed work is cancellation, not a panic; the aborting command owns completion.
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let handle = spawn_observed(
        &rt,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        },
        async { tokio::time::sleep(Duration::from_secs(3600)).await },
    );
    // `sleep` must be constructed INSIDE the block_on future: building it outside would run
    // before the runtime context is entered (no reactor yet) and panic.
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await }); // let the inner task start
    handle.abort();
    let joined = rt.block_on(handle);
    assert!(joined.is_err(), "aborting the observer cancels it");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "abort is not a panic");
}

#[test]
fn login_panic_backstop_clears_pending_and_completes_with_fixed_error() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("panic-login");
    std::fs::create_dir_all(&dir).unwrap();
    let state = test_state(PathBuf::from(&dir), Config::default(), None);
    let (captcha_tx, _rx) = oneshot::channel::<String>();
    {
        let mut guard = lock_state(&state);
        let st = guard.as_mut().unwrap();
        st.pending_captcha.insert("acc".into(), captcha_tx);
        st.login_in_flight.insert("acc".into());
    }

    recover_login_panic(&state, collect_event, 77, "acc");

    {
        let guard = lock_state(&state);
        let st = guard.as_ref().unwrap();
        assert!(
            st.pending_captcha.is_empty(),
            "panic backstop must drop the stale captcha sender"
        );
        assert!(st.login_in_flight.is_empty());
    }
    let events = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = events
        .iter()
        .find(|e| e["event"] == "LoginResult" && e["id"] == 77)
        .expect("fixed LoginResult completes the awaiting command");
    assert_eq!(result["ok"], false);
    assert_eq!(result["reason"], INTERNAL_ERROR);
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "MonitoringSnapshot"),
        "panic must publish the authoritative monitoring snapshot"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "LoginResult" && e["id"] == 77)
            .count(),
        1,
        "exactly one terminal event for the command"
    );
}

#[test]
fn monitor_panic_watchdog_tears_down_only_the_matching_running_generation() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = data_dir("watchdog");
    std::fs::create_dir_all(&dir).unwrap();
    let state = test_state(PathBuf::from(&dir), Config::default(), None);
    let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut guard = lock_state(&state);
        guard.as_mut().unwrap().monitor = MonitorLifecycle::Running(RunningMonitor {
            generation: 7,
            handle: monitor::MonitorHandle::new(tx),
            loaded_account_ids: HashSet::new(),
            loading_account_ids: HashSet::new(),
        });
    }
    spawn_monitor_watchdog(
        rt.handle().clone(),
        state.clone(),
        collect_event,
        7,
        panic_rx,
    );

    // A panic ping for the matching generation tears the monitor down and emits one closed error.
    panic_tx.send(()).unwrap();
    assert!(
        wait_for(
            |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
            5
        )
        .is_some(),
        "the watchdog must publish monitor_panicked"
    );
    {
        let guard = lock_state(&state);
        assert!(
            matches!(guard.as_ref().unwrap().monitor, MonitorLifecycle::Idle),
            "the panicked monitor must be taken down to Idle"
        );
    }

    // A second ping is a no-op: already Idle, no duplicate events.
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    panic_tx.send(()).unwrap();
    assert!(
        none_for(
            |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
            1
        ),
        "a ping after teardown must not emit anything"
    );
}

#[test]
fn monitor_panic_watchdog_ignores_stale_generation_and_sender_close() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = data_dir("watchdog-stale");
    std::fs::create_dir_all(&dir).unwrap();
    let state = test_state(PathBuf::from(&dir), Config::default(), None);
    let (panic_tx, panic_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut guard = lock_state(&state);
        guard.as_mut().unwrap().monitor = MonitorLifecycle::Running(RunningMonitor {
            generation: 9,
            handle: monitor::MonitorHandle::new(tx),
            loaded_account_ids: HashSet::new(),
            loading_account_ids: HashSet::new(),
        });
    }
    // Watchdog from an OLDER start (generation 8): its ping must not cancel generation 9.
    spawn_monitor_watchdog(
        rt.handle().clone(),
        state.clone(),
        collect_event,
        8,
        panic_rx,
    );
    panic_tx.send(()).unwrap();
    assert!(
        none_for(
            |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
            1
        ),
        "a stale watchdog must never cancel a newer session"
    );
    {
        let guard = lock_state(&state);
        assert!(
            matches!(
                guard.as_ref().unwrap().monitor,
                MonitorLifecycle::Running(_)
            ),
            "a stale watchdog must never cancel a newer session"
        );
    }

    // Normal close: all senders dropped (a plain stop drops the handle) → the watchdog exits
    // without touching the running monitor.
    drop(panic_tx);
    assert!(
        none_for(
            |v| v["event"] == "Error" && v["code"] == "monitor_panicked",
            1
        ),
        "channel close (normal stop) must be a no-op"
    );
    {
        let guard = lock_state(&state);
        assert!(
            matches!(
                guard.as_ref().unwrap().monitor,
                MonitorLifecycle::Running(_)
            ),
            "channel close must not tear down a running monitor"
        );
    }
}

#[test]
fn stopping_a_start_aborts_its_observed_task_and_child() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    use std::sync::atomic::{AtomicBool, Ordering};
    struct TrackDrop(Arc<AtomicBool>);
    impl Drop for TrackDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut lifecycle = MonitorLifecycle::Idle;
    lifecycle.begin_start(1, 501).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let track = TrackDrop(dropped.clone());
    let task = spawn_observed(&rt, || {}, async move {
        let _track = track; // held by the child future; drop proves the abort reached it
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await }); // child starts and holds _track
    lifecycle.attach_start_task(1, task);
    stop_monitor(&mut lifecycle, collect_event, "cancelled");
    assert!(matches!(lifecycle, MonitorLifecycle::Idle));

    // The abort propagated into the child: the tracked value must drop promptly (no 3600s linger).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "aborting the start task must abort its child"
        );
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
    }
}

#[test]
fn submit_captcha_to_a_dead_sender_fails_instead_of_succeeding() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("dead-captcha");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    // A challenge whose awaiting login task is already gone (the receiver side was dropped):
    // the entry still exists (the panic backstop raced the submit), but the send must fail.
    let (dead_tx, rx) = oneshot::channel::<String>();
    drop(rx); // `_rx` would stay alive to end of scope — drop it NOW so the sender is dead
    {
        let mut guard = lock_state(&core.state);
        guard
            .as_mut()
            .unwrap()
            .pending_captcha
            .insert("acc".into(), dead_tx);
    }
    send(
        &core,
        r#"{"id":2,"cmd":"SubmitCaptcha","account_id":"acc","text":"A1B2"}"#.as_bytes(),
    );
    let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).expect("submit reply");
    assert_eq!(reply["ok"], false, "a dead challenge cannot be submitted");
    assert!(
        reply["error"].as_str().unwrap().contains("withdrawn"),
        "error: {}",
        reply["error"]
    );
}

#[test]
fn account_mutations_recover_a_stale_journal_before_mutating() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("stale-journal");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
    send(
        &core,
        r#"{"id":3,"cmd":"AddAccount","label":"dave","school":"thu","username":"dave","password":"secret"}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
        5
    )
    .is_some());

    // Simulate a crash between begin and complete: a stale journal is now on disk.
    AccountJournal::begin(Path::new(&dir), AccountMutation::Add, "dave").unwrap();

    // AddAccount recovers it first: warn LogLine, and the mutation is NOT blocked.
    send(
        &core,
        r#"{"id":4,"cmd":"AddAccount","label":"eve","school":"thu","username":"eve","password":"secret"}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 4 && v["ok"] == true,
        5
    )
    .is_some());
    {
        // Scoped so the guard never shadows the `events()` helper for later assertions.
        let events = events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            events.iter().any(|e| {
                e["event"] == "LogLine"
                    && e["level"] == "warn"
                    && e["text"].as_str().is_some_and(|t| t.contains("已恢復"))
            }),
            "a recovered stale journal warns"
        );
    }

    // A CORRUPT journal fails the mutation with the FIXED error (no file content echoed).
    std::fs::write(
        Path::new(&dir).join("account-transaction.json"),
        b"NOT-JSON-CONTENT-SECRET",
    )
    .unwrap();
    send(
        &core,
        r#"{"id":5,"cmd":"AddAccount","label":"frank","school":"thu","username":"frank","password":"secret"}"#
            .as_bytes(),
    );
    let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == 5, 5)
        .expect("mutation must fail on a corrupt journal");
    assert_eq!(reply["ok"], false);
    assert_eq!(
        reply["error"],
        "account transaction journal unreadable; recovery skipped"
    );
    assert!(
        !events()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|e| e.to_string().contains("NOT-JSON-CONTENT-SECRET")),
        "the fixed error must never echo journal file content"
    );
}

#[test]
fn captcha_timeout_reports_failure_and_allows_relogin() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let base = start_fake();
    let dir = data_dir("captcha-timeout");
    // This core alone shortens its captcha answer window — every other core (and every other
    // module's e2e running in parallel) keeps the production 180 s.
    let mut core = init(collect_event).unwrap();
    core.captcha_answer_timeout = Duration::from_millis(200);
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
    post(
        &base,
        "/_test/captcha",
        r#"{"required":true,"expected":"A1B2"}"#,
    );
    send(
        &core,
        format!(
            r#"{{"id":3,"cmd":"AddAccount","label":"dave","school":"{base}","username":"dave","password":"secret"}}"#
        )
        .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
        5
    )
    .is_some());
    let acc = account_id_by_label("dave").expect("account id");

    // Park on the challenge and let the (test-shortened) answer window elapse.
    send(
        &core,
        format!(r#"{{"id":4,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
            10
        )
        .is_some(),
        "login blocks on the captcha challenge"
    );
    let timed_out = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 4, 15)
        .expect("timeout must produce a terminal LoginResult");
    assert_eq!(timed_out["ok"], false);
    assert!(
        timed_out["reason"].as_str().unwrap().contains("timed out"),
        "reason: {}",
        timed_out["reason"]
    );

    // The single-flight marker and stale pending entry were released: the SAME account can
    // log in again immediately, and answering the fresh challenge succeeds. Clear first so the
    // awaited CaptchaChallenge provably belongs to the NEW attempt, not the timed-out one.
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    send(
        &core,
        format!(r#"{{"id":5,"cmd":"Login","account_id":"{acc}"}}"#).as_bytes(),
    );
    assert!(
        wait_for(
            |v| v["event"] == "CaptchaChallenge" && v["account_id"] == acc,
            10
        )
        .is_some(),
        "after a timeout the account must be able to log in again"
    );
    send(
        &core,
        format!(r#"{{"id":6,"cmd":"SubmitCaptcha","account_id":"{acc}","text":"A1B2"}}"#)
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 6 && v["ok"] == true,
        5
    )
    .is_some());
    let ok = wait_for(|v| v["event"] == "LoginResult" && v["id"] == 5, 10).expect("relogin result");
    assert_eq!(ok["ok"], true, "relogin with the fresh answer succeeds");
}

#[test]
fn qr_remote_update_config_disabled_empty_accepted_enabled_empty_rejected_without_touching_disk() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("qr-update-config");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    // Materialize file then freeze baseline (Init alone leaves file absent).
    send(
        &core,
        r#"{"id":2,"cmd":"UpdateConfig","patch":{"countdown_secs":2}}"#.as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 2 && v["ok"] == true,
        5
    )
    .is_some());
    let config_path = std::path::Path::new(&dir).join("config.json");
    // Distributed check — disabled+empty patch accepted, settings never contains raw key, disk has canonical "" not default.
    let before: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(before["settings"]["qr_remote_enabled"], false);
    let _pre_len = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .len();

    // Disabled + trimmed empty is accepted as canonical empty string.
    send(
        &core,
        r#"{"id":10,"cmd":"UpdateConfig","patch":{"qr_remote_enabled":false,"qr_remote_base_url":""}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 10 && v["ok"] == true,
        5
    )
    .is_some());
    send(
        &core,
        r#"{"id":11,"cmd":"UpdateConfig","patch":{"qr_remote_enabled":false,"qr_remote_base_url":"   "}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 11 && v["ok"] == true,
        5
    )
    .is_some());
    // Settings after id 11 — find newest Settings (ring never cleared mid-test for id reuse pattern).
    let snapshot = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let ev = snapshot
        .iter()
        .rev()
        .find(|v| v["event"] == "Settings" && v["id"] == Value::Null)
        .expect("Settings after disabled+empty");
    assert_eq!(ev["settings"]["qr_remote_base_url"], "");
    assert_eq!(ev["settings"]["qr_remote_enabled"], false);
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(on_disk["settings"]["qr_remote_base_url"], "");
    assert_eq!(on_disk["settings"]["qr_remote_enabled"], false);

    // Enabled + empty / whitespace must be rejected at Settings::validate boundary and must NOT touch disk.
    let frozen = std::fs::read(&config_path).unwrap();
    for (id, patch) in [
        (
            12u64,
            r#"{"qr_remote_enabled":true,"qr_remote_base_url":""}"#,
        ),
        (
            13u64,
            r#"{"qr_remote_enabled":true,"qr_remote_base_url":"   "}"#,
        ),
    ] {
        send(
            &core,
            format!(r#"{{"id":{id},"cmd":"UpdateConfig","patch":{patch}}}"#).as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == id, 5)
            .unwrap_or_else(|| panic!("no reply for patch {patch}"));
        assert_eq!(
            reply["ok"], false,
            "enabled+empty patch {patch} must be rejected"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            frozen,
            "enabled+empty must not touch file"
        );
    }
    // Unknown / malformed qr base field types are rejected (strict typing) and also leave disk untouched.
    for (id, patch) in [
        (14u64, r#"{"qr_remote_base_url":123}"#),
        (15u64, r#"{"qr_remote_base_url":true}"#),
        (16u64, r#"{"qr_remote_enabled":"true"}"#),
    ] {
        send(
            &core,
            format!(r#"{{"id":{id},"cmd":"UpdateConfig","patch":{patch}}}"#).as_bytes(),
        );
        let reply = wait_for(|v| v["event"] == "Reply" && v["id"] == id, 5)
            .unwrap_or_else(|| panic!("no reply for patch {patch}"));
        assert_eq!(
            reply["ok"], false,
            "malformed qr patch {patch} must be rejected"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            frozen,
            "malformed qr patch must not touch file"
        );
    }
    // A valid nonempty patch still applies and leaves canonical normalized value (trailing slash stripped).
    send(
        &core,
        r#"{"id":20,"cmd":"UpdateConfig","patch":{"qr_remote_enabled":true,"qr_remote_base_url":"https://example.com/path/"}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 20 && v["ok"] == true,
        5
    )
    .is_some());
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(on_disk["settings"]["qr_remote_enabled"], true);
    assert_eq!(
        on_disk["settings"]["qr_remote_base_url"], "https://example.com/path",
        "core must canonical-normalize trailing slash"
    );
    // default-port input must be stored canonical without port (UI hint never does this).
    send(
        &core,
        r#"{"id":21,"cmd":"UpdateConfig","patch":{"qr_remote_base_url":"https://example.com:443/"}}"#
            .as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 21 && v["ok"] == true,
        5
    )
    .is_some());
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        on_disk["settings"]["qr_remote_base_url"],
        "https://example.com"
    );
}

#[test]
fn settings_event_never_contains_raw_qr_key_and_redaction_covers_it() {
    let _g = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let dir = data_dir("qr-settings-no-raw");
    let core = init(collect_event).unwrap();
    send(
        &core,
        format!(r#"{{"id":1,"cmd":"Init","data_dir":"{dir}"}}"#).as_bytes(),
    );
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 1, 10).is_some());
    send(&core, r#"{"id":2,"cmd":"CreateVault"}"#.as_bytes());
    assert!(wait_for(|v| v["event"] == "Reply" && v["id"] == 2, 5).is_some());
    let raw_key = "qr-secret-value";
    send(
        &core,
        format!(r#"{{"id":3,"cmd":"SetQrRemoteKey","key":"{raw_key}"}}"#).as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 3 && v["ok"] == true,
        5
    )
    .is_some());
    // In-memory ring already holds the Settings emitted by SetQrRemoteKey; scan it before it rolls.
    let events_snapshot = events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let settings_lines: Vec<String> = events_snapshot
        .iter()
        .filter(|v| v["event"] == "Settings")
        .map(|v| v.to_string())
        .collect();
    assert!(
        !settings_lines.is_empty(),
        "Settings must have been emitted after SetQrRemoteKey"
    );
    for line in &settings_lines {
        assert!(
            !line.contains(raw_key),
            "Settings event must never contain raw key; line={line}"
        );
    }
    // Settings event shape is bool-only for has_qr_remote_key, and includes the canonical qr fields.
    let latest = events_snapshot
        .iter()
        .rev()
        .find(|v| v["event"] == "Settings")
        .expect("latest Settings");
    let s = &latest["settings"];
    assert!(
        s["has_qr_remote_key"].is_boolean(),
        "has_qr_remote_key must be bool, got {}",
        s["has_qr_remote_key"]
    );
    assert_eq!(s["has_qr_remote_key"], true);
    assert!(
        s["qr_remote_enabled"].is_boolean(),
        "qr_remote_enabled must be bool"
    );
    assert!(s["qr_remote_base_url"].is_string());
    assert!(
        s.get("qr_remote_key").is_none() && s.get("qr_secret").is_none(),
        "Settings must not have raw key field"
    );
    // Clear emits a Settings with has_qr_remote_key=false, still without raw.
    events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    send(
        &core,
        r#"{"id":4,"cmd":"SetQrRemoteKey","key":""}"#.as_bytes(),
    );
    assert!(wait_for(
        |v| v["event"] == "Reply" && v["id"] == 4 && v["ok"] == true,
        5
    )
    .is_some());
    let cleared = wait_for(|v| v["event"] == "Settings", 5).expect("Settings after clear");
    assert_eq!(cleared["settings"]["has_qr_remote_key"], false);
    assert!(
        !cleared.to_string().contains(raw_key),
        "clear Settings must not contain raw key"
    );
}

#[test]
fn a_change_against_a_stale_revision_is_refused_with_the_current_snapshot() {
    let _serial = LIFECYCLE_SEQ
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = PathBuf::from(data_dir("stale-revision"));
    std::fs::create_dir_all(&dir).unwrap();
    let state = test_state(dir, Config::default(), None);
    let current = Config::default().config_revision;
    let command = json!({ "id": 9041, "cmd": "DeleteGroup", "group_id": "g", "expected_revision": current + 1 });
    commands::handle_sync_state(
        &state,
        collect_event,
        serde_json::from_value(command).unwrap(),
    );

    let reply = wait_for(|event| event["event"] == "Reply" && event["id"] == 9041, 5).unwrap();
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], "revision_conflict");
    assert_eq!(
        reply["data"]["snapshot"]["config_revision"], current,
        "the refusal carries the state the caller should have seen"
    );
    let guard = state.lock().unwrap();
    assert_eq!(guard.as_ref().unwrap().config.config_revision, current);
}
