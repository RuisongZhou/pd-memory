//! Deterministic in-process provider for tests, examples and mock-mode E2E.
//!
//! [`MockProvider`] never touches the network and never reads the clock: the
//! same request always produces the same response. It is selected automatically
//! by [`crate::provider_from_config`] when `PD_MEMORY_PROVIDER=mock`, and can be
//! constructed directly by tests and examples.
//!
//! Responses follow the JSON shapes the indexing/recall/dream pipelines expect.
//! The heuristics are intentionally generous so that end-to-end smoke tests
//! ("capture a turn, flush, recall it") observe durable memory instead of
//! empty results.

use pd_memory_core::extraction::schemas as extraction;
use pd_memory_core::ports::{CompletionRequest, CompletionResponse, ModelProvider, TokenUsage};
use serde_json::{json, Value};

/// Cues that indicate a durable collaboration rule / preference.
const DURABLE_CUES: &[&str] = &[
    "请记住",
    "帮我记住",
    "记住",
    "以后",
    "永久",
    "长期",
    "always",
    "prefer",
    "remember",
    "规则",
    "rule",
    "convention",
    "never",
];

const MAX_NAME_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 320;
const MAX_SELECTED_FILES: usize = 5;

/// Deterministic [`ModelProvider`] used for tests and WorkBuddy mock E2E runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockProvider;

impl MockProvider {
    /// Create a mock provider.
    pub fn new() -> Self {
        Self
    }
}

impl ModelProvider for MockProvider {
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, pd_memory_core::error::MemoryError> {
        let text = mock_response(&request.schema_name, &request.user);
        let input_chars = request.system.chars().count() + request.user.chars().count();
        let output_chars = text.chars().count();
        Ok(CompletionResponse {
            text,
            usage: Some(TokenUsage {
                input_tokens: Some((input_chars / 4) as u64),
                output_tokens: Some((output_chars / 4) as u64),
            }),
        })
    }
}

/// Build the deterministic JSON response for `schema`.
pub(crate) fn mock_response(schema: &str, user_prompt: &str) -> String {
    match schema {
        extraction::CLASSIFY => classify_response(user_prompt),
        extraction::CREATE_PROJECT => create_response("project", user_prompt),
        extraction::CREATE_USER => create_response("user", user_prompt),
        extraction::CREATE_FEEDBACK => create_response("feedback", user_prompt),
        "memory_gate" | "memory_route_project" => gate_response(user_prompt),
        "memory_select_files" => select_files_response(user_prompt),
        pd_memory_core::dream::schemas::CLUSTER => cluster_response(user_prompt),
        pd_memory_core::dream::schemas::MERGE => merge_response(user_prompt),
        pd_memory_core::dream::schemas::PROJECT_META => project_meta_response(user_prompt),
        pd_memory_core::dream::schemas::USER_PROFILE => user_profile_response(user_prompt),
        other => json!({"mock": true, "schema": other}).to_string(),
    }
}

// ---------------------------------------------------------------------------
// Prompt inspection helpers
// ---------------------------------------------------------------------------

/// A `(lowercased key, value)` pair collected from a JSON prompt.
fn collect_entries(value: &Value, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                out.push((key.to_ascii_lowercase(), child.clone()));
                collect_entries(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_entries(item, out);
            }
        }
        _ => {}
    }
}

fn prompt_entries(prompt: &str) -> Vec<(String, Value)> {
    let mut entries = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(prompt.trim()) {
        collect_entries(&value, &mut entries);
    }
    entries
}

/// Render a message-ish JSON value as plain text (`content`/`text` arrays are
/// concatenated).
fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(value_text).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        Value::Object(map) => {
            for key in ["content", "text", "message", "prompt"] {
                if let Some(child) = map.get(key) {
                    if let Some(text) = value_text(child) {
                        return Some(text);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn find_text(entries: &[(String, Value)], predicate: impl Fn(&str) -> bool) -> Option<String> {
    entries
        .iter()
        .find(|(key, _)| predicate(key))
        .and_then(|(_, value)| value_text(value))
}

/// Focus user turn text extracted from the prompt, with pragmatic fallbacks.
fn focus_text(prompt: &str) -> String {
    let entries = prompt_entries(prompt);
    if let Some(text) = find_text(&entries, |key| key.contains("focus")) {
        return text;
    }
    // Create-note prompts embed the focus turn inside a context window; use the
    // full conversation context (it contains the focus user text verbatim)
    // before falling back to the shorter classification evidence quote.
    if let Some(text) = find_text(&entries, |key| {
        matches!(
            key,
            "batch_context_messages"
                | "batch_context"
                | "context_messages"
                | "messages"
                | "conversation"
        )
    }) {
        return text;
    }
    if let Some(text) = find_text(&entries, |key| key == "evidence") {
        return text;
    }
    if let Some(text) = find_text(&entries, |key| {
        matches!(key, "content" | "text" | "markdown" | "user")
    }) {
        return text;
    }
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        "MockProvider deterministic memory".to_string()
    } else {
        trimmed.chars().take(2_000).collect()
    }
}

/// Query text extracted from a recall prompt.
fn query_text(prompt: &str) -> Option<String> {
    let entries = prompt_entries(prompt);
    find_text(&entries, |key| key == "query")
}

/// Truncate by characters (never splits a UTF-8 code point) and normalise
/// whitespace runs to single spaces.
fn truncate_chars(raw: &str, max_chars: usize) -> String {
    let normalised = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    normalised.chars().take(max_chars).collect()
}

fn first_line(raw: &str) -> &str {
    raw.lines().next().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Candidate-file discovery (recall / dream prompts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FileCandidate {
    id: String,
    haystack: String,
}

fn collect_candidates_from_value(value: &Value, out: &mut Vec<FileCandidate>) {
    match value {
        Value::Object(map) => {
            let mut id = None;
            for key in ["id", "relative_path", "relativepath", "path"] {
                if let Some(Value::String(candidate)) = map.get(key) {
                    if candidate.trim().ends_with(".md") {
                        id = Some(candidate.trim().to_string());
                        break;
                    }
                }
            }
            if let Some(id) = id {
                let mut haystack = id.clone();
                for key in ["description", "name", "title", "summary", "reason"] {
                    if let Some(Value::String(text)) = map.get(key) {
                        haystack.push(' ');
                        haystack.push_str(text);
                    }
                }
                out.push(FileCandidate { id, haystack });
            }
            if let Some(Value::Array(paths)) = map.get("member_relative_paths") {
                for path in paths {
                    if let Some(path) = path.as_str() {
                        if path.ends_with(".md") {
                            out.push(FileCandidate {
                                id: path.to_string(),
                                haystack: path.to_string(),
                            });
                        }
                    }
                }
            }
            for child in map.values() {
                collect_candidates_from_value(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_candidates_from_value(item, out);
            }
        }
        _ => {}
    }
}

fn text_path_candidates(prompt: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut token = String::new();
    for ch in prompt.chars() {
        let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/');
        if allowed {
            token.push(ch);
        } else if !token.is_empty() {
            if token.ends_with(".md") {
                paths.push(token.trim_start_matches("./").to_string());
            }
            token.clear();
        }
    }
    if token.ends_with(".md") {
        paths.push(token.trim_start_matches("./").to_string());
    }
    paths
}

fn file_candidates(prompt: &str) -> Vec<FileCandidate> {
    let mut candidates = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(prompt.trim()) {
        collect_candidates_from_value(&value, &mut candidates);
    }
    let mut seen: Vec<String> = Vec::new();
    let mut deduped: Vec<FileCandidate> = Vec::new();
    for candidate in candidates {
        if !seen.contains(&candidate.id) {
            seen.push(candidate.id.clone());
            deduped.push(candidate);
        }
    }
    if deduped.is_empty() {
        for id in text_path_candidates(prompt) {
            if !seen.contains(&id) {
                seen.push(id.clone());
                deduped.push(FileCandidate {
                    id: id.clone(),
                    haystack: id,
                });
            }
        }
    }
    deduped
}

fn prompt_limit(prompt: &str) -> usize {
    let entries = prompt_entries(prompt);
    entries
        .iter()
        .find(|(key, _)| key == "limit")
        .and_then(|(_, value)| value.as_u64())
        .map(|limit| (limit as usize).clamp(1, MAX_SELECTED_FILES))
        .unwrap_or(MAX_SELECTED_FILES)
}

/// CJK/ASCII tokenisation used for deterministic relevance ranking.
fn tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        } else {
            if word.len() > 1 {
                out.push(std::mem::take(&mut word));
            } else {
                word.clear();
            }
            if is_cjk(ch) {
                out.push(ch.to_string());
            }
        }
    }
    if word.len() > 1 {
        out.push(word);
    }
    out
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

fn overlap_score(query: &[String], haystack: &[String]) -> usize {
    query
        .iter()
        .filter(|token| haystack.iter().any(|candidate| candidate == *token))
        .count()
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn classify_response(prompt: &str) -> String {
    let text = focus_text(prompt);
    let lowered = text.to_lowercase();
    let durable = DURABLE_CUES.iter().any(|cue| lowered.contains(cue));
    let evidence = truncate_chars(&text, 120);
    if !durable && text.chars().count() < 6 {
        return json!({"should_store": false, "labels": []}).to_string();
    }
    let labels = if durable {
        json!([
            {
                "type": "feedback",
                "reason": "mock: durable collaboration cue detected",
                "evidence": evidence,
            },
            {
                "type": "project",
                "reason": "mock: durable project fact detected",
                "evidence": evidence,
            }
        ])
    } else {
        json!([{
            "type": "project",
            "reason": "mock: default project memory for captured user text",
            "evidence": evidence,
        }])
    };
    json!({"should_store": true, "labels": labels}).to_string()
}

fn create_response(kind: &str, prompt: &str) -> String {
    let text = focus_text(prompt);
    let raw_name = first_line(&text);
    let name = {
        let name = truncate_chars(raw_name, MAX_NAME_CHARS);
        if name.is_empty() {
            format!("Mock {kind} memory")
        } else {
            name
        }
    };
    let description = {
        let description = truncate_chars(&text, MAX_DESCRIPTION_CHARS);
        if description.is_empty() {
            format!("Deterministic mock {kind} memory note")
        } else {
            description
        }
    };
    let markdown = format!("## {name}\n\n{description}\n\n### Captured\n\n{text}\n");
    json!({
        "skip": false,
        "name": name,
        "description": description,
        "markdown": markdown,
    })
    .to_string()
}

fn gate_response(prompt: &str) -> String {
    // Prefer the explicit `query` field of the recall prompt; an empty query
    // means "nothing to recall". Non-JSON prompts are treated as a query.
    let entries = prompt_entries(prompt);
    let explicit_query =
        entries
            .iter()
            .find(|(key, _)| key == "query")
            .map(|(_, value)| match value {
                Value::String(text) => text.clone(),
                other => value_text(other).unwrap_or_default(),
            });
    let route = match explicit_query {
        Some(query) if !query.trim().is_empty() => "mix",
        Some(_) => "none",
        None => {
            if focus_text(prompt).trim().is_empty() {
                "none"
            } else {
                "mix"
            }
        }
    };
    json!({"route": route}).to_string()
}

fn select_files_response(prompt: &str) -> String {
    let candidates = file_candidates(prompt);
    let query = query_text(prompt).unwrap_or_default();
    let query_tokens = tokens(&query);
    let mut ranked: Vec<(usize, usize, FileCandidate)> = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let score = overlap_score(&query_tokens, &tokens(&candidate.haystack));
            (score, index, candidate)
        })
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let selected: Vec<String> = ranked
        .into_iter()
        .take(prompt_limit(prompt))
        .map(|(_, _, candidate)| candidate.id)
        .collect();
    json!({"selected_ids": selected}).to_string()
}

fn cluster_response(prompt: &str) -> String {
    let candidates = file_candidates(prompt);
    if candidates.len() < 2 {
        return json!({
            "summary": "mock: not enough files to form a dream cluster",
            "clusters": [],
        })
        .to_string();
    }
    let first = candidates[0].id.clone();
    let second = candidates[1].id.clone();
    let kind = if prompt
        .to_ascii_lowercase()
        .contains("\"kind\": \"feedback\"")
        || prompt
            .to_ascii_lowercase()
            .contains("\"kind\":\"feedback\"")
    {
        "feedback"
    } else {
        "project"
    };
    json!({
        "summary": "mock: deterministic dream cluster plan",
        "clusters": [{
            "member_relative_paths": [first, second],
            "reason": format!("mock: two {kind} files share a deterministic cluster"),
        }],
    })
    .to_string()
}

fn merge_response(prompt: &str) -> String {
    let entries = prompt_entries(prompt);
    let mut contents: Vec<String> = Vec::new();
    for (key, value) in &entries {
        if matches!(key.as_str(), "content" | "markdown" | "body") {
            if let Some(text) = value_text(value) {
                if !text.trim().is_empty() && !contents.contains(&text) {
                    contents.push(text);
                }
            }
        }
    }
    if contents.is_empty() {
        contents.push(focus_text(prompt));
    }
    let merged = contents.join("\n\n---\n\n");
    let markdown = format!("## Merged\n\n{merged}\n");
    let summary = format!("mock: merged {} memory record(s)", contents.len());
    json!({
        "summary": summary,
        "name": "Merged Memory",
        "description": "Deterministic mock merged memory note",
        "markdown": markdown,
    })
    .to_string()
}

fn project_meta_response(prompt: &str) -> String {
    let entries = prompt_entries(prompt);
    let project_name = find_text(&entries, |key| {
        matches!(key, "project_name" | "projectname" | "name")
    })
    .map(|text| truncate_chars(&text, 80))
    .filter(|text| !text.is_empty())
    .unwrap_or_else(|| "Mock Project".to_string());
    let description = find_text(&entries, |key| key == "description")
        .map(|text| truncate_chars(&text, 320))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Deterministic mock project metadata".to_string());
    json!({
        "should_update": false,
        "reason": "mock: preserving current project metadata",
        "project_name": project_name,
        "description": description,
        "status": "in_progress",
    })
    .to_string()
}

fn user_profile_response(prompt: &str) -> String {
    let entries = prompt_entries(prompt);
    let mut bullets: Vec<String> = Vec::new();
    for (key, value) in &entries {
        if matches!(
            key.as_str(),
            "notes"
                | "identity_background"
                | "identity_background_markdown"
                | "content"
                | "markdown"
        ) {
            if let Some(text) = value_text(value) {
                for line in text.lines() {
                    let line = line.trim().trim_start_matches(['-', '*']).trim();
                    if !line.is_empty() && !bullets.iter().any(|existing| existing == line) {
                        bullets.push(line.chars().take(200).collect());
                    }
                }
            }
        }
    }
    if bullets.is_empty() {
        bullets.push("MockProvider identity background".to_string());
    }
    let markdown = bullets
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "summary": "mock: deterministic user profile",
        "identity_background_markdown": markdown,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("mock response is valid JSON")
    }

    #[test]
    fn classify_with_durable_cue_returns_feedback_and_project() {
        let prompt = json!({
            "focus_user_turn": {"role": "user", "content": "请记住：以后交付先给结论"},
            "batch_context_messages": [],
        })
        .to_string();
        let value = parse(&mock_response(extraction::CLASSIFY, &prompt));
        assert_eq!(value["should_store"], true);
        let labels = value["labels"].as_array().unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0]["type"], "feedback");
        assert_eq!(labels[1]["type"], "project");
    }

    #[test]
    fn classify_short_text_does_not_store() {
        let value = parse(&mock_response(
            extraction::CLASSIFY,
            "{\"focus_user_turn\":\"hi\"}",
        ));
        assert_eq!(value["should_store"], false);
    }

    #[test]
    fn create_project_includes_focus_text_verbatim() {
        let focus = "这个项目叫 Boreal，是一个本地知识库整理工具";
        let prompt = json!({
            "classification": {"type": "project", "reason": "r", "evidence": focus},
            "context": {
                "focus_user_turn": {"role": "user", "content": focus},
                "batch_context_messages": [{"role": "assistant", "content": "ok"}]
            }
        })
        .to_string();
        let value = parse(&mock_response(extraction::CREATE_PROJECT, &prompt));
        assert_eq!(value["name"], "这个项目叫 Boreal，是一个本地知识库整理工具");
        let markdown = value["markdown"].as_str().unwrap();
        assert!(markdown.contains(focus));
    }

    #[test]
    fn gate_uses_query_presence() {
        let with_query = json!({"query": "what do I prefer?", "recent_messages": []}).to_string();
        assert_eq!(
            parse(&mock_response("memory_gate", &with_query))["route"],
            "mix"
        );
        let empty = json!({"query": ""}).to_string();
        assert_eq!(
            parse(&mock_response("memory_gate", &empty))["route"],
            "none"
        );
    }

    #[test]
    fn select_files_prefers_query_overlap_and_respects_limit() {
        let prompt = json!({
            "query": "deployment pipeline",
            "manifest": [
                {"id": "Project/unrelated-abc.md", "description": "travel notes"},
                {"id": "Project/deploy-123.md", "description": "deployment pipeline details"},
                {"id": "Feedback/style-def.md", "description": "reply style"}
            ],
            "limit": 2
        })
        .to_string();
        let value = parse(&mock_response("memory_select_files", &prompt));
        let ids: Vec<&str> = value["selected_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "Project/deploy-123.md");
    }

    #[test]
    fn select_files_falls_back_to_text_paths() {
        let prompt = "candidates: Project/a.md Feedback/b.md";
        let value = parse(&mock_response("memory_select_files", prompt));
        assert_eq!(
            value["selected_ids"],
            json!(["Project/a.md", "Feedback/b.md"])
        );
    }

    #[test]
    fn dream_cluster_uses_first_two_candidates() {
        let prompt = json!({
            "kind": "project",
            "headers": [
                {"relative_path": "Project/a.md"},
                {"relative_path": "Project/b.md"},
                {"relative_path": "Project/c.md"}
            ]
        })
        .to_string();
        let value = parse(&mock_response(
            pd_memory_core::dream::schemas::CLUSTER,
            &prompt,
        ));
        assert_eq!(
            value["clusters"][0]["member_relative_paths"],
            json!(["Project/a.md", "Project/b.md"])
        );
    }

    #[test]
    fn dream_cluster_empty_for_single_candidate() {
        let prompt = json!({"headers": [{"relative_path": "Project/a.md"}]}).to_string();
        let value = parse(&mock_response(
            pd_memory_core::dream::schemas::CLUSTER,
            &prompt,
        ));
        assert!(value["clusters"].as_array().unwrap().is_empty());
    }

    #[test]
    fn dream_merge_concatenates_and_project_meta_preserves() {
        let prompt = json!({
            "records": [
                {"name": "a", "content": "# A\nfirst"},
                {"name": "b", "content": "# B\nsecond"}
            ]
        })
        .to_string();
        let merged = parse(&mock_response(
            pd_memory_core::dream::schemas::MERGE,
            &prompt,
        ));
        let markdown = merged["markdown"].as_str().unwrap();
        assert!(markdown.starts_with("## Merged"));
        assert!(markdown.contains("first") && markdown.contains("second"));

        let meta = parse(&mock_response(
            pd_memory_core::dream::schemas::PROJECT_META,
            &json!({"project_name": "Boreal"}).to_string(),
        ));
        assert_eq!(meta["should_update"], false);
        assert_eq!(meta["project_name"], "Boreal");
    }

    #[test]
    fn user_profile_returns_bullets() {
        let prompt =
            json!({"records": [{"notes": ["likes Rust", "works in Shanghai"]}]}).to_string();
        let value = parse(&mock_response(
            pd_memory_core::dream::schemas::USER_PROFILE,
            &prompt,
        ));
        let markdown = value["identity_background_markdown"].as_str().unwrap();
        assert!(markdown.contains("- likes Rust"));
    }

    #[test]
    fn unknown_schema_returns_object() {
        let value = parse(&mock_response("something_new", "prompt"));
        assert_eq!(value["mock"], true);
        assert_eq!(value["schema"], "something_new");
    }

    #[test]
    fn provider_is_deterministic_across_calls() {
        let request = CompletionRequest {
            system: "system".into(),
            user: json!({"focus_user_turn": "remember: always reply in Chinese"}).to_string(),
            schema_name: extraction::CLASSIFY.into(),
            timeout: std::time::Duration::from_secs(1),
            max_output_tokens: None,
            temperature: None,
        };
        let provider = MockProvider::new();
        let first = provider.complete(request.clone()).unwrap();
        let second = provider.complete(request).unwrap();
        assert_eq!(first.text, second.text);
    }

    #[test]
    fn tokenization_handles_cjk_and_ascii() {
        assert_eq!(
            tokens("部署 deploy-123 流水线"),
            vec!["部", "署", "deploy", "123", "流", "水", "线"]
        );
        assert_eq!(
            overlap_score(&tokens("deploy"), &tokens("deployment deploy")),
            1
        );
    }
}
