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

/// Per-account secret blob. This is what callers store; the vault encrypts the whole map.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountSecret {
    pub password: String,
    /// Serialized cookie-store JSON for session restore (empty until first login).
    #[serde(default)]
    pub cookies: String,
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
    pub fn create_with_key(path: &Path, key: [u8; 32]) -> Result<VaultFile, String> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(|e| e.to_string())?;
        let vault = VaultFile { path: path.to_path_buf(), salt, key: Some(key), data: BTreeMap::new() };
        vault.persist()?;
        Ok(vault)
    }

    /// Unlock using the raw 32-byte device key. A wrong key fails the AEAD authentication tag → clean
    /// error, no partial read.
    pub fn unlock_with_key(path: &Path, key: [u8; 32]) -> Result<VaultFile, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read vault: {e}"))?;
        if bytes.len() < SALT_LEN + NONCE_LEN {
            return Err("vault file corrupt".into());
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[..SALT_LEN]);
        let nonce = &bytes[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ciphertext = &bytes[SALT_LEN + NONCE_LEN..];
        Self::open_with_key(path, salt, key, nonce, ciphertext, "stored key does not match vault")
    }

    /// Shared decrypt+parse for both unlock paths. `salt`/`key` are already recovered; `err` is the
    /// message for a failed AEAD authentication (wrong password or wrong stored key).
    fn open_with_key(
        path: &Path,
        salt: [u8; SALT_LEN],
        key: [u8; 32],
        nonce: &[u8],
        ciphertext: &[u8],
        err: &str,
    ) -> Result<VaultFile, String> {
        let cipher = XChaCha20Poly1305::new((&key).into());
        let plaintext = cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| err.to_string())?;
        let data = serde_json::from_slice(&plaintext).map_err(|e| e.to_string())?;
        Ok(VaultFile { path: path.to_path_buf(), salt, key: Some(key), data })
    }

    /// Re-encrypt the whole map with a FRESH nonce and write it out. Called after every mutation.
    fn persist(&self) -> Result<(), String> {
        let key = self.key.as_ref().ok_or("vault is locked")?;

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?; // fresh, every write

        let plaintext = serde_json::to_vec(&self.data).map_err(|e| e.to_string())?;
        let cipher = XChaCha20Poly1305::new(key.into());
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|e| e.to_string())?;

        let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        std::fs::write(&self.path, out).map_err(|e| format!("write vault: {e}"))
    }

    pub fn get(&self, account_id: &str) -> Option<AccountSecret> {
        self.data.get(account_id).cloned()
    }

    pub fn set(&mut self, account_id: &str, secret: AccountSecret) -> Result<(), String> {
        self.data.insert(account_id.to_string(), secret);
        self.persist()
    }

    pub fn delete(&mut self, account_id: &str) -> Result<(), String> {
        self.data.remove(account_id);
        self.persist()
    }

    // The LLM API key rides in a reserved vault entry (never in config/logs).
    pub fn set_llm_key(&mut self, key: String) -> Result<(), String> {
        self.set(LLM_KEY_ID, AccountSecret { password: key, cookies: String::new() })
    }
    pub fn get_llm_key(&self) -> Option<String> {
        self.get(LLM_KEY_ID).map(|s| s.password).filter(|k| !k.is_empty())
    }

    pub fn lock(&mut self) {
        if let Some(mut key) = self.key.take() {
            key.zeroize();
        }
    }
}

impl Drop for VaultFile {
    fn drop(&mut self) {
        self.lock();
    }
}

/// Load the persistent 32-byte device key from `key_path`, generating + storing it on first run.
/// This is what makes the vault auto-unlock with no master password: the key lives beside the vault.
/// ponytail: a keyfile next to the vault protects a stolen vault.bin alone (a stray backup/sync of
/// just that file), NOT a full-device compromise — bind to the OS keystore (Windows DPAPI / Android
/// Keystore) for real device-binding when that Phase-B integration lands.
pub fn load_or_create_device_key(key_path: &Path) -> Result<[u8; 32], String> {
    if let Ok(bytes) = std::fs::read(key_path) {
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| e.to_string())?;
    std::fs::write(key_path, key).map_err(|e| format!("write device key: {e}"))?;
    Ok(key)
}
