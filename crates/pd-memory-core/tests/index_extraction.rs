//! Extraction parity tests (offline, scripted model provider).
//!
//! Covers the `complete_json` single-response parse contract, the classify and
//! create-note schemas, the ±2-turn prompt window and the candidate mapping.

use std::sync::Arc;
use std::time::Duration;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::error::MemoryError;
use pd_memory_core::extraction::{
    complete_json, LlmExtractor, CLASSIFY_TIMEOUT_MS, CREATE_TIMEOUT_MS,
};
use pd_memory_core::types::{MemoryMessage, MemoryRecordType, MemoryScope, ProjectMetaRecord};
use pd_memory_testkit::ScriptedModelProvider;
use serde_json::json;

fn config() -> MemoryConfig {
    MemoryConfig::for_data_root(std::env::temp_dir().join("pd-memory-extraction-tests"))
}

fn message(role: &str, content: &str) -> MemoryMessage {
    MemoryMessage::new(role, content)
}

fn project_meta() -> ProjectMetaRecord {
    ProjectMetaRecord {
        project_id: "current_project".into(),
        project_name: "Boreal".into(),
        description: "local knowledge base tooling".into(),
        status: "in_progress".into(),
        created_at: "2026-10-18T09:00:00.000Z".into(),
        updated_at: "2026-10-18T09:00:00.000Z".into(),
        ..Default::default()
    }
}

fn classification_payload() -> String {
    json!({
        "should_store": true,
        "labels": [
            {"type": "project", "reason": "project fact", "evidence": "quote 1"},
            {"type": "project", "reason": "duplicate project label", "evidence": "quote 2"},
            {"type": "feedback", "reason": "delivery rule", "evidence": "quote 3"},
            {"type": "unknown", "reason": "ignored", "evidence": ""}
        ]
    })
    .to_string()
}

fn create_payload(kind: &str) -> String {
    json!({
        "skip": false,
        "reason": "",
        "name": format!("{kind} note"),
        "description": format!("{kind} description"),
        "markdown": format!("## Summary\n{kind} body"),
    })
    .to_string()
}

fn extractor(provider: &Arc<ScriptedModelProvider>) -> LlmExtractor {
    LlmExtractor::new(provider.clone())
}

// ---------------------------------------------------------------------------
// complete_json
// ---------------------------------------------------------------------------

#[test]
fn complete_json_passes_prompts_through_and_parses_fenced_output() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(None, "```json\n{\"ok\": true}\n```");
    let value = complete_json(
        provider.as_ref(),
        "test_schema",
        "system prompt",
        "user prompt",
        1_234,
        Some(512),
    )
    .expect("parsed");
    assert_eq!(value["ok"], json!(true));
    assert_eq!(provider.request_count(), 1);
    let log = provider.log.lock().unwrap();
    assert_eq!(log[0].schema_name, "test_schema");
    assert_eq!(log[0].system, "system prompt");
    assert_eq!(log[0].user, "user prompt");
    assert_eq!(log[0].timeout, Duration::from_millis(1_234));
    assert_eq!(log[0].max_output_tokens, Some(512));
}

#[test]
fn complete_json_does_not_request_repairs_for_invalid_output() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(None, "not JSON");
    provider.push(None, "{\"ok\":true}");
    let error =
        complete_json(provider.as_ref(), "test_schema", "sys", "usr", 100, None).unwrap_err();
    assert!(matches!(error, MemoryError::ModelOutput { .. }));
    assert_eq!(provider.request_count(), 1);
    assert!(
        complete_json(provider.as_ref(), "test_schema", "sys", "usr", 100, None).unwrap()["ok"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn complete_json_maps_provider_errors_to_model_output() {
    let provider = Arc::new(ScriptedModelProvider::new());
    let error = complete_json(provider.as_ref(), "test_schema", "sys", "usr", 100, None)
        .expect_err("provider has no scripted response");
    assert_eq!(error.code(), "model_output");
}

// ---------------------------------------------------------------------------
// classify
// ---------------------------------------------------------------------------

#[test]
fn classify_turn_parses_dedupes_and_uses_reference_prompt() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_classify"), classification_payload());
    let extractor = extractor(&provider);
    let focus_turn = vec![
        message("user", "这个项目叫 Boreal，先做本地知识库"),
        message("assistant", "好的"),
    ];
    let context = focus_turn.clone();
    let result = extractor
        .classify_turn(&config(), &focus_turn, &context, Some(&project_meta()))
        .expect("classified");
    assert!(result.should_store);
    assert_eq!(
        result.kinds(),
        vec![MemoryRecordType::Project, MemoryRecordType::Feedback]
    );
    assert_eq!(result.labels[0].reason, "project fact");
    assert_eq!(result.labels[0].evidence, "quote 1");

    let log = provider.log.lock().unwrap();
    assert_eq!(log[0].schema_name, "memory_classify");
    assert_eq!(log[0].timeout, Duration::from_millis(CLASSIFY_TIMEOUT_MS));
    assert!(log[0]
        .system
        .contains("Base the decision on the focus user turn first."));
    assert!(log[0].system.contains("Assistant text is context only."));
    let prompt: serde_json::Value = serde_json::from_str(&log[0].user).expect("user prompt json");
    assert_eq!(
        prompt["focus_user_turn"]["content"],
        json!("这个项目叫 Boreal，先做本地知识库")
    );
    assert_eq!(
        prompt["current_project_meta"]["project_name"],
        json!("Boreal")
    );
}

#[test]
fn classify_turn_requires_a_stored_label() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_classify"),
        json!({"should_store": true, "labels": []}).to_string(),
    );
    let focus_turn = vec![message("user", "hello")];
    let result = extractor(&provider)
        .classify_turn(&config(), &focus_turn, &focus_turn, None)
        .expect("classified");
    assert!(!result.should_store);
}

#[test]
fn classify_turn_without_user_message_is_invalid_input() {
    let provider = Arc::new(ScriptedModelProvider::new());
    let focus_turn = vec![message("assistant", "no user message")];
    let error = extractor(&provider)
        .classify_turn(&config(), &focus_turn, &focus_turn, None)
        .expect_err("invalid focus turn");
    assert_eq!(error.code(), "invalid_input");
}

#[test]
fn classify_prompt_window_contains_two_neighbour_turns_each_side() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_classify"),
        json!({"should_store": false, "labels": []}).to_string(),
    );
    let mut context = Vec::new();
    for index in 0..5 {
        context.push(message("user", &format!("u{index}")));
        context.push(message("assistant", &format!("a{index}")));
    }
    let focus_turn = vec![context[4].clone(), context[5].clone()]; // u2 / a2
    extractor(&provider)
        .classify_turn(&config(), &focus_turn, &context, None)
        .expect("classified");
    let log = provider.log.lock().unwrap();
    let prompt: serde_json::Value = serde_json::from_str(&log[0].user).expect("user prompt json");
    assert_eq!(prompt["previous_turns"].as_array().map(Vec::len), Some(2));
    assert_eq!(prompt["next_turns"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        prompt["previous_turns"][0]["messages"][0]["content"],
        json!("u0")
    );
    assert_eq!(
        prompt["next_turns"][1]["messages"][0]["content"],
        json!("u4")
    );
    assert_eq!(
        prompt["focus_turn_with_neighbor_assistant_context"]["messages"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    // Reference key order (spec §9.1): the JSON object keys are emitted in the
    // same order as the TypeScript literal.
    let raw = &log[0].user;
    let positions: Vec<usize> = [
        "\"current_project_meta\"",
        "\"focus_user_turn\"",
        "\"focus_turn_with_neighbor_assistant_context\"",
        "\"previous_turns\"",
        "\"next_turns\"",
    ]
    .iter()
    .map(|key| raw.find(key).expect("key present"))
    .collect();
    assert!(
        positions.windows(2).all(|window| window[0] < window[1]),
        "prompt window key order drifted: {positions:?}"
    );
}

#[test]
fn prompt_window_truncates_long_messages() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_classify"),
        json!({"should_store": false, "labels": []}).to_string(),
    );
    let long_content = "长".repeat(600);
    let focus_turn = vec![message("user", &long_content), message("assistant", "ok")];
    extractor(&provider)
        .classify_turn(&config(), &focus_turn, &focus_turn, None)
        .expect("classified");
    let log = provider.log.lock().unwrap();
    let prompt: serde_json::Value = serde_json::from_str(&log[0].user).expect("user prompt json");
    let focus_content = prompt["focus_user_turn"]["content"]
        .as_str()
        .expect("focus content");
    assert_eq!(focus_content.chars().count(), 400);
    let turn_content = prompt["focus_turn_with_neighbor_assistant_context"]["messages"][0]
        ["content"]
        .as_str()
        .expect("turn content");
    assert_eq!(turn_content.chars().count(), 320);
}

// ---------------------------------------------------------------------------
// create_note
// ---------------------------------------------------------------------------

#[test]
fn create_project_note_maps_payload_to_candidate() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_create_project"), create_payload("project"));
    let mut config = config();
    config.model.max_output_tokens = Some(2048);
    let focus_turn = vec![
        message("user", "Boreal 目前是设计阶段，先做本地知识库"),
        message("assistant", "收到"),
    ];
    let classification = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::Project,
        reason: "durable project fact".into(),
        evidence: "quote".into(),
    };
    let candidate = extractor(&provider)
        .create_note(
            &config,
            MemoryRecordType::Project,
            &classification,
            &focus_turn,
            &focus_turn,
            Some(&project_meta()),
        )
        .expect("created")
        .expect("candidate");
    assert_eq!(candidate.kind, MemoryRecordType::Project);
    assert_eq!(candidate.scope, MemoryScope::Project);
    assert_eq!(candidate.name, "project note");
    assert_eq!(candidate.description, "project description");
    assert_eq!(candidate.body, "## Summary\nproject body");
    // Origin identity is filled by indexing, not by extraction.
    assert!(candidate.captured_at.is_none());
    assert!(candidate.source_session_key.is_none());

    let log = provider.log.lock().unwrap();
    assert_eq!(log[0].schema_name, "memory_create_project");
    assert_eq!(log[0].timeout, Duration::from_millis(CREATE_TIMEOUT_MS));
    assert_eq!(log[0].max_output_tokens, Some(2048));
    assert!(log[0].system.contains("## Current Stage"));
    assert!(log[0].system.contains("## Constraints"));
    assert!(log[0]
        .system
        .contains("visible output language must follow the dominant user language"));
    let prompt: serde_json::Value = serde_json::from_str(&log[0].user).expect("user prompt json");
    assert_eq!(prompt["classification"]["type"], json!("project"));
    assert_eq!(prompt["classification"]["evidence"], json!("quote"));
    assert!(prompt["context"]["focus_user_turn"]["content"]
        .as_str()
        .expect("content")
        .contains("Boreal"));
    // Reference key order (spec §9.1): classification before context.
    let raw = &log[0].user;
    let classification_pos = raw.find("\"classification\"").expect("classification key");
    let context_pos = raw.find("\"context\"").expect("context key");
    assert!(
        classification_pos < context_pos,
        "create prompt key order drifted"
    );
    let type_pos = raw.find("\"type\"").expect("type key");
    let reason_pos = raw.find("\"reason\"").expect("reason key");
    let evidence_pos = raw.find("\"evidence\"").expect("evidence key");
    assert!(
        type_pos < reason_pos && reason_pos < evidence_pos,
        "classification key order drifted"
    );
}

#[test]
fn create_user_and_feedback_notes_use_kind_specific_prompts() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_create_user"), create_payload("user"));
    provider.push(Some("memory_create_feedback"), create_payload("feedback"));
    let extractor = extractor(&provider);
    let focus_turn = vec![
        message("user", "我是算法工程师，以后交付先给结论"),
        message("assistant", "好"),
    ];
    let user_label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::User,
        reason: "identity".into(),
        evidence: "我是算法工程师".into(),
    };
    let feedback_label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::Feedback,
        reason: "delivery".into(),
        evidence: "先给结论".into(),
    };
    let user = extractor
        .create_note(
            &config(),
            MemoryRecordType::User,
            &user_label,
            &focus_turn,
            &focus_turn,
            None,
        )
        .expect("user note")
        .expect("user candidate");
    assert_eq!(user.scope, MemoryScope::Global);
    let feedback = extractor
        .create_note(
            &config(),
            MemoryRecordType::Feedback,
            &feedback_label,
            &focus_turn,
            &focus_turn,
            None,
        )
        .expect("feedback note")
        .expect("feedback candidate");
    assert_eq!(feedback.scope, MemoryScope::Project);

    let log = provider.log.lock().unwrap();
    assert!(log[0].system.contains("append-only user memory note"));
    assert!(log[1].system.contains("## Rule"));
    assert!(log[1].system.contains("## How To Apply"));
}

#[test]
fn create_note_tolerates_mock_schema_without_skip_field() {
    // The shared mock provider / WorkBuddy harness returns exactly
    // {name, description, markdown} (no skip/reason).
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_create_project"),
        json!({"name": "Boreal", "description": "kb tool", "markdown": "## Summary\nbody"})
            .to_string(),
    );
    let focus_turn = vec![
        message("user", "project context"),
        message("assistant", "ok"),
    ];
    let label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::Project,
        reason: String::new(),
        evidence: String::new(),
    };
    let candidate = extractor(&provider)
        .create_note(
            &config(),
            MemoryRecordType::Project,
            &label,
            &focus_turn,
            &focus_turn,
            None,
        )
        .expect("created")
        .expect("candidate");
    assert_eq!(candidate.name, "Boreal");
}

#[test]
fn create_note_skips_on_explicit_skip_or_bad_payload() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_create_project"),
        json!({"skip": true, "name": "x", "description": "y", "markdown": "z"}).to_string(),
    );
    provider.push(
        Some("memory_create_project"),
        json!({"name": "only name"}).to_string(),
    );
    provider.push(
        Some("memory_create_project"),
        json!({"name": "project", "description": "d", "markdown": "m"}).to_string(),
    );
    let extractor = extractor(&provider);
    let focus_turn = vec![message("user", "context"), message("assistant", "ok")];
    let label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::Project,
        reason: String::new(),
        evidence: String::new(),
    };
    for _ in 0..3 {
        let candidate = extractor
            .create_note(
                &config(),
                MemoryRecordType::Project,
                &label,
                &focus_turn,
                &focus_turn,
                None,
            )
            .expect("request succeeds");
        assert!(candidate.is_none());
    }
}

#[test]
fn create_note_rejects_general_project_meta() {
    let provider = Arc::new(ScriptedModelProvider::new());
    let focus_turn = vec![message("user", "context")];
    let label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::GeneralProjectMeta,
        reason: String::new(),
        evidence: String::new(),
    };
    let error = extractor(&provider)
        .create_note(
            &config(),
            MemoryRecordType::GeneralProjectMeta,
            &label,
            &focus_turn,
            &focus_turn,
            None,
        )
        .expect_err("unsupported kind");
    assert_eq!(error.code(), "invalid_input");
}

#[test]
fn classification_and_create_failures_degrade_without_repair() {
    for reply in [None, Some("invalid JSON")] {
        let provider = Arc::new(ScriptedModelProvider::new());
        if let Some(reply) = reply {
            provider.push(None, reply);
        }
        let extractor = extractor(&provider);
        let turn = [MemoryMessage::user("Boreal 是本地知识库工具")];
        let classification = extractor
            .classify_turn(&config(), &turn, &turn, None)
            .unwrap();
        assert!(!classification.should_store);
        assert_eq!(provider.request_count(), 1);
        if let Some(reply) = reply {
            provider.push(None, reply);
        }
        let label = pd_memory_core::extraction::ClassificationLabel {
            kind: MemoryRecordType::Project,
            reason: String::new(),
            evidence: String::new(),
        };
        assert!(extractor
            .create_note(&config(), label.kind, &label, &turn, &turn, None)
            .unwrap()
            .is_none());
        assert_eq!(provider.request_count(), 2);
    }
}

#[test]
fn create_note_accepts_reference_loose_payload_without_an_extra_call() {
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        None,
        r###"{"name":"Boreal note","description":"Uses "local" data","markdown":"## Summary
Boreal keeps "local" data."}"###,
    );
    let label = pd_memory_core::extraction::ClassificationLabel {
        kind: MemoryRecordType::Project,
        reason: String::new(),
        evidence: String::new(),
    };
    let turn = [MemoryMessage::user("Boreal 用本地数据")];
    let note = extractor(&provider)
        .create_note(&config(), label.kind, &label, &turn, &turn, None)
        .unwrap()
        .unwrap();
    assert_eq!(note.description, "Uses \"local\" data");
    assert!(note.body.contains("Boreal keeps \"local\" data."));
    assert_eq!(provider.request_count(), 1);
}
