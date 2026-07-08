//! Vault unlock layer (docs 10 §unlock layer). The encrypted vault is the trunk; a platform keystore
//! is an OPTIONAL layer that holds only the vault's single 32-byte key, enabling passwordless/biometric
//! unlock. Where no reliable keystore exists we degrade to the master-password path (already in
//! `secrets.rs`) — never break.
//!
//! This slice ships the trait + flow + an in-memory stub for tests. Real platform backends are Phase B.

use std::sync::Mutex;

/// Stores/loads the vault's one 32-byte key. The platform库 keeps a single key, not N secrets.
pub trait KeyStore: Send + Sync {
    fn store(&self, key: &[u8; 32]) -> Result<(), String>;
    fn load(&self) -> Option<[u8; 32]>;
    /// True only for a persistent/biometric-capable backend. The in-memory stub returns false, so
    /// `Caps.biometric_unlock` stays false until a real backend lands (Phase B).
    fn available(&self) -> bool;
}

/// In-memory, process-lifetime stub — the key is lost when the process exits, so it is only useful
/// within one run (and for tests). ponytail: a real Keychain / Android Keystore / Windows DPAPI /
/// macOS Keychain backend is the upgrade path (Phase B — needs a real device to verify biometrics).
#[derive(Default)]
pub struct MemKeyStore {
    key: Mutex<Option<[u8; 32]>>,
}

impl KeyStore for MemKeyStore {
    fn store(&self, key: &[u8; 32]) -> Result<(), String> {
        *self.key.lock().unwrap() = Some(*key);
        Ok(())
    }
    fn load(&self) -> Option<[u8; 32]> {
        *self.key.lock().unwrap()
    }
    fn available(&self) -> bool {
        false // not persistent, not biometric — a stub
    }
}
