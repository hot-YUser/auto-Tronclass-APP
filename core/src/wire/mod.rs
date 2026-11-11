//! The schema across the FFI seam: commands in, events out, both as UTF-8 JSON.

mod command;
mod emit;
mod event;
mod snapshot;

pub use command::*;
pub use emit::{emit, respond, EventCb};
pub use event::*;
pub use snapshot::*;
