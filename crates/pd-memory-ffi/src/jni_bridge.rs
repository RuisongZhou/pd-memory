//! JNI bindings for the Java/Kotlin class `com.pilotdeck.memory.PdMemoryBridge`.
//!
//! The Android sample declares the matching native methods as `static native`
//! in `android/pd-memory/src/main/java/com/pilotdeck/memory/PdMemoryBridge.java`:
//!
//! ```java
//! static native long   newEngine(String configJson);
//! static native String callEngine(long handle, String op, String argsJson);
//! static native void   freeEngine(long handle);
//! static native String lastError(long handle);
//! static native String version();
//! ```
//!
//! # Handle lifecycle over JNI
//!
//! `newEngine` builds an engine and returns its boxed address as a `jlong`
//! (`0` = failure). Kotlin keeps that number for the lifetime of the
//! `PdMemory` wrapper and passes it back to `callEngine` / `lastError`, then
//! calls `freeEngine(handle)` exactly once from `PdMemory.close()`. The wrapper
//! zeroes its field before calling `freeEngine` so `close()` is idempotent.
//!
//! Every entry point is wrapped in `catch_unwind`: a panic is converted into a
//! `java.lang.RuntimeException` (or, for `callEngine`, a normal error envelope
//! whenever the engine itself reported the failure), so Rust panics never unwind
//! into the JVM.
//!
//! The engine is blocking; callers must dispatch from a background thread
//! (`Dispatchers.IO` in the Kotlin wrapper) and never from the UI thread.

use std::panic::{catch_unwind, AssertUnwindSafe};

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

use crate::{
    call_envelope, engine_from_config_json, error_envelope, panic_message, pd_memory_version,
    set_thread_last_error, thread_last_error, FfiError, PdMemoryHandle,
};

/// Exception class used for JNI-level failures (invalid arguments, init errors).
const RUNTIME_EXCEPTION: &str = "java/lang/RuntimeException";

/// Format an FFI error as the JSON envelope used on the C surface.
fn error_json(error: &FfiError) -> String {
    error_envelope(&error.code, &error.message)
}

/// Throw a `RuntimeException` carrying `envelope` as its message.
fn throw(env: &mut JNIEnv, envelope: &str) {
    // A failed throw is not recoverable from Rust; the JVM will surface an
    // exception on the next JNI call anyway.
    let _ = env.throw_new(RUNTIME_EXCEPTION, envelope);
}

/// Read a required Java string into a Rust `String`.
fn read_jstring(env: &mut JNIEnv, value: &JString, what: &str) -> Result<String, FfiError> {
    if value.as_raw().is_null() {
        return Err(FfiError::new(
            "null_pointer",
            format!("`{what}` must not be null"),
        ));
    }
    env.get_string(value).map(String::from).map_err(|error| {
        FfiError::new("invalid_input", format!("could not read `{what}`: {error}"))
    })
}

/// Allocate a Java string; throws and returns null on allocation failure.
fn new_jstring(env: &mut JNIEnv, value: String) -> jstring {
    match env.new_string(value) {
        Ok(string) => string.into_raw(),
        Err(error) => {
            throw(
                env,
                &error_envelope("jni", &format!("could not allocate Java string: {error}")),
            );
            std::ptr::null_mut()
        }
    }
}

/// `static native long newEngine(String configJson)`.
///
/// Returns the handle address, or `0` after throwing a `RuntimeException` whose
/// message is the JSON error envelope (also stored in the thread-local error
/// slot so `lastError(0)` can read it).
#[no_mangle]
pub extern "system" fn Java_com_pilotdeck_memory_PdMemoryBridge_newEngine(
    mut env: JNIEnv,
    _class: JClass,
    config_json: JString,
) -> jlong {
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<jlong, FfiError> {
        let config_json = read_jstring(&mut env, &config_json, "configJson")?;
        let engine = engine_from_config_json(&config_json)?;
        let handle = Box::into_raw(Box::new(PdMemoryHandle::new(engine)));
        Ok(handle as jlong)
    }));

    match outcome {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => {
            let envelope = error_json(&error);
            set_thread_last_error(envelope.clone());
            throw(&mut env, &envelope);
            0
        }
        Err(payload) => {
            let envelope = error_envelope(
                "panic",
                &format!(
                    "panic while creating the memory engine: {}",
                    panic_message(payload.as_ref())
                ),
            );
            set_thread_last_error(envelope.clone());
            throw(&mut env, &envelope);
            0
        }
    }
}

/// `static native String callEngine(long handle, String op, String argsJson)`.
///
/// Returns the JSON envelope (`{"ok":...}`). Engine-level failures are normal
/// envelopes; JNI-level failures (null/undecodable strings, panic) throw.
#[no_mangle]
pub extern "system" fn Java_com_pilotdeck_memory_PdMemoryBridge_callEngine(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    op: JString,
    args_json: JString,
) -> jstring {
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<String, FfiError> {
        let op = read_jstring(&mut env, &op, "op")?;
        let args_json = read_jstring(&mut env, &args_json, "argsJson")?;
        if handle == 0 {
            return Ok(error_envelope(
                "null_handle",
                "callEngine received a null handle (was the engine already closed?)",
            ));
        }
        // SAFETY: the Kotlin wrapper owns this address until freeEngine; a
        // non-zero handle is a live `PdMemoryHandle` from `newEngine`.
        let handle_ref = unsafe { &*(handle as *mut PdMemoryHandle) };
        Ok(call_envelope(handle_ref, &op, &args_json))
    }));

    match outcome {
        Ok(Ok(envelope)) => new_jstring(&mut env, envelope),
        Ok(Err(error)) => {
            let envelope = error_json(&error);
            set_thread_last_error(envelope.clone());
            throw(&mut env, &envelope);
            std::ptr::null_mut()
        }
        Err(payload) => {
            let envelope = error_envelope(
                "panic",
                &format!("panic in callEngine: {}", panic_message(payload.as_ref())),
            );
            set_thread_last_error(envelope.clone());
            throw(&mut env, &envelope);
            std::ptr::null_mut()
        }
    }
}

/// `static native void freeEngine(long handle)`.
///
/// `0` is a no-op. The handle must not be used afterwards; double frees are the
/// caller's responsibility (the Kotlin wrapper zeroes its slot first).
#[no_mangle]
pub extern "system" fn Java_com_pilotdeck_memory_PdMemoryBridge_freeEngine(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller contract: address from `newEngine`, freed exactly once.
        let _handle = unsafe { Box::from_raw(handle as *mut PdMemoryHandle) };
    }));
}

/// `static native String lastError(long handle)`.
///
/// Returns the last error envelope for the handle, or the thread-local error
/// when `handle == 0`. Never null: an empty string means "no error recorded".
#[no_mangle]
pub extern "system" fn Java_com_pilotdeck_memory_PdMemoryBridge_lastError(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let message = match catch_unwind(AssertUnwindSafe(|| {
        if handle == 0 {
            thread_last_error()
        } else {
            // SAFETY: caller contract: live handle from `newEngine`.
            unsafe { &*(handle as *mut PdMemoryHandle) }
                .last_error()
                .unwrap_or_default()
        }
    })) {
        Ok(message) => message,
        Err(_) => String::new(),
    };
    new_jstring(&mut env, message)
}

/// `static native String version()`.
#[no_mangle]
pub extern "system" fn Java_com_pilotdeck_memory_PdMemoryBridge_version(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    // SAFETY: `pd_memory_version` returns a static NUL-terminated UTF-8 string.
    let version = unsafe {
        std::ffi::CStr::from_ptr(pd_memory_version())
            .to_string_lossy()
            .into_owned()
    };
    new_jstring(&mut env, version)
}
