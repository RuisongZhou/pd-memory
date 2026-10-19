//! Recall pipeline integration tests (offline, deterministic).
//!
//! Covers: gate `none`, gate + model selection + render, cache hits (zero model
//! calls), recency fallback when selection fails, gated explicit search,
//! TTL-expired cache, user-route identity and system-context rendering.

#[path = "recall_support.rs"]
mod support;

use std::sync::Mutex;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::ports::SystemClock;
use pd_memory_core::recall::{self, RecallCache, RetrieveOptions};
use pd_memory_core::types::*;
use pd_memory_testkit::ScriptedModelProvider;

use support::*;

const PROJECT_PATH: &str = "Project/boreal-1a2b3c4d5e.md";
const FEEDBACK_PATH: &str = "Feedback/delivery-rule-9f8e7d6c5b.md";
const UPDATED_AT: &str = "2026-10-18T09:13:02.114Z";

fn fixture_store(config: &MemoryConfig) -> FakeStore {
    let mut store = FakeStore::new(config);
    store.add(
        project_entry(
            PROJECT_PATH,
            "Boreal",
            "本地知识库整理工具；当前处于设计阶段",
            UPDATED_AT,
        ),
        "## Summary\n\nBoreal 是一个本地知识库整理工具，目标是先把 Markdown 导入链路跑通。\n\n## Current Stage\n\n设计阶段\n",
    );
    store.add(
        feedback_entry(
            FEEDBACK_PATH,
            "delivery-rule",
            "交付时先给 3 个标题，再给正文",
            "2026-10-18T09:20:00.000Z",
        ),
        "## Rule\n每次交付先给 3 个标题，再给正文，再给封面文案。\n",
    );
    store.project_meta = Some(project_meta(UPDATED_AT));
    store
}

fn run_retrieve(
    provider: &ScriptedModelProvider,
    store: &FakeStore,
    config: &MemoryConfig,
    cache: &Mutex<RecallCache>,
    query: &str,
    options: RetrieveOptions,
) -> RetrievalResult {
    recall::retrieve(
        config,
        store,
        provider,
        &SystemClock,
        cache,
        query,
        &options,
        None,
    )
    .expect("retrieve should succeed")
}

fn default_options() -> RetrieveOptions {
    RetrieveOptions {
        recent_messages: vec![MemoryMessage::user("Boreal 现在到哪个阶段了？")],
        workspace_hint: None,
        explicit: false,
    }
}

#[test]
fn gate_none_returns_empty_context_without_selection() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"none"}"#);
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "今天天气怎么样？",
        default_options(),
    );

    assert_eq!(result.route, MemoryRoute::None);
    assert_eq!(result.context, None);
    assert_eq!(result.system_context, None);
    assert!(result.selected_ids.is_empty());
    assert_eq!(result.intent.as_deref(), Some("none"));
    assert!(!result.cache_hit);
    assert_eq!(provider.request_count(), 1, "only the gate may be called");
}

#[test]
fn gate_then_selection_renders_the_recall_block() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    provider.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{PROJECT_PATH}"]}}"#),
    );
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "Boreal 现在到哪个阶段了？",
        default_options(),
    );

    assert_eq!(result.route, MemoryRoute::Project);
    assert_eq!(result.selected_ids, vec![PROJECT_PATH.to_string()]);
    assert_eq!(provider.request_count(), 2, "gate + selection");
    let context = result.context.expect("context");
    assert!(
        context.starts_with("## ClawXMemory Recall\nroute=project"),
        "{context}"
    );
    assert!(
        context.contains("### [project_meta] project.meta.md (2026-10-18T09:13:02.114Z)"),
        "{context}"
    );
    assert!(context.contains("## Project Name\nBoreal"), "{context}");
    assert!(
        context.contains("### [project] Project/boreal-1a2b3c4d5e.md (2026-10-18T09:13:02.114Z)"),
        "{context}"
    );
    assert!(
        context.contains("Boreal 是一个本地知识库整理工具"),
        "{context}"
    );
    assert!(
        context.ends_with(
            "Treat these file memories as the authoritative long-term memory for this turn when relevant."
        ),
        "{context}"
    );
    // Selection details are recorded in the trace.
    let trace = &result.trace;
    assert_eq!(trace["route"], "project");
    assert!(!trace["steps"].as_array().expect("steps").is_empty());

    let system = recall::build_memory_recall_system_context(&context).expect("system context");
    assert!(system.contains("## Project Meta\n\n## Project Name\nBoreal"));
    assert!(system.contains("## Project Memory"));
    assert!(!system.contains("### [project]"));
}

#[test]
fn cache_hit_performs_zero_model_calls() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    provider.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{PROJECT_PATH}"]}}"#),
    );
    let cache = Mutex::new(RecallCache::new());
    let options = default_options();

    let first = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "Boreal 现在到哪个阶段了？",
        options.clone(),
    );
    assert!(!first.cache_hit);
    assert_eq!(provider.request_count(), 2);

    let second = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "Boreal 现在到哪个阶段了？",
        options,
    );
    assert!(second.cache_hit, "second call must be served from cache");
    assert_eq!(
        provider.request_count(),
        2,
        "cache hit must not call the model"
    );
    assert_eq!(second.context, first.context);
    assert_eq!(second.route, first.route);
}

#[test]
fn cache_expiry_ttl_zero_forces_a_fresh_retrieval() {
    let mut config = config();
    config.retrieval.cache_ttl_ms = 0;
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    for _ in 0..2 {
        provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
        provider.push(
            Some("memory_select_files"),
            format!(r#"{{"selected_ids":["{PROJECT_PATH}"]}}"#),
        );
    }
    let cache = Mutex::new(RecallCache::new());
    let options = default_options();

    let first = run_retrieve(&provider, &store, &config, &cache, "阶段", options.clone());
    let second = run_retrieve(&provider, &store, &config, &cache, "阶段", options);
    assert!(!first.cache_hit);
    assert!(!second.cache_hit);
    assert_eq!(provider.request_count(), 4);
}

#[test]
fn full_manifest_reaches_selection_and_fallback_uses_recency() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        project_entry(
            "Project/alpha.md",
            "Alpha",
            "推荐系统",
            "2026-10-18T09:00:00.000Z",
        ),
        "## Summary\nBM25 recall tuning for the recommendation pipeline.\n",
    );
    store.add(
        project_entry(
            "Project/beta.md",
            "Beta",
            "做饭",
            "2026-10-19T09:00:00.000Z",
        ),
        "## Summary\nUnrelated cooking notes.\n",
    );
    let provider = ScriptedModelProvider::new();
    // Both notes reach selection; a failure falls back to newest-first order.
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "BM25 recall tuning",
        default_options(),
    );

    assert_eq!(result.route, MemoryRoute::Project);
    assert_eq!(
        result.selected_ids.first().map(String::as_str),
        Some("Project/beta.md"),
        "fallback must preserve newest-first manifest order: {:?}",
        result.selected_ids
    );
    let context = result.context.expect("context");
    assert!(context.contains("BM25 recall tuning"), "{context}");
    let requests = provider.log.lock().expect("requests");
    let selection: serde_json::Value =
        serde_json::from_str(&requests[1].user).expect("selection prompt");
    assert_eq!(selection["manifest"].as_array().expect("manifest").len(), 2);
}

#[test]
fn explicit_search_uses_the_gate() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    provider.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{PROJECT_PATH}"]}}"#),
    );
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "Boreal",
        RetrieveOptions {
            recent_messages: Vec::new(),
            workspace_hint: None,
            explicit: true,
        },
    );

    assert_eq!(result.route, MemoryRoute::Project);
    let requests = provider.log.lock().expect("log");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].schema_name, "memory_gate",
        "explicit search must call the gate"
    );
    drop(requests);
    assert!(result.context.is_some());
}

#[test]
fn invalid_gate_output_degrades_to_none() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), "not json at all");
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "阶段",
        default_options(),
    );

    assert_eq!(result.route, MemoryRoute::None);
    assert_eq!(result.context, None);
    assert!(provider.request_count() >= 1);
}

#[test]
fn user_route_renders_identity_background_only() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        user_entry(
            "global/UserIdentity/user-profile.md",
            "user-profile",
            "算法工程师",
            "2026-10-19T00:05:00.000Z",
        ),
        "## 身份背景\n- 算法工程师，长期做推荐系统\n- 有一个上小学的女儿\n",
    );
    store.add(
        user_entry(
            "UserIdentityNotes/note-0001.md",
            "身份背景：算法工程师",
            "用户是算法工程师",
            "2026-10-18T09:30:00.000Z",
        ),
        "## 身份背景\n- 用户是算法工程师，长期做推荐系统。\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"user"}"#);
    provider.push(
        Some("memory_select_files"),
        r#"{"selected_ids":["UserIdentityNotes/note-0001.md"]}"#,
    );
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "我是做什么工作的？",
        default_options(),
    );

    assert_eq!(result.route, MemoryRoute::User);
    let context = result.context.expect("context");
    assert!(
        context
            .contains("### [user] global/UserIdentity/user-profile.md (2026-10-19T00:05:00.000Z)"),
        "{context}"
    );
    assert!(context.contains("## 身份背景"), "{context}");
    assert!(
        context.contains("- 算法工程师，长期做推荐系统"),
        "{context}"
    );
    // TS parity: the `user` route renders the compact identity-background block
    // only; selected user notes are read for traceability but not injected.
    assert!(
        !context.contains("### [user] UserIdentityNotes/note-0001.md"),
        "{context}"
    );
    assert_eq!(
        result.selected_ids,
        vec!["UserIdentityNotes/note-0001.md".to_string()]
    );
}

#[test]
fn fallback_uses_updated_at_desc_order() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        project_entry(
            "Project/older.md",
            "Older",
            "first",
            "2026-10-17T09:00:00.000Z",
        ),
        "## Summary\nolder\n",
    );
    store.add(
        project_entry(
            "Project/newer.md",
            "Newer",
            "second",
            "2026-10-19T09:00:00.000Z",
        ),
        "## Summary\nnewer\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    let cache = Mutex::new(RecallCache::new());

    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "unmatched query",
        default_options(),
    );
    assert_eq!(
        result.selected_ids,
        vec![
            "Project/newer.md".to_string(),
            "Project/older.md".to_string()
        ],
        "fallback must follow updated_at desc"
    );
}

#[test]
fn system_context_groups_all_kinds_and_preserves_body_order() {
    let raw = "## ClawXMemory Recall\nroute=mix\n\n### [feedback] Feedback/a.md (t)\n## Rule\n中文\n\n### [user] global/UserIdentity/user-profile.md (t)\n## 身份背景\n- 工程师\n\n### [project_meta] project.meta.md (t)\nBoreal\n\n### [project] Project/a.md (t)\nFirst\n### [project] Project/b.md (t)\nSecond\nTreat these file memories as the authoritative long-term memory for this turn when relevant.";
    let expected = [
        "## ClawXMemory Recall",
        "These are retrieved long-term memory references for the current turn.",
        "Some content may be relevant while some may not be directly useful.",
        "Use only the parts that are relevant to the current question.",
        "If retrieved memory conflicts with explicit new user instructions in the current turn, follow the current-turn user instructions.",
        "## User Profile\n\n## 身份背景\n- 工程师",
        "## Project Meta\n\nBoreal",
        "## Project Memory\n\nFirst\n\n---\n\nSecond",
        "## Feedback Memory\n\n## Rule\n中文",
    ].join("\n\n");
    assert_eq!(
        recall::build_memory_recall_system_context(raw),
        Some(expected)
    );
    for empty in ["", "  \n\t", "## ClawXMemory Recall\nroute=none"] {
        assert_eq!(recall::build_memory_recall_system_context(empty), None);
    }
}

#[test]
fn explicit_search_respects_none_from_gate() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    provider.push(Some("memory_gate"), r#"{"route":"none"}"#);
    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &Mutex::new(RecallCache::new()),
        "unrelated",
        RetrieveOptions {
            explicit: true,
            ..Default::default()
        },
    );
    assert_eq!(result.route, MemoryRoute::None);
    assert!(result.context.is_none());
    assert_eq!(provider.request_count(), 1);
}

#[test]
fn cache_ttl_starts_after_model_completion_and_expires_at_the_boundary() {
    use chrono::{DateTime, Duration, Utc};
    use pd_memory_core::error::Result;
    use pd_memory_core::ports::{Clock, CompletionRequest, CompletionResponse, ModelProvider};
    use std::sync::Arc;
    struct TestClock(Mutex<DateTime<Utc>>);
    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }
    struct SlowModel {
        clock: Arc<TestClock>,
        scripted: ScriptedModelProvider,
    }
    impl ModelProvider for SlowModel {
        fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            *self.clock.0.lock().unwrap() += Duration::seconds(45);
            self.scripted.complete(request)
        }
    }
    let clock = Arc::new(TestClock(Mutex::new(
        "2030-01-01T00:00:00.000Z".parse().unwrap(),
    )));
    let model = SlowModel {
        clock: clock.clone(),
        scripted: ScriptedModelProvider::new(),
    };
    model
        .scripted
        .push(Some("memory_gate"), r#"{"route":"project"}"#);
    model.scripted.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{PROJECT_PATH}"]}}"#),
    );
    model
        .scripted
        .push(Some("memory_gate"), r#"{"route":"none"}"#);
    let config = config();
    let store = fixture_store(&config);
    let cache = Mutex::new(RecallCache::new());
    let retrieve = || {
        recall::retrieve(
            &config,
            &store,
            &model,
            clock.as_ref(),
            &cache,
            "Boreal",
            &RetrieveOptions::default(),
            None,
        )
        .unwrap()
    };
    assert!(!retrieve().cache_hit);
    *clock.0.lock().unwrap() += Duration::milliseconds(29_999);
    assert!(retrieve().cache_hit);
    assert_eq!(model.scripted.request_count(), 2);
    *clock.0.lock().unwrap() += Duration::milliseconds(1);
    assert!(!retrieve().cache_hit);
    assert_eq!(model.scripted.request_count(), 3);
}

#[test]
fn cache_does_not_evict_a_live_result_after_sixty_four_other_queries() {
    let config = config();
    let store = fixture_store(&config);
    let provider = ScriptedModelProvider::new();
    let cache = Mutex::new(RecallCache::new());
    for index in 0..65 {
        provider.push(Some("memory_gate"), r#"{"route":"none"}"#);
        let result = run_retrieve(
            &provider,
            &store,
            &config,
            &cache,
            &format!("query {index}"),
            RetrieveOptions::default(),
        );
        assert!(!result.cache_hit);
    }
    let result = run_retrieve(
        &provider,
        &store,
        &config,
        &cache,
        "query 0",
        RetrieveOptions::default(),
    );
    assert!(result.cache_hit);
    assert_eq!(provider.request_count(), 65);
}
