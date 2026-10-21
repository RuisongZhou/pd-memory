//! FileMemoryStore behavior tests (task-1, dev A).
//!
//! Covers: naming/slug/hash, sameOrigin merge, manifest rebuild, list/get
//! truncation, project meta, user notes/profile, edit/delete/deprecate/clear,
//! export/import, path-traversal rejection, stage commit/rollback.

use std::collections::BTreeMap;

use pd_memory_core::error::MemoryError;
use pd_memory_core::files::FileMemoryStore;
use pd_memory_core::ports::{MemoryStore, StoreRoots};
use pd_memory_core::types::*;

fn make_store() -> (tempfile::TempDir, FileMemoryStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace_dir = dir.path().join("workspaces").join("ws");
    let roots = StoreRoots {
        memory_dir: workspace_dir.join("memory"),
        global_dir: dir.path().join("global"),
        workspace_dir,
    };
    let mut store = FileMemoryStore::new(roots);
    store.ensure_layout().expect("layout");
    (dir, store)
}

fn project_candidate(name: &str, description: &str) -> MemoryCandidate {
    MemoryCandidate {
        kind: MemoryRecordType::Project,
        scope: MemoryScope::Project,
        name: name.to_string(),
        description: description.to_string(),
        body: format!("## Summary\n\n{description}\n\n## Current Stage\n\n设计阶段"),
        captured_at: Some("2026-10-18T09:12:33.412Z".to_string()),
        source_session_key: Some("sess-7f3a".to_string()),
        ..Default::default()
    }
}

fn user_note_candidate(name: &str, description: &str) -> MemoryCandidate {
    MemoryCandidate {
        kind: MemoryRecordType::User,
        scope: MemoryScope::Global,
        name: name.to_string(),
        description: description.to_string(),
        body: "## 身份背景\n\n- 用户是算法工程师".to_string(),
        captured_at: Some("2026-10-18T09:29:58.000Z".to_string()),
        source_session_key: Some("sess-7f3a".to_string()),
        ..Default::default()
    }
}

fn hex10_suffix(entry: &MemoryManifestEntry, prefix: &str) -> String {
    let relative = entry.relative_path.as_str();
    let rest = relative
        .strip_prefix(prefix)
        .unwrap_or_else(|| panic!("unexpected path {relative}"))
        .strip_suffix(".md")
        .expect("md suffix");
    rest.to_string()
}

fn snapshot_files(store: &FileMemoryStore) -> BTreeMap<String, String> {
    store
        .export_bundle()
        .expect("export")
        .files
        .into_iter()
        .map(|file| (file.relative_path, file.content))
        .collect()
}

#[test]
fn upsert_names_project_files_and_builds_manifest() {
    let (_dir, mut store) = make_store();
    let entry = store
        .upsert_candidate(&project_candidate("Boreal", "本地知识库整理工具"))
        .expect("upsert");
    assert!(entry.relative_path.starts_with("Project/boreal-"));
    let suffix = hex10_suffix(&entry, "Project/boreal-");
    assert_eq!(suffix.len(), 10, "hash10 is 10 hex chars: {suffix}");
    assert!(suffix
        .chars()
        .all(|character| character.is_ascii_hexdigit()));
    assert_eq!(entry.frontmatter.kind, MemoryRecordType::Project);
    assert_eq!(
        entry.frontmatter.project_id.as_deref(),
        Some("current_project")
    );
    assert_eq!(
        entry.frontmatter.captured_at.as_deref(),
        Some("2026-10-18T09:12:33.412Z")
    );

    // Project metadata is auto-seeded from the first project candidate.
    let meta = store.get_project_meta().expect("meta").expect("present");
    assert_eq!(meta.project_name, "Boreal");
    assert_eq!(meta.description, "本地知识库整理工具");
    assert_eq!(meta.status, "in_progress");

    // Manifest exists, includes the project section, and lists the entry.
    let manifest = store.manifest_text().expect("manifest").expect("present");
    assert!(manifest.starts_with("# EdgeClaw Memory\n\nUpdated: "));
    assert!(manifest.contains("## Current Project Meta"));
    assert!(manifest.contains("## Project Memory"));
    assert!(manifest.contains(&format!("({})", entry.relative_path)));

    // list/get round-trip.
    let entries = store
        .list_entries(&MemoryListOptions {
            include_deprecated: false,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(entries.len(), 1);
    let records = store
        .get_records(std::slice::from_ref(&entry.relative_path), 80)
        .expect("records");
    assert_eq!(records.len(), 1);
    assert!(records[0].content.starts_with("## Summary"));
    assert!(records[0].preview.starts_with("Summary"));
}

#[test]
fn long_cjk_names_fit_the_file_name_limit() {
    // A CJK character is 3 bytes; Linux/Android file names stop at 255 bytes.
    let (_dir, mut store) = make_store();

    // 80 characters (the extraction cap): the slug fits as-is (254-byte file
    // name), but the atomic-write temp sibling must not exceed the limit.
    let at_cap = "长".repeat(80);
    let first = store
        .upsert_candidate(&project_candidate(&at_cap, "八十字名称"))
        .expect("upsert an 80-character CJK name");
    let prefix = format!("Project/{at_cap}-");
    assert_eq!(hex10_suffix(&first, &prefix).len(), 10);
    assert_eq!(first.frontmatter.name, at_cap);

    // 120 characters (the Dream cap): the 360-byte slug is cut on a character
    // boundary to 80 characters so the file name fits; the name is kept. A
    // distinct origin makes this a new file rather than a same-origin rewrite.
    let over_cap = "长".repeat(120);
    let mut candidate = project_candidate(&over_cap, "一百二十字名称");
    candidate.source_session_key = Some("sess-dream".to_string());
    let second = store
        .upsert_candidate(&candidate)
        .expect("upsert a 120-character CJK name");
    assert_ne!(second.relative_path, first.relative_path);
    let file_name = second.relative_path.rsplit('/').next().expect("file name");
    assert!(
        file_name.len() <= 255,
        "{} bytes: {file_name}",
        file_name.len()
    );
    assert_eq!(hex10_suffix(&second, &prefix).len(), 10);
    assert_eq!(second.frontmatter.name, over_cap);
    let raw = std::fs::read_to_string(&second.absolute_path).expect("read written file");
    assert!(raw.contains("一百二十字名称"), "{raw}");
    assert_eq!(
        store
            .list_entries(&MemoryListOptions {
                kinds: vec![MemoryRecordType::Project],
                ..Default::default()
            })
            .expect("list")
            .len(),
        2
    );
}

#[test]
fn same_origin_overwrites_the_same_file_without_merging_fields() {
    let (_dir, mut store) = make_store();
    let first = store
        .upsert_candidate(&project_candidate("Boreal", "第一版"))
        .expect("first");
    let mut renamed = project_candidate("Renamed", "第二版");
    renamed.body = "## Summary\n\n第二版正文".into();
    let second = store.upsert_candidate(&renamed).expect("second");
    assert_eq!(
        first.relative_path, second.relative_path,
        "sameOrigin must reuse the existing path"
    );
    assert_eq!(second.frontmatter.name, "Renamed");
    let entries = store
        .list_entries(&MemoryListOptions::default())
        .expect("list");
    assert_eq!(entries.len(), 1, "no duplicate file");
    let records = store
        .get_records(std::slice::from_ref(&first.relative_path), 80)
        .expect("records");
    assert_eq!(records[0].content, "## Summary\n\n第二版正文");

    // A different origin (captured_at) creates a new file.
    let mut next_turn = project_candidate("Renamed", "第二版");
    next_turn.captured_at = Some("2026-10-18T10:00:00.000Z".into());
    let third = store.upsert_candidate(&next_turn).expect("third");
    assert_ne!(third.relative_path, first.relative_path);
    assert_eq!(
        store
            .list_entries(&MemoryListOptions::default())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn user_notes_are_append_only_and_profile_uses_the_trait_method() {
    let (_dir, mut store) = make_store();
    let note = store
        .upsert_candidate(&user_note_candidate(
            "身份背景：算法工程师",
            "用户是算法工程师",
        ))
        .expect("note");
    assert!(
        note.relative_path
            .starts_with("global/UserIdentityNotes/身份背景-算法工程师-"),
        "unexpected note path {}",
        note.relative_path
    );
    let suffix = hex10_suffix(&note, "global/UserIdentityNotes/身份背景-算法工程师-");
    assert_eq!(suffix.len(), 10);

    // User notes are visible in list entries and rendered into MEMORY.md with
    // the same filesystem-relative link as the reference manifest.
    let manifest = store.manifest_text().unwrap().unwrap();
    assert!(manifest.contains("## User Memory"));
    assert!(manifest.contains(&format!("](../../../{})", note.relative_path)));

    // Profile rewrite through the trait method.
    let profile_candidate = MemoryCandidate {
        kind: MemoryRecordType::User,
        scope: MemoryScope::Global,
        name: "user-profile".into(),
        description: "算法工程师".into(),
        body: "## 身份背景\n\n- 算法工程师\n- 有一个上小学的女儿".into(),
        captured_at: Some("2026-10-18T09:29:58.000Z".into()),
        source_session_key: Some("sess-7f3a".into()),
        ..Default::default()
    };
    let profile = store
        .upsert_user_profile(&profile_candidate)
        .expect("profile");
    assert_eq!(profile.relative_path, "global/UserIdentity/user-profile.md");
    // The physical file lives under global_dir WITHOUT the exposed prefix.
    assert!(store
        .roots()
        .global_dir
        .join("UserIdentity/user-profile.md")
        .exists());

    let summary = store.get_user_summary().expect("summary");
    assert_eq!(summary.identity_background.len(), 2);
    assert!(summary
        .identity_background
        .iter()
        .any(|fact| fact.contains("算法工程师")));
    assert!(summary.files.len() >= 2);
    assert_eq!(
        summary.files[0].entry.relative_path,
        "global/UserIdentity/user-profile.md"
    );
}

#[test]
fn get_records_truncates_and_previews() {
    let (_dir, mut store) = make_store();
    let mut candidate = project_candidate("Long", "long body");
    candidate.body = (1..=10)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let entry = store.upsert_candidate(&candidate).unwrap();
    let truncated = store
        .get_records(std::slice::from_ref(&entry.relative_path), 3)
        .unwrap();
    assert_eq!(truncated[0].content, "line 1\nline 2\nline 3\n...");
    let full = store
        .get_records(std::slice::from_ref(&entry.relative_path), 80)
        .unwrap();
    assert_eq!(full[0].content.lines().count(), 10);
    let none = store
        .get_records(std::slice::from_ref(&entry.relative_path), 0)
        .unwrap();
    assert!(none[0].content.is_empty());
    assert_eq!(
        full[0].preview,
        "line 1 line 2 line 3 line 4 line 5 line 6 line 7 line 8 line 9 line 10"
    );
}

#[test]
fn rejects_path_traversal() {
    let (_dir, mut store) = make_store();
    let bundle = MemoryExportBundle {
        format_version: MEMORY_EXPORT_FORMAT_VERSION.to_string(),
        files: vec![MemorySnapshotFileRecord {
            relative_path: "../../escape.md".into(),
            content: "---\ntype: project\nscope: project\n---\n".into(),
        }],
        ..Default::default()
    };
    match store.import_bundle(&bundle) {
        Err(MemoryError::PathEscape(_)) => {}
        other => panic!("expected PathEscape, got {other:?}"),
    }
    assert!(store
        .get_records(&["../../etc/passwd".to_string()], 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .delete_entries(&["../outside.md".to_string()])
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .set_deprecated(&["../outside.md".to_string()], true)
            .unwrap(),
        0
    );
}

#[test]
fn edit_deprecate_restore_delete() {
    let (_dir, mut store) = make_store();
    let entry = store
        .upsert_candidate(&project_candidate("Boreal", "first"))
        .unwrap();
    let edited = store
        .edit_entry(&entry.relative_path, "Boreal Renamed", "second", None)
        .expect("edit");
    assert_eq!(edited.frontmatter.name, "Boreal Renamed");
    assert_eq!(edited.frontmatter.description, "second");
    // Body is preserved when no fields are supplied.
    let record = store
        .get_records(std::slice::from_ref(&entry.relative_path), 80)
        .unwrap();
    assert!(record[0].content.contains("## Current Stage"));

    let fields = MemoryEntryEditFields {
        stage: Some("已上线".into()),
        decisions: vec!["只做本地".into()],
        ..Default::default()
    };
    store
        .edit_entry(
            &entry.relative_path,
            "Boreal Renamed",
            "second",
            Some(&fields),
        )
        .expect("edit fields");
    let record = store
        .get_records(std::slice::from_ref(&entry.relative_path), 80)
        .unwrap();
    assert!(record[0].content.contains("## Current Stage\n已上线"));
    assert!(record[0].content.contains("## Decisions\n- 只做本地"));

    assert_eq!(
        store
            .set_deprecated(std::slice::from_ref(&entry.relative_path), true)
            .unwrap(),
        1
    );
    assert!(store
        .list_entries(&MemoryListOptions::default())
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_entries(&MemoryListOptions {
                include_deprecated: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
    let manifest = store.manifest_text().unwrap().unwrap();
    assert!(manifest.contains("## Deprecated"));
    assert_eq!(
        store
            .set_deprecated(std::slice::from_ref(&entry.relative_path), false)
            .unwrap(),
        1
    );
    assert!(!store
        .manifest_text()
        .unwrap()
        .unwrap()
        .contains("## Deprecated"));

    assert_eq!(
        store
            .delete_entries(std::slice::from_ref(&entry.relative_path))
            .unwrap(),
        1
    );
    assert!(store
        .list_entries(&MemoryListOptions::default())
        .unwrap()
        .is_empty());
    assert_eq!(
        store.get_records(&[entry.relative_path], 10).unwrap().len(),
        0
    );
}

#[test]
fn clear_current_project_keeps_global_user_memory() {
    let (_dir, mut store) = make_store();
    store
        .upsert_candidate(&project_candidate("Boreal", "project"))
        .unwrap();
    store
        .upsert_candidate(&user_note_candidate("身份背景", "note"))
        .unwrap();
    let counts = store.clear(ClearMemoryScope::CurrentProject).unwrap();
    assert_eq!(counts.project, 1);
    assert_eq!(counts.user, 0);
    assert!(counts.managed_files >= 2);
    let entries = store.list_entries(&MemoryListOptions::default()).unwrap();
    assert_eq!(entries.len(), 1, "global user note survives project clear");
    assert_eq!(entries[0].frontmatter.kind, MemoryRecordType::User);
    assert!(store.get_project_meta().unwrap().is_none());

    let counts = store.clear(ClearMemoryScope::AllMemory).unwrap();
    assert_eq!(counts.user, 1);
    assert!(store
        .list_entries(&MemoryListOptions::default())
        .unwrap()
        .is_empty());
}

#[test]
fn export_import_roundtrip() {
    let (_dir, mut store) = make_store();
    store
        .upsert_candidate(&project_candidate("Boreal", "project"))
        .unwrap();
    store
        .upsert_candidate(&user_note_candidate("身份背景", "note"))
        .unwrap();
    let bundle = store.export_bundle().expect("export");
    assert_eq!(bundle.format_version, MEMORY_EXPORT_FORMAT_VERSION);
    assert!(!bundle
        .files
        .iter()
        .any(|file| file.relative_path == "MEMORY.md"));
    assert_eq!(bundle.files.len(), 3, "project + project.meta + user note");

    let (_other_dir, mut other) = make_store();
    let imported = other.import_bundle(&bundle).expect("import");
    assert_eq!(imported.imported.project, 1);
    assert_eq!(imported.imported.user, 1);
    assert_eq!(imported.imported.project_metas, 1);
    assert_eq!(
        other
            .list_entries(&MemoryListOptions::default())
            .unwrap()
            .len(),
        2
    );
    assert!(other
        .manifest_text()
        .unwrap()
        .unwrap()
        .contains("## Project Memory"));
    assert_eq!(
        other.get_project_meta().unwrap().unwrap().project_name,
        "Boreal"
    );

    // Unsupported format version is rejected.
    let mut bad = bundle.clone();
    bad.format_version = "old-format.v1".into();
    assert!(other.import_bundle(&bad).is_err());
}

#[test]
fn stage_commit_and_rollback_restore_bytes() {
    let (_dir, mut store) = make_store();
    let original = store
        .upsert_candidate(&project_candidate("Boreal", "original"))
        .unwrap();
    let before = snapshot_files(&store);

    let mut stage = store.create_stage("dream").expect("stage");
    let staged_entry = stage
        .upsert_candidate(&project_candidate("Boreal", "original"))
        .expect("staged write");
    assert_eq!(
        staged_entry.relative_path, original.relative_path,
        "staged store starts as a copy of live"
    );
    let mut added = project_candidate("Second Topic", "added by dream");
    added.captured_at = Some("2026-10-18T11:00:00.000Z".into());
    let added_entry = stage.upsert_candidate(&added).expect("added");
    let stage_paths = stage.roots().clone();
    store
        .commit_stage(
            stage.as_ref(),
            &DreamRuntimeState::default(),
            &DreamRuntimeState::default(),
        )
        .expect("commit");

    let live_entries = store.list_entries(&MemoryListOptions::default()).unwrap();
    assert_eq!(live_entries.len(), 2);
    assert!(live_entries
        .iter()
        .any(|entry| entry.relative_path == added_entry.relative_path));
    let snapshot = store.last_dream_snapshot().unwrap().expect("snapshot");
    assert_eq!(snapshot["version"], 1);
    assert_eq!(
        snapshot["after"]["workspaceVersion"]
            .as_str()
            .unwrap()
            .len(),
        10,
        "version is sha1[:10]"
    );
    assert_eq!(snapshot["sourceAction"], "dream");
    assert!(store
        .roots()
        .workspace_dir
        .join("last_dream/metadata.json")
        .exists());
    // Stage directory is gone after commit.
    assert!(!stage_paths.memory_dir.parent().unwrap().exists());

    // Rollback (live still matches the after-version) restores the bytes.
    let rollback = store
        .rollback_last_dream(&DreamRuntimeState::default())
        .expect("rollback");
    assert!(!rollback.snapshot_captured_at.is_empty());
    assert_eq!(snapshot_files(&store), before);
    let entries = store.list_entries(&MemoryListOptions::default()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        store.last_dream_snapshot().unwrap().unwrap()["sourceAction"],
        "rollback"
    );
    // A second rollback restores the post-Dream tree; a third restores the original.
    store
        .rollback_last_dream(&DreamRuntimeState::default())
        .expect("redo");
    assert_eq!(
        store
            .list_entries(&MemoryListOptions::default())
            .unwrap()
            .len(),
        2
    );
    store
        .rollback_last_dream(&DreamRuntimeState::default())
        .expect("undo redo");
    assert_eq!(snapshot_files(&store), before);

    // A non-dream change invalidates rollback.
    let mut stage = store.create_stage("dream").expect("stage2");
    let mut another = project_candidate("Third", "third");
    another.captured_at = Some("2026-10-18T12:00:00.000Z".into());
    stage.upsert_candidate(&another).unwrap();
    store
        .commit_stage(
            stage.as_ref(),
            &DreamRuntimeState::default(),
            &DreamRuntimeState::default(),
        )
        .expect("commit2");
    let mut later = project_candidate("Later Edit", "changed after dream");
    later.captured_at = Some("2026-10-18T13:00:00.000Z".into());
    store.upsert_candidate(&later).unwrap();
    match store.rollback_last_dream(&DreamRuntimeState::default()) {
        Err(MemoryError::InvalidInput(message)) => {
            assert!(
                message.contains("no longer matches"),
                "unexpected: {message}"
            );
        }
        other => panic!("expected rollback refusal, got {other:?}"),
    }
}

#[test]
fn dispose_stage_cleans_stage_directories() {
    let (_dir, store) = make_store();
    let stage = store.create_stage("dream").unwrap();
    let stage_roots = stage.roots().clone();
    assert!(stage_roots.memory_dir.exists());
    store.dispose_stage(stage);
    assert!(!stage_roots.memory_dir.exists());
    assert!(!stage_roots.global_dir.exists());
}

#[test]
fn overview_counters_exclude_global_user_from_dream_changes() {
    let (_dir, mut store) = make_store();
    store
        .upsert_candidate(&project_candidate("Boreal", "project"))
        .unwrap();
    store
        .upsert_candidate(&user_note_candidate("身份背景", "note"))
        .unwrap();
    let overview = store.overview(None).unwrap();
    assert_eq!(overview.total_files, 2);
    assert_eq!(overview.project_memories, 1);
    assert_eq!(overview.user_profiles, 1);
    assert_eq!(overview.feedback_memories, 0);
    assert_eq!(overview.deprecated_files, 0);
    // Only the workspace project file counts as a dream-relevant change.
    assert_eq!(overview.changed_files_since_last_dream, 1);
    assert_eq!(overview.workspace_mode, WorkspaceMode::Single);
    assert!(overview.project_meta.is_some());

    let old = store.overview(Some("2020-01-01T00:00:00.000Z")).unwrap();
    assert_eq!(old.changed_files_since_last_dream, 1);
    let future = store.overview(Some("2030-01-01T00:00:00.000Z")).unwrap();
    assert_eq!(future.changed_files_since_last_dream, 0);

    let entry = store
        .list_entries(&MemoryListOptions::default())
        .unwrap()
        .into_iter()
        .find(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
        .unwrap();
    store.set_deprecated(&[entry.relative_path], true).unwrap();
    let overview = store.overview(None).unwrap();
    assert_eq!(overview.deprecated_files, 1);
}
