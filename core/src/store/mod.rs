//! What the core keeps on disk: the config, the vault, the school registry, and the journal
//! that keeps the first two in step.

pub mod atomic_file;
pub mod config;
pub mod journal;
pub mod providers;
pub mod secrets;
