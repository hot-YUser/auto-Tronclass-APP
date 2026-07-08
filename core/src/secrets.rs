//! Encrypted secret vault (docs 10 SecretStore; docs 90 §7 security boundary — NOT simplified).
//!
//! File layout: `salt(16) || nonce(24) || XChaCha20-Poly1305(ciphertext)`.
//! - The **salt is fixed** for the vault's life so unlocking re-derives the same Argon2id key
//!   without re-running the KDF's cost each time.
//! - Every single write generates a **FRESH random 24-byte nonce**. A nonce is NEVER reused:
//!   reuse under a fixed key breaks XChaCha20-Poly1305 confidentiality *and* integrity (the
//!   Poly1305 one-time key becomes recoverable, enabling forgery). XChaCha's 192-bit nonce is
//!   wide enough that random selection is collision-safe — no counter needed.
//!
//! Secrets never leave via events or logs; they are withheld at the source.

use argon2::{Algorithm, Argon2, Params, Version};
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

// Argon2id cost. Moderate on purpose: high memory cost OOMs/hangs low-end phones (armv7).
// ponytail: fixed here; lift to a config knob if a device needs it. m=19 MiB, t=2, p=1.
const ARGON_M_COST: u32 = 19_456;
const ARGON_T_COST: u32 = 2;
const ARGON_P_COST: u32 = 1;

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

    /// Create a brand-new empty vault protected by `master_pw`.
    pub fn create(path: &Path, master_pw: &str) -> Result<VaultFile, String> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(|e| e.to_string())?;
        let key = derive_key(master_pw, &salt)?;
        let vault = VaultFile {
            path: path.to_path_buf(),
            salt,
            key: Some(key),
            data: BTreeMap::new(),
        };
        vault.persist()?;
        Ok(vault)
    }

    /// Unlock an existing vault. A wrong password fails AEAD authentication → clean error.
    pub fn unlock(path: &Path, master_pw: &str) -> Result<VaultFile, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read vault: {e}"))?;
        if bytes.len() < SALT_LEN + NONCE_LEN {
            return Err("vault file corrupt".into());
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[..SALT_LEN]);
        let nonce = &bytes[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ciphertext = &bytes[SALT_LEN + NONCE_LEN..];

        let key = derive_key(master_pw, &salt)?;
        Self::open_with_key(path, salt, key, nonce, ciphertext, "wrong master password")
    }

    /// Unlock using a raw 32-byte key from the platform keystore (docs 10 unlock layer), bypassing
    /// the Argon2id KDF. A wrong key fails the AEAD authentication tag → clean error, no partial read.
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

    /// The current vault key, to hand to the platform keystore after a master-password unlock/create
    /// so subsequent unlocks can be passwordless. `None` while locked.
    pub fn key_bytes(&self) -> Option<[u8; 32]> {
        self.key
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

fn derive_key(master_pw: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; 32], String> {
    let params =
        Params::new(ARGON_M_COST, ARGON_T_COST, ARGON_P_COST, Some(32)).map_err(|e| e.to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(master_pw.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}
