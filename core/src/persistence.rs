//! Crash-recovery journal for mutations spanning `config.json` and `vault.bin`.
//!
//! The journal contains no secrets. Ordering makes recovery deterministic:
//! - Add writes vault first, then config. If config lacks the id, recovery removes the orphan secret.
//! - Delete writes config first, then vault. If config lacks the id, recovery completes secret deletion.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const JOURNAL_FILE: &str = "account-transaction.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountMutation {
    Add,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountJournal {
    version: u8,
    pub mutation: AccountMutation,
    pub account_id: String,
}

impl AccountJournal {
    pub fn begin(
        data_dir: &Path,
        mutation: AccountMutation,
        account_id: &str,
    ) -> Result<Self, String> {
        let journal = Self {
            version: 1,
            mutation,
            account_id: account_id.to_string(),
        };
        let bytes = serde_json::to_vec(&journal).map_err(|error| error.to_string())?;
        crate::atomic_file::create_new(&path(data_dir), &bytes)
            .map_err(|error| format!("create account transaction: {error}"))?;
        Ok(journal)
    }

    pub fn load(data_dir: &Path) -> Result<Option<Self>, String> {
        let path = path(data_dir);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read account transaction: {error}")),
        };
        let journal: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid account transaction: {error}"))?;
        if journal.version != 1 || journal.account_id.is_empty() {
            return Err("invalid account transaction metadata".to_string());
        }
        Ok(Some(journal))
    }

    pub fn complete(data_dir: &Path) -> Result<(), String> {
        crate::atomic_file::remove(&path(data_dir))
            .map_err(|error| format!("remove account transaction: {error}"))
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_is_secret_free_and_create_new() {
        let dir = std::env::temp_dir().join(format!("tron-journal-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        AccountJournal::begin(&dir, AccountMutation::Add, "opaque-account").unwrap();
        let raw = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!raw.contains("password") && !raw.contains("cookie"));
        assert!(AccountJournal::begin(&dir, AccountMutation::Delete, "other").is_err());
        assert_eq!(
            AccountJournal::load(&dir).unwrap().unwrap().mutation,
            AccountMutation::Add
        );
        AccountJournal::complete(&dir).unwrap();
        assert!(AccountJournal::load(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
