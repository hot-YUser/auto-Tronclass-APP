//! FFI surface for the TronClass core. The entire C ABI is three functions plus one
//! event callback; everything richer rides across as UTF-8 JSON (see `protocol`/`engine`).
//! csbindgen reads THIS file to generate the C# bindings — keep the surface narrow.

use std::ffi::c_void;

mod answer;
mod atomic_file;
mod config;
mod course_context;
mod courses;
mod engine;
mod http;
mod llm;
mod login;
mod monitor;
mod persistence;
mod protocol;
mod providers;
mod qr_remote;
mod quiz;
mod radar;
mod redaction;
mod rollcall;
mod secrets;
mod supervisor;
mod teacher_qr;

#[cfg(any(test, feature = "fakeserver"))]
pub mod fake;

#[cfg(test)]
mod test_support;

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

#[cfg(test)]
mod teacher_qr_test;

#[cfg(test)]
mod r1_test;

#[cfg(test)]
mod r2_test;

#[cfg(test)]
mod r3a_test;

#[cfg(test)]
mod r3b_test;

#[cfg(test)]
mod r4_test;

#[cfg(test)]
mod r5_test;

#[cfg(test)]
mod live_test;

use engine::Core;

/// Start the core. `cb` is invoked (from runtime worker threads) with UTF-8 JSON event
/// bytes that are valid only for the duration of each call. Returns an opaque handle; a null
/// `cb` (or a runtime build failure) yields a null handle, which the host must treat as
/// core-unavailable.
#[no_mangle]
pub extern "C" fn core_init(cb: Option<extern "C" fn(*const u8, usize)>) -> *mut c_void {
    // `Option<extern fn>` is a nullable function pointer in the C ABI. A panic here (e.g. runtime
    // build failure) must never unwind across C — null is the only failure signal a host can consume.
    // (The fn type is written literally, not as the EventCb alias, so csbindgen keeps emitting the
    // same `delegate*` C# signature — Option<fn> has no C# annotation.)
    std::panic::catch_unwind(|| match cb {
        Some(cb) => engine::init(cb)
            .ok()
            .map(|core| Box::into_raw(core) as *mut c_void),
        None => None,
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
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
    // Rust panics must never unwind across the C ABI (release profile unwinds, so this catch is
    // real). On a caught panic the awaiting command is completed with a FIXED error — the input is
    // never echoed back (it may carry secrets).
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine::send(core, bytes)));
    if result.is_err() {
        engine::panic_reply(core, bytes);
    }
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
    // Dropping the runtime must not unwind across C either.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(Box::from_raw(handle as *mut Core));
    }));
}
