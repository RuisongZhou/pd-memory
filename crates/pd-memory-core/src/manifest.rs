//! MEMORY.md manifest rendering and scanning (dev A).
//!
//! The manifest is a derived document; it is rebuilt after every write and is
//! used by recall to give the model a bounded catalogue (<= manifest_limit).
//!
//! Layout and ordering mirror `file-memory.ts::buildManifest`:
//! header + `Updated:` + optional `## Current Project Meta` + active
//! project/feedback/user sections + `## Deprecated`. Empty sections are omitted
//! entirely (no placeholder blank lines). Entries are sorted by `updated_at`
//! descending, then `relative_path` ascending.
//!
//! Global user links follow the reference's filesystem-relative paths; exposed
//! memory ids remain unchanged.

use std::path::{Path, PathBuf};

use crate::types::{MemoryManifestEntry, MemoryRecordType, ProjectMetaRecord};

pub const MANIFEST_FILE: &str = "MEMORY.md";
pub const PROJECT_META_FILE: &str = "project.meta.md";
/// Public id (relative to the data root) of the global user profile.
///
/// Physically the file lives at `<global_dir>/UserIdentity/user-profile.md`;
/// the `global/` prefix is the externally exposed id prefix, matching the
/// TypeScript `toExposedGlobalRelativePath` convention.
pub const USER_PROFILE_RELATIVE_PATH: &str = "global/UserIdentity/user-profile.md";
pub const CURRENT_PROJECT_ID: &str = "current_project";

/// Render the manifest.
///
/// `memory_dir` anchors external user-file links. `updated_at` is supplied by
/// the caller, so rendering itself stays a pure function.
pub fn render_manifest(
    entries: &[MemoryManifestEntry],
    project_meta: Option<&ProjectMetaRecord>,
    memory_dir: &Path,
    updated_at: &str,
) -> String {
    let mut active: Vec<&MemoryManifestEntry> = entries
        .iter()
        .filter(|entry| !entry.frontmatter.deprecated)
        .collect();
    let mut deprecated: Vec<&MemoryManifestEntry> = entries
        .iter()
        .filter(|entry| entry.frontmatter.deprecated)
        .collect();
    sort_entries(&mut active);
    sort_entries(&mut deprecated);

    let project_entries: Vec<&MemoryManifestEntry> = active
        .iter()
        .copied()
        .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
        .collect();
    let feedback_entries: Vec<&MemoryManifestEntry> = active
        .iter()
        .copied()
        .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Feedback)
        .collect();
    let user_entries: Vec<&MemoryManifestEntry> = active
        .iter()
        .copied()
        .filter(|entry| entry.frontmatter.kind == MemoryRecordType::User)
        .collect();

    let mut lines: Vec<String> = vec![
        "# EdgeClaw Memory".to_string(),
        String::new(),
        format!("Updated: {updated_at}"),
        String::new(),
    ];

    if let Some(meta) = project_meta {
        lines.push("## Current Project Meta".to_string());
        lines.push(format!(
            "- [{}]({}) — {}",
            meta.project_name, PROJECT_META_FILE, meta.description
        ));
        lines.push(String::new());
    }
    render_section(&mut lines, "Project Memory", &project_entries, None);
    render_section(&mut lines, "Feedback Memory", &feedback_entries, None);
    render_section(&mut lines, "User Memory", &user_entries, Some(memory_dir));
    render_section(&mut lines, "Deprecated", &deprecated, None);

    format!("{}\n", lines.join("\n").trim())
}

fn render_section(
    lines: &mut Vec<String>,
    title: &str,
    entries: &[&MemoryManifestEntry],
    memory_dir: Option<&Path>,
) {
    if entries.is_empty() {
        return;
    }
    lines.push(format!("## {title}"));
    for entry in entries {
        let link = memory_dir.map_or_else(
            || entry.relative_path.clone(),
            |directory| manifest_link(entry, directory),
        );
        lines.push(format!(
            "- [{}]({}) — {}",
            entry.frontmatter.name, link, entry.frontmatter.description
        ));
    }
    lines.push(String::new());
}

/// Match `resolveManifestLinkPath`: external files use relative filesystem
/// paths, while files within the manifest root retain their exposed ids.
fn manifest_link(entry: &MemoryManifestEntry, memory_dir: &Path) -> String {
    let base: Vec<_> = memory_dir.components().collect();
    let target: Vec<_> = Path::new(&entry.absolute_path).components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    if common == 0 && (memory_dir.has_root() || Path::new(&entry.absolute_path).has_root()) {
        return entry.relative_path.clone();
    }
    let mut relative = PathBuf::new();
    for _ in &base[common..] {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    let link = relative.to_string_lossy().replace('\\', "/");
    if link.starts_with("..") {
        link
    } else {
        entry.relative_path.clone()
    }
}

fn sort_entries(entries: &mut [&MemoryManifestEntry]) {
    entries.sort_by(|left, right| {
        right
            .frontmatter
            .updated_at
            .cmp(&left.frontmatter.updated_at)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryFileFrontmatter, MemoryScope};

    fn fixture_absolute_path(relative: &str) -> String {
        match relative.strip_prefix("global/") {
            Some(path) => format!("/tmp/data/global/{path}"),
            None => format!("/tmp/data/workspaces/ws/memory/{relative}"),
        }
    }

    fn entry(kind: MemoryRecordType, name: &str, rel: &str, updated: &str) -> MemoryManifestEntry {
        MemoryManifestEntry {
            frontmatter: MemoryFileFrontmatter {
                name: name.to_string(),
                description: format!("desc {name}"),
                kind,
                scope: if kind == MemoryRecordType::User {
                    MemoryScope::Global
                } else {
                    MemoryScope::Project
                },
                project_id: (kind != MemoryRecordType::User)
                    .then(|| CURRENT_PROJECT_ID.to_string()),
                updated_at: updated.to_string(),
                ..Default::default()
            },
            file: rel.rsplit('/').next().unwrap_or(rel).to_string(),
            relative_path: rel.to_string(),
            absolute_path: fixture_absolute_path(rel),
        }
    }

    #[test]
    fn renders_exact_layout() {
        let entries = vec![
            entry(
                MemoryRecordType::Project,
                "Boreal",
                "Project/boreal-1a2b3c4d5e.md",
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
            MemoryManifestEntry {
                frontmatter: MemoryFileFrontmatter {
                    deprecated: true,
                    ..entry(
                        MemoryRecordType::Feedback,
                        "old",
                        "Feedback/old-1234567890.md",
                        "2026-10-17T00:00:00.000Z",
                    )
                    .frontmatter
                },
                ..entry(
                    MemoryRecordType::Feedback,
                    "old",
                    "Feedback/old-1234567890.md",
                    "2026-10-17T00:00:00.000Z",
                )
            },
        ];
        let meta = ProjectMetaRecord {
            project_id: CURRENT_PROJECT_ID.into(),
            project_name: "Boreal".into(),
            description: "本地知识库整理工具".into(),
            status: "in_progress".into(),
            created_at: "2026-10-18T09:13:02.114Z".into(),
            updated_at: "2026-10-18T09:13:02.114Z".into(),
            relative_path: PROJECT_META_FILE.into(),
            ..Default::default()
        };
        let rendered = render_manifest(
            &entries,
            Some(&meta),
            Path::new("/tmp/data/workspaces/ws/memory"),
            "2026-10-18T09:41:00.000Z",
        );
        let expected = "# EdgeClaw Memory\n\nUpdated: 2026-10-18T09:41:00.000Z\n\n## Current Project Meta\n- [Boreal](project.meta.md) — 本地知识库整理工具\n\n## Project Memory\n- [Boreal](Project/boreal-1a2b3c4d5e.md) — desc Boreal\n\n## Feedback Memory\n- [delivery-rule](Feedback/delivery-rule-9f8e7d6c5b.md) — desc delivery-rule\n\n## User Memory\n- [user-profile](../../../global/UserIdentity/user-profile.md) — desc user-profile\n\n## Deprecated\n- [old](Feedback/old-1234567890.md) — desc old\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn omits_empty_sections() {
        assert_eq!(
            render_manifest(&[], None, Path::new("/tmp/data/workspaces/ws/memory"), "t"),
            "# EdgeClaw Memory\n\nUpdated: t\n"
        );
    }
}
