//! MEMORY.md manifest rendering tests (task-1, dev A).

use std::path::Path;

use pd_memory_core::manifest::{
    render_manifest, CURRENT_PROJECT_ID, MANIFEST_FILE, PROJECT_META_FILE,
    USER_PROFILE_RELATIVE_PATH,
};
use pd_memory_core::types::{
    MemoryFileFrontmatter, MemoryManifestEntry, MemoryRecordType, MemoryScope, ProjectMetaRecord,
};

fn fixture_absolute_path(relative: &str) -> String {
    match relative.strip_prefix("global/") {
        Some(path) => format!("/tmp/data/global/{path}"),
        None => format!("/tmp/data/workspaces/ws/memory/{relative}"),
    }
}

fn entry(
    kind: MemoryRecordType,
    name: &str,
    relative_path: &str,
    updated_at: &str,
) -> MemoryManifestEntry {
    MemoryManifestEntry {
        frontmatter: MemoryFileFrontmatter {
            name: name.into(),
            description: format!("desc of {name}"),
            kind,
            scope: if kind == MemoryRecordType::User {
                MemoryScope::Global
            } else {
                MemoryScope::Project
            },
            project_id: (kind != MemoryRecordType::User).then(|| CURRENT_PROJECT_ID.to_string()),
            updated_at: updated_at.into(),
            ..Default::default()
        },
        file: relative_path
            .rsplit('/')
            .next()
            .unwrap_or(relative_path)
            .into(),
        relative_path: relative_path.into(),
        absolute_path: fixture_absolute_path(relative_path),
    }
}

fn project_meta() -> ProjectMetaRecord {
    ProjectMetaRecord {
        project_id: CURRENT_PROJECT_ID.into(),
        project_name: "Boreal".into(),
        description: "本地知识库整理工具".into(),
        status: "in_progress".into(),
        created_at: "2026-10-18T09:13:02.114Z".into(),
        updated_at: "2026-10-18T09:13:02.114Z".into(),
        relative_path: PROJECT_META_FILE.into(),
        ..Default::default()
    }
}

#[test]
fn renders_exact_reference_layout() {
    let entries = vec![
        entry(
            MemoryRecordType::Project,
            "Boreal",
            "Project/boreal-471bad1760.md",
            "2026-10-18T09:13:02.114Z",
        ),
        entry(
            MemoryRecordType::Feedback,
            "delivery-rule",
            "Feedback/delivery-rule-9f8e7d6c5b.md",
            "2026-10-18T09:20:00.000Z",
        ),
        entry(
            MemoryRecordType::User,
            "user-profile",
            USER_PROFILE_RELATIVE_PATH,
            "2026-10-19T00:05:00.000Z",
        ),
        {
            let mut deprecated = entry(
                MemoryRecordType::Feedback,
                "old",
                "Feedback/old-1234567890.md",
                "2026-10-17T00:00:00.000Z",
            );
            deprecated.frontmatter.deprecated = true;
            deprecated
        },
    ];
    let rendered = render_manifest(
        &entries,
        Some(&project_meta()),
        Path::new("/tmp/data/workspaces/ws/memory"),
        "2026-10-18T09:41:00.000Z",
    );
    let expected = "# EdgeClaw Memory\n\
\n\
Updated: 2026-10-18T09:41:00.000Z\n\
\n\
## Current Project Meta\n\
- [Boreal](project.meta.md) — 本地知识库整理工具\n\
\n\
## Project Memory\n\
- [Boreal](Project/boreal-471bad1760.md) — desc of Boreal\n\
\n\
## Feedback Memory\n\
- [delivery-rule](Feedback/delivery-rule-9f8e7d6c5b.md) — desc of delivery-rule\n\
\n\
## User Memory\n\
- [user-profile](../../../global/UserIdentity/user-profile.md) — desc of user-profile\n\
\n\
## Deprecated\n\
- [old](Feedback/old-1234567890.md) — desc of old\n";
    assert_eq!(rendered, expected);
    assert_eq!(MANIFEST_FILE, "MEMORY.md");
}

#[test]
fn omits_empty_sections_and_sorts_by_updated_at_desc() {
    assert_eq!(
        render_manifest(
            &[],
            None,
            Path::new("/tmp/data/workspaces/ws/memory"),
            "2026-10-18T09:41:00.000Z"
        ),
        "# EdgeClaw Memory\n\nUpdated: 2026-10-18T09:41:00.000Z\n"
    );
    let entries = vec![
        entry(
            MemoryRecordType::Project,
            "older",
            "Project/older-0000000000.md",
            "2026-01-01T00:00:00.000Z",
        ),
        entry(
            MemoryRecordType::Project,
            "newer",
            "Project/newer-1111111111.md",
            "2026-02-01T00:00:00.000Z",
        ),
    ];
    let rendered = render_manifest(
        &entries,
        None,
        Path::new("/tmp/data/workspaces/ws/memory"),
        "t",
    );
    let newer = rendered.find("[newer]").unwrap();
    let older = rendered.find("[older]").unwrap();
    assert!(newer < older, "manifest must sort updated_at desc");
}

#[test]
fn user_links_follow_physical_roots_without_changing_memory_ids() {
    let mut active = entry(
        MemoryRecordType::User,
        "identity",
        "global/UserIdentityNotes/identity.md",
        "2026-10-19T00:00:00.000Z",
    );
    active.absolute_path = "/tmp/shared/users/identity.md".into();
    let original_id = active.relative_path.clone();
    let mut deprecated = active.clone();
    deprecated.frontmatter.name = "retired".into();
    deprecated.frontmatter.deprecated = true;
    let manifest = render_manifest(
        &[active.clone(), deprecated],
        None,
        Path::new("/tmp/workspaces/deep/agent/memory"),
        "t",
    );
    assert!(manifest.contains("[identity](../../../../shared/users/identity.md)"));
    assert!(manifest.contains("[retired](global/UserIdentityNotes/identity.md)"));
    assert_eq!(active.relative_path, original_id);

    active.absolute_path = "shared/users/identity.md".into();
    let manifest = render_manifest(
        &[active.clone()],
        None,
        Path::new("workspaces/ws/memory"),
        "t",
    );
    assert!(manifest.contains("[identity](../../../shared/users/identity.md)"));

    active.absolute_path = "/tmp/custom/memory/identities/identity.md".into();
    let manifest = render_manifest(&[active], None, Path::new("/tmp/custom/memory"), "t");
    assert!(manifest.contains("[identity](global/UserIdentityNotes/identity.md)"));
}
