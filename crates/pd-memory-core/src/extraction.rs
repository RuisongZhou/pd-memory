//! LLM extraction: classification and note creation for the indexing pipeline.
//!
//! Port of the *live* indexing half of the TypeScript reference
//! (`core/skills/llm-extraction.ts:218-327, 994-1105, 2519-2683`). The
//! behaviourally important parts are:
//!
//! 1. the two prompt families (classify one focus user turn; create one
//!    append-only note per classified category) with the output-language rule.
//!    The system prompts are the verbatim texts recorded in appendix A of
//!    `docs/research/memory-behavior-spec.zh.md` (parity requirement §9.1);
//! 2. the fixed prompt window (focus turn ± 2 turns, truncated per message) and
//!    its exact JSON key order;
//! 3. one JSON parse per response, with the reference loose parser for created notes.
//!
//! All model calls go through [`crate::ports::ModelProvider`]; failures are
//! degraded to no classification / no note, as in the reference.
//!
//! This module intentionally does **not** port dead code listed in the
//! behaviour spec §0.3 (e.g. `extractFileMemoryCandidates`,
//! `planDreamFileMemory`).

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::MemoryConfig;
use crate::error::{MemoryError, Result};
use crate::ports::{CompletionRequest, CompletionResponse, ModelProvider};
use crate::types::{
    MemoryCandidate, MemoryMessage, MemoryRecordType, MemoryScope, ProjectMetaRecord,
};

/// Schema names passed to [`ModelProvider::complete`]. The value is a hint for
/// provider-side JSON/response-format handling; the engine always parses and
/// validates the text itself.
pub mod schemas {
    pub const CLASSIFY: &str = "memory_classify";
    pub const CREATE_PROJECT: &str = "memory_create_project";
    pub const CREATE_USER: &str = "memory_create_user";
    pub const CREATE_FEEDBACK: &str = "memory_create_feedback";
    pub const ROUTE_PROJECT: &str = "memory_route_project";
}

/// LLM timeout for classify calls (ms), matching the reference
/// `DEFAULT_FILE_MEMORY_EXTRACTION_TIMEOUT_MS` (`75_000`).
pub const CLASSIFY_TIMEOUT_MS: u64 = 75_000;
/// LLM timeout for note creation calls (ms), same reference constant.
pub const CREATE_TIMEOUT_MS: u64 = 75_000;

/// Number of neighbouring turns included before/after the focus turn.
const NEIGHBOR_TURN_COUNT: usize = 2;
/// Per-message character budget inside the prompt window.
const CONTEXT_MESSAGE_CHAR_LIMIT: usize = 320;
/// Character budget for the focus user message echoed in the window.
const FOCUS_MESSAGE_CHAR_LIMIT: usize = 400;
/// Character budget for the current project description.
const PROJECT_DESCRIPTION_CHAR_LIMIT: usize = 220;
/// Character budget for a classification `reason` / `evidence` string.
const CLASSIFICATION_TEXT_CHAR_LIMIT: usize = 220;
/// Character budget for a created note name.
const NOTE_NAME_CHAR_LIMIT: usize = 80;
/// Character budget for a created note description.
const NOTE_DESCRIPTION_CHAR_LIMIT: usize = 180;
/// System prompt: decide which memory categories a focus user turn produces.
///
/// Verbatim from the reference (`llm-extraction.ts:218-251`, spec appendix A);
/// `skip`/`reason` stay in the create schemas even though the shared mock
/// provider omits them, because the parser treats them as optional.
const CLASSIFICATION_SYSTEM_PROMPT: &str = r#"You classify one focus user turn for a long-term memory indexing pipeline.

You are only deciding categories. Do not generate the memory file yet.

Rules:
- Base the decision on the focus user turn first.
- You may use the neighboring user/assistant turns only to disambiguate the focus turn.
- Assistant text is context only. Never classify something that exists only in assistant wording.
- A turn can match multiple categories, but at most once per category.
- Allowed categories:
  - user: cross-project durable personal identity/background facts about who the user is, such as name, profession, long-term role context, life background, or durable relationship context.
  - project: durable current-project facts such as what the project is, goals, scope, important progress, blockers, risks, key decisions.
  - feedback: current-project collaboration rules, delivery rules, output structure, title/body template rules, confirmed style guidance, language rules, and file/tool boundaries.
- Identity test: only use user when the focus turn is describing the user as a person.
- Override test: if another project could reasonably override this rule or preference, it is not user; classify it as feedback.
- Output test: if the turn is constraining how the assistant should reply, write, format, deliver, or touch files/tools, classify it as feedback.
- Project memory should prefer stable facts. Do not classify short-lived time-flow updates, percentages, or fleeting scheduling notes as project memory unless they carry a durable blocker/risk/fact.
- If the user explicitly says "请记住", "帮我记住", or "remember this", treat that as a stronger signal for durable memory. This is still inferred from the visible user text only.
- If nothing durable should be remembered, return should_store=false and labels=[].
- Return JSON only.

Use this exact JSON shape:
{
  "should_store": true,
  "labels": [
    {
      "type": "user | project | feedback",
      "reason": "why this category applies",
      "evidence": "short quote or evidence summary from the focus turn"
    }
  ]
}"#;

/// System prompt: create one append-only user identity note (reference A.2).
const USER_NOTE_CREATE_SYSTEM_PROMPT: &str = r#"You create one append-only user memory note from a focus user turn.

Rules:
- Create at most one user note.
- The note must capture only durable cross-project personal identity/background information about who the user is.
- Keep only long-lived identity facts such as name, profession, stable role context, life background, or durable relationship context.
- Do not include language choices, answer structure, formatting habits, style preferences, file boundaries, tool boundaries, or project-specific collaboration rules.
- One note should express one durable identity/background fact rather than a full profile rewrite.
- The visible output language must follow the dominant user language in the focus user turn and neighboring user turns.
- If the surrounding dialogue mixes languages, prefer the focus user turn language first, then the nearest neighboring user language.
- Apply this language rule consistently to the title/name, description, markdown headings, and markdown body text.
- Keep the note readable markdown.
- Do not force the note into a fixed profile template. Use headings only when they genuinely help readability.
- Return JSON only.

Use this exact JSON shape:
{
  "skip": false,
  "reason": "",
  "name": "short user-memory title",
  "description": "one-line description",
  "markdown": "markdown body"
}"#;

/// System prompt: create one append-only project note (reference A.3).
const PROJECT_NOTE_CREATE_SYSTEM_PROMPT: &str = r#"You create one append-only project memory note from a focus user turn.

Rules:
- Create at most one project note.
- The note belongs to the current project only.
- Capture durable project facts: what the project is, stable scope, goals, key progress, blockers, risks, important decisions, important next steps.
- Do not reduce the note to a vague status line.
- Do not focus on highly volatile percentages, fleeting schedules, or trivial short-term updates unless they reveal a durable blocker/risk/fact.
- The visible output language must follow the dominant user language in the focus user turn and neighboring user turns.
- If the surrounding dialogue mixes languages, prefer the focus user turn language first, then the nearest neighboring user language.
- Apply this language rule consistently to the title/name, description, markdown headings, and markdown body text.
- Keep the note readable markdown.
- Prefer meaningful headings when useful, such as: ## Summary, ## Current Stage, ## Constraints, ## Blockers, ## Next Steps, ## Timeline, ## Notes.
- Return JSON only.

Use this exact JSON shape:
{
  "skip": false,
  "reason": "",
  "name": "short project-memory title",
  "description": "one-line description",
  "markdown": "markdown body"
}"#;

/// System prompt: create one append-only feedback (collaboration rule) note
/// (reference A.4).
const FEEDBACK_NOTE_CREATE_SYSTEM_PROMPT: &str = r#"You create one append-only feedback memory note from a focus user turn.

Rules:
- Create at most one feedback note.
- The note belongs to the current project only.
- Use feedback for collaboration rules, delivery order, style constraints, title/body template rules, confirmed output expectations, language rules, and file/tool boundaries.
- The visible output language must follow the dominant user language in the focus user turn and neighboring user turns.
- If the surrounding dialogue mixes languages, prefer the focus user turn language first, then the nearest neighboring user language.
- Apply this language rule consistently to the title/name, description, markdown headings, and markdown body text.
- Keep the note readable markdown.
- Prefer meaningful headings when useful, especially: ## Rule, ## Why, ## How To Apply, ## Notes.
- Return JSON only.

Use this exact JSON shape:
{
  "skip": false,
  "reason": "",
  "name": "short feedback-memory title",
  "description": "one-line description",
  "markdown": "markdown body"
}"#;

/// Extraction service used by the indexing pipeline.
#[derive(Clone)]
pub struct LlmExtractor {
    pub model: Arc<dyn ModelProvider>,
}

/// One classified memory category for a focus user turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationLabel {
    /// Category to create a note for (`user`, `project` or `feedback`).
    pub kind: MemoryRecordType,
    /// Model-provided justification (truncated to 220 chars).
    pub reason: String,
    /// Model-provided evidence quote (truncated to 220 chars).
    pub evidence: String,
}

/// Focus-turn classification result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassificationResult {
    /// `true` only when the model asked to store something *and* at least one
    /// valid, de-duplicated category was returned.
    pub should_store: bool,
    /// Categories in model order, at most one per category.
    pub labels: Vec<ClassificationLabel>,
}

impl ClassificationResult {
    /// Convenience view of the category kinds (at most one per kind).
    pub fn kinds(&self) -> Vec<MemoryRecordType> {
        self.labels.iter().map(|label| label.kind).collect()
    }
}

impl LlmExtractor {
    pub fn new(model: Arc<dyn ModelProvider>) -> Self {
        Self { model }
    }

    /// Decide whether a focus turn should be stored and which categories it
    /// produces.
    ///
    /// `focus_turn` is the turn that starts with the focus user message and may
    /// already carry its assistant replies; `context_turns` is the flattened
    /// conversation of the batch (assistant messages are context only). The
    /// focus message is located inside the context to build the ±2-turn window.
    pub fn classify_turn(
        &self,
        config: &MemoryConfig,
        focus_turn: &[MemoryMessage],
        context_turns: &[MemoryMessage],
        project_meta: Option<&ProjectMetaRecord>,
    ) -> Result<ClassificationResult> {
        let focus_user = first_user_message(focus_turn)?;
        let context = if context_turns.is_empty() {
            focus_turn
        } else {
            context_turns
        };
        let window = build_index_prompt_window(context, focus_user, project_meta);
        let user_prompt = serialize_pretty(&window, "memory classify prompt");
        let parsed = complete_json(
            self.model.as_ref(),
            schemas::CLASSIFY,
            CLASSIFICATION_SYSTEM_PROMPT,
            &user_prompt,
            CLASSIFY_TIMEOUT_MS,
            config.model.max_output_tokens,
        );
        Ok(match parsed {
            Ok(parsed) => normalize_classification(&parsed),
            Err(error) => {
                log::warn!("memory turn classification fallback: {error}");
                ClassificationResult::default()
            }
        })
    }

    /// Create one candidate note of a given kind for a classified focus turn.
    ///
    /// Returns `Ok(None)` when the model explicitly skips the note
    /// (`skip: true`), when the payload is unusable (missing/short fields), or
    /// when a project note would get a generic placeholder name — mirroring
    /// `createMemoryNote` in the TypeScript reference.
    ///
    /// `captured_at` / `source_session_key` are deliberately left empty here;
    /// the indexing pipeline fills them from the L0 session (spec §3.9).
    pub fn create_note(
        &self,
        config: &MemoryConfig,
        kind: MemoryRecordType,
        classification: &ClassificationLabel,
        focus_turn: &[MemoryMessage],
        context_turns: &[MemoryMessage],
        project_meta: Option<&ProjectMetaRecord>,
    ) -> Result<Option<MemoryCandidate>> {
        let (schema, system_prompt) =
            match kind {
                MemoryRecordType::User => (schemas::CREATE_USER, USER_NOTE_CREATE_SYSTEM_PROMPT),
                MemoryRecordType::Project => {
                    (schemas::CREATE_PROJECT, PROJECT_NOTE_CREATE_SYSTEM_PROMPT)
                }
                MemoryRecordType::Feedback => {
                    (schemas::CREATE_FEEDBACK, FEEDBACK_NOTE_CREATE_SYSTEM_PROMPT)
                }
                MemoryRecordType::GeneralProjectMeta => return Err(MemoryError::InvalidInput(
                    "general_project_meta notes are produced by project routing, not by extraction"
                        .into(),
                )),
            };
        let focus_user = first_user_message(focus_turn)?;
        let context = if context_turns.is_empty() {
            focus_turn
        } else {
            context_turns
        };
        let prompt = CreateNotePrompt {
            classification: ClassificationPrompt {
                r#type: kind.as_str().to_string(),
                reason: classification.reason.clone(),
                evidence: classification.evidence.clone(),
            },
            context: build_index_prompt_window(context, focus_user, project_meta),
        };
        let user_prompt = serialize_pretty(&prompt, "memory create-note prompt");
        let response = complete_once(
            self.model.as_ref(),
            schema,
            system_prompt,
            &user_prompt,
            CREATE_TIMEOUT_MS,
            config.model.max_output_tokens,
        );
        let parsed = response.and_then(|response| {
            parse_json_object(&response.text)
                .or_else(|| parse_loose_create_payload(&response.text))
                .ok_or_else(|| MemoryError::ModelOutput {
                    schema: schema.to_string(),
                    message: "response was not a parseable memory note".into(),
                })
        });
        Ok(match parsed {
            Ok(parsed) => build_candidate_from_create_payload(kind, &parsed),
            Err(error) => {
                log::warn!("memory note creation fallback: {error}");
                None
            }
        })
    }
}

/// Shared JSON-mode completion helper used by indexing, recall and dream.
///
/// Contract:
/// - sends `system` + `user` verbatim (no prompt rewriting) with the logical
///   `schema_name` and `timeout_ms`;
/// - parses the first balanced JSON object found in the response (models
///   sometimes wrap it in code fences or prose);
/// - transport or parse failures become [`MemoryError::ModelOutput`];
/// - parsing never issues an extra model request.
pub fn complete_json(
    model: &dyn ModelProvider,
    schema: &str,
    system: &str,
    user: &str,
    timeout_ms: u64,
    max_output_tokens: Option<u32>,
) -> Result<Value> {
    let first = complete_once(model, schema, system, user, timeout_ms, max_output_tokens)?;
    parse_json_object(&first.text).ok_or_else(|| MemoryError::ModelOutput {
        schema: schema.to_string(),
        message: "response was not a parseable JSON object".into(),
    })
}

/// Extract the first balanced JSON object from a model response.
///
/// Tolerates markdown code fences, leading/trailing prose and nested objects;
/// returns `None` for empty, truncated or non-object output.
pub fn parse_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return serde_json::from_str::<Value>(trimmed)
            .ok()
            .filter(Value::is_object);
    }
    let start = trimmed.find('{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in trimmed[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + character.len_utf8();
                    let value = serde_json::from_str::<Value>(&trimmed[start..end]).ok()?;
                    return value.is_object().then_some(value);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// First JSON object of a prompt window, in the exact reference key order:
/// `current_project_meta, focus_user_turn,
/// focus_turn_with_neighbor_assistant_context, previous_turns, next_turns`.
#[derive(Debug, Serialize)]
struct PromptWindow {
    current_project_meta: Option<PromptProjectMeta>,
    focus_user_turn: PromptFocusUserTurn,
    focus_turn_with_neighbor_assistant_context: PromptTurn,
    previous_turns: Vec<PromptTurn>,
    next_turns: Vec<PromptTurn>,
}

#[derive(Debug, Serialize)]
struct PromptProjectMeta {
    project_id: String,
    project_name: String,
    description: String,
    status: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct PromptFocusUserTurn {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct PromptTurn {
    turn_index: usize,
    messages: Vec<PromptTurnMessage>,
}

#[derive(Debug, Serialize)]
struct PromptTurnMessage {
    role: String,
    content: String,
}

/// Create-note user prompt: `{classification, context}` in reference key order.
#[derive(Debug, Serialize)]
struct CreateNotePrompt {
    classification: ClassificationPrompt,
    context: PromptWindow,
}

#[derive(Debug, Serialize)]
struct ClassificationPrompt {
    r#type: String,
    reason: String,
    evidence: String,
}

fn complete_once(
    model: &dyn ModelProvider,
    schema: &str,
    system: &str,
    user: &str,
    timeout_ms: u64,
    max_output_tokens: Option<u32>,
) -> Result<CompletionResponse> {
    let request = CompletionRequest {
        system: system.to_string(),
        user: user.to_string(),
        schema_name: schema.to_string(),
        timeout: Duration::from_millis(timeout_ms),
        max_output_tokens,
        temperature: None,
    };
    model
        .complete(request)
        .map_err(|error| MemoryError::ModelOutput {
            schema: schema.to_string(),
            message: format!("model request failed: {error}"),
        })
}

// The reference only tolerates malformed JSON for create-note payloads.
fn parse_loose_create_payload(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    let envelope = raw.get(start..=end)?;
    let mut payload = serde_json::Map::new();
    let lower = envelope.to_ascii_lowercase();
    if let Some(tail) = loose_property_tail(envelope, &lower, "skip") {
        let tail = tail.to_ascii_lowercase();
        for (word, value) in [("true", true), ("false", false)] {
            if tail.starts_with(word) {
                payload.insert("skip".into(), json!(value));
            }
        }
    }
    for (key, next_keys) in [
        ("reason", &["name", "description", "markdown"][..]),
        ("name", &["description", "markdown"][..]),
        ("description", &["markdown"][..]),
        ("markdown", &[][..]),
    ] {
        let Some(tail) =
            loose_property_tail(envelope, &lower, key).and_then(|s| s.strip_prefix('"'))
        else {
            continue;
        };
        let end = if next_keys.is_empty() {
            tail.rfind('"').filter(|end| tail[end + 1..].trim() == "}")
        } else {
            tail.match_indices('"').find_map(|(offset, _)| {
                let rest = tail[offset + 1..]
                    .trim_start()
                    .strip_prefix(',')?
                    .trim_start();
                let rest_lower = rest.to_ascii_lowercase();
                next_keys
                    .iter()
                    .any(|next| {
                        rest_lower
                            .strip_prefix(&format!("\"{next}\""))
                            .is_some_and(|rest| rest.trim_start().starts_with(':'))
                    })
                    .then_some(offset)
            })
        };
        if let Some(value) = end
            .and_then(|end| tail.get(..end))
            .filter(|s| !s.is_empty())
        {
            let value = value
                .replace("\\r\\n", "\n")
                .replace("\\n", "\n")
                .replace("\\r", "\r")
                .replace("\\t", "\t")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            payload.insert(key.into(), json!(value));
        }
    }
    ["name", "description", "markdown"]
        .iter()
        .all(|key| payload.contains_key(*key))
        .then_some(Value::Object(payload))
}

fn loose_property_tail<'a>(source: &'a str, lower: &str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    lower.match_indices(&pattern).find_map(|(offset, _)| {
        source[offset + pattern.len()..]
            .trim_start()
            .strip_prefix(':')
            .map(str::trim_start)
    })
}

/// Build the fixed prompt window shared by classify and create calls.
///
/// Shape is the reference `buildIndexPromptWindow`: current project meta, the
/// focus user turn, the focus turn with its assistant context, then the two
/// neighbouring turns on each side.
fn build_index_prompt_window(
    context_messages: &[MemoryMessage],
    focus_user: &MemoryMessage,
    project_meta: Option<&ProjectMetaRecord>,
) -> PromptWindow {
    let turns = conversation_turns(context_messages);
    let focus_index = turns
        .iter()
        .position(|turn| turn.iter().any(|message| same_message(message, focus_user)));

    let (focus_turn, previous_turns, next_turns): (
        Vec<MemoryMessage>,
        Vec<Vec<MemoryMessage>>,
        Vec<Vec<MemoryMessage>>,
    ) = match focus_index {
        Some(index) => {
            let previous = turns[index.saturating_sub(NEIGHBOR_TURN_COUNT)..index].to_vec();
            let next_start = (index + 1).min(turns.len());
            let next_end = (index + 1 + NEIGHBOR_TURN_COUNT).min(turns.len());
            (
                turns[index].clone(),
                previous,
                turns[next_start..next_end].to_vec(),
            )
        }
        None => (vec![focus_user.clone()], Vec::new(), Vec::new()),
    };

    PromptWindow {
        current_project_meta: project_meta.map(|meta| PromptProjectMeta {
            project_id: meta.project_id.clone(),
            project_name: meta.project_name.clone(),
            description: truncate_for_prompt(&meta.description, PROJECT_DESCRIPTION_CHAR_LIMIT),
            status: meta.status.clone(),
            updated_at: meta.updated_at.clone(),
        }),
        focus_user_turn: PromptFocusUserTurn {
            role: focus_user.role.clone(),
            content: truncate_for_prompt(&focus_user.content, FOCUS_MESSAGE_CHAR_LIMIT),
        },
        focus_turn_with_neighbor_assistant_context: prompt_turn(&focus_turn, 1),
        previous_turns: previous_turns
            .iter()
            .enumerate()
            .map(|(offset, turn)| prompt_turn(turn, offset + 1))
            .collect(),
        next_turns: next_turns
            .iter()
            .enumerate()
            .map(|(offset, turn)| prompt_turn(turn, offset + 1))
            .collect(),
    }
}

fn prompt_turn(turn: &[MemoryMessage], turn_index: usize) -> PromptTurn {
    PromptTurn {
        turn_index,
        messages: turn
            .iter()
            .map(|message| PromptTurnMessage {
                role: message.role.clone(),
                content: truncate_for_prompt(&message.content, CONTEXT_MESSAGE_CHAR_LIMIT),
            })
            .collect(),
    }
}

fn serialize_pretty<T: Serialize>(value: &T, label: &str) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(text) => text,
        Err(error) => {
            log::error!("could not serialise {label}: {error}");
            String::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing / normalization
// ---------------------------------------------------------------------------

fn normalize_classification(parsed: &Value) -> ClassificationResult {
    // The reference emits `labels`; `categories` is accepted as an alias so a
    // slightly drifting model/provider does not silently lose categories.
    let labels_value = parsed
        .get("labels")
        .or_else(|| parsed.get("categories"))
        .and_then(Value::as_array);
    let mut labels: Vec<ClassificationLabel> = Vec::new();
    if let Some(items) = labels_value {
        for item in items {
            let Some(object) = item.as_object() else {
                continue;
            };
            let Some(raw_type) = object.get("type").and_then(Value::as_str) else {
                continue;
            };
            let Some(kind) = parse_create_kind(raw_type) else {
                continue;
            };
            // At most one label per category, first occurrence wins.
            if labels.iter().any(|label| label.kind == kind) {
                continue;
            }
            labels.push(ClassificationLabel {
                kind,
                reason: classification_text(object.get("reason")),
                evidence: classification_text(object.get("evidence")),
            });
        }
    }
    let should_store = truthy(parsed.get("should_store")) && !labels.is_empty();
    ClassificationResult {
        should_store,
        labels,
    }
}

fn build_candidate_from_create_payload(
    kind: MemoryRecordType,
    payload: &Value,
) -> Option<MemoryCandidate> {
    // The reference treats `skip === true` as an explicit "no note".
    if payload.get("skip") == Some(&Value::Bool(true)) {
        return None;
    }
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(|value| truncate_for_prompt(value, NOTE_NAME_CHAR_LIMIT))
        .unwrap_or_default();
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .map(|value| truncate_for_prompt(value, NOTE_DESCRIPTION_CHAR_LIMIT))
        .unwrap_or_default();
    let body = payload
        .get("markdown")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if name.is_empty() || description.is_empty() || body.is_empty() {
        return None;
    }
    if kind == MemoryRecordType::Project && is_generic_project_name(&name) {
        return None;
    }
    Some(MemoryCandidate {
        kind,
        scope: if kind == MemoryRecordType::User {
            MemoryScope::Global
        } else {
            MemoryScope::Project
        },
        name,
        description,
        body,
        ..Default::default()
    })
}

fn parse_create_kind(value: &str) -> Option<MemoryRecordType> {
    match value.trim() {
        "user" => Some(MemoryRecordType::User),
        "project" => Some(MemoryRecordType::Project),
        "feedback" => Some(MemoryRecordType::Feedback),
        _ => None,
    }
}

fn is_generic_project_name(name: &str) -> bool {
    let normalized = normalize_whitespace(name).to_lowercase();
    matches!(
        normalized.as_str(),
        "" | "overview" | "project" | "project-item" | "memory-item"
    )
}

/// `Boolean(value)` semantics of the TypeScript reference (only
/// `null`/`false`/`0`/`""` are falsey).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn classification_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(|value| truncate_for_prompt(value, CLASSIFICATION_TEXT_CHAR_LIMIT))
        .unwrap_or_default()
}

fn first_user_message(focus_turn: &[MemoryMessage]) -> Result<&MemoryMessage> {
    focus_turn
        .iter()
        .find(|message| message.role == "user")
        .ok_or_else(|| {
            MemoryError::InvalidInput("focus turn must contain at least one user message".into())
        })
}

// ---------------------------------------------------------------------------
// Small shared helpers (also used by `indexing`)
// ---------------------------------------------------------------------------

/// Split a flat message list into turns: a `user` message starts a turn, the
/// `assistant` messages that follow belong to it. Non-dialogue roles (`tool`,
/// `system`, ...) are ignored, and an assistant message that precedes the first
/// user message is dropped — matching `buildConversationTurns` in the reference.
pub(crate) fn conversation_turns(messages: &[MemoryMessage]) -> Vec<Vec<MemoryMessage>> {
    let mut turns: Vec<Vec<MemoryMessage>> = Vec::new();
    let mut current: Vec<MemoryMessage> = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "user" => {
                if !current.is_empty() {
                    turns.push(std::mem::take(&mut current));
                }
                current.push(message.clone());
            }
            "assistant" => {
                if !current.is_empty() {
                    current.push(message.clone());
                }
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

/// Message identity used by the prefix diff: role + content, ignoring `msg_id`.
pub(crate) fn same_message(left: &MemoryMessage, right: &MemoryMessage) -> bool {
    left.role == right.role && left.content == right.content
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max_units` UTF-16 code units, mirroring JS `String#slice`
/// semantics without ever splitting a surrogate pair (spec §9.3 #20).
fn truncate_utf16(value: &str, max_units: usize) -> String {
    if value.encode_utf16().count() <= max_units {
        return value.to_string();
    }
    let mut units = 0usize;
    let mut end = 0usize;
    for (offset, character) in value.char_indices() {
        let char_units = character.len_utf16();
        if units + char_units > max_units {
            break;
        }
        units += char_units;
        end = offset + character.len_utf8();
    }
    value[..end].to_string()
}

/// `truncateForPrompt`: whitespace-normalise, then truncate without ellipsis.
fn truncate_for_prompt(value: &str, max_units: usize) -> String {
    let normalized = normalize_whitespace(value);
    truncate_utf16(&normalized, max_units)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> MemoryMessage {
        MemoryMessage::new(role, content)
    }

    #[test]
    fn parse_json_object_handles_fences_and_prose() {
        let value = parse_json_object("```json\n{\"a\": 1}\n```").expect("fenced json");
        assert_eq!(value["a"], json!(1));
        let value =
            parse_json_object("Here is the result: {\"a\": {\"b\": 2}} done").expect("prose");
        assert_eq!(value["a"]["b"], json!(2));
        let value = parse_json_object("{\"text\": \"brace } inside string\"}").expect("nested");
        assert_eq!(value["text"], json!("brace } inside string"));
    }

    #[test]
    fn parse_json_object_rejects_broken_output() {
        assert!(parse_json_object("").is_none());
        assert!(parse_json_object("no json here").is_none());
        assert!(parse_json_object("{\"a\": ").is_none());
        assert!(parse_json_object("{broken}").is_none());
    }

    #[test]
    fn turns_start_at_user_messages() {
        let messages = vec![
            message("assistant", "leading assistant is dropped"),
            message("user", "u1"),
            message("assistant", "a1"),
            message("assistant", "a2"),
            message("tool", "tool output"),
            message("user", "u2"),
            message("user", "u3"),
            message("assistant", "a3"),
        ];
        let turns = conversation_turns(&messages);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].len(), 3);
        assert_eq!(turns[1].len(), 1);
        assert_eq!(turns[2].len(), 2);
        assert_eq!(turns[0][0].content, "u1");
        assert_eq!(turns[2][0].content, "u3");
    }

    #[test]
    fn prompt_window_keeps_two_neighbour_turns_each_side() {
        let mut messages = Vec::new();
        for index in 0..6 {
            messages.push(message("user", &format!("u{index}")));
            messages.push(message("assistant", &format!("a{index}")));
        }
        let focus = messages[4].clone(); // u2
        let window = build_index_prompt_window(&messages, &focus, None);
        assert_eq!(window.previous_turns.len(), 2);
        assert_eq!(window.next_turns.len(), 2);
        assert_eq!(window.previous_turns[0].messages[0].content, "u0");
        assert_eq!(window.previous_turns[1].messages[0].content, "u1");
        assert_eq!(window.next_turns[0].messages[0].content, "u3");
        assert_eq!(window.next_turns[1].messages[0].content, "u4");
        assert_eq!(window.focus_user_turn.content, "u2");
        assert_eq!(
            window
                .focus_turn_with_neighbor_assistant_context
                .messages
                .len(),
            2
        );
    }

    #[test]
    fn truthiness_matches_javascript_boolean() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!("false"))));
        assert!(truthy(Some(&json!([]))));
    }

    #[test]
    fn create_payload_normalises_and_rejects_generic_names() {
        let payload = json!({
            "name": "  Boreal   project  ",
            "description": "  local knowledge tool \n",
            "markdown": "  ## Summary\nbody  ",
        });
        let candidate = build_candidate_from_create_payload(MemoryRecordType::Project, &payload)
            .expect("candidate");
        assert_eq!(candidate.name, "Boreal project");
        assert_eq!(candidate.description, "local knowledge tool");
        assert_eq!(candidate.body, "## Summary\nbody");
        assert_eq!(candidate.scope, MemoryScope::Project);

        let generic = json!({"name": "project", "description": "d", "markdown": "m"});
        assert!(build_candidate_from_create_payload(MemoryRecordType::Project, &generic).is_none());
    }
}
