//! Dream consolidation integration tests (offline, deterministic).
//!
//! Covers: no-op when nothing exists, cluster merge (delete old + write new),
//! generic same-project cluster rejection, invalid member ids tolerated,
//! conservative project meta calibration and the user profile rewrite/keep
//! semantics.

#[path = "recall_support.rs"]
mod support;

use pd_memory_core::dream::{self, schemas};
use pd_memory_core::ports::{MemoryStore, NoopTelemetry, SystemClock};
use pd_memory_core::types::*;
use pd_memory_testkit::ScriptedModelProvider;

use support::*;

fn run_dream(
    provider: &ScriptedModelProvider,
    store: &mut FakeStore,
    config: &pd_memory_core::config::MemoryConfig,
) -> DreamRunResult {
    dream::run_dream(
        config,
        provider,
        &SystemClock,
        &NoopTelemetry,
        store,
        DreamTrigger::Manual,
    )
    .expect("dream should succeed")
}

fn request_schemas(provider: &ScriptedModelProvider) -> Vec<String> {
    provider
        .log
        .lock()
        .expect("log")
        .iter()
        .map(|request| request.schema_name.clone())
        .collect()
}

#[test]
fn dream_is_a_no_op_without_memory_files() {
    let config = config();
    let mut store = FakeStore::new(&config);
    let provider = ScriptedModelProvider::new();

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert!(result.is_no_op);
    assert_eq!(result.reviewed_files, 0);
    assert_eq!(result.rewritten_projects, 0);
    assert_eq!(result.deleted_files, 0);
    assert!(!result.profile_updated);
    assert!(result.summary.contains("nothing to organize"));
    assert_eq!(provider.request_count(), 0);
    assert!(!result.started_at.is_empty());
    assert!(!result.finished_at.is_empty());
}

#[test]
fn dream_merges_a_cluster_and_deletes_the_old_files() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        project_entry(
            "Project/boreal-a.md",
            "Boreal",
            "项目定义",
            "2026-10-18T09:00:00.000Z",
        ),
        "## Summary\nBoreal 是一个本地知识库整理工具。\n",
    );
    store.add(
        project_entry(
            "Project/boreal-b.md",
            "Boreal 阶段",
            "当前阶段",
            "2026-10-18T10:00:00.000Z",
        ),
        "## Current Stage\n设计阶段。\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::CLUSTER),
        r#"{"summary":"plan","clusters":[{"member_relative_paths":["Project/boreal-a.md","Project/boreal-b.md"],"reason":"duplicate facts about the same project definition overlap"}]}"#,
    );
    provider.push(
        Some(schemas::MERGE),
        r###"{"summary":"merged","name":"boreal-state","description":"合并后的项目状态","markdown":"## Summary\nBoreal 是本地知识库整理工具。\n\n## Current Stage\n设计阶段。\n"}"###,
    );
    provider.push(
        Some(schemas::PROJECT_META),
        r#"{"should_update":false,"reason":"keep","project_name":"Boreal","description":"本地知识库整理工具","status":"in_progress"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert_eq!(result.rewritten_projects, 1);
    assert_eq!(result.deleted_files, 2);
    assert_eq!(result.duplicate_topic_count, 1);
    assert_eq!(result.conflict_topic_count, 0);
    assert_eq!(result.reviewed_files, 2);
    assert!(result.summary.contains("Refined 1 dream clusters."));
    assert!(result.summary.contains("Deleted 2 absorbed source files."));
    assert!(!store.contains("Project/boreal-a.md"));
    assert!(!store.contains("Project/boreal-b.md"));
    let merged: Vec<MemoryManifestEntry> = store
        .entries
        .iter()
        .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
        .cloned()
        .collect();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].frontmatter.name, "boreal-state");
    let merged_id = merged[0].relative_path.clone();
    let record = store
        .get_records(&[merged_id], 200)
        .expect("records")
        .into_iter()
        .next()
        .expect("merged record");
    assert!(record.content.contains("Boreal 是本地知识库整理工具。"));
    assert_eq!(
        request_schemas(&provider),
        vec![
            schemas::CLUSTER.to_string(),
            schemas::MERGE.to_string(),
            schemas::PROJECT_META.to_string()
        ]
    );
}

#[test]
fn dream_drops_generic_same_project_clusters() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        project_entry("Project/a.md", "A", "first", "2026-10-18T09:00:00.000Z"),
        "## Summary\nfirst\n",
    );
    store.add(
        project_entry("Project/b.md", "B", "second", "2026-10-18T10:00:00.000Z"),
        "## Summary\nsecond\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::CLUSTER),
        r#"{"summary":"plan","clusters":[{"member_relative_paths":["Project/a.md","Project/b.md"],"reason":"both files belong to the same project"}]}"#,
    );
    provider.push(
        Some(schemas::PROJECT_META),
        r#"{"should_update":false,"reason":"keep"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert!(result.is_no_op);
    assert_eq!(result.duplicate_topic_count, 0);
    assert_eq!(result.conflict_topic_count, 1);
    assert_eq!(result.rewritten_projects, 0);
    assert!(store.contains("Project/a.md"));
    assert!(store.contains("Project/b.md"));
    assert!(
        !request_schemas(&provider).contains(&schemas::MERGE.to_string()),
        "a generic cluster must not be merged"
    );
}

#[test]
fn dream_ignores_member_ids_outside_the_header_scan() {
    let config = config();
    let mut store = FakeStore::new(&config);
    for (path, updated_at) in [
        ("Project/a.md", "2026-10-18T09:00:00.000Z"),
        ("Project/b.md", "2026-10-18T10:00:00.000Z"),
        ("Project/c.md", "2026-10-18T11:00:00.000Z"),
    ] {
        let name = path.rsplit('/').next().expect("name");
        store.add(
            project_entry(path, name, "note", updated_at),
            "## Summary\nnote\n",
        );
    }
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::CLUSTER),
        r#"{"summary":"plan","clusters":[{"member_relative_paths":["Project/a.md","Project/missing.md"],"reason":"duplicate fact overlap"},{"member_relative_paths":["Project/b.md","Project/c.md"],"reason":"overlapping project state facts"}]}"#,
    );
    provider.push(
        Some(schemas::MERGE),
        r###"{"summary":"merged","name":"merged-bc","description":"merged","markdown":"## Summary\nmerged b and c\n"}"###,
    );
    provider.push(
        Some(schemas::PROJECT_META),
        r#"{"should_update":false,"reason":"keep"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert_eq!(result.rewritten_projects, 1);
    assert_eq!(result.deleted_files, 2);
    assert_eq!(result.conflict_topic_count, 1);
    // The invalid first cluster is dropped; a.md stays untouched.
    assert!(store.contains("Project/a.md"));
    assert!(!store.contains("Project/b.md"));
    assert!(!store.contains("Project/c.md"));
    assert_eq!(
        request_schemas(&provider),
        vec![
            schemas::CLUSTER.to_string(),
            schemas::MERGE.to_string(),
            schemas::PROJECT_META.to_string()
        ]
    );
}

#[test]
fn dream_calibrates_project_meta_conservatively() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        project_entry(
            "Project/boreal.md",
            "Boreal",
            "本地知识库整理工具",
            "2026-10-18T10:00:00.000Z",
        ),
        "## Current Stage\n设计阶段。\n",
    );
    store.project_meta = Some(project_meta("2026-10-18T09:00:00.000Z"));
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::PROJECT_META),
        r#"{"should_update":true,"reason":"the note supersedes the stale stage","project_name":"Boreal 2","description":"本地知识库整理工具（设计阶段）","status":"active"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    let meta = store.project_meta.clone().expect("project meta");
    assert_eq!(meta.project_name, "Boreal 2");
    assert_eq!(meta.description, "本地知识库整理工具（设计阶段）");
    assert_eq!(meta.status, "active");
    assert!(meta.dream_updated_at.is_some());
    assert_eq!(
        request_schemas(&provider),
        vec![schemas::PROJECT_META.to_string()]
    );
}

#[test]
fn dream_rewrites_profile_and_absorbs_user_notes_when_changed() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        user_entry(
            "UserIdentityNotes/n1-0001.md",
            "身份背景：算法工程师",
            "用户是算法工程师",
            "2026-10-18T09:00:00.000Z",
        ),
        "## 身份背景\n- 算法工程师，长期做推荐系统\n",
    );
    store.add(
        user_entry(
            "UserIdentityNotes/n2-0002.md",
            "身份背景：家庭",
            "用户有一个女儿",
            "2026-10-18T10:00:00.000Z",
        ),
        "## 身份背景\n- 有一个上小学的女儿\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::USER_PROFILE),
        r#"{"identity_background_markdown":"- 算法工程师，长期做推荐系统\n- 有一个上小学的女儿"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert!(result.profile_updated);
    assert_eq!(result.deleted_files, 2);
    assert_eq!(result.rewritten_projects, 0);
    assert!(!store.contains("UserIdentityNotes/n1-0001.md"));
    assert!(!store.contains("UserIdentityNotes/n2-0002.md"));
    let profile = store
        .entries
        .iter()
        .find(|entry| {
            entry
                .relative_path
                .ends_with("UserIdentity/user-profile.md")
        })
        .expect("user profile written");
    assert_eq!(profile.frontmatter.kind, MemoryRecordType::User);
    assert_eq!(profile.frontmatter.scope, MemoryScope::Global);
    assert_eq!(profile.frontmatter.name, "user-profile");
    let record = store
        .get_records(&["UserIdentity/user-profile.md".to_string()], 200)
        .expect("record")
        .into_iter()
        .next()
        .expect("profile record");
    assert!(record.content.starts_with("## 身份背景\n- 算法工程师"));
    assert_eq!(
        request_schemas(&provider),
        vec![schemas::USER_PROFILE.to_string()]
    );
}

#[test]
fn dream_keeps_user_notes_when_the_profile_did_not_change() {
    let config = config();
    let mut store = FakeStore::new(&config);
    store.add(
        user_entry(
            "UserIdentity/user-profile.md",
            "user-profile",
            "算法工程师",
            "2026-10-19T00:05:00.000Z",
        ),
        "## 身份背景\n- 算法工程师\n",
    );
    store.add(
        user_entry(
            "UserIdentityNotes/n1-0001.md",
            "身份背景：算法工程师",
            "用户是算法工程师",
            "2026-10-18T09:00:00.000Z",
        ),
        "## 身份背景\n- 算法工程师\n",
    );
    let provider = ScriptedModelProvider::new();
    provider.push(
        Some(schemas::USER_PROFILE),
        r#"{"identity_background_markdown":"- 算法工程师"}"#,
    );

    let result = run_dream(&provider, &mut store, &config);

    assert_eq!(result.status, DreamPipelineStatus::Success);
    assert!(result.is_no_op);
    assert!(!result.profile_updated);
    assert_eq!(result.deleted_files, 0);
    assert!(store.contains("UserIdentityNotes/n1-0001.md"));
    let profiles: Vec<&MemoryManifestEntry> = store
        .entries
        .iter()
        .filter(|entry| entry.relative_path == "UserIdentity/user-profile.md")
        .collect();
    assert_eq!(profiles.len(), 1, "profile must not be duplicated");
    assert_eq!(
        profile_body(&store),
        "## 身份背景\n- 算法工程师",
        "existing profile body must be preserved"
    );
}

fn profile_body(store: &FakeStore) -> String {
    store
        .get_records(&["UserIdentity/user-profile.md".to_string()], 200)
        .expect("record")
        .into_iter()
        .next()
        .map(|record| record.content.trim().to_string())
        .unwrap_or_default()
}
