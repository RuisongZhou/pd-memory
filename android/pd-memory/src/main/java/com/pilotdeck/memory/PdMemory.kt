package com.pilotdeck.memory

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import java.io.Closeable

/**
 * Coroutine-friendly wrapper around the Rust `pd-memory` engine.
 *
 * ## Lifecycle
 *
 * Create with [PdMemory.create] (suspending, runs on [Dispatchers.IO]), use the
 * `suspend` methods, then call [close] exactly once. [close] is idempotent; a
 * closed engine rejects further calls with [PdMemoryException] code `closed`.
 *
 * ```kotlin
 * val memory = PdMemory.create(
 *     """{"data_root":"${filesDir}/pd-memory","workspace_id":"my-project"}"""
 * )
 * memory.addTurn("user", "Remember: the build is cargo-based.", sessionId = "s1")
 * val context = memory.memoryContext("how do we build?")   // null without a model
 * memory.close()
 * ```
 *
 * ## Threading
 *
 * The engine is blocking: it does file I/O and may call a model over the
 * network. Every public call therefore dispatches to [Dispatchers.IO]; never
 * call the engine synchronously from the UI thread. The handle itself is
 * thread-safe (the Rust engine serialises its mutable state), so multiple
 * coroutines may share one [PdMemory] instance.
 *
 * ## Configuration
 *
 * [PdMemory.create] takes the `pd-memory-core` `MemoryConfig` JSON. Capture,
 * list, get, overview, export and settings work without a model. Retrieval,
 * flush and dream need `model.model` / `model.base_url` **and** a native library
 * built with the `http-model` feature (see android/README.md).
 */
class PdMemory private constructor() : Closeable, EngineCall {

    @Volatile
    private var handle: Long = 0L
    private val lock = Any()

    /** Whether the native engine is still open. */
    val isOpen: Boolean
        get() = synchronized(lock) { handle != 0L }

    override suspend fun call(op: String, argsJson: String): Any = withContext(Dispatchers.IO) {
        val address = synchronized(lock) { handle }
        if (address == 0L) {
            throw PdMemoryException("closed", "PdMemory engine is closed")
        }
        val raw: String = try {
            PdMemoryBridge.callEngine(address, op, argsJson)
        } catch (error: RuntimeException) {
            // JNI-level failure: the RuntimeException message is the JSON envelope.
            throw PdMemoryException.fromEnvelope(error.message, "jni_error")
        }
        try {
            PdMemoryProtocol.parseEnvelope(raw)
        } catch (error: JSONException) {
            throw PdMemoryException(
                "protocol_error",
                "op '$op' returned a non-JSON envelope: $raw",
                error,
            )
        }
    }

    /** [call] with no arguments. */
    suspend fun call(op: String): Any = call(op, "{}")

    /** Object-result convenience wrapper. */
    suspend fun callObject(op: String, argsJson: String = "{}"): JSONObject =
        PdMemoryProtocol.asObject(call(op, argsJson))

    /** Array-result convenience wrapper. */
    suspend fun callArray(op: String, argsJson: String = "{}"): JSONArray =
        PdMemoryProtocol.asArray(call(op, argsJson))

    /** Queues one conversation turn (no model call). */
    suspend fun addTurn(
        messages: List<TurnMessage>,
        sessionId: String? = null,
        source: String? = null,
    ): JSONObject = callObject("add", PdMemoryProtocol.captureArgs(messages, sessionId, source))

    /** Queues a single message turn. */
    suspend fun addTurn(
        role: String,
        content: String,
        sessionId: String,
        source: String? = null,
    ): JSONObject = addTurn(listOf(TurnMessage(role, content)), sessionId, source)

    /** Model-guided recall for a query (needs a configured model). */
    suspend fun retrieve(query: String): JSONObject =
        callObject("retrieve", PdMemoryProtocol.queryArgs(query))

    /** Explicit search using the model gate (needs a configured model). */
    suspend fun search(query: String): JSONObject =
        callObject("search", PdMemoryProtocol.queryArgs(query))

    /**
     * Returns the rendered four-section system context for [query], or
     * `null` when nothing matched / no model is configured.
     */
    suspend fun memoryContext(query: String): String? =
        callObject("memory_context", PdMemoryProtocol.queryArgs(query))
            .optString("systemContext")
            .takeIf { it.isNotBlank() }

    /** Engine diagnostics: file counts, pending queue, last dream state. */
    suspend fun overview(): JSONObject = callObject("overview")

    /** Manifest entries (optionally filtered/paged through [options]). */
    suspend fun list(options: JSONObject = JSONObject()): JSONArray =
        callArray("list", options.toString())

    /** Full records for [ids] (`relative_path` values). */
    suspend fun get(ids: List<String>, maxLines: Int? = null): JSONArray {
        val args = JSONObject().put("ids", JSONArray(ids))
        if (maxLines != null) args.put("max_lines", maxLines)
        return callArray("get", args.toString())
    }

    /** Runs one extraction batch (needs a configured model). */
    suspend fun flush(reason: String = "manual", batchSize: Int? = null): JSONObject {
        val args = JSONObject().put("reason", reason)
        if (batchSize != null) args.put("batch_size", batchSize)
        return callObject("flush", args.toString())
    }

    /** Consolidation pass (`trigger`: `manual` or `scheduled`; needs a model). */
    suspend fun dream(trigger: String = "manual"): JSONObject =
        callObject("dream", JSONObject().put("trigger", trigger).toString())

    /** Restores the snapshot taken before the last dream. */
    suspend fun rollback(): JSONObject = callObject("rollback")

    /** Exports the full memory bundle as JSON (safe to persist/transfer). */
    suspend fun exportBundle(): JSONObject = callObject("export")

    /** Imports a bundle produced by [exportBundle]. */
    suspend fun importBundle(bundle: JSONObject): JSONObject =
        callObject("import", JSONObject().put("bundle", bundle).toString())

    /** Current indexing settings. */
    suspend fun settings(): JSONObject = callObject("settings_get")

    /** Persists indexing settings. */
    suspend fun saveSettings(settings: JSONObject): JSONObject =
        callObject("settings_save", JSONObject().put("settings", settings).toString())

    /**
     * Administration action (`edit_project_meta`, `edit_entry`,
     * `delete_entries`, `deprecate_entries`, `restore_entries`); the JSON object
     * is the internally-tagged `MemoryActionRequest`.
     */
    suspend fun act(action: JSONObject): JSONObject = callObject("act", action.toString())

    /** Reads one pipeline-state value (JSON value or `JSONObject.NULL`). */
    suspend fun state(key: String): Any = call("state_get", PdMemoryProtocol.keyArgs(key))

    /**
     * Releases the native engine. Idempotent and safe to call from
     * `onDestroy`; no further calls are allowed afterwards.
     */
    override fun close() {
        val address = synchronized(lock) {
            val current = handle
            handle = 0L
            current
        }
        if (address != 0L) {
            PdMemoryBridge.freeEngine(address)
        }
    }

    private fun attach(address: Long) {
        synchronized(lock) {
            check(handle == 0L) { "PdMemory engine is already open" }
            handle = address
        }
    }

    companion object {
        /**
         * Loads the Rust cdylib `libpd_memory_ffi.so` from `jniLibs/<abi>/`.
         * `System.loadLibrary` is idempotent, so the bridge loading it again is
         * harmless.
         */
        init {
            System.loadLibrary("pd_memory_ffi")
        }

        /** Native library version, e.g. `"0.1.0"`. Safe before [create]. */
        @JvmStatic
        fun version(): String = PdMemoryBridge.version()

        /**
         * Creates an engine from a `pd-memory-core` `MemoryConfig` JSON string.
         *
         * Runs on [Dispatchers.IO] because creating the file store may touch the
         * filesystem. Throws [PdMemoryException] when the config is rejected
         * (`invalid_config`), a model is configured but the native library was
         * built without `http-model` (`unsupported`), etc.
         */
        suspend fun create(configJson: String): PdMemory = withContext(Dispatchers.IO) {
            val address: Long = try {
                PdMemoryBridge.newEngine(configJson)
            } catch (error: RuntimeException) {
                // The Rust side throws with the JSON error envelope as message.
                throw PdMemoryException.fromEnvelope(error.message, "engine_init_failed")
            }
            if (address == 0L) {
                throw PdMemoryException.fromEnvelope(
                    PdMemoryBridge.lastError(0L),
                    "engine_init_failed",
                )
            }
            PdMemory().also { it.attach(address) }
        }
    }
}
