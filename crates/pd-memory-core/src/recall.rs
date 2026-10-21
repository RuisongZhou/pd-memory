//! Model-guided retrieval (dev B2).
//!
//! Port of `reasoning-loop.ts`: cache -> model gate -> manifest scan -> model
//! file selection -> read & render. Both automatic recall and explicit search
//! use the gate. Gate failures degrade to `none`; selection failures fall back
//! to the most recent manifest entries.
//!
//! The cache key follows the TypeScript implementation: retrieval mode,
//! normalised query, the last up-to-four user messages and a snapshot hash of
//! all exported file bytes and the last Dream timestamp.
//! A cache hit performs zero model calls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::MemoryConfig;
use crate::error::{MemoryError, Result};
use crate::extraction::complete_json;
use crate::ports::{format_iso, Clock, MemoryStore, ModelProvider};
use crate::types::*;

/// Schema names passed to [`ModelProvider::complete`] by the retrieval pipeline.
pub mod schemas {
    /// Recall gate: `{"route": "none|user|project|mix"}`.
    pub const GATE: &str = "memory_gate";
    /// File selection: `{"selected_ids": ["<relative_path>", ...]}`.
    pub const SELECT: &str = "memory_select_files";
}

/// LLM timeout for the recall gate, matching the TypeScript implementation.
const GATE_TIMEOUT_MS: u64 = 45_000;
/// LLM timeout for manifest file selection, matching the TypeScript implementation.
const SELECTION_TIMEOUT_MS: u64 = 45_000;
/// Number of files selected when the model returns nothing for a project route.
const PROJECT_FALLBACK_LIMIT: usize = 3;
/// User recall only injects the single most relevant user note (TS parity).
const USER_ROUTE_SELECTION_LIMIT: usize = 1;
/// Characters of each recent message shown to the model.
const RECENT_MESSAGE_MAX_CHARS: usize = 220;
/// Characters of a manifest description shown to the selection model.
const MANIFEST_DESCRIPTION_MAX_CHARS: usize = 200;
/// The reference prompt hard-codes "at most 5 ids"; config may only lower it.
const MAX_SELECTION_LIMIT: usize = 5;

/// System prompt for the cheap recall gate (schema [`schemas::GATE`]).
///
/// Re-authored from `llm-extraction.ts::decideFileMemoryRoute`; wording is kept
/// close to the reference because routing quality is product behaviour.
const GATE_SYSTEM_PROMPT: &str = r#"You decide whether the current query should trigger long-term memory recall.
Return JSON only with a single field route.
Valid route values: none, user, project, mix.
Use none unless the query clearly needs long-term memory.
Use user only when the query is asking about stable personal identity/background facts about who the user is, such as name, profession, long-term role context, life background, or durable relationships.
Do not use user for reply preferences, language choices, formatting rules, style guidance, file/tool boundaries, or delivery rules; those belong to project.
Use project when the query only needs current project memory, including project facts, collaboration rules, delivery style, file boundaries, or project status.
Use mix only when the query genuinely needs both current project memory and the user's stable identity/background at the same time.
Do not use mix just because both could be helpful; choose mix only when both are actually necessary to answer well."#;

/// System prompt for manifest file selection (schema [`schemas::SELECT`]).
///
/// Re-authored verbatim from `llm-extraction.ts::selectFileManifestEntries`.
const SELECT_SYSTEM_PROMPT: &str = r#"You select a small number of memory files from a compact manifest.
Return JSON only with selected_ids.
Select at most 5 ids and prefer recent items that are directly useful for the query."#;

/// Options for a retrieval call.
#[derive(Debug, Clone, Default)]
pub struct RetrieveOptions {
    /// Recent conversation messages (newest last); only the last four user
    /// messages influence the gate and selection.
    pub recent_messages: Vec<MemoryMessage>,
    /// Optional workspace hint recorded in the trace (the SDK has no general
    /// multi-workspace mode yet).
    pub workspace_hint: Option<String>,
    /// Distinguishes explicit searches from automatic recall in traces and cache keys.
    pub explicit: bool,
}

/// In-process recall cache (TTL from `config.retrieval.cache_ttl_ms`), keyed by
/// retrieval mode + query + recent user messages + full file snapshot hash.
/// Owned by the engine and passed in on every call.
#[derive(Debug, Default)]
pub struct RecallCache {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    expires_at_ms: i64,
    result: RetrievalResult,
}

impl RecallCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached retrieval result.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn get(&self, key: &str, now_ms: i64) -> Option<&RetrievalResult> {
        self.entries
            .get(key)
            .filter(|entry| entry.expires_at_ms > now_ms)
            .map(|entry| &entry.result)
    }

    fn put(&mut self, key: String, result: RetrievalResult, expires_at_ms: i64) {
        self.entries.insert(
            key,
            CacheEntry {
                expires_at_ms,
                result,
            },
        );
    }
}

/// Run the retrieval pipeline and render memory context.
///
/// `cache` is the engine-owned recall cache; it is locked internally for the
/// duration of a lookup/insert only, never across a model call.
// The engine passes each port separately; bundling them would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn retrieve(
    config: &MemoryConfig,
    store: &dyn MemoryStore,
    model: &dyn ModelProvider,
    clock: &dyn Clock,
    cache: &Mutex<RecallCache>,
    query: &str,
    options: &RetrieveOptions,
    last_dream_at: Option<&str>,
) -> Result<RetrievalResult> {
    let started = clock.now();
    let started_at = format_iso(started);
    let started_ms = started.timestamp_millis();
    let mode = if options.explicit { "explicit" } else { "auto" };
    let recent_user = recent_user_messages(&options.recent_messages);

    // --- cache lookup -----------------------------------------------------
    // Like the reference, hash the complete exported file contents, including
    // deprecated records and project metadata, before looking up the cache.
    let snapshot = snapshot_version(&store.export_bundle()?.files, last_dream_at);
    let cache_key = build_cache_key(mode, query, &recent_user, &snapshot);
    if let Some(mut cached) = {
        let guard = cache
            .lock()
            .map_err(|_| MemoryError::Other("recall cache mutex poisoned".into()))?;
        guard.get(&cache_key, started_ms).cloned()
    } {
        cached.cache_hit = true;
        if let Some(object) = cached.trace.as_object_mut() {
            object.insert("cache_hit".to_string(), json!(true));
            object.insert("cache_hit_at".to_string(), json!(started_at));
        }
        return Ok(cached);
    }

    let all_entries = store.list_entries(&MemoryListOptions {
        include_deprecated: true,
        ..Default::default()
    })?;
    // --- tracing ----------------------------------------------------------
    let mut trace = RetrievalTrace {
        trace_id: build_trace_id(query, &started_at),
        started_at: started_at.clone(),
        finished_at: String::new(),
        route: MemoryRoute::None,
        cache_hit: false,
        steps: Vec::new(),
        selected_ids: Vec::new(),
        candidate_count: 0,
    };
    push_step(
        &mut trace,
        "recall_start",
        "Recall Started",
        "info",
        query.to_string(),
        format!("mode={mode}"),
        Some(json!({
            "query": query,
            "mode": mode,
            "recent_user_messages": recent_user.len(),
            "workspace_hint": options.workspace_hint.clone().unwrap_or_default(),
            "manifest_entries": all_entries.len(),
        })),
    );

    // --- model gate -------------------------------------------------------
    let recent_for_gate = recent_messages_for_gate(&options.recent_messages);
    let (route, gate_error) = gate_route(model, query, &recent_for_gate);
    match gate_error {
        Some(error) => {
            log::warn!("[pd-memory] memory gate degraded to none: {error}");
            push_step(
                &mut trace,
                "memory_gate",
                "Memory Gate",
                "warning",
                query.to_string(),
                format!("route=none (gate failed: {error})"),
                Some(json!({"route": "none", "degraded": true, "error": error})),
            );
        }
        None => push_step(
            &mut trace,
            "memory_gate",
            "Memory Gate",
            if route == MemoryRoute::None {
                "warning"
            } else {
                "success"
            },
            query.to_string(),
            format!("route={}", route.as_str()),
            Some(json!({"route": route.as_str()})),
        ),
    }
    let mut selected_ids: Vec<String> = Vec::new();
    let mut candidate_headers: Vec<MemoryManifestEntry> = Vec::new();
    let mut context: Option<String> = None;

    if route != MemoryRoute::None {
        let needs_user_summary = matches!(route, MemoryRoute::User | MemoryRoute::Mix);
        let needs_project_memory = matches!(route, MemoryRoute::Project | MemoryRoute::Mix);
        let user_summary = if needs_user_summary {
            store.get_user_summary()?
        } else {
            MemoryUserSummary::default()
        };
        let project_meta = if needs_project_memory {
            store.get_project_meta()?
        } else {
            None
        };

        // --- manifest scan ------------------------------------------------
        candidate_headers = scan_manifest(&all_entries, route, config.retrieval.manifest_limit);
        push_step(
            &mut trace,
            "manifest_scanned",
            "Manifest Scanned",
            if candidate_headers.is_empty() {
                "warning"
            } else {
                "success"
            },
            format!("{} live entries", all_entries.len()),
            format!("{} recall headers ready", candidate_headers.len()),
            Some(json!({
                "route": route.as_str(),
                "count": candidate_headers.len(),
                "limit": config.retrieval.manifest_limit,
                "candidates": candidate_headers.iter().map(describe_header).collect::<Vec<_>>(),
            })),
        );

        // --- model file selection ----------------------------------------
        let selection_limit = selection_limit_for(route, config.retrieval.selection_limit);
        let (mut model_ids, selection_error) = if candidate_headers.is_empty() {
            (Vec::new(), None)
        } else {
            select_files(
                model,
                route,
                query,
                &recent_user,
                project_meta.as_ref(),
                &candidate_headers,
                selection_limit,
            )?
        };
        let model_selected_count = model_ids.len();
        if model_ids.is_empty() && !candidate_headers.is_empty() {
            // Match the reference fallback: newest manifest entries first.
            model_ids = candidate_headers
                .iter()
                .take(fallback_limit_for(route, selection_limit))
                .map(|entry| entry.relative_path.clone())
                .collect();
        }
        selected_ids = model_ids;
        push_step(
            &mut trace,
            "manifest_selected",
            "Manifest Selected",
            if selected_ids.is_empty() {
                if candidate_headers.is_empty() {
                    "skipped"
                } else {
                    "warning"
                }
            } else {
                "success"
            },
            format!("{} candidates", candidate_headers.len()),
            format!(
                "{} file ids selected (model={}, fallback={})",
                selected_ids.len(),
                model_selected_count,
                selected_ids.len().saturating_sub(model_selected_count)
            ),
            {
                let mut details = json!({
                    "candidate_ids": candidate_headers.iter().map(|entry| entry.relative_path.clone()).collect::<Vec<_>>(),
                    "selected_ids": selected_ids,
                    "selection_limit": selection_limit,
                    "model_selected": model_selected_count,
                });
                if let Some(error) = selection_error {
                    details["error"] = json!(error);
                }
                Some(details)
            },
        );

        // --- load selected files -----------------------------------------
        let records = if selected_ids.is_empty() {
            Vec::new()
        } else {
            store.get_records(&selected_ids, config.retrieval.recall_file_max_lines)?
        };
        push_step(
            &mut trace,
            "files_loaded",
            "Files Loaded",
            if records.is_empty() {
                "skipped"
            } else {
                "success"
            },
            format!("{} requested", selected_ids.len()),
            format!("{} files loaded", records.len()),
            Some(json!({
                "requested": selected_ids,
                "loaded": records.iter().map(|record| record.entry.relative_path.clone()).collect::<Vec<_>>(),
                "max_lines": config.retrieval.recall_file_max_lines,
            })),
        );

        // --- render -------------------------------------------------------
        context = Some(render_context(
            route,
            &user_summary,
            project_meta.as_ref(),
            &records,
        ))
        .filter(|rendered| !rendered.is_empty());
        push_step(
            &mut trace,
            "context_rendered",
            "Context Rendered",
            match (&context, route) {
                (Some(_), _) => "success",
                (None, MemoryRoute::None) => "skipped",
                _ => "warning",
            },
            format!("{} records", records.len()),
            context
                .as_ref()
                .map(|block| format!("{} chars", block.chars().count()))
                .unwrap_or_else(|| "no recall context was prepared".to_string()),
            Some(json!({
                "route": route.as_str(),
                "file_count": records.len(),
                "characters": context.as_ref().map(|block| block.chars().count()).unwrap_or(0),
            })),
        );
    } else {
        for (kind, title) in [
            ("manifest_scanned", "Manifest Scanned"),
            ("manifest_selected", "Manifest Selected"),
            ("files_loaded", "Files Loaded"),
            ("context_rendered", "Context Rendered"),
        ] {
            push_step(
                &mut trace,
                kind,
                title,
                "skipped",
                "route=none",
                "No recall needed for this turn.",
                None,
            );
        }
    }

    // --- finish -----------------------------------------------------------
    trace.finished_at = clock.now_iso();
    trace.route = route;
    trace.selected_ids = selected_ids.clone();
    trace.candidate_count = candidate_headers.len();
    let result = RetrievalResult {
        route,
        context,
        system_context: None,
        selected_ids,
        intent: Some(route.as_str().to_string()),
        cache_hit: false,
        trace: serde_json::to_value(&trace)?,
    };
    let expires_at_ms = clock
        .now()
        .timestamp_millis()
        .saturating_add(config.retrieval.cache_ttl_ms as i64);
    {
        let mut guard = cache
            .lock()
            .map_err(|_| MemoryError::Other("recall cache mutex poisoned".into()))?;
        guard.put(cache_key, result.clone(), expires_at_ms);
    }
    Ok(result)
}

/// Build the four-section system context used by PilotDeck's service facade.
/// File ids and routing metadata remain in the raw recall block only.
pub fn build_memory_recall_system_context(evidence: &str) -> Option<String> {
    let mut sections: [Vec<String>; 4] = Default::default();
    let mut current = None;
    let mut buffer = Vec::new();
    for line in evidence.lines() {
        let trimmed = line.trim();
        let next = ["user", "project_meta", "project", "feedback"]
            .iter()
            .position(|kind| trimmed.starts_with(&format!("### [{kind}]")));
        if let Some(next) = next {
            flush_recall_section(&mut sections, current, &mut buffer);
            current = Some(next);
        } else if current.is_some()
            && trimmed != "## ClawXMemory Recall"
            && !trimmed.starts_with("route=")
            && trimmed != "Treat these file memories as the authoritative long-term memory for this turn when relevant."
        {
            buffer.push(if trimmed.is_empty() { "" } else { line });
        }
    }
    flush_recall_section(&mut sections, current, &mut buffer);
    let blocks: Vec<String> = [
        "User Profile",
        "Project Meta",
        "Project Memory",
        "Feedback Memory",
    ]
    .iter()
    .zip(sections)
    .filter(|(_, bodies)| !bodies.is_empty())
    .map(|(heading, bodies)| format!("## {heading}\n\n{}", bodies.join("\n\n---\n\n")))
    .collect();
    if blocks.is_empty() {
        return None;
    }
    let mut parts = vec![
        "## ClawXMemory Recall".to_string(),
        "These are retrieved long-term memory references for the current turn.".to_string(),
        "Some content may be relevant while some may not be directly useful.".to_string(),
        "Use only the parts that are relevant to the current question.".to_string(),
        "If retrieved memory conflicts with explicit new user instructions in the current turn, follow the current-turn user instructions.".to_string(),
    ];
    parts.extend(blocks);
    Some(parts.join("\n\n"))
}

fn flush_recall_section(
    sections: &mut [Vec<String>; 4],
    current: Option<usize>,
    buffer: &mut Vec<&str>,
) {
    if let Some(index) = current {
        let body = buffer.join("\n").trim().to_string();
        if !body.is_empty() {
            sections[index].push(body);
        }
    }
    buffer.clear();
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn recent_user_messages(messages: &[MemoryMessage]) -> Vec<&MemoryMessage> {
    let mut recent: Vec<&MemoryMessage> = messages
        .iter()
        .filter(|message| message.role == "user")
        .collect();
    let excess = recent.len().saturating_sub(4);
    if excess > 0 {
        recent.drain(0..excess);
    }
    recent
}

fn recent_messages_for_gate(messages: &[MemoryMessage]) -> Vec<&MemoryMessage> {
    let mut recent: Vec<&MemoryMessage> = messages.iter().rev().take(4).collect();
    recent.reverse();
    recent
}

fn normalize_query_key(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn build_cache_key(
    mode: &str,
    query: &str,
    recent_user: &[&MemoryMessage],
    snapshot: &str,
) -> String {
    let payload = json!({
        "mode": mode,
        "query": normalize_query_key(query),
        "recent": recent_user.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        "snapshot": snapshot,
    });
    let mut hasher = Sha256::new();
    hasher.update(payload.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn snapshot_version(files: &[MemorySnapshotFileRecord], last_dream_at: Option<&str>) -> String {
    let payload = json!({"lastDreamAt": last_dream_at.unwrap_or_default(), "files": files});
    let mut hasher = Sha256::new();
    hasher.update(payload.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn build_trace_id(query: &str, started_at: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(query.as_bytes());
    hasher.update(b":");
    hasher.update(started_at.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("trace_{}", &digest[..10])
}

/// Push a trace step. Steps are free-form JSON so the UI can evolve without a
/// core type change; the fields match the TypeScript `RetrievalTraceStep`.
fn push_step(
    trace: &mut RetrievalTrace,
    kind: &str,
    title: &str,
    status: &str,
    input_summary: impl Into<String>,
    output_summary: impl Into<String>,
    details: Option<Value>,
) {
    let step_id = format!("{}:step:{}", trace.trace_id, trace.steps.len() + 1);
    let mut step = json!({
        "step_id": step_id,
        "kind": kind,
        "title": title,
        "status": status,
        "input_summary": input_summary.into(),
        "output_summary": output_summary.into(),
    });
    if let Some(details) = details {
        step["details"] = details;
    }
    trace.steps.push(step);
}

/// User prompt for the gate: `{query, recent_messages:[{role,content}]}`.
#[derive(Debug, Serialize)]
struct GatePrompt<'a> {
    query: &'a str,
    recent_messages: Vec<PromptMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct PromptMessage<'a> {
    role: &'a str,
    content: String,
}

fn gate_route(
    model: &dyn ModelProvider,
    query: &str,
    recent_messages: &[&MemoryMessage],
) -> (MemoryRoute, Option<String>) {
    let prompt = GatePrompt {
        query,
        recent_messages: recent_messages
            .iter()
            .map(|message| PromptMessage {
                role: &message.role,
                content: truncate_for_prompt(&message.content, RECENT_MESSAGE_MAX_CHARS),
            })
            .collect(),
    };
    let user_prompt = match serde_json::to_string_pretty(&prompt) {
        Ok(prompt) => prompt,
        Err(error) => return (MemoryRoute::None, Some(error.to_string())),
    };
    match complete_json(
        model,
        schemas::GATE,
        GATE_SYSTEM_PROMPT,
        &user_prompt,
        GATE_TIMEOUT_MS,
        None,
    ) {
        Ok(value) => (parse_route(&value), None),
        Err(error) => (MemoryRoute::None, Some(error.to_string())),
    }
}

fn parse_route(value: &Value) -> MemoryRoute {
    match value.get("route").and_then(Value::as_str).map(str::trim) {
        Some("user") => MemoryRoute::User,
        Some("project") => MemoryRoute::Project,
        Some("mix") => MemoryRoute::Mix,
        _ => MemoryRoute::None,
    }
}

fn scan_manifest(
    entries: &[MemoryManifestEntry],
    route: MemoryRoute,
    limit: usize,
) -> Vec<MemoryManifestEntry> {
    let mut candidates: Vec<MemoryManifestEntry> = entries
        .iter()
        .filter(|entry| !entry.frontmatter.deprecated)
        .filter(|entry| match route {
            MemoryRoute::User => entry.frontmatter.kind == MemoryRecordType::User,
            MemoryRoute::Project => matches!(
                entry.frontmatter.kind,
                MemoryRecordType::Project | MemoryRecordType::Feedback
            ),
            // TS parity: mix recalls user identity via the summary block and
            // only scans project + feedback in the manifest.
            MemoryRoute::Mix => matches!(
                entry.frontmatter.kind,
                MemoryRecordType::Project | MemoryRecordType::Feedback
            ),
            MemoryRoute::None => false,
        })
        .cloned()
        .collect();
    sort_entries(&mut candidates);
    candidates.truncate(limit);
    candidates
}

fn sort_entries(entries: &mut [MemoryManifestEntry]) {
    entries.sort_by(|left, right| {
        right
            .frontmatter
            .updated_at
            .cmp(&left.frontmatter.updated_at)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
}

fn selection_limit_for(route: MemoryRoute, configured: usize) -> usize {
    let configured = configured.clamp(1, MAX_SELECTION_LIMIT);
    match route {
        MemoryRoute::User => USER_ROUTE_SELECTION_LIMIT,
        _ => configured,
    }
}

fn fallback_limit_for(route: MemoryRoute, selection_limit: usize) -> usize {
    match route {
        MemoryRoute::User => USER_ROUTE_SELECTION_LIMIT.min(selection_limit),
        _ => PROJECT_FALLBACK_LIMIT.min(selection_limit),
    }
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

/// User prompt for file selection, matching the TS object literal field order.
#[derive(Debug, Serialize)]
struct SelectPrompt<'a> {
    query: &'a str,
    route: &'a str,
    recent_user_messages: Vec<String>,
    project: Option<SelectProject<'a>>,
    manifest: Vec<SelectManifestEntry<'a>>,
    limit: usize,
}

#[derive(Debug, Serialize)]
struct SelectProject<'a> {
    project_id: &'a str,
    project_name: &'a str,
    description: String,
    status: &'a str,
}

#[derive(Debug, Serialize)]
struct SelectManifestEntry<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    scope: &'a str,
    project_id: Option<&'a str>,
    updated_at: &'a str,
    description: String,
}

fn select_files(
    model: &dyn ModelProvider,
    route: MemoryRoute,
    query: &str,
    recent_user: &[&MemoryMessage],
    project_meta: Option<&ProjectMetaRecord>,
    candidates: &[MemoryManifestEntry],
    limit: usize,
) -> Result<(Vec<String>, Option<String>)> {
    let prompt = SelectPrompt {
        query,
        route: route.as_str(),
        recent_user_messages: recent_user
            .iter()
            .map(|message| truncate_for_prompt(&message.content, RECENT_MESSAGE_MAX_CHARS))
            .collect(),
        project: project_meta.map(|meta| SelectProject {
            project_id: &meta.project_id,
            project_name: &meta.project_name,
            description: truncate_for_prompt(&meta.description, 180),
            status: &meta.status,
        }),
        manifest: candidates
            .iter()
            .map(|entry| SelectManifestEntry {
                id: &entry.relative_path,
                kind: entry.frontmatter.kind.as_str(),
                scope: entry.frontmatter.scope.as_str(),
                project_id: entry.frontmatter.project_id.as_deref(),
                updated_at: &entry.frontmatter.updated_at,
                description: truncate_for_prompt(
                    &entry.frontmatter.description,
                    MANIFEST_DESCRIPTION_MAX_CHARS,
                ),
            })
            .collect(),
        limit,
    };
    let user_prompt = serde_json::to_string_pretty(&prompt)?;
    match complete_json(
        model,
        schemas::SELECT,
        SELECT_SYSTEM_PROMPT,
        &user_prompt,
        SELECTION_TIMEOUT_MS,
        None,
    ) {
        Ok(value) => Ok((normalize_selected_ids(&value, candidates, limit), None)),
        Err(error) => {
            log::warn!("[pd-memory] memory file selection failed: {error}");
            Ok((Vec::new(), Some(error.to_string())))
        }
    }
}

fn normalize_selected_ids(
    value: &Value,
    candidates: &[MemoryManifestEntry],
    limit: usize,
) -> Vec<String> {
    // TS `normalizeStringArray` accepts either a single string or a string array.
    let requested: Vec<&str> = match value.get("selected_ids") {
        Some(Value::String(single)) => vec![single.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => return Vec::new(),
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut selected: Vec<String> = Vec::new();
    for raw in requested {
        let requested = raw.trim();
        if requested.is_empty() {
            continue;
        }
        let Some(resolved) = candidates
            .iter()
            .find(|entry| same_memory_id(&entry.relative_path, requested))
            .map(|entry| entry.relative_path.clone())
        else {
            continue;
        };
        if !seen.insert(resolved.clone()) {
            continue;
        }
        selected.push(resolved);
        if selected.len() >= limit {
            break;
        }
    }
    selected
}

/// Memory ids are exposed either with or without the `global/` prefix depending
/// on the store; compare both forms.
fn same_memory_id(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (left.strip_prefix("global/"), right.strip_prefix("global/")) {
        (Some(stripped), None) => stripped == right,
        (None, Some(stripped)) => left == stripped,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the raw recall block exactly like `reasoning-loop.ts::renderContext`.
///
/// Returns an empty string when the route produced nothing to inject.
fn render_context(
    route: MemoryRoute,
    user_summary: &MemoryUserSummary,
    project_meta: Option<&ProjectMetaRecord>,
    records: &[MemoryFileRecord],
) -> String {
    let has_user_summary = !user_summary.identity_background.is_empty();
    let user_summary_paths: HashSet<&str> = user_summary
        .files
        .iter()
        .map(|file| file.entry.relative_path.as_str())
        .collect();
    let unique_records: Vec<&MemoryFileRecord> = records
        .iter()
        .filter(|record| !user_summary_paths.contains(record.entry.relative_path.as_str()))
        .collect();
    let mut lines: Vec<String> = vec![
        "## ClawXMemory Recall".to_string(),
        format!("route={}", route.as_str()),
        String::new(),
    ];
    match route {
        MemoryRoute::None => return String::new(),
        MemoryRoute::User => {
            if !has_user_summary {
                return String::new();
            }
            lines.extend(render_user_summary_block(user_summary));
        }
        MemoryRoute::Project => {
            if project_meta.is_none() && unique_records.is_empty() {
                return String::new();
            }
            if let Some(meta) = project_meta {
                lines.extend(render_project_meta_block(meta));
            }
            lines.extend(render_records_block(&unique_records));
        }
        MemoryRoute::Mix => {
            if !has_user_summary && project_meta.is_none() && unique_records.is_empty() {
                return String::new();
            }
            if has_user_summary {
                lines.extend(render_user_summary_block(user_summary));
            }
            if let Some(meta) = project_meta {
                lines.extend(render_project_meta_block(meta));
            }
            lines.extend(render_records_block(&unique_records));
        }
    }
    lines.push(
        "Treat these file memories as the authoritative long-term memory for this turn when relevant."
            .to_string(),
    );
    lines.join("\n").trim().to_string()
}

fn render_user_summary_block(user_summary: &MemoryUserSummary) -> Vec<String> {
    if user_summary.identity_background.is_empty() {
        return Vec::new();
    }
    let updated_at = user_summary
        .files
        .first()
        .map(|file| file.entry.frontmatter.updated_at.as_str())
        .unwrap_or("");
    let relative_path = user_summary
        .files
        .first()
        .map(|file| file.entry.relative_path.as_str())
        .unwrap_or("global/UserIdentity/user-profile.md");
    let header = if updated_at.is_empty() {
        format!("### [user] {relative_path}")
    } else {
        format!("### [user] {relative_path} ({updated_at})")
    };
    let mut lines = vec![header, "## 身份背景".to_string()];
    if user_summary.identity_background.is_empty() {
        lines.push("- 暂无稳定用户画像信息。".to_string());
    } else {
        lines.extend(
            user_summary
                .identity_background
                .iter()
                .map(|item| format!("- {item}")),
        );
    }
    lines.push(String::new());
    lines
}

fn render_project_meta_block(project_meta: &ProjectMetaRecord) -> Vec<String> {
    let relative_path = if project_meta.relative_path.is_empty() {
        "project.meta.md"
    } else {
        project_meta.relative_path.as_str()
    };
    let description = if project_meta.description.is_empty() {
        "No project description yet."
    } else {
        project_meta.description.as_str()
    };
    vec![
        format!(
            "### [project_meta] {relative_path} ({})",
            project_meta.updated_at
        ),
        "## Project Name".to_string(),
        project_meta.project_name.clone(),
        String::new(),
        "## Description".to_string(),
        description.to_string(),
        String::new(),
        "## Status".to_string(),
        project_meta.status.clone(),
        String::new(),
    ]
}

fn render_records_block(records: &[&MemoryFileRecord]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for record in records {
        lines.push(format!(
            "### [{}] {} ({})",
            record.entry.frontmatter.kind.as_str(),
            record.entry.relative_path,
            record.entry.frontmatter.updated_at
        ));
        lines.push(record.content.trim().to_string());
        lines.push(String::new());
    }
    lines
}

fn describe_header(entry: &MemoryManifestEntry) -> String {
    format!(
        "{} | {} | {} | {}",
        entry.frontmatter.updated_at,
        entry.frontmatter.kind.as_str(),
        entry.relative_path,
        entry.frontmatter.description
    )
}

// ---------------------------------------------------------------------------
// Text helpers (ported from llm-extraction.ts / text.ts)
// ---------------------------------------------------------------------------

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let normalized = normalize_whitespace(value);
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    normalized
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    truncate_text(value, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_memory_id_accepts_both_global_forms() {
        assert!(same_memory_id(
            "global/UserIdentityNotes/a.md",
            "UserIdentityNotes/a.md"
        ));
        assert!(same_memory_id(
            "UserIdentityNotes/a.md",
            "global/UserIdentityNotes/a.md"
        ));
        assert!(!same_memory_id("Project/a.md", "Project/b.md"));
    }

    #[test]
    fn snapshot_version_tracks_contents_and_last_dream() {
        let mut files = vec![MemorySnapshotFileRecord {
            relative_path: "Project/a.md".into(),
            content: "first body".into(),
        }];
        let first = snapshot_version(&files, None);
        files[0].content = "edited body".into();
        assert_ne!(first, snapshot_version(&files, None));
        assert_ne!(
            snapshot_version(&files, None),
            snapshot_version(&files, Some("2026-10-19T00:00:00.000Z"))
        );
    }

    #[test]
    fn query_key_is_normalised() {
        assert_eq!(normalize_query_key("  Boreal   Stage  "), "boreal stage");
    }
}
