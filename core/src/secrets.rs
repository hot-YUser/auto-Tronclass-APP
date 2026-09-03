//! Encrypted secret vault, auto-unlocked with a per-device key — secrets stay encrypted at rest with
//! no master password (user decision 2026-07: "no lock password").
//!
//! File layout: `salt(16) || nonce(24) || XChaCha20-Poly1305(ciphertext)`. The salt is vestigial (the
//! key comes from `device.key`, not a KDF) but kept so the on-disk layout is fixed.
//! - Every write generates a **FRESH random 24-byte nonce**. A nonce is NEVER reused: reuse under a
//!   fixed key breaks XChaCha20-Poly1305 confidentiality *and* integrity (the Poly1305 one-time key
//!   becomes recoverable, enabling forgery). XChaCha's 192-bit nonce is wide enough that random
//!   selection is collision-safe — no counter needed.
//! - Secrets never leave via events or logs; they are withheld at the source.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroize;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
/// Reserved vault entry id for the LLM API key (accounts use random hex ids, so no collision).
const LLM_KEY_ID: &str = "__llm__";
const QR_REMOTE_KEY_ID: &str = "__qr_remote__";

/// A string secret whose `Debug`/`Display` are masked, so a stray `{:?}`/log of a struct holding it
/// (e.g. the monitor's `Account`, which carries a password for session re-login) never leaks it. The
/// real value is reachable only via `expose()`. `redaction::emit` covers the event seam; this covers
/// accidental debug logging that the seam can't see.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}
impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Per-account secret blob. This is what callers store; the vault encrypts the whole map.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AccountSecret {
    pub password: String,
    /// Serialized cookie-store JSON for session restore (empty until first login).
    #[serde(default)]
    pub cookies: String,
}

impl AccountSecret {
    /// Transfer ownership without leaving an additional copy for Drop to wipe.
    pub fn into_parts(mut self) -> (String, String) {
        (
            std::mem::take(&mut self.password),
            std::mem::take(&mut self.cookies),
        )
    }
}

impl std::fmt::Debug for AccountSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSecret")
            .field("password", &"***")
            .field("cookies", &"***")
            .finish()
    }
}

impl Drop for AccountSecret {
    fn drop(&mut self) {
        self.password.zeroize();
        self.cookies.zeroize();
    }
}

pub struct VaultFile {
    path: PathBuf,
    salt: [u8; SALT_LEN],
    key: Option<[u8; 32]>, // Some(..) while unlocked; zeroized on lock/drop
    data: BTreeMap<String, AccountSecret>,
}

impl VaultFile {
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Create a brand-new empty vault encrypted under a raw 32-byte key — the auto-unlock path (a
    /// device key, no master password / Argon2). The salt is vestigial for a raw-key vault but kept
    /// so the on-disk layout (salt||nonce||ct) is identical to a password vault.
    pub fn create_with_key(path: &Path, mut key: [u8; 32]) -> Result<VaultFile, String> {
        let mut salt = [0u8; SALT_LEN];
        if let Err(error) = getrandom::getrandom(&mut salt) {
            key.zeroize();
            return Err(error.to_string());
        }
        let vault = VaultFile {
            path: path.to_path_buf(),
            salt,
            key: Some(key),
            data: BTreeMap::new(),
        };
        vault.persist_new()?;
        Ok(vault)
    }

    /// Unlock using the raw 32-byte device key. A wrong key fails the AEAD authentication tag → clean
    /// error, no partial read.
    pub fn unlock_with_key(path: &Path, key: [u8; 32]) -> Result<VaultFile, String> {
        let mut key = key;
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                key.zeroize();
                return Err(format!("read vault: {error}"));
            }
        };
        if bytes.len() < SALT_LEN + NONCE_LEN {
            key.zeroize();
            return Err("vault file corrupt".into());
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[..SALT_LEN]);
        let nonce = &bytes[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ciphertext = &bytes[SALT_LEN + NONCE_LEN..];
        Self::open_with_key(
            path,
            salt,
            key,
            nonce,
            ciphertext,
            "stored key does not match vault",
        )
    }

    /// Shared decrypt+parse for both unlock paths. `salt`/`key` are already recovered; `err` is the
    /// message for a failed AEAD authentication (wrong password or wrong stored key).
    fn open_with_key(
        path: &Path,
        salt: [u8; SALT_LEN],
        mut key: [u8; 32],
        nonce: &[u8],
        ciphertext: &[u8],
        err: &str,
    ) -> Result<VaultFile, String> {
        let cipher = XChaCha20Poly1305::new((&key).into());
        let mut plaintext = match cipher.decrypt(XNonce::from_slice(nonce), ciphertext) {
            Ok(plaintext) => plaintext,
            Err(_) => {
                key.zeroize();
                return Err(err.to_string());
            }
        };
        let data = match serde_json::from_slice(&plaintext) {
            Ok(data) => data,
            // Fixed message, never the serde literal: the plaintext IS secrets (passwords, cookies,
            // the LLM key), and serde's "invalid type: string \"<value>\"" would echo them verbatim.
            Err(_) => {
                plaintext.zeroize();
                key.zeroize();
                return Err("vault data corrupt".into());
            }
        };
        plaintext.zeroize();
        Ok(VaultFile {
            path: path.to_path_buf(),
            salt,
            key: Some(key),
            data,
        })
    }

    fn sealed_bytes(&self) -> Result<Vec<u8>, String> {
        let key = self.key.as_ref().ok_or("vault is locked")?;

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?; // fresh, every write

        let mut plaintext = serde_json::to_vec(&self.data).map_err(|e| e.to_string())?;
        let cipher = XChaCha20Poly1305::new(key.into());
        let ciphertext_result = cipher.encrypt(XNonce::from_slice(&nonce), plaintext.as_ref());
        plaintext.zeroize();
        let ciphertext = ciphertext_result.map_err(|e| e.to_string())?;

        let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Re-encrypt the whole map with a FRESH nonce and atomically replace it. Called after every
    /// mutation; a crash can expose either the complete old vault or the complete new vault, never a
    /// partially truncated file.
    fn persist(&self) -> Result<(), String> {
        let out = self.sealed_bytes()?;
        crate::atomic_file::replace(&self.path, &out).map_err(|e| format!("write vault: {e}"))
    }

    fn persist_new(&self) -> Result<(), String> {
        let out = self.sealed_bytes()?;
        crate::atomic_file::create_new(&self.path, &out).map_err(|e| format!("create vault: {e}"))
    }

    pub fn get(&self, account_id: &str) -> Option<AccountSecret> {
        self.key.as_ref()?;
        self.data.get(account_id).cloned()
    }

    pub fn set(&mut self, account_id: &str, secret: AccountSecret) -> Result<(), String> {
        let account_id = account_id.to_string();
        let previous = self.data.insert(account_id.clone(), secret);
        if let Err(error) = self.persist() {
            match previous {
                Some(secret) => self.data.insert(account_id, secret),
                None => self.data.remove(&account_id),
            };
            return Err(error);
        }
        Ok(())
    }

    pub fn delete(&mut self, account_id: &str) -> Result<(), String> {
        let Some(previous) = self.data.remove(account_id) else {
            return Ok(());
        };
        if let Err(error) = self.persist() {
            self.data.insert(account_id.to_string(), previous);
            return Err(error);
        }
        Ok(())
    }

    // The LLM API key rides in a reserved vault entry (never in config/logs).
    pub fn set_llm_key(&mut self, mut key: String) -> Result<(), String> {
        let result = self.set(
            LLM_KEY_ID,
            AccountSecret {
                password: std::mem::take(&mut key),
                cookies: String::new(),
            },
        );
        key.zeroize(); // whatever remains after the take — the value is now owned by the map
        result
    }
    pub fn get_llm_key(&self) -> Option<String> {
        self.key.as_ref()?;
        self.data
            .get(LLM_KEY_ID)
            .map(|secret| secret.password.clone())
            .filter(|key| !key.is_empty())
    }
    /// Whether a non-empty LLM API key is stored — a locked vault is `false`, and no clone of the
    /// key is made (callers that only need to know skip `get_llm_key`'s copy).
    pub fn has_llm_key(&self) -> bool {
        self.key.is_some()
            && self
                .data
                .get(LLM_KEY_ID)
                .is_some_and(|secret| !secret.password.is_empty())
    }

    pub fn set_qr_remote_key(&mut self, mut key: String) -> Result<(), String> {
        let trimmed = key.trim().to_string();
        if trimmed.is_empty() {
            key.zeroize();
            return self.delete(QR_REMOTE_KEY_ID);
        }
        if let Err(error) = validate_qr_remote_key(&trimmed) {
            key.zeroize();
            return Err(error);
        }
        let result = self.set(
            QR_REMOTE_KEY_ID,
            AccountSecret {
                password: trimmed,
                cookies: String::new(),
            },
        );
        key.zeroize();
        result
    }
    pub fn get_qr_remote_key_secret(&self) -> Option<Secret> {
        self.key.as_ref()?;
        self.data
            .get(QR_REMOTE_KEY_ID)
            .map(|secret| Secret::new(secret.password.clone()))
            .filter(|secret| !secret.expose().is_empty())
    }
    pub fn has_qr_remote_key(&self) -> bool {
        self.key.is_some()
            && self
                .data
                .get(QR_REMOTE_KEY_ID)
                .is_some_and(|secret| !secret.password.is_empty())
    }

    pub fn lock(&mut self) {
        if let Some(mut key) = self.key.take() {
            key.zeroize();
        }
        // Locking must also destroy the decrypted plaintext map, or a locked vault (e.g. after
        // Shutdown) would still hand out secrets through a stale reference.
        for secret in self.data.values_mut() {
            secret.password.zeroize();
            secret.cookies.zeroize();
        }
        self.data.clear();
    }
}

fn validate_qr_remote_key(trimmed: &str) -> Result<(), String> {
    if trimmed.is_empty() {
        return Err("qr remote key 不得為空".to_string());
    }
    if trimmed.bytes().any(|b| !(0x21..=0x7E).contains(&b)) {
        return Err("qr remote key 包含空白或不可見字元".to_string());
    }
    let bearer = zeroize::Zeroizing::new(format!("Bearer {trimmed}"));
    reqwest::header::HeaderValue::from_str(bearer.as_str())
        .map(|_| ())
        .map_err(|_| "qr remote key 格式不正確".to_string())?;
    Ok(())
}

impl Drop for VaultFile {
    fn drop(&mut self) {
        self.lock();
    }
}

/// Headless/test compatibility path: load a persistent 32-byte device key from `key_path`,
/// generating + storing it on first run. Production GUI hosts supply the same raw key in memory
/// after recovering it through Windows DPAPI or Android Keystore and never call this function.
pub fn load_or_create_device_key(key_path: &Path) -> Result<[u8; 32], String> {
    match std::fs::read(key_path) {
        Ok(mut bytes) => {
            let result = parse_device_key(&bytes);
            bytes.zeroize();
            return result;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read device key: {error}")),
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| e.to_string())?;
    match crate::atomic_file::create_new(key_path, &key) {
        Ok(()) => Ok(key),
        // Another initializer won the race. Its complete key is authoritative; never replace it.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            key.zeroize();
            let mut bytes =
                std::fs::read(key_path).map_err(|e| format!("read raced device key: {e}"))?;
            let result = parse_device_key(&bytes);
            bytes.zeroize();
            result
        }
        Err(error) => {
            key.zeroize();
            Err(format!("write device key: {error}"))
        }
    }
}

fn parse_device_key(bytes: &[u8]) -> Result<[u8; 32], String> {
    if bytes.len() != 32 {
        return Err(format!(
            "device key corrupt: expected 32 bytes, found {}",
            bytes.len()
        ));
    }
    let mut key = [0_u8; 32];
    key.copy_from_slice(bytes);
    Ok(key)
}

/// Decode the one exact standard-Base64 shape used by a 32-byte host key. Keeping this decoder
/// deliberately narrow avoids another direct dependency and rejects alternate/non-canonical forms
/// at the FFI trust boundary.
pub fn decode_device_key_base64(encoded: &str) -> Result<[u8; 32], String> {
    let input = encoded.as_bytes();
    if input.len() != 44 || input[43] != b'=' || input[..43].contains(&b'=') {
        return Err("device key must be canonical base64 for exactly 32 bytes".into());
    }

    fn sextet(value: u8) -> Option<u8> {
        match value {
            b'A'..=b'Z' => Some(value - b'A'),
            b'a'..=b'z' => Some(value - b'a' + 26),
            b'0'..=b'9' => Some(value - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut key = [0_u8; 32];
    let result = (|| {
        let mut output = 0;
        for (index, chunk) in input.chunks_exact(4).enumerate() {
            let a = sextet(chunk[0]).ok_or("device key contains invalid base64")?;
            let b = sextet(chunk[1]).ok_or("device key contains invalid base64")?;
            let c = sextet(chunk[2]).ok_or("device key contains invalid base64")?;
            key[output] = (a << 2) | (b >> 4);
            key[output + 1] = (b << 4) | (c >> 2);
            output += 2;

            if index < 10 {
                let d = sextet(chunk[3]).ok_or("device key contains invalid base64")?;
                key[output] = (c << 6) | d;
                output += 1;
            } else if c & 0b11 != 0 {
                return Err("device key base64 is not canonical".to_string());
            }
        }
        debug_assert_eq!(output, key.len());
        Ok(())
    })();
    if let Err(error) = result {
        key.zeroize();
        return Err(error);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_strict_32_byte_device_key_base64() {
        let decoded =
            decode_device_key_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        assert_eq!(
            decoded,
            std::array::from_fn::<_, 32, _>(|index| index as u8)
        );

        for malformed in [
            "",
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh==",
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh!=",
        ] {
            assert!(decode_device_key_base64(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn locked_vault_reads_are_unavailable_and_plaintext_cleared() {
        let dir = std::env::temp_dir().join(format!("tron-vault-lock-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = [9u8; 32];
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        vault
            .set(
                "acc",
                AccountSecret {
                    password: "pw-secret".into(),
                    cookies: "ck-secret".into(),
                },
            )
            .unwrap();
        vault.set_llm_key("llm-secret".into()).unwrap();
        assert!(vault.get("acc").is_some());
        assert_eq!(vault.get_llm_key().as_deref(), Some("llm-secret"));

        vault.lock();

        assert!(
            vault.get("acc").is_none(),
            "locked vault must never serve plaintext secrets"
        );
        assert!(
            vault.get_llm_key().is_none(),
            "locked vault must never serve the LLM key"
        );
        assert!(
            vault.data.is_empty(),
            "lock must clear the decrypted plaintext map"
        );
        assert!(
            vault
                .set(
                    "acc",
                    AccountSecret {
                        password: "x".into(),
                        cookies: String::new()
                    }
                )
                .is_err(),
            "locked vault must not accept writes"
        );
        assert!(vault.set_llm_key("y".into()).is_err());
        assert!(
            vault.delete("acc").is_ok(),
            "delete on a locked vault is a no-op"
        );
        assert!(vault.get("acc").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_vault_plaintext_error_never_echoes_decrypted_content() {
        let dir = std::env::temp_dir().join(format!("tron-vault-echo-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = [0x5au8; 32];
        let path = dir.join("vault.bin");
        drop(VaultFile::create_with_key(&path, key).unwrap());

        // Replace the ciphertext with an encryption of a plaintext whose serde error literal would
        // echo it verbatim ("invalid type: string \"VAULT-SECRET-ECHO\", expected a map ...").
        let mut raw = std::fs::read(&path).unwrap();
        let cipher = XChaCha20Poly1305::new((&key).into());
        let mut payload = b"\"VAULT-SECRET-ECHO\"".to_vec();
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&raw[SALT_LEN..SALT_LEN + NONCE_LEN]),
                payload.as_ref(),
            )
            .unwrap();
        payload.zeroize();
        raw.truncate(SALT_LEN + NONCE_LEN);
        raw.extend_from_slice(&ciphertext);
        std::fs::write(&path, &raw).unwrap();

        let error = VaultFile::unlock_with_key(&path, key)
            .err()
            .expect("corrupt vault must fail to unlock");
        assert!(
            !error.contains("VAULT-SECRET-ECHO"),
            "vault parse error must not echo decrypted content: {error}"
        );
        assert_eq!(error, "vault data corrupt");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn has_llm_key_reports_presence_without_cloning_and_follows_lock() {
        let dir =
            std::env::temp_dir().join(format!("tron-vault-has-key-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = [3u8; 32];
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        assert!(!vault.has_llm_key(), "a fresh vault has no LLM key");
        vault.set_llm_key("llm-key".into()).unwrap();
        assert!(vault.has_llm_key());
        vault.set_llm_key(String::new()).unwrap();
        assert!(
            !vault.has_llm_key(),
            "an empty key is not a usable key (matches get_llm_key)"
        );
        vault.set_llm_key("again".into()).unwrap();
        vault.lock();
        assert!(
            !vault.has_llm_key(),
            "a locked vault must never report a key"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_debug_output_never_contains_plaintext() {
        let secret = Secret::new("super-secret-password");
        let account = AccountSecret {
            password: "account-password".into(),
            cookies: "session-cookie".into(),
        };
        let output = format!("{secret:?} {account:?}");
        assert!(!output.contains("super-secret-password"));
        assert!(!output.contains("account-password"));
        assert!(!output.contains("session-cookie"));
    }

    #[test]
    fn vault_qr_remote_key_set_get_clear_and_redaction() {
        let dir = std::env::temp_dir().join(format!("tron-vault-qr-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = [7u8; 32];
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        assert!(!vault.has_qr_remote_key());
        vault.set_qr_remote_key("qr-secret-value".into()).unwrap();
        assert!(vault.has_qr_remote_key());
        assert_eq!(
            vault
                .get_qr_remote_key_secret()
                .as_ref()
                .map(|secret| secret.expose()),
            Some("qr-secret-value")
        );
        // Redaction must hide the raw key
        let mut value = serde_json::json!({"qr_remote_key": "qr-secret-value", "other": "ok"});
        crate::redaction::redact(&mut value);
        assert_eq!(value["qr_remote_key"], "[redacted]");
        assert_eq!(value["other"], "ok");
        // Also case-insensitive
        let mut value2 = serde_json::json!({"QR_REMOTE_KEY": "secret"});
        crate::redaction::redact(&mut value2);
        assert_eq!(value2["QR_REMOTE_KEY"], "[redacted]");
        // Empty clears
        vault.set_qr_remote_key(String::new()).unwrap();
        assert!(!vault.has_qr_remote_key());
        assert!(vault.get_qr_remote_key_secret().is_none());
        // Wrong key fails to unlock
        let wrong = [9u8; 32];
        assert!(VaultFile::unlock_with_key(&dir.join("vault.bin"), wrong).is_err());
        vault.lock();
        assert!(!vault.has_qr_remote_key());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qr_remote_key_validation_vault_unchanged_clear_and_secret() {
        let dir = std::env::temp_dir().join(format!(
            "tron-vault-qr-validate-{}",
            crate::config::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key = [7u8; 32];
        let mut vault = VaultFile::create_with_key(&dir.join("vault.bin"), key).unwrap();
        vault.set_qr_remote_key("good-key-123".into()).unwrap();
        assert!(vault.has_qr_remote_key());
        // Invalid interior whitespace / control / non-visible-ASCII must be rejected BEFORE vault write, vault unchanged
        for bad in [
            "bad key",
            "bad\tkey",
            "bad\nkey",
            "bad\u{7f}key",
            "bad\u{00}key",
            "bad\u{1f}key",
        ] {
            let before = vault
                .get_qr_remote_key_secret()
                .as_ref()
                .map(|secret| secret.expose().to_string());
            let err = vault.set_qr_remote_key(bad.to_string()).unwrap_err();
            assert!(
                err.contains("空白") || err.contains("格式"),
                "bad {bad:?} err {err}"
            );
            assert_eq!(
                vault
                    .get_qr_remote_key_secret()
                    .as_ref()
                    .map(|secret| secret.expose().to_string()),
                before,
                "vault unchanged for {bad:?}"
            );
        }
        // Surrounding whitespace is trimmed to a valid interior — must succeed after trim
        for (raw, canonical) in [
            (" bad-key", "bad-key"),
            ("bad-key ", "bad-key"),
            ("  good-key-123  ", "good-key-123"),
        ] {
            vault.set_qr_remote_key(raw.to_string()).unwrap();
            assert_eq!(
                vault
                    .get_qr_remote_key_secret()
                    .as_ref()
                    .map(|secret| secret.expose()),
                Some(canonical),
                "surrounding whitespace must trim for {raw:?}"
            );
        }
        // Restore good key for subsequent steps
        vault.set_qr_remote_key("good-key-123".into()).unwrap();
        // Whitespace-only clears (empty means clear)
        vault.set_qr_remote_key("   ".into()).unwrap();
        assert!(!vault.has_qr_remote_key());
        assert!(vault.get_qr_remote_key_secret().is_none());
        // Set again and verify zeroizing Secret accessor
        vault.set_qr_remote_key("qr-secret-value".into()).unwrap();
        let secret = vault.get_qr_remote_key_secret().unwrap();
        assert_eq!(secret.expose(), "qr-secret-value");
        assert!(
            format!("{secret:?}").contains("***")
                && !format!("{secret:?}").contains("qr-secret-value")
        );
        // Verify Secret accessor is zeroizing (no long-lived plain clone outside vault) — Debug masked
        let secret2 = vault.get_qr_remote_key_secret().unwrap();
        assert_eq!(secret2.expose(), "qr-secret-value");
        assert!(!format!("{secret2:?}").contains("qr-secret-value"));
        // Empty string clears via delete
        vault.set_qr_remote_key(String::new()).unwrap();
        assert!(!vault.has_qr_remote_key());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_qr_remote_key_uses_zeroizing_buffer() {
        assert!(validate_qr_remote_key("valid-qr-key-123").is_ok());
        // RFC 7230 invalid HeaderValue characters must still be rejected even under Zeroizing
        assert!(
            validate_qr_remote_key("bad\nkey")
                .unwrap_err()
                .contains("空白")
                || validate_qr_remote_key("bad\nkey")
                    .unwrap_err()
                    .contains("格式")
        );
    }
}
