package com.pilotdeck.memory;

/**
 * Raw JNI shim implemented in Rust
 * (`sdk/pd-memory/crates/pd-memory-ffi/src/jni_bridge.rs`).
 *
 * <p>The engine address returned by {@link #newEngine(String)} is an opaque
 * pointer to the Rust `PdMemoryHandle`. It is only valid until
 * {@link #freeEngine(long)} is called, and the caller must call that exactly
 * once. {@link PdMemory} owns the address and exposes a safe Kotlin API; this
 * class is package-private on purpose.</p>
 *
 * <p>All methods are blocking (file I/O and possibly model HTTP calls), so they
 * must run on a background thread. {@link PdMemory} dispatches on
 * `Dispatchers.IO`.</p>
 */
final class PdMemoryBridge {

    static {
        System.loadLibrary("pd_memory_ffi");
    }

    private PdMemoryBridge() {
        // Static native methods only.
    }

    /**
     * Creates an engine from a JSON config.
     *
     * @return the handle address, or {@code 0} after throwing a
     *         {@link RuntimeException} whose message is the JSON error envelope.
     */
    static native long newEngine(String configJson);

    /**
     * Runs one operation and returns the JSON envelope
     * ({@code {"ok":true,"result":...}} or {@code {"ok":false,...}}).
     * Throws {@link RuntimeException} only for JNI-level failures.
     */
    static native String callEngine(long handle, String op, String argsJson);

    /** Frees an engine handle. {@code 0} is a no-op. */
    static native void freeEngine(long handle);

    /** Last error envelope for the handle, or the thread-local error for {@code 0}. */
    static native String lastError(long handle);

    /** Native library version, e.g. {@code "0.1.0"}. */
    static native String version();
}
