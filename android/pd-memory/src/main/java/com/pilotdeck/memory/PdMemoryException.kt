package com.pilotdeck.memory

import org.json.JSONException

/**
 * Error returned by the Rust engine (a `{"ok":false,"code":...,"message":...}`
 * envelope) or by the JNI bridge itself.
 *
 * [code] is the stable machine-readable error code documented in
 * `crates/pd-memory-ffi/include/pd_memory.h` (`invalid_config`, `io`, `model`,
 * `unsupported`, `invalid_input`, `unknown_op`, `null_handle`, `panic`, ...).
 */
class PdMemoryException(
    val code: String,
    message: String,
    cause: Throwable? = null,
) : RuntimeException("[$code] $message", cause) {

    internal companion object {
        /**
         * Builds an exception from a JSON error envelope. JNI failures carry the
         * envelope as the `RuntimeException` message; anything unparseable falls
         * back to [fallbackCode] with the raw text as the message.
         */
        fun fromEnvelope(raw: String?, fallbackCode: String): PdMemoryException {
            if (raw.isNullOrBlank()) {
                return PdMemoryException(fallbackCode, "pd-memory operation failed (no error detail)")
            }
            return try {
                val envelope = org.json.JSONObject(raw)
                if (envelope.optBoolean("ok", false)) {
                    PdMemoryException(fallbackCode, raw)
                } else {
                    PdMemoryException(
                        envelope.optString("code", fallbackCode),
                        envelope.optString("message", raw),
                    )
                }
            } catch (error: JSONException) {
                PdMemoryException(fallbackCode, raw, error)
            }
        }
    }
}
