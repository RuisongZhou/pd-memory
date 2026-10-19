package com.pilotdeck.memory

import org.json.JSONArray
import org.json.JSONObject

/**
 * A decoded memory manifest entry, as rendered by the SDK `list` op.
 *
 * Field names mirror the Markdown frontmatter (`relative_path` is the public
 * memory id).
 */
data class MemoryEntry(
    val id: String,
    val name: String,
    val description: String,
    val kind: String,
    val updatedAt: String,
    val deprecated: Boolean,
)

/**
 * Test seam over the JNI bridge.
 *
 * [PdMemory] implements it; unit tests substitute a fake so repository mapping
 * can be verified on the JVM without loading the native library.
 */
fun interface EngineCall {
    /** Executes one op and returns the decoded `result` part of the envelope. */
    suspend fun call(op: String, argsJson: String): Any
}

/**
 * Small typed repository used by the demo app and unit tests.
 *
 * It only depends on [EngineCall], so the JSON mapping is testable without an
 * Android device (see `MemoryRepositoryTest`).
 */
class MemoryRepository(private val engine: EngineCall) {

    /** Captures one message turn; returns the engine's `captured` flag. */
    suspend fun addTurn(
        role: String,
        content: String,
        sessionId: String,
        source: String? = "android",
    ): Boolean {
        val result = engine.call(
            "add",
            PdMemoryProtocol.captureArgs(listOf(TurnMessage(role, content)), sessionId, source),
        ) as? JSONObject ?: return false
        return result.optBoolean("captured", false)
    }

    /** Recall context for prompt injection; `null` when no memory matched. */
    suspend fun memoryContext(query: String): String? {
        val result = engine.call("memory_context", PdMemoryProtocol.queryArgs(query)) as? JSONObject
            ?: return null
        return result.optString("systemContext").takeIf { it.isNotBlank() }
    }

    /** Lists the most recent memory entries. */
    suspend fun listEntries(limit: Int = 20): List<MemoryEntry> {
        val result = engine.call("list", JSONObject().put("limit", limit).toString())
        val entries = result as? JSONArray ?: return emptyList()
        return (0 until entries.length()).map { index -> decodeEntry(entries.getJSONObject(index)) }
    }

    /** Raw engine overview payload. */
    suspend fun overview(): JSONObject =
        engine.call("overview", "{}") as? JSONObject ?: JSONObject()

    private fun decodeEntry(json: JSONObject): MemoryEntry = MemoryEntry(
        id = json.optString("relative_path"),
        name = json.optString("name"),
        description = json.optString("description"),
        kind = json.optString("type"),
        updatedAt = json.optString("updated_at"),
        deprecated = json.optBoolean("deprecated", false),
    )
}
