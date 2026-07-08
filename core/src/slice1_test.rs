//! Offline slice-1 unit tests: vault crypto, registry resolution, config CRUD, and login
//! feature-detection. The end-to-end login-over-the-seam test lives in `seam_test.rs`.

use crate::config::{new_id, AccountMeta, Config};
use crate::providers::{Endpoints, Registry};
use crate::secrets::{AccountSecret, VaultFile};

fn tmp(name: &str) -> std::path::PathBuf {
    // A per-test path under the OS temp dir; unique via a CSPRNG id so tests don't collide.
    std::env::temp_dir().join(format!("tron-slice1-{}-{}", name, new_id()))
}

#[test]
fn vault_roundtrip_and_wrong_password() {
    let path = tmp("vault");
    let mut v = VaultFile::create(&path, "hunter2").unwrap();
    v.set("acc1", AccountSecret { password: "s3cret".into(), cookies: String::new() }).unwrap();
    drop(v);

    // correct password → decrypts, secret intact
    let v2 = VaultFile::unlock(&path, "hunter2").unwrap();
    assert_eq!(v2.get("acc1").unwrap().password, "s3cret");

    // wrong password → AEAD auth failure, never a partial/garbage read
    assert!(VaultFile::unlock(&path, "nope").is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn vault_never_reuses_a_nonce() {
    let path = tmp("nonce");
    let mut v = VaultFile::create(&path, "pw").unwrap();
    // Writing the *same* data twice must still change the on-disk nonce (bytes 16..40).
    v.set("a", AccountSecret { password: "x".into(), cookies: String::new() }).unwrap();
    let first = std::fs::read(&path).unwrap();
    v.set("a", AccountSecret { password: "x".into(), cookies: String::new() }).unwrap();
    let second = std::fs::read(&path).unwrap();

    assert_eq!(&first[..16], &second[..16], "salt is fixed");
    assert_ne!(&first[16..40], &second[16..40], "nonce MUST be fresh every write");
    assert_ne!(first, second, "ciphertext differs under a fresh nonce");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn registry_resolves_key_alias_and_raw_url() {
    let reg: Registry = serde_json::from_str(
        r#"{"default_key":"demo","schools":[
             {"key":"demo","label":"Demo U","base_url":"https://demo.example.edu",
              "aliases":["dm","demo-university"],"notes":""}]}"#,
    )
    .unwrap();

    assert_eq!(reg.resolve("demo").unwrap(), "https://demo.example.edu");
    assert_eq!(reg.resolve("DEMO-University").unwrap(), "https://demo.example.edu"); // ci alias
    assert_eq!(reg.resolve("http://raw.host:8080").unwrap(), "http://raw.host:8080"); // raw url
    assert!(reg.resolve("unknown").is_none());

    let e = Endpoints::derive("https://demo.example.edu/");
    assert_eq!(e.current_semester(), "https://demo.example.edu/api/current-semester-info");
}

#[test]
fn factory_registry_ships_empty() {
    let reg = Registry::factory();
    assert!(reg.schools.is_empty(), "seed ships empty; schools are user-added");
}

#[test]
fn config_account_crud_persists() {
    let path = tmp("config.json");
    let mut cfg = Config::default();
    let id = new_id();
    cfg.accounts.push(AccountMeta {
        id: id.clone(),
        label: "me".into(),
        school_ref: "https://demo.example.edu".into(),
        username: "student".into(),
        device_id: "dev-1".into(),
        is_teacher: false,
        course_id: None,
    });
    cfg.active_account = Some(id.clone());
    cfg.save(&path).unwrap();

    let reloaded = Config::load(&path);
    assert_eq!(reloaded.active_account.as_deref(), Some(id.as_str()));
    assert_eq!(reloaded.account(&id).unwrap().username, "student");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn detect_login_kind_classifies_fixtures() {
    use crate::login::{detect_login_kind, LoginKind};

    let password_form = r#"<html><body>
        <form action="/do-login" method="post">
          <input type="hidden" name="csrf" value="tok123">
          <input type="text" name="user">
          <input type="password" name="pass">
          <button>Sign in</button>
        </form></body></html>"#;
    match detect_login_kind(password_form, "https://x/login") {
        LoginKind::PasswordForm(form) => {
            assert_eq!(form.action, "/do-login");
            assert_eq!(form.user_field, "user");
            assert_eq!(form.pass_field, "pass");
            assert_eq!(form.hidden, vec![("csrf".to_string(), "tok123".to_string())]);
        }
        other => panic!("expected PasswordForm, got {other:?}"),
    }

    // A captcha page carries its password form + the captcha image URL and input field name.
    let captcha_page = r#"<html><body>
        <form action="/login" method="post">
          <input type="hidden" name="csrf" value="tok123">
          <input type="text" name="username">
          <input type="password" name="password">
          <img src="/captcha.png">
          <input type="text" name="captcha">
          <button>Sign in</button>
        </form></body></html>"#;
    match detect_login_kind(captcha_page, "u") {
        LoginKind::Captcha { form, image_url, captcha_field } => {
            assert_eq!(form.pass_field, "password");
            assert_eq!(image_url, "/captcha.png");
            assert_eq!(captcha_field, "captcha");
        }
        other => panic!("expected Captcha, got {other:?}"),
    }
    assert!(matches!(
        detect_login_kind(r#"<html><meta name="saml"><body>redirecting to nidp</body>"#, "u"),
        LoginKind::SsoRedirect
    ));
    assert!(matches!(detect_login_kind(r#"<div id="app"></div>"#, "u"), LoginKind::EmailSpa));
}
