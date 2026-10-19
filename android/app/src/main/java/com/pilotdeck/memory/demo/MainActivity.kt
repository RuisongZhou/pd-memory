package com.pilotdeck.memory.demo

import android.os.Bundle
import android.text.InputType
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import com.pilotdeck.memory.MemoryRepository
import com.pilotdeck.memory.PdMemory
import kotlinx.coroutines.launch
import org.json.JSONObject
import java.io.File

/**
 * Minimal demo of the pd-memory Android SDK.
 *
 * Creates the engine under `filesDir/pd-memory` (capture-only config: no model
 * provider), captures turns into the L0 queue and shows the engine overview.
 * Retrieval / flush / dream need a model configured in the JSON **and** a native
 * library built with `--features http-model`; see android/README.md.
 */
class MainActivity : ComponentActivity() {

    private var memory: PdMemory? = null
    private var repository: MemoryRepository? = null
    private lateinit var input: EditText
    private lateinit var output: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildUi())

        append("pd-memory native version: ${PdMemory.version()}")
        lifecycleScope.launch {
            try {
                val config = JSONObject()
                    .put("data_root", File(filesDir, "pd-memory").absolutePath)
                    .put("workspace_id", "android-demo")
                    .put("workspace_name", "Android demo")
                    .toString()
                val engine = PdMemory.create(config)
                memory = engine
                repository = MemoryRepository(engine)
                append("engine ready")
                append(repository?.overview()?.toString(2).orEmpty())
            } catch (error: Throwable) {
                append("engine init failed: ${error.message}")
            }
        }
    }

    private fun captureTurn() {
        val text = input.text?.toString()?.trim().orEmpty()
        if (text.isEmpty()) {
            append("nothing to capture")
            return
        }
        val repository = repository
        if (repository == null) {
            append("engine is not ready yet")
            return
        }
        lifecycleScope.launch {
            try {
                val captured = repository.addTurn(
                    role = "user",
                    content = text,
                    sessionId = "demo-session",
                    source = "android-demo",
                )
                append(if (captured) "captured turn: $text" else "turn was normalised away")
                input.setText("")
            } catch (error: Throwable) {
                append("add failed: ${error.message}")
            }
        }
    }

    private fun refreshOverview() {
        val repository = repository
        if (repository == null) {
            append("engine is not ready yet")
            return
        }
        lifecycleScope.launch {
            try {
                append(repository.overview().toString(2))
            } catch (error: Throwable) {
                append("overview failed: ${error.message}")
            }
        }
    }

    override fun onDestroy() {
        // freeEngine only drops the Rust engine; it does no blocking work.
        memory?.close()
        memory = null
        repository = null
        super.onDestroy()
    }

    private fun append(line: String) {
        output.append(line)
        output.append("\n")
    }

    private fun buildUi(): ScrollView {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 48, 48, 48)
        }
        root.addView(
            TextView(this).apply {
                text = "pd-memory Android demo"
                textSize = 22f
            },
        )
        root.addView(
            TextView(this).apply {
                text = "Capture-only demo: turns are queued in filesDir/pd-memory. " +
                    "Configure a model JSON and build the .so with --features http-model " +
                    "to enable retrieval and dream."
                textSize = 12f
            },
        )
        input = EditText(this).apply {
            hint = "Something the agent should remember"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            minLines = 2
        }
        root.addView(input)
        root.addView(
            Button(this).apply {
                text = "Add turn"
                setOnClickListener { captureTurn() }
            },
        )
        root.addView(
            Button(this).apply {
                text = "Refresh overview"
                setOnClickListener { refreshOverview() }
            },
        )
        output = TextView(this).apply {
            textSize = 12f
            setTextIsSelectable(true)
        }
        root.addView(output)
        return ScrollView(this).apply { addView(root) }
    }
}
