//! Test-only persistent engine driver for the TypeScript/Rust runtime probes.
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use pd_memory_core::config::MemoryConfig;
use pd_memory_core::control_plane::FileControlPlane;
use pd_memory_core::engine::{FlushOptions, MemoryEngine, MemoryEngineOptions};
use pd_memory_core::files::FileMemoryStore;
use pd_memory_core::ports::{Clock, NoopTelemetry, StateStore};
use pd_memory_core::recall::RetrieveOptions;
use pd_memory_core::redact::DefaultRedactor;
use pd_memory_core::types::{ClearMemoryScope, IndexingSettings, MemoryMessage, MemoryRecordType};
use serde_json::{json, Value};

struct TestClock(Mutex<DateTime<Utc>>);
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("missing config path")?;
    let mut config: MemoryConfig = serde_json::from_slice(&std::fs::read(path)?)?;
    config.retrieval.cache_ttl_ms = 30_000;
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: Value = serde_json::from_str(&input)?;
    let clock = Arc::new(TestClock(Mutex::new(
        input["now"].as_str().ok_or("missing now")?.parse()?,
    )));
    let roots = config.store_layout().roots;
    let state = FileControlPlane::new(roots.workspace_dir.clone())?;
    let model = pd_memory_llm::provider_from_config(&config.model)?;
    let engine = MemoryEngine::new(MemoryEngineOptions {
        store: Box::new(FileMemoryStore::new(roots.clone())),
        queue: Box::new(FileControlPlane::new(roots.workspace_dir.clone())?),
        state: Box::new(FileControlPlane::new(roots.workspace_dir.clone())?),
        model,
        clock: clock.clone(),
        telemetry: Arc::new(NoopTelemetry),
        redactor: Arc::new(DefaultRedactor::new(config.redaction.clone())),
        config,
    })?;
    let mut events = Vec::new();
    for action in input["actions"].as_array().ok_or("missing actions")? {
        let op = action["op"].as_str().ok_or("missing op")?;
        let mut event = match op {
            "capture" => {
                let messages: Vec<MemoryMessage> =
                    serde_json::from_value(action["messages"].clone())?;
                let captured = engine.capture_turn(
                    &messages,
                    action["session"].as_str().unwrap_or("runtime"),
                    Some(
                        action["timestamp"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| clock.now_iso()),
                    ),
                    "parity",
                )?;
                json!({"captured": captured.captured})
            }
            "flush" => {
                let stats = engine.flush(FlushOptions {
                    reason: action["reason"].as_str().unwrap_or("manual").into(),
                    batch_size: action["batchSize"].as_u64().map(|value| value as usize),
                    session_keys: action
                        .get("sessionKeys")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()?,
                })?;
                json!({"written": stats.written_files, "processed": stats.processed_sessions, "failed": stats.errors.len()})
            }
            "maintain" => {
                let result = engine.run_due_scheduled_maintenance(
                    action["reason"].as_str().unwrap_or("scheduled"),
                )?;
                let reason = if result.index_ran {
                    state
                        .get("indexTraces")?
                        .and_then(|value| value[0]["reason"].as_str().map(str::to_string))
                } else {
                    None
                };
                json!({"indexRan": result.index_ran, "dreamRan": result.dream_ran, "indexReason": reason})
            }
            "recall" => {
                let result = engine.retrieve_context(
                    action["query"].as_str().ok_or("missing query")?,
                    RetrieveOptions {
                        explicit: action["explicit"].as_bool().unwrap_or(false),
                        recent_messages: action
                            .get("recent")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()?
                            .unwrap_or_default(),
                        ..Default::default()
                    },
                )?;
                json!({"cacheHit": result.cache_hit, "route": result.route.as_str(), "context": result.system_context.unwrap_or_default(), "selectedCount": result.selected_ids.len()})
            }
            "advance" => {
                *clock.0.lock().unwrap() +=
                    Duration::milliseconds(action["ms"].as_i64().ok_or("missing ms")?);
                json!({})
            }
            "state" => {
                let key = action["key"].as_str().ok_or("missing key")?;
                if action["value"].is_null() {
                    state.remove(key)?;
                } else {
                    state.set(key, action["value"].clone())?;
                }
                json!({})
            }
            "settings" => {
                let mut settings = serde_json::to_value(engine.settings()?)?;
                for (key, value) in action["values"]
                    .as_object()
                    .ok_or("missing settings values")?
                {
                    settings[key] = value.clone();
                }
                let settings =
                    engine.save_settings(serde_json::from_value::<IndexingSettings>(settings)?)?;
                json!({"indexMinutes": settings.auto_index_interval_minutes, "dreamMinutes": settings.auto_dream_interval_minutes})
            }
            "clear" => {
                let scope = if action["scope"] == "all_memory" {
                    ClearMemoryScope::AllMemory
                } else {
                    ClearMemoryScope::CurrentProject
                };
                engine.clear(scope)?;
                json!({})
            }
            "import" => {
                let mut bundle = engine.export_bundle()?;
                bundle.metadata.last_indexed_at = None;
                bundle.metadata.last_dream_at = None;
                bundle.metadata.last_dream_status = None;
                bundle.metadata.last_dream_summary = None;
                engine.import_bundle(&bundle)?;
                json!({})
            }
            "edit_body" => {
                let entry = engine
                    .list(&Default::default())?
                    .into_iter()
                    .find(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
                    .ok_or("missing project note")?;
                let path = &entry.absolute_path;
                let content = std::fs::read_to_string(path)?;
                let edited = content.replace(
                    action["from"].as_str().ok_or("missing from")?,
                    action["to"].as_str().ok_or("missing to")?,
                );
                if content == edited {
                    return Err("edit must change the fixture body".into());
                }
                std::fs::write(path, edited)?;
                json!({})
            }
            _ => return Err(format!("unknown runtime op {op}").into()),
        };
        event["op"] = json!(op);
        event["pending"] = json!(engine.overview()?.pending_dialogue_turns);
        event["fileCount"] = json!(engine.list(&Default::default())?.len());
        for key in ["autoIndexAnchorAt", "autoDreamAnchorAt", "lastIndexedAt"] {
            event[key] = state.get(key)?.unwrap_or(Value::Null);
        }
        events.push(event);
    }
    println!("{}", json!({"events": events}));
    Ok(())
}
