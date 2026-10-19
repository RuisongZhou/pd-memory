//! Shared deterministic doubles for the recall / dream integration tests.
//!
//! This file is included with `#[path = "recall_support.rs"] mod support;` from
//! `recall_retrieve.rs` and `recall_dream.rs`; cargo also compiles it as an empty
//! test target of its own, which is harmless.
#![allow(dead_code)]

use std::collections::HashMap;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{MemoryStore, StoreLayout, StoreRoots};
use pd_memory_core::types::*;

/// A tiny in-memory [`MemoryStore`] used to drive recall/dream offline.
#[derive(Debug, Clone)]
pub struct FakeStore {
    pub entries: Vec<MemoryManifestEntry>,
    pub records: HashMap<String, MemoryFileRecord>,
    pub project_meta: Option<ProjectMetaRecord>,
    pub user_summary: Option<MemoryUserSummary>,
    roots: StoreRoots,
    next_id: u32,
}

impl FakeStore {
    pub fn new(config: &MemoryConfig) -> Self {
        let StoreLayout { roots } = config.store_layout();
        Self {
            entries: Vec::new(),
            records: HashMap::new(),
            project_meta: None,
            user_summary: None,
            roots,
            next_id: 0,
        }
    }

    /// Insert a manifest entry plus its full record.
    pub fn add(&mut self, entry: MemoryManifestEntry, content: &str) {
        let mut record = MemoryFileRecord {
            entry: entry.clone(),
            content: content.to_string(),
            preview: String::new(),
        };
        record.preview = content.chars().take(220).collect();
        self.records.insert(entry.relative_path.clone(), record);
        self.entries.push(entry);
    }

    /// Replace the stored content of an existing entry (keeps the manifest).
    pub fn set_content(&mut self, relative_path: &str, content: &str) {
        if let Some(record) = self.records.get_mut(relative_path) {
            record.content = content.to_string();
        }
    }

    pub fn contains(&self, relative_path: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.relative_path == relative_path)
    }

    pub fn entry_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect()
    }

    fn find_record(&self, id: &str) -> Option<&MemoryFileRecord> {
        self.records.get(id).or_else(|| {
            let stripped = id.strip_prefix("global/").unwrap_or(id);
            self.records.iter().find_map(|(key, record)| {
                if key.strip_prefix("global/").unwrap_or(key) == stripped {
                    Some(record)
                } else {
                    None
                }
            })
        })
    }

    fn next_updated_at(&mut self) -> String {
        self.next_id += 1;
        format!("2026-10-19T00:00:{:02}.000Z", self.next_id)
    }
}

impl FakeStore {
    /// Shared write path for `upsert_candidate` / `upsert_user_profile`.
    fn write_candidate(
        &mut self,
        candidate: &MemoryCandidate,
        relative_path: String,
    ) -> Result<MemoryManifestEntry> {
        let updated_at = self.next_updated_at();
        let frontmatter = MemoryFileFrontmatter {
            name: candidate.name.clone(),
            description: candidate.description.clone(),
            kind: candidate.kind,
            scope: candidate.scope,
            project_id: candidate.project_id.clone().or_else(|| {
                if candidate.scope == MemoryScope::Project {
                    Some("current_project".to_string())
                } else {
                    None
                }
            }),
            updated_at,
            captured_at: candidate.captured_at.clone(),
            source_session_key: candidate.source_session_key.clone(),
            ..Default::default()
        };
        let file = relative_path
            .rsplit('/')
            .next()
            .unwrap_or(relative_path.as_str())
            .to_string();
        let absolute_path = if candidate.kind == MemoryRecordType::User {
            self.roots.global_dir.join(&relative_path)
        } else {
            self.roots.memory_dir.join(&relative_path)
        }
        .to_string_lossy()
        .to_string();
        let entry = MemoryManifestEntry {
            frontmatter,
            file,
            absolute_path,
            relative_path: relative_path.clone(),
        };
        let body = if candidate.body.is_empty() {
            empty_body_for(candidate)
        } else {
            candidate.body.clone()
        };
        // Existing file at the same path is replaced (upsert semantics).
        self.entries
            .retain(|existing| existing.relative_path != relative_path);
        let mut record = MemoryFileRecord {
            entry: entry.clone(),
            content: body,
            preview: String::new(),
        };
        record.preview = record.content.chars().take(220).collect();
        self.records.insert(relative_path, record);
        self.entries.push(entry.clone());
        Ok(entry)
    }
}

impl MemoryStore for FakeStore {
    fn roots(&self) -> &StoreRoots {
        &self.roots
    }

    fn ensure_layout(&mut self) -> Result<()> {
        Ok(())
    }

    fn list_entries(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
        let mut entries: Vec<MemoryManifestEntry> = self
            .entries
            .iter()
            .filter(|entry| options.include_deprecated || !entry.frontmatter.deprecated)
            .filter(|entry| {
                options.kinds.is_empty() || options.kinds.contains(&entry.frontmatter.kind)
            })
            .filter(|entry| match options.query.as_deref() {
                Some(query) => {
                    let query = query.to_lowercase();
                    entry.frontmatter.name.to_lowercase().contains(&query)
                        || entry
                            .frontmatter
                            .description
                            .to_lowercase()
                            .contains(&query)
                }
                None => true,
            })
            .cloned()
            .collect();
        entries.sort_by(|left, right| {
            right
                .frontmatter
                .updated_at
                .cmp(&left.frontmatter.updated_at)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
        let start = options.offset.min(entries.len());
        entries.drain(0..start);
        if let Some(limit) = options.limit {
            entries.truncate(limit);
        }
        Ok(entries)
    }

    fn get_records(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
        let mut records = Vec::new();
        for id in ids {
            if let Some(record) = self.find_record(id) {
                let mut record = record.clone();
                record.content = truncate_lines(&record.content, max_lines);
                records.push(record);
            }
        }
        Ok(records)
    }

    fn upsert_candidate(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        let suffix = {
            self.next_id += 1;
            self.next_id
        };
        let slug = slugify(&candidate.name);
        let relative_path = match candidate.kind {
            MemoryRecordType::User if candidate.name == "user-profile" => {
                "global/UserIdentity/user-profile.md".to_string()
            }
            MemoryRecordType::User => format!("UserIdentityNotes/{slug}-{suffix:04}.md"),
            MemoryRecordType::Feedback => format!("Feedback/{slug}-{suffix:04}.md"),
            MemoryRecordType::Project => format!("Project/{slug}-{suffix:04}.md"),
            MemoryRecordType::GeneralProjectMeta => {
                format!("general_projects/{slug}-{suffix:04}.md")
            }
        };
        self.write_candidate(candidate, relative_path)
    }

    fn upsert_user_profile(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        self.write_candidate(candidate, "global/UserIdentity/user-profile.md".to_string())
    }

    fn rebuild_manifest(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn manifest_text(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn get_project_meta(&self) -> Result<Option<ProjectMetaRecord>> {
        Ok(self.project_meta.clone())
    }

    fn set_project_meta(&mut self, meta: &ProjectMetaRecord) -> Result<()> {
        self.project_meta = Some(meta.clone());
        Ok(())
    }

    fn list_project_metas(&self) -> Result<Vec<ProjectMetaRecord>> {
        Ok(self.project_meta.iter().cloned().collect())
    }

    fn get_user_summary(&self) -> Result<MemoryUserSummary> {
        if let Some(summary) = &self.user_summary {
            return Ok(summary.clone());
        }
        let Some(profile) = self
            .entries
            .iter()
            .find(|entry| is_user_profile_path(&entry.relative_path))
        else {
            return Ok(MemoryUserSummary::default());
        };
        let files: Vec<MemoryFileRecord> = self
            .find_record(&profile.relative_path)
            .cloned()
            .into_iter()
            .collect();
        let identity_background = files
            .first()
            .map(|record| {
                record
                    .content
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("- "))
                    .map(|fact| fact.trim().to_string())
                    .filter(|fact| !fact.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(MemoryUserSummary {
            identity_background,
            files,
        })
    }

    fn overview(&self, _last_dream_at: Option<&str>) -> Result<MemoryOverview> {
        let mut overview = MemoryOverview {
            total_files: self.entries.len(),
            ..Default::default()
        };
        for entry in &self.entries {
            match entry.frontmatter.kind {
                MemoryRecordType::Project => overview.project_memories += 1,
                MemoryRecordType::Feedback => overview.feedback_memories += 1,
                MemoryRecordType::User => overview.user_profiles += 1,
                MemoryRecordType::GeneralProjectMeta => {}
            }
        }
        Ok(overview)
    }

    fn edit_entry(
        &mut self,
        _id: &str,
        _name: &str,
        _description: &str,
        _fields: Option<&MemoryEntryEditFields>,
    ) -> Result<MemoryManifestEntry> {
        Err(MemoryError::Unsupported("FakeStore::edit_entry".into()))
    }

    fn delete_entries(&mut self, ids: &[String]) -> Result<usize> {
        let mut deleted = 0usize;
        for id in ids {
            let before = self.entries.len();
            self.entries
                .retain(|entry| !same_memory_id(&entry.relative_path, id));
            if self.entries.len() != before {
                deleted += before - self.entries.len();
                self.records.retain(|key, _| !same_memory_id(key, id));
            }
        }
        Ok(deleted)
    }

    fn set_deprecated(&mut self, ids: &[String], deprecated: bool) -> Result<usize> {
        let mut changed = 0usize;
        for entry in &mut self.entries {
            if ids
                .iter()
                .any(|id| same_memory_id(&entry.relative_path, id))
                && entry.frontmatter.deprecated != deprecated
            {
                entry.frontmatter.deprecated = deprecated;
                changed += 1;
            }
        }
        Ok(changed)
    }

    fn clear(&mut self, _scope: ClearMemoryScope) -> Result<MemoryTransferCounts> {
        let counts = MemoryTransferCounts {
            managed_files: self.entries.len(),
            memory_files: self.entries.len(),
            ..Default::default()
        };
        self.entries.clear();
        self.records.clear();
        self.project_meta = None;
        Ok(counts)
    }

    fn export_bundle(&self) -> Result<MemoryExportBundle> {
        let mut files: Vec<_> = self
            .records
            .iter()
            .map(|(path, record)| MemorySnapshotFileRecord {
                relative_path: path.clone(),
                content: format!(
                    "{}\n{}",
                    serde_json::to_string(&record.entry.frontmatter).unwrap(),
                    record.content
                ),
            })
            .collect();
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        files.push(MemorySnapshotFileRecord {
            relative_path: "project.meta.md".into(),
            content: serde_json::to_string(&self.project_meta).unwrap(),
        });
        Ok(MemoryExportBundle {
            files,
            ..Default::default()
        })
    }

    fn import_bundle(&mut self, _bundle: &MemoryExportBundle) -> Result<MemoryImportResult> {
        Err(MemoryError::Unsupported("FakeStore::import_bundle".into()))
    }

    fn create_stage(&self, _tag: &str) -> Result<Box<dyn MemoryStore>> {
        Ok(Box::new(self.clone()))
    }

    fn commit_stage(
        &mut self,
        _stage: &dyn MemoryStore,
        _before: &DreamRuntimeState,
        _after: &DreamRuntimeState,
    ) -> Result<()> {
        Ok(())
    }

    fn dispose_stage(&self, _stage: Box<dyn MemoryStore>) {}

    fn rollback_last_dream(&mut self, _current: &DreamRuntimeState) -> Result<DreamRollbackResult> {
        Err(MemoryError::Unsupported(
            "FakeStore::rollback_last_dream".into(),
        ))
    }

    fn last_dream_snapshot(&self) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

/// A config with a test data root and an explicit workspace key.
pub fn config() -> MemoryConfig {
    let mut config = MemoryConfig::for_data_root("/tmp/pd-memory-test");
    config.workspace_id = Some("test-workspace".into());
    config
}

/// Build a manifest entry of any kind.
pub fn memory_entry(
    kind: MemoryRecordType,
    scope: MemoryScope,
    relative_path: &str,
    name: &str,
    description: &str,
    updated_at: &str,
) -> MemoryManifestEntry {
    let file = relative_path
        .rsplit('/')
        .next()
        .unwrap_or(relative_path)
        .to_string();
    MemoryManifestEntry {
        frontmatter: MemoryFileFrontmatter {
            name: name.to_string(),
            description: description.to_string(),
            kind,
            scope,
            project_id: if scope == MemoryScope::Project {
                Some("current_project".into())
            } else {
                None
            },
            updated_at: updated_at.to_string(),
            ..Default::default()
        },
        file,
        relative_path: relative_path.to_string(),
        absolute_path: format!("/tmp/pd-memory-test/{relative_path}"),
    }
}

/// Build a project-scoped `project` entry.
pub fn project_entry(
    relative_path: &str,
    name: &str,
    description: &str,
    updated_at: &str,
) -> MemoryManifestEntry {
    memory_entry(
        MemoryRecordType::Project,
        MemoryScope::Project,
        relative_path,
        name,
        description,
        updated_at,
    )
}

/// Build a project-scoped `feedback` entry.
pub fn feedback_entry(
    relative_path: &str,
    name: &str,
    description: &str,
    updated_at: &str,
) -> MemoryManifestEntry {
    memory_entry(
        MemoryRecordType::Feedback,
        MemoryScope::Project,
        relative_path,
        name,
        description,
        updated_at,
    )
}

/// Build a global `user` entry.
pub fn user_entry(
    relative_path: &str,
    name: &str,
    description: &str,
    updated_at: &str,
) -> MemoryManifestEntry {
    memory_entry(
        MemoryRecordType::User,
        MemoryScope::Global,
        relative_path,
        name,
        description,
        updated_at,
    )
}

/// A deterministic current-project meta record.
pub fn project_meta(updated_at: &str) -> ProjectMetaRecord {
    ProjectMetaRecord {
        project_id: "current_project".into(),
        project_name: "Boreal".into(),
        description: "本地知识库整理工具".into(),
        status: "in_progress".into(),
        created_at: "2026-10-18T09:00:00.000Z".into(),
        updated_at: updated_at.to_string(),
        relative_path: "project.meta.md".into(),
        absolute_path: "/tmp/pd-memory-test/project.meta.md".into(),
        ..Default::default()
    }
}

fn empty_body_for(candidate: &MemoryCandidate) -> String {
    match candidate.kind {
        MemoryRecordType::User => "## 身份背景\n".to_string(),
        MemoryRecordType::Feedback => format!(
            "## Rule\n{}\n",
            if candidate.description.is_empty() {
                &candidate.name
            } else {
                &candidate.description
            }
        ),
        _ => format!(
            "## Summary\n{}\n",
            if candidate.description.is_empty() {
                &candidate.name
            } else {
                &candidate.description
            }
        ),
    }
}

fn truncate_lines(content: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let lines: Vec<&str> = content.trim().lines().collect();
    if lines.len() <= max_lines {
        return lines.join("\n");
    }
    let mut truncated = lines[..max_lines].join("\n");
    truncated.push_str("\n...");
    truncated
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for character in name.to_lowercase().chars() {
        if character.is_alphanumeric() || character == '-' || character == '_' {
            slug.push(character);
        } else {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memory-item".to_string()
    } else {
        trimmed
    }
}

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

fn is_user_profile_path(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    normalized == "UserIdentity/user-profile.md"
        || normalized == "global/UserIdentity/user-profile.md"
}
