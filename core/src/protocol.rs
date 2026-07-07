//! The wire schema across the FFI seam. Commands (UI → core) are parsed strictly —
//! this is a trust boundary, so a malformed command becomes an Error event, never a panic.
//! Events (core → UI) are emitted as free-form JSON at the call site (see `engine::emit`);
//! for a skeleton the event set is tiny and a rigid enum would be premature.

use serde::Deserialize;

/// UI → core. Internally tagged by `cmd`; every variant carries the correlation `id`
/// the caller assigned, which the core echoes back on the matching reply event.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    Login {
        id: u64,
        base_url: String,
        username: String,
        password: String,
    },
}
