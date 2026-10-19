package com.pilotdeck.memory

import org.json.JSONArray
import org.json.JSONObject

/** One chat message handed to the capture API. */
data class TurnMessage(
    val role: String,
    val content: String,
) {
    fun toJson(): JSONObject = JSONObject().put("role", role).put("content", content)
}

/**
 * Envelope parsing and argument encoding for the JSON-in / JSON-out FFI.
 *
 * Pure JVM code (no JNI), so it is covered by local unit tests in
 * `src/test/java`.
 */
object PdMemoryProtocol {

    /**
     * Parses the SDK envelope and returns the decoded `result`.
     *
     * The result is a [JSONObject], [JSONArray] or a scalar; a JSON `null` result
     * is returned as [JSONObject.NULL].
     *
     * @throws PdMemoryException when `ok` is false (engine-level error)
     * @throws JSONException when the raw text is not a JSON envelope
     */
    fun parseEnvelope(raw: String): Any {
        val envelope = JSONObject(raw)
        if (!envelope.optBoolean("ok", false)) {
            throw PdMemoryException(
                code = envelope.optString("code", "unknown"),
                message = envelope.optString("message", raw),
            )
        }
        if (!envelope.has("result") || envelope.isNull("result")) {
            return JSONObject.NULL
        }
        return envelope.get("result")
    }

    /** Encodes `add` / `capture_turn` arguments. */
    fun captureArgs(
        messages: List<TurnMessage>,
        sessionId: String? = null,
        source: String? = null,
    ): String {
        val payload = JSONObject()
        val array = JSONArray()
        messages.forEach { message -> array.put(message.toJson()) }
        payload.put("messages", array)
        if (sessionId != null) payload.put("session_id", sessionId)
        if (source != null) payload.put("source", source)
        return payload.toString()
    }

    /** Encodes `retrieve` / `search` / `memory_context` arguments. */
    fun queryArgs(query: String): String = JSONObject().put("query", query).toString()

    /** Encodes `state_get` arguments. */
    fun keyArgs(key: String): String = JSONObject().put("key", key).toString()

    /** JSON-quotes [value] for embedding in a config document. */
    fun quote(value: String): String = JSONObject.quote(value)

    /** Treats a non-array result as an empty array. */
    fun asArray(result: Any): JSONArray = result as? JSONArray ?: JSONArray()

    /** Treats a non-object result as an empty object. */
    fun asObject(result: Any): JSONObject = result as? JSONObject ?: JSONObject()
}
