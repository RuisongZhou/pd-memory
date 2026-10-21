//! C ABI + JNI bindings so `pd-memory` runs inside Android apps (dev D).
//!
//! # Design
//!
//! One opaque [`PdMemoryHandle`] wraps an `Arc<MemoryEngine>` created from a JSON
//! config. Every operation is JSON-in / JSON-out:
//!
//! ```text
//! config JSON  --pd_memory_new-->  *mut PdMemoryHandle
//! handle + op + args JSON --pd_memory_call--> heap C string
//!   {"ok":true,"result":<op result>}
//!   {"ok":false,"code":"<stable code>","message":"<human readable>"}
//! ```
//!
//! The same dispatch table backs the JNI surface used by the Kotlin wrapper
//! (`com.pilotdeck.memory.PdMemoryBridge`, see the `jni_bridge` module), so
//! Android and C hosts observe exactly the same behaviour. The engine is
//! synchronous by design (`pd_memory_core::engine::MemoryEngine` is blocking):
//! hosts must call it from a worker thread, never from a UI thread. The Kotlin
//! wrapper does this with `Dispatchers.IO`.
//!
//! # Handle lifecycle
//!
//! * `pd_memory_new` returns an owned pointer (or `NULL` on error; read
//!   [`pd_memory_last_error`] with a `NULL` handle for the reason).
//! * The handle is `Send + Sync`-safe: the engine serialises its own mutable
//!   state, so multiple threads may call `pd_memory_call` on the same handle.
//! * `pd_memory_free` must be called exactly once and no other call may run
//!   concurrently with it. On the JNI surface `freeEngine` takes the address
//!   back; the Kotlin `PdMemory.close()` wrapper is idempotent.
//! * Strings returned by `pd_memory_call` / `pd_memory_last_error` are
//!   heap-allocated and must be released with `pd_memory_string_free`.
//!   `pd_memory_version` returns a static string that must NOT be freed.
//!
//! # Panic safety
//!
//! Every entry point catches panics (`catch_unwind`) and converts them into
//! error envelopes / thrown `RuntimeException`s, so a bug inside the engine
//! cannot unwind across the C/JNI boundary (which would be undefined behaviour).
//! Note: the workspace release profile uses `panic = "abort"`, so in release
//! builds an abort still terminates the process; the guard protects debug and
//! test builds, and all engine code is expected to return `MemoryError`.
//!
//! # Features
//!
//! `http-model` (off by default) links `pd-memory-llm` and therefore the
//! `ureq`/`rustls`/`ring` stack. Android device builds enable it; the default
//! (model-free) build compiles for `*-linux-android` without an NDK so CI can
//! verify the FFI surface with a plain `cargo check --target`.

use std::any::Any;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::MemoryError;

mod dispatch;
mod jni_bridge;
mod model;

/// Op names accepted by [`pd_memory_call`] (case/separator-insensitive).
pub(crate) const SUPPORTED_OPS: &str = "add (alias capture_turn), retrieve, search, list, get, overview, \
     flush, dream, rollback, export, import, settings_get, settings_save, act, state_get, memory_context";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A stable FFI error: `code` is machine-readable, `message` is for humans.
#[derive(Debug, Clone)]
pub(crate) struct FfiError {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl FfiError {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub(crate) fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new("other", message)
    }

    pub(crate) fn unknown_op(op: &str) -> Self {
        Self::new(
            "unknown_op",
            format!("unknown op `{op}`; supported ops: {SUPPORTED_OPS}"),
        )
    }
}

impl From<MemoryError> for FfiError {
    fn from(error: MemoryError) -> Self {
        Self {
            code: error.code().to_string(),
            message: error.to_string(),
        }
    }
}

/// Build the `{"ok":true,"result":...}` envelope (cannot fail).
fn ok_envelope(result: serde_json::Value) -> String {
    json!({ "ok": true, "result": result }).to_string()
}

/// Build the `{"ok":false,"code":...,"message":...}` envelope (cannot fail).
fn error_envelope(code: &str, message: &str) -> String {
    json!({ "ok": false, "code": code, "message": message }).to_string()
}

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Opaque engine handle shared by the C ABI and JNI surfaces.
///
/// Created by [`pd_memory_new`], consumed by [`pd_memory_free`]. The struct is
/// deliberately not `#[repr(C)]`: C/JNI hosts only ever hold a pointer and must
/// not inspect the layout.
pub struct PdMemoryHandle {
    engine: Arc<MemoryEngine>,
    /// Last error envelope observed on this handle (`pd_memory_last_error`).
    last_error: Mutex<Option<String>>,
}

impl PdMemoryHandle {
    /// Wrap an already-built engine.
    pub fn new(engine: MemoryEngine) -> Self {
        Self {
            engine: Arc::new(engine),
            last_error: Mutex::new(None),
        }
    }

    /// The engine behind this handle.
    pub fn engine(&self) -> &MemoryEngine {
        &self.engine
    }

    /// Record an error envelope observed on this handle.
    pub(crate) fn set_last_error(&self, envelope: String) {
        match self.last_error.lock() {
            Ok(mut slot) => *slot = Some(envelope),
            // A poisoned mutex must not take down the FFI boundary: the payload
            // is still readable and we can overwrite it.
            Err(poisoned) => *poisoned.into_inner() = Some(envelope),
        }
    }

    /// Last error envelope, if any.
    pub(crate) fn last_error(&self) -> Option<String> {
        match self.last_error.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local error slot (errors that happen before a handle exists)
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_FFI_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn set_thread_last_error(envelope: String) {
    LAST_FFI_ERROR.with(|slot| *slot.borrow_mut() = Some(envelope));
}

fn thread_last_error() -> String {
    LAST_FFI_ERROR.with(|slot| slot.borrow().clone().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// Version of the FFI crate as a static NUL-terminated UTF-8 string.
///
/// Do **not** free the returned pointer.
#[no_mangle]
pub extern "C" fn pd_memory_version() -> *const c_char {
    // `concat!` yields a `&'static str`; the trailing NUL makes it a C string.
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION.as_ptr().cast()
}

/// Create an engine handle from a JSON config.
///
/// Returns `NULL` on failure; in that case [`pd_memory_last_error`] called with
/// a `NULL` handle returns the error envelope of the current thread.
///
/// # Safety
/// `config_json` must be `NULL` or point to a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn pd_memory_new(config_json: *const c_char) -> *mut PdMemoryHandle {
    // A new attempt starts with a clean thread-local error slot.
    LAST_FFI_ERROR.with(|slot| *slot.borrow_mut() = None);

    let outcome = catch_unwind(AssertUnwindSafe(
        || -> Result<*mut PdMemoryHandle, FfiError> {
            // SAFETY: caller contract guarantees NULL or a NUL-terminated string.
            let config_json = unsafe { read_c_string(config_json, "config_json") }?;
            let engine = engine_from_config_json(&config_json)?;
            Ok(Box::into_raw(Box::new(PdMemoryHandle::new(engine))))
        },
    ));

    match outcome {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => {
            set_thread_last_error(error_envelope(&error.code, &error.message));
            std::ptr::null_mut()
        }
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            set_thread_last_error(error_envelope(
                "panic",
                &format!("panic while creating the memory engine: {message}"),
            ));
            std::ptr::null_mut()
        }
    }
}

/// Free a handle. `NULL` is a no-op.
///
/// # Safety
/// `handle` must come from [`pd_memory_new`], must be freed exactly once, and
/// must not be used (or freed) concurrently.
#[no_mangle]
pub unsafe extern "C" fn pd_memory_free(handle: *mut PdMemoryHandle) {
    if handle.is_null() {
        return;
    }
    // Dropping the engine should never panic, but a panic must not cross the C
    // boundary: swallow it if it happens.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller contract: pointer comes from `pd_memory_new`, freed once.
        let _handle = unsafe { Box::from_raw(handle) };
    }));
}

/// Call one operation on a handle.
///
/// `op` is a case/separator-insensitive operation name (see the crate docs);
/// `args_json` is an optional JSON object (`NULL` or `""` means `{}`). Returns a
/// heap-allocated JSON envelope and is never `NULL` unless allocation fails;
/// free it with [`pd_memory_string_free`].
///
/// # Safety
/// `handle` must be a live handle from [`pd_memory_new`]. `op` and `args_json`
/// must be `NULL` or valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn pd_memory_call(
    handle: *mut PdMemoryHandle,
    op: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    let envelope = match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded caller contract (live handle, valid C strings).
        unsafe { call_impl(handle, op, args_json) }
    })) {
        Ok(envelope) => envelope,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            let envelope = error_envelope("panic", &format!("panic in pd_memory_call: {message}"));
            if handle.is_null() {
                set_thread_last_error(envelope.clone());
            } else {
                set_handle_last_error(handle, &envelope);
            }
            envelope
        }
    };
    c_string(envelope)
}

/// Return the last error envelope observed for `handle`.
///
/// With a `NULL` handle this returns the thread-local error of the last failed
/// [`pd_memory_new`] (or `NULL`-handle call). The returned string is
/// heap-allocated (possibly empty) and must be freed with
/// [`pd_memory_string_free`].
///
/// # Safety
/// `handle` must be `NULL` or a live handle from [`pd_memory_new`].
#[no_mangle]
pub unsafe extern "C" fn pd_memory_last_error(handle: *mut PdMemoryHandle) -> *mut c_char {
    let message = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            thread_last_error()
        } else {
            // SAFETY: caller contract guarantees a live handle.
            unsafe { (*handle).last_error() }.unwrap_or_default()
        }
    }))
    .unwrap_or_default();
    c_string(message)
}

/// Free a heap string returned by [`pd_memory_call`] / [`pd_memory_last_error`].
///
/// # Safety
/// `value` must be `NULL` or a pointer returned by this crate that has not been
/// freed yet. Never pass a static string such as [`pd_memory_version`]'s.
#[no_mangle]
pub unsafe extern "C" fn pd_memory_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller contract: pointer came from `CString::into_raw` here.
        drop(unsafe { CString::from_raw(value) });
    }));
}

// ---------------------------------------------------------------------------
// Internals shared with the JNI surface
// ---------------------------------------------------------------------------

/// Build an engine from a JSON config (model provider included).
pub(crate) fn engine_from_config_json(config_json: &str) -> Result<MemoryEngine, FfiError> {
    let config = MemoryConfig::from_json_str(config_json).map_err(FfiError::from)?;
    let model = model::build_model(&config.model).map_err(FfiError::from)?;
    MemoryEngine::open(config, model).map_err(FfiError::from)
}

/// Run one op through the dispatcher and wrap it in an envelope.
///
/// Used by both [`pd_memory_call`] and JNI `callEngine`; errors are also
/// recorded as the handle's last error.
pub(crate) fn call_envelope(handle: &PdMemoryHandle, op: &str, args_json: &str) -> String {
    match dispatch::dispatch(handle.engine(), op, args_json) {
        Ok(result) => ok_envelope(result),
        Err(error) => {
            let envelope = error_envelope(&error.code, &error.message);
            handle.set_last_error(envelope.clone());
            envelope
        }
    }
}

unsafe fn call_impl(
    handle: *mut PdMemoryHandle,
    op: *const c_char,
    args_json: *const c_char,
) -> String {
    let op = match unsafe { read_c_string(op, "op") } {
        Ok(op) => op,
        Err(error) => {
            let envelope = error_envelope(&error.code, &error.message);
            if handle.is_null() {
                set_thread_last_error(envelope.clone());
            } else {
                set_handle_last_error(handle, &envelope);
            }
            return envelope;
        }
    };
    let args_json = match unsafe { read_optional_c_string(args_json, "args_json") } {
        Ok(args_json) => args_json,
        Err(error) => {
            let envelope = error_envelope(&error.code, &error.message);
            if handle.is_null() {
                set_thread_last_error(envelope.clone());
            } else {
                set_handle_last_error(handle, &envelope);
            }
            return envelope;
        }
    };
    if handle.is_null() {
        let envelope = error_envelope("null_handle", "pd_memory_call received a NULL handle");
        set_thread_last_error(envelope.clone());
        return envelope;
    }
    // SAFETY: caller contract requires a live handle; checked non-null above.
    let handle = unsafe { &*handle };
    call_envelope(handle, &op, &args_json)
}

fn set_handle_last_error(handle: *mut PdMemoryHandle, envelope: &str) {
    // SAFETY: only touched when non-null; caller contract requires a live handle.
    if let Some(handle) = unsafe { handle.as_ref() } {
        handle.set_last_error(envelope.to_string());
    }
}

/// Read a required C string argument.
unsafe fn read_c_string(ptr: *const c_char, what: &str) -> Result<String, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::new(
            "null_pointer",
            format!("`{what}` must not be NULL"),
        ));
    }
    // SAFETY: caller contract: non-null pointer to a NUL-terminated C string.
    let text = unsafe { CStr::from_ptr(ptr) };
    text.to_str()
        .map(str::to_string)
        .map_err(|error| FfiError::invalid_input(format!("`{what}` is not valid UTF-8: {error}")))
}

/// Read an optional C string argument (`NULL` -> empty string).
unsafe fn read_optional_c_string(ptr: *const c_char, what: &str) -> Result<String, FfiError> {
    if ptr.is_null() {
        Ok(String::new())
    } else {
        // SAFETY: caller contract: non-null pointer to a NUL-terminated string.
        unsafe { read_c_string(ptr, what) }
    }
}

/// Allocate a heap C string. Interior NUL bytes (impossible in JSON, but panic
/// messages are arbitrary text) are dropped rather than failing the call.
fn c_string(value: String) -> *mut c_char {
    let mut bytes = value.into_bytes();
    bytes.retain(|byte| *byte != 0);
    match CString::new(bytes) {
        Ok(value) => value.into_raw(),
        // Unreachable: the bytes contain no NUL, so `CString::new` cannot fail.
        Err(_) => CString::default().into_raw(),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests: drive the unsafe C ABI directly (host target, no JVM)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::path::Path;

    fn c(value: &str) -> CString {
        CString::new(value).expect("test string has no NUL")
    }

    fn read_c(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null(), "expected a heap C string");
        // SAFETY: every pointer produced by this crate is NUL-terminated UTF-8.
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .expect("FFI strings are valid UTF-8")
            .to_string();
        unsafe { pd_memory_string_free(ptr) };
        text
    }

    fn read_static_c(ptr: *const c_char) -> String {
        assert!(!ptr.is_null());
        // SAFETY: static NUL-terminated version string.
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .expect("version is UTF-8")
            .to_string()
    }

    fn call(handle: *mut PdMemoryHandle, op: &str, args: &str) -> serde_json::Value {
        let op = c(op);
        let args = c(args);
        // SAFETY: the handle is live for the duration of the test.
        let out = unsafe { pd_memory_call(handle, op.as_ptr(), args.as_ptr()) };
        let text = read_c(out);
        serde_json::from_str(&text).expect("FFI envelope is JSON")
    }

    fn last_error(handle: *mut PdMemoryHandle) -> String {
        // SAFETY: NULL is explicitly supported for the thread-local error.
        let out = unsafe { pd_memory_last_error(handle) };
        read_c(out)
    }

    fn new_handle(dir: &Path) -> *mut PdMemoryHandle {
        let config = json!({
            "data_root": dir.to_string_lossy(),
            "workspace_id": "ffi-tests",
        });
        let config = c(&config.to_string());
        // SAFETY: valid NUL-terminated config JSON.
        let handle = unsafe { pd_memory_new(config.as_ptr()) };
        if handle.is_null() {
            panic!("pd_memory_new failed: {}", last_error(std::ptr::null_mut()));
        }
        handle
    }

    #[test]
    fn version_is_a_static_utf8_string() {
        let first = pd_memory_version();
        let second = pd_memory_version();
        assert_eq!(first, second, "version pointer must be static");
        assert_eq!(read_static_c(first), env!("CARGO_PKG_VERSION"));
        // Freeing NULL must be harmless (hosts commonly call it unconditionally).
        unsafe { pd_memory_string_free(std::ptr::null_mut()) };
    }

    #[test]
    fn smoke_new_overview_add_list_export_free() {
        let dir = pd_memory_testkit::temp_data_root();
        let handle = new_handle(dir.path());

        let overview = call(handle, "overview", "");
        assert_eq!(overview["ok"], json!(true), "overview envelope: {overview}");
        assert!(
            overview["result"]["total_files"].is_number(),
            "overview result: {}",
            overview["result"]
        );

        let add_args = json!({
            "messages": [
                {"role": "user", "content": "The project uses Cargo workspaces."},
                {"role": "assistant", "content": "Noted."}
            ],
            "session_id": "ffi-session",
            "source": "test"
        });
        let added = call(handle, "add", &add_args.to_string());
        assert_eq!(added["ok"], json!(true), "add envelope: {added}");
        assert_eq!(added["result"]["captured"], json!(true));
        assert_eq!(added["result"]["session_key"], json!("ffi-session"));

        // `capture_turn` is an alias of `add`.
        let alias = json!({
            "messages": [{"role": "user", "content": "Second turn."}],
            "session_id": "ffi-session"
        });
        let captured = call(handle, "capture_turn", &alias.to_string());
        assert_eq!(captured["ok"], json!(true), "capture_turn: {captured}");

        let listed = call(handle, "list", "{}");
        assert_eq!(listed["ok"], json!(true), "list envelope: {listed}");
        assert!(listed["result"].is_array());

        let got = call(
            handle,
            "get",
            &json!({"ids": [], "max_lines": 10}).to_string(),
        );
        assert_eq!(got["ok"], json!(true), "get envelope: {got}");
        assert!(got["result"].is_array());

        let settings = call(
            handle,
            "settings_save",
            &json!({"settings": {
                "reasoning_mode": "accuracy_first",
                "auto_index_interval_minutes": 15,
                "auto_dream_interval_minutes": 45
            }})
            .to_string(),
        );
        assert_eq!(settings["ok"], json!(true), "settings_save: {settings}");
        let loaded = call(handle, "settings_get", "");
        assert_eq!(loaded["result"]["auto_index_interval_minutes"], json!(15));

        let exported = call(handle, "export", "");
        assert_eq!(exported["ok"], json!(true), "export envelope: {exported}");
        assert!(exported["result"]["format_version"].is_string());

        let state = call(
            handle,
            "state_get",
            &json!({"key": "workspaceDir"}).to_string(),
        );
        assert_eq!(state["ok"], json!(true), "state_get: {state}");
        assert!(state["result"].is_string());

        // Freeing is the end of the handle's life; do not touch it afterwards.
        unsafe { pd_memory_free(handle) };
    }

    #[test]
    fn unknown_op_returns_error_envelope_and_records_last_error() {
        let dir = pd_memory_testkit::temp_data_root();
        let handle = new_handle(dir.path());

        let result = call(handle, "not_an_operation", "{}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["code"], json!("unknown_op"));
        assert!(
            result["message"]
                .as_str()
                .unwrap_or_default()
                .contains("add"),
            "message should hint at the supported ops: {result}"
        );

        let recorded = last_error(handle);
        assert!(recorded.contains("unknown_op"), "last_error: {recorded}");
        assert!(recorded.contains("\"ok\":false"));

        unsafe { pd_memory_free(handle) };
    }

    #[test]
    fn invalid_arguments_are_reported_without_panicking() {
        let dir = pd_memory_testkit::temp_data_root();
        let handle = new_handle(dir.path());

        let broken_json = call(handle, "add", "{not json");
        assert_eq!(broken_json["ok"], json!(false));
        assert_eq!(broken_json["code"], json!("invalid_input"));

        let missing_messages = call(handle, "add", &json!({"session_id": "s"}).to_string());
        assert_eq!(missing_messages["code"], json!("invalid_input"));

        let missing_query = call(handle, "retrieve", "{}");
        assert_eq!(missing_query["code"], json!("invalid_input"));

        let bad_trigger = call(
            handle,
            "dream",
            &json!({"trigger": "sometimes"}).to_string(),
        );
        assert_eq!(bad_trigger["code"], json!("invalid_input"));

        unsafe { pd_memory_free(handle) };
    }

    #[test]
    fn null_handles_are_safe() {
        let null = std::ptr::null_mut();

        let result = call(null, "overview", "{}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["code"], json!("null_handle"));
        let thread_error = last_error(null);
        assert!(
            thread_error.contains("null_handle"),
            "last_error: {thread_error}"
        );

        // Freeing NULL handles/strings is a documented no-op.
        unsafe {
            pd_memory_free(null);
            pd_memory_string_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn invalid_config_returns_null_with_thread_local_error() {
        let broken = c("{this is not json");
        // SAFETY: NUL-terminated UTF-8 string.
        let handle = unsafe { pd_memory_new(broken.as_ptr()) };
        assert!(handle.is_null(), "invalid config must not create a handle");
        let error = last_error(std::ptr::null_mut());
        assert!(error.contains("invalid_config"), "last_error: {error}");

        // A config without data_root must be rejected too.
        let empty = c("{}");
        let handle = unsafe { pd_memory_new(empty.as_ptr()) };
        assert!(handle.is_null());
        assert!(last_error(std::ptr::null_mut()).contains("invalid_config"));

        // NULL config pointer is reported, not dereferenced.
        let handle = unsafe { pd_memory_new(std::ptr::null()) };
        assert!(handle.is_null());
        assert!(last_error(std::ptr::null_mut()).contains("null_pointer"));

        // Non-UTF-8 config bytes are rejected.
        let invalid_utf8 = CStr::from_bytes_with_nul(&[0xffu8, 0xfe, b'{', b'}', 0])
            .expect("NUL-terminated bytes");
        let handle = unsafe { pd_memory_new(invalid_utf8.as_ptr()) };
        assert!(handle.is_null());
        assert!(last_error(std::ptr::null_mut()).contains("not valid UTF-8"));
    }

    #[test]
    fn non_utf8_op_name_is_rejected() {
        let dir = pd_memory_testkit::temp_data_root();
        let handle = new_handle(dir.path());

        let op = CStr::from_bytes_with_nul(&[0xffu8, b'o', b'p', 0]).expect("NUL-terminated bytes");
        let args = c("{}");
        // SAFETY: handle is live; args is a valid C string; op is invalid UTF-8.
        let out = unsafe { pd_memory_call(handle, op.as_ptr(), args.as_ptr()) };
        let envelope: serde_json::Value =
            serde_json::from_str(&read_c(out)).expect("FFI envelope is JSON");
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["code"], json!("invalid_input"));

        unsafe { pd_memory_free(handle) };
    }

    #[test]
    fn model_configured_without_feature_is_rejected() {
        if cfg!(feature = "http-model") {
            return;
        }
        let dir = pd_memory_testkit::temp_data_root();
        let config = json!({
            "data_root": dir.path().to_string_lossy(),
            "workspace_id": "ffi-tests",
            "model": {"model": "gpt-4o-mini", "base_url": "https://example.invalid/v1"}
        });
        let config = c(&config.to_string());
        // SAFETY: valid NUL-terminated config JSON.
        let handle = unsafe { pd_memory_new(config.as_ptr()) };
        assert!(
            handle.is_null(),
            "a configured model without the http-model feature must fail loudly"
        );
        let error = last_error(std::ptr::null_mut());
        assert!(error.contains("unsupported"), "last_error: {error}");
        assert!(error.contains("http-model"), "last_error: {error}");
    }
}
