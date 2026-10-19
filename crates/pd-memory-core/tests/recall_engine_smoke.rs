//! Engine-level smoke test: recall and dream through the real
//! [`MemoryEngine`] + file-backed store (no network, scripted model).
//!
//! This complements the pure `recall.rs` / `dream.rs` tests: it exercises the
//! frozen engine signatures, the four-section system context, the staged dream
//! commit and the pipeline state written by the engine.

use std::sync::Arc;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::files::FileMemoryStore;
use pd_memory_core::ports::{MemoryStore, ModelProvider};
use pd_memory_core::recall::RetrieveOptions;
use pd_memory_core::types::*;
use pd_memory_testkit::ScriptedModelProvider;

fn engine_config(root: &std::path::Path) -> MemoryConfig {
    let mut config = MemoryConfig::for_data_root(root);
    config.workspace_id = Some("engine-smoke".into());
    config
}

fn seed_store(config: &MemoryConfig) -> FileMemoryStore {
    let layout = config.store_layout();
    let mut store = FileMemoryStore::new(layout.roots.clone());
    store.ensure_layout().expect("layout");
    store
}

fn candidate(
    kind: MemoryRecordType,
    scope: MemoryScope,
    name: &str,
    description: &str,
    body: &str,
) -> MemoryCandidate {
    MemoryCandidate {
        kind,
        scope,
        name: name.to_string(),
        description: description.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn engine(config: MemoryConfig, provider: &Arc<ScriptedModelProvider>) -> MemoryEngine {
    let model: Arc<dyn ModelProvider> = provider.clone();
    MemoryEngine::open(config, model).expect("engine")
}

#[test]
fn engine_recall_wraps_context_and_serves_the_cache() {
    let root = pd_memory_testkit::temp_data_root();
    let config = engine_config(root.path());
    let mut store = seed_store(&config);
    let project = store
        .upsert_candidate(&candidate(
            MemoryRecordType::Project,
            MemoryScope::Project,
            "Boreal",
            "本地知识库整理工具",
            "## Summary\nBoreal 是一个本地知识库整理工具。\n",
        ))
        .expect("project note");

    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    provider.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{}"]}}"#, project.relative_path),
    );
    let engine = engine(config, &provider);

    let result = engine
        .retrieve_context("Boreal 现在到哪个阶段了？", RetrieveOptions::default())
        .expect("retrieve");
    assert_eq!(result.route, MemoryRoute::Project);
    let raw = result.context.clone().expect("raw context");
    assert!(raw.contains("## ClawXMemory Recall"), "{raw}");
    assert!(raw.contains("Boreal 是一个本地知识库整理工具。"), "{raw}");
    let system_context = result.system_context.clone().expect("system context");
    assert!(
        system_context.starts_with("## ClawXMemory Recall\n\n"),
        "{system_context}"
    );
    assert!(system_context.contains("follow the current-turn user instructions."));
    assert_eq!(provider.request_count(), 2);

    // Second identical call is served from the engine cache: zero model calls.
    let cached = engine
        .retrieve_context("Boreal 现在到哪个阶段了？", RetrieveOptions::default())
        .expect("retrieve cached");
    assert!(cached.cache_hit);
    assert_eq!(provider.request_count(), 2);

    // Explicit search uses the same gate and selection stages.
    provider.push(Some("memory_gate"), r#"{"route":"project"}"#);
    provider.push(
        Some("memory_select_files"),
        format!(r#"{{"selected_ids":["{}"]}}"#, project.relative_path),
    );
    let search = engine.search("Boreal", &[]).expect("explicit search");
    assert_eq!(search.route, MemoryRoute::Project);
    assert!(search.system_context.is_some());
    assert_eq!(provider.request_count(), 4);
}

#[test]
fn engine_dream_stages_merges_and_updates_the_profile() {
    let root = pd_memory_testkit::temp_data_root();
    let config = engine_config(root.path());
    let mut store = seed_store(&config);
    let project_a = store
        .upsert_candidate(&candidate(
            MemoryRecordType::Project,
            MemoryScope::Project,
            "Boreal",
            "项目定义",
            "## Summary\nBoreal 是一个本地知识库整理工具。\n",
        ))
        .expect("project a");
    let project_b = store
        .upsert_candidate(&candidate(
            MemoryRecordType::Project,
            MemoryScope::Project,
            "Boreal 阶段",
            "当前阶段",
            "## Current Stage\n设计阶段。\n",
        ))
        .expect("project b");
    let note_a = store
        .upsert_candidate(&candidate(
            MemoryRecordType::User,
            MemoryScope::Global,
            "身份背景：算法工程师",
            "用户是算法工程师",
            "## 身份背景\n- 算法工程师，长期做推荐系统\n",
        ))
        .expect("note a");
    let note_b = store
        .upsert_candidate(&candidate(
            MemoryRecordType::User,
            MemoryScope::Global,
            "身份背景：家庭",
            "用户有一个女儿",
            "## 身份背景\n- 有一个上小学的女儿\n",
        ))
        .expect("note b");
    drop(store);

    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("dream_cluster"),
        format!(
            r#"{{"summary":"plan","clusters":[{{"member_relative_paths":["{}","{}"],"reason":"duplicate project definition facts overlap"}}]}}"#,
            project_a.relative_path, project_b.relative_path
        ),
    );
    provider.push(
        Some("dream_merge"),
        r###"{"summary":"merged","name":"boreal-state","description":"合并后的项目状态","markdown":"## Summary\nBoreal 是本地知识库整理工具。\n\n## Current Stage\n设计阶段。\n"}"###,
    );
    provider.push(
        Some("dream_project_meta"),
        r#"{"should_update":false,"reason":"keep"}"#,
    );
    provider.push(
        Some("dream_user_profile"),
        r#"{"identity_background_markdown":"- 算法工程师，长期做推荐系统\n- 有一个上小学的女儿"}"#,
    );
    let engine = engine(config, &provider);

    let result = engine.dream(DreamTrigger::Manual).expect("dream");

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert_eq!(result.rewritten_projects, 1);
    assert!(result.profile_updated);
    assert_eq!(result.deleted_files, 4);
    assert!(result.summary.contains("Refined 1 dream clusters."));

    let entries = engine
        .list(&MemoryListOptions {
            include_deprecated: true,
            ..Default::default()
        })
        .expect("list");
    let ids: Vec<String> = entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect();
    assert!(!ids.contains(&project_a.relative_path), "{ids:?}");
    assert!(!ids.contains(&project_b.relative_path), "{ids:?}");
    assert!(!ids.contains(&note_a.relative_path), "{ids:?}");
    assert!(!ids.contains(&note_b.relative_path), "{ids:?}");
    let merged: Vec<&MemoryManifestEntry> = entries
        .iter()
        .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
        .collect();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].frontmatter.name, "boreal-state");
    let profile: Vec<&MemoryManifestEntry> = entries
        .iter()
        .filter(|entry| {
            entry
                .relative_path
                .ends_with("UserIdentity/user-profile.md")
        })
        .collect();
    assert_eq!(profile.len(), 1, "{ids:?}");
    let summary = engine.get_user_summary().expect("user summary");
    assert!(
        summary
            .identity_background
            .iter()
            .any(|fact| fact.contains("算法工程师")),
        "{:?}",
        summary.identity_background
    );

    let overview = engine.overview().expect("overview");
    assert_eq!(
        overview.last_dream_status,
        Some(DreamPipelineStatus::Success)
    );
    assert!(overview.last_dream_at.is_some());
    let after = engine.export_bundle().expect("post-Dream bundle");
    let calls = provider.request_count();
    engine.rollback_last_dream().expect("undo");
    assert!(engine.state_get("lastDreamAt").expect("state").is_none());
    assert!(engine
        .state_get("lastDreamStatus")
        .expect("state")
        .is_none());
    assert!(engine
        .state_get("lastDreamSummary")
        .expect("state")
        .is_none());
    let restored = engine
        .list(&MemoryListOptions::default())
        .expect("original notes");
    assert_eq!(restored.len(), 4);
    engine.rollback_last_dream().expect("redo");
    let redone = engine.export_bundle().expect("redone bundle");
    assert_eq!(after.files, redone.files);
    let redone_overview = engine.overview().expect("redone overview");
    assert_eq!(redone_overview.last_dream_at, overview.last_dream_at);
    assert_eq!(
        redone_overview.last_dream_status,
        overview.last_dream_status
    );
    assert_eq!(
        redone_overview.last_dream_summary,
        overview.last_dream_summary
    );
    assert_eq!(
        provider.request_count(),
        calls,
        "rollback must not call the model"
    );
}

#[test]
fn no_op_dream_succeeds_without_creating_a_snapshot() {
    let root = pd_memory_testkit::temp_data_root();
    let provider = Arc::new(ScriptedModelProvider::new());
    let engine = engine(engine_config(root.path()), &provider);
    let result = engine.dream(DreamTrigger::Manual).expect("empty dream");
    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert!(result.is_no_op);
    assert!(engine.last_dream_snapshot().expect("snapshot").is_none());
    assert!(engine.rollback_last_dream().is_err());
    assert_eq!(provider.request_count(), 0);
}
