//! FFI surface for the TronClass core. The entire C ABI is three functions plus one
//! event callback; everything richer rides across as UTF-8 JSON (see `protocol`/`engine`).
//! csbindgen reads THIS file to generate the C# bindings — keep the surface narrow.

use std::ffi::c_void;

mod answer;
mod config;
mod engine;
mod keystore;
mod llm;
mod login;
mod monitor;
mod protocol;
mod providers;
mod quiz;
mod radar;
mod redaction;
mod rollcall;
mod secrets;

#[cfg(any(test, feature = "fakeserver"))]
pub mod fake;

#[cfg(test)]
mod seam_test;

#[cfg(test)]
mod slice1_test;

#[cfg(test)]
mod slice2_test;

#[cfg(test)]
mod slice3_test;

#[cfg(test)]
mod slice4_test;

use engine::Core;

/// Start the core. `cb` is invoked (from runtime worker threads) with UTF-8 JSON event
/// bytes that are valid only for the duration of each call. Returns an opaque handle.
#[no_mangle]
pub extern "C" fn core_init(cb: extern "C" fn(*const u8, usize)) -> *mut c_void {
    Box::into_raw(engine::init(cb)) as *mut c_void
}

/// Send one UTF-8 JSON command. Returns immediately; results arrive via the callback.
///
/// # Safety
/// `handle` must be a live pointer from `core_init`; `json_ptr`/`json_len` must describe
/// a valid byte range for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn core_send(handle: *mut c_void, json_ptr: *const u8, json_len: usize) {
    if handle.is_null() || json_ptr.is_null() {
        return;
    }
    let core = &*(handle as *const Core);
    let bytes = std::slice::from_raw_parts(json_ptr, json_len);
    engine::send(core, bytes);
}

/// Free the handle and shut down its runtime. The handle must not be used afterwards.
///
/// # Safety
/// `handle` must be a live pointer from `core_init` and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn core_free(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    drop(Box::from_raw(handle as *mut Core));
}
