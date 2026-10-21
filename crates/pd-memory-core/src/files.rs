//! Markdown file store: the system of record (dev A).
//!
//! Parity target: `file-memory.ts` + the file-store parts of
//! `storage/sqlite.ts` (stage/snapshot/rollback/import/export), merged into one
//! [`MemoryStore`] because in this SDK there is a single store instance that
//! owns both the workspace memory root and the global user root.
//!
//! Layout (see behavior spec §2.2):
//!
//! ```text
//! <memory_dir>/
//!   MEMORY.md
//!   project.meta.md
//!   Project/<slug>-<hash10>.md
//!   Feedback/<slug>-<hash10>.md
//!   general_projects/<slug>-<hash12>.md   # general mode only (unused here)
//! <global_dir>/
//!   UserIdentity/user-profile.md
//!   UserIdentityNotes/<slug>-<hash10>.md
//! <workspace_dir>/
//!   last_dream/{metadata.json,workspace/**,global/**}
//! ```
//!
//! Two id conventions are in play:
//!
//! * workspace records use ids relative to `memory_dir` (`Project/...`);
//! * global user records are *exposed* with a `global/` prefix
//!   (`global/UserIdentityNotes/...`, `global/UserIdentity/user-profile.md`),
//!   matching the TypeScript `toExposedGlobalRelativePath`, while physically
//!   living under `global_dir` **without** that prefix.
//!
//! Invariants: every write is temp-file + rename; ids are rejected when they
//! escape their root; the manifest is rebuilt after every memory write;
//! deletion of dream staging is best-effort.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::config::format_hash10;
use crate::error::{MemoryError, Result};
use crate::frontmatter::{
    normalize_description, normalize_whitespace, parse_flat_fields, parse_markdown, render_markdown,
};
use crate::manifest::{
    render_manifest, CURRENT_PROJECT_ID, MANIFEST_FILE, PROJECT_META_FILE,
    USER_PROFILE_RELATIVE_PATH,
};
use crate::ports::{MemoryStore, StoreRoots};
use crate::types::*;

/// Directory holding project memory files (relative to `memory_dir`).
pub const PROJECT_DIR: &str = "Project";
/// Directory holding feedback memory files (relative to `memory_dir`).
pub const FEEDBACK_DIR: &str = "Feedback";
/// Directory holding general-project metadata files (relative to `memory_dir`).
pub const GENERAL_PROJECTS_DIR: &str = "general_projects";
/// Directory holding append-only user notes (exposed id prefix included).
pub const USER_NOTES_DIR: &str = "global/UserIdentityNotes";
/// Dream snapshot directory (inside `workspace_dir`).
pub const LAST_DREAM_DIR: &str = "last_dream";
/// Dream snapshot metadata file name.
pub const LAST_DREAM_METADATA_FILE: &str = "metadata.json";

const DEFAULT_PROJECT_NAME: &str = "Current Project";
const DEFAULT_PROJECT_DESCRIPTION: &str = "Current project memory.";
const DEFAULT_PROJECT_STATUS: &str = "in_progress";
const DEFAULT_USER_PROFILE_NAME: &str = "user-profile";
const PREVIEW_MAX_CHARS: usize = 220;
const UNIQUE_STRINGS_MAX: usize = 50;
/// Longest file name, in bytes, that Linux and Android filesystems (ext4,
/// f2fs) accept. A CJK character takes 3 bytes, so long CJK names hit it.
const MAX_FILE_NAME_BYTES: usize = 255;

/// Markdown-backed [`MemoryStore`].
#[derive(Debug)]
pub struct FileMemoryStore {
    pub roots: StoreRoots,
    /// `true` only for stores created by [`FileMemoryStore::create_stage`].
    staged: bool,
}

impl FileMemoryStore {
    /// Create a store over the given roots. No I/O happens here; call
    /// [`MemoryStore::ensure_layout`] before relying on the directory
    /// skeleton (the engine does this on construction).
    pub fn new(roots: StoreRoots) -> Self {
        Self {
            roots,
            staged: false,
        }
    }

    // -----------------------------------------------------------------
    // Public helpers (used by hosts, tests and the dream module)
    // -----------------------------------------------------------------

    /// Read the global user profile as a full record (untruncated).
    pub fn read_user_profile(&self) -> Result<Option<MemoryFileRecord>> {
        self.read_record(USER_PROFILE_RELATIVE_PATH, usize::MAX)
    }

    /// Whether a store instance is a staged copy created by `create_stage`.
    pub fn is_stage(&self) -> bool {
        self.staged
    }

    // -----------------------------------------------------------------
    // Path handling
    // -----------------------------------------------------------------

    fn resolve_relative(&self, relative_path: &str) -> Result<PathBuf> {
        let trimmed = relative_path.trim();
        if trimmed.is_empty() {
            return Err(MemoryError::PathEscape(
                "empty relative memory path".to_string(),
            ));
        }
        let path = Path::new(trimmed);
        if path.is_absolute() {
            return Err(MemoryError::PathEscape(trimmed.to_string()));
        }
        for component in path.components() {
            if !matches!(component, Component::Normal(_)) {
                return Err(MemoryError::PathEscape(trimmed.to_string()));
            }
        }
        if let Some(rest) = trimmed.strip_prefix("global/") {
            if rest.is_empty() || rest == "." || rest == ".." {
                return Err(MemoryError::PathEscape(trimmed.to_string()));
            }
            return Ok(self.roots.global_dir.join(rest));
        }
        if trimmed == "global" {
            return Err(MemoryError::PathEscape(trimmed.to_string()));
        }
        Ok(self.roots.memory_dir.join(trimmed))
    }

    fn absolute_path_string(&self, relative_path: &str) -> Result<String> {
        Ok(self
            .resolve_relative(relative_path)?
            .to_string_lossy()
            .to_string())
    }

    // -----------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------

    fn build_entry(
        &self,
        relative_path: &str,
        mut frontmatter: MemoryFileFrontmatter,
    ) -> Result<MemoryManifestEntry> {
        if frontmatter.scope == MemoryScope::Project {
            let project_id = normalize_whitespace(frontmatter.project_id.as_deref().unwrap_or(""));
            frontmatter.project_id = Some(if project_id.is_empty() {
                CURRENT_PROJECT_ID.to_string()
            } else {
                project_id
            });
        }
        let file = Path::new(relative_path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| relative_path.to_string());
        Ok(MemoryManifestEntry {
            frontmatter,
            file,
            relative_path: relative_path.to_string(),
            absolute_path: self.absolute_path_string(relative_path)?,
        })
    }

    fn read_entry(&self, relative_path: &str) -> Result<Option<MemoryManifestEntry>> {
        let absolute = match self.resolve_relative(relative_path) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let raw = match fs::read_to_string(&absolute) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MemoryError::io(&absolute, error)),
        };
        let Some(parsed) = parse_markdown(&raw)? else {
            return Ok(None);
        };
        Ok(Some(self.build_entry(relative_path, parsed.frontmatter)?))
    }

    /// Read a record with the reference preview / line truncation rules.
    fn read_record(
        &self,
        relative_path: &str,
        max_lines: usize,
    ) -> Result<Option<MemoryFileRecord>> {
        let absolute = match self.resolve_relative(relative_path) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let raw = match fs::read_to_string(&absolute) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MemoryError::io(&absolute, error)),
        };
        let Some(parsed) = parse_markdown(&raw)? else {
            return Ok(None);
        };
        let entry = self.build_entry(relative_path, parsed.frontmatter)?;
        let content = trim_content_lines(parsed.body.trim(), max_lines);
        let preview = preview_content(&parsed.body);
        Ok(Some(MemoryFileRecord {
            entry,
            content,
            preview,
        }))
    }

    /// Read frontmatter + the *full* body (never truncated) for rewrites.
    fn read_full_document(
        &self,
        relative_path: &str,
    ) -> Result<Option<(MemoryFileFrontmatter, String)>> {
        let absolute = match self.resolve_relative(relative_path) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let raw = match fs::read_to_string(&absolute) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MemoryError::io(&absolute, error)),
        };
        let Some(parsed) = parse_markdown(&raw)? else {
            return Ok(None);
        };
        Ok(Some((parsed.frontmatter, parsed.body)))
    }

    fn collect_directory_entries(
        &self,
        directory_relative: &str,
    ) -> Result<Vec<MemoryManifestEntry>> {
        let directory = self.resolve_relative(directory_relative)?;
        let read_dir = match fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(MemoryError::io(&directory, error)),
        };
        let mut entries: Vec<MemoryManifestEntry> = Vec::new();
        for item in read_dir {
            let item = item.map_err(|error| MemoryError::io(&directory, error))?;
            let file_type = item
                .file_type()
                .map_err(|error| MemoryError::io(item.path(), error))?;
            if !file_type.is_file() {
                continue;
            }
            let name = item.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") || name == MANIFEST_FILE || name == PROJECT_META_FILE {
                continue;
            }
            let relative_path = format!("{directory_relative}/{name}");
            if let Some(entry) = self.read_entry(&relative_path)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    fn ensure_directories(&self) -> Result<()> {
        for directory in [
            self.roots.memory_dir.join(PROJECT_DIR),
            self.roots.memory_dir.join(FEEDBACK_DIR),
            self.roots.global_dir.join("UserIdentity"),
            self.roots.global_dir.join("UserIdentityNotes"),
        ] {
            fs::create_dir_all(&directory).map_err(|error| MemoryError::io(&directory, error))?;
        }
        fs::create_dir_all(&self.roots.memory_dir)
            .map_err(|error| MemoryError::io(&self.roots.memory_dir, error))?;
        Ok(())
    }

    fn collect_all_entries(&self) -> Result<Vec<MemoryManifestEntry>> {
        let mut entries: Vec<MemoryManifestEntry> = Vec::new();
        entries.extend(self.collect_directory_entries(PROJECT_DIR)?);
        entries.extend(self.collect_directory_entries(FEEDBACK_DIR)?);
        entries.extend(self.collect_directory_entries(GENERAL_PROJECTS_DIR)?);
        if let Some(profile) = self.read_entry(USER_PROFILE_RELATIVE_PATH)? {
            entries.push(profile);
        }
        entries.extend(self.collect_directory_entries(USER_NOTES_DIR)?);
        sort_entries(&mut entries);
        Ok(entries)
    }

    // -----------------------------------------------------------------
    // Writing
    // -----------------------------------------------------------------

    fn write_file(&self, relative_path: &str, contents: &str) -> Result<()> {
        let absolute = self.resolve_relative(relative_path)?;
        atomic_write(&absolute, contents.as_bytes())
    }

    fn write_record(
        &mut self,
        relative_path: &str,
        frontmatter: &MemoryFileFrontmatter,
        body: &str,
    ) -> Result<MemoryManifestEntry> {
        self.write_file(relative_path, &render_markdown(frontmatter, body))?;
        self.repair_manifest()?;
        self.read_entry(relative_path)?
            .ok_or_else(|| MemoryError::Corrupt {
                path: self
                    .resolve_relative(relative_path)
                    .unwrap_or_else(|_| PathBuf::from(relative_path)),
                message: "record could not be parsed back after writing".into(),
            })
    }

    /// Public (crate-internal) alias for the manifest repair used after writes.
    fn repair_manifest(&mut self) -> Result<bool> {
        self.repair_manifest_internal()
    }

    fn repair_manifest_internal(&mut self) -> Result<bool> {
        let entries = self.collect_all_entries()?;
        let project_meta = self.get_project_meta()?;
        let content = render_manifest(
            &entries,
            project_meta.as_ref(),
            &self.roots.memory_dir,
            &now_iso(),
        );
        let path = self.resolve_relative(MANIFEST_FILE)?;
        let previous = fs::read_to_string(&path).ok();
        if previous.as_deref() == Some(content.as_str()) {
            return Ok(false);
        }
        self.write_file(MANIFEST_FILE, &content)?;
        Ok(true)
    }

    // -----------------------------------------------------------------
    // Project metadata
    // -----------------------------------------------------------------

    fn build_project_meta_seed(&self) -> (String, String, String) {
        let entries = self.collect_all_entries().unwrap_or_default();
        let active: Vec<&MemoryManifestEntry> = entries
            .iter()
            .filter(|entry| {
                entry.frontmatter.scope == MemoryScope::Project && !entry.frontmatter.deprecated
            })
            .collect();
        let first_project = active
            .iter()
            .find(|entry| entry.frontmatter.kind == MemoryRecordType::Project);
        let first_feedback = active
            .iter()
            .find(|entry| entry.frontmatter.kind == MemoryRecordType::Feedback);
        let Some(first) = first_project.or(first_feedback) else {
            return (
                DEFAULT_PROJECT_NAME.to_string(),
                DEFAULT_PROJECT_DESCRIPTION.to_string(),
                DEFAULT_PROJECT_STATUS.to_string(),
            );
        };
        let project_name = first_project
            .map(|entry| entry.frontmatter.name.clone())
            .filter(|name| !normalize_whitespace(name).is_empty())
            .unwrap_or_else(|| DEFAULT_PROJECT_NAME.to_string());
        let description = first_project
            .map(|entry| entry.frontmatter.description.clone())
            .filter(|description| !normalize_whitespace(description).is_empty())
            .unwrap_or_else(|| first.frontmatter.description.clone());
        let description = normalize_description(
            &description,
            if project_name.is_empty() {
                DEFAULT_PROJECT_NAME
            } else {
                &project_name
            },
        );
        (
            project_name,
            description,
            DEFAULT_PROJECT_STATUS.to_string(),
        )
    }

    /// Upsert `project.meta.md` from explicit fields + existing/seed state.
    fn upsert_project_meta_internal(
        &mut self,
        project_name: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        dream_updated_at: Option<&str>,
    ) -> Result<ProjectMetaRecord> {
        let existing = self.get_project_meta()?;
        let (seed_name, seed_description, seed_status) = self.build_project_meta_seed();
        let now = now_iso();
        let resolved_name = normalize_whitespace(project_name.unwrap_or(""));
        let name = if !resolved_name.is_empty() {
            resolved_name
        } else {
            existing
                .as_ref()
                .map(|meta| meta.project_name.clone())
                .filter(|name| !normalize_whitespace(name).is_empty())
                .unwrap_or(seed_name)
        };
        let fallback_description = existing
            .as_ref()
            .map(|meta| meta.description.as_str())
            .filter(|description| !normalize_whitespace(description).is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| seed_description);
        let resolved_description =
            normalize_description(description.unwrap_or(""), &fallback_description);
        let resolved_status = {
            let explicit = normalize_whitespace(status.unwrap_or(""));
            if !explicit.is_empty() {
                explicit
            } else {
                existing
                    .as_ref()
                    .map(|meta| normalize_whitespace(&meta.status))
                    .filter(|status| !status.is_empty())
                    .unwrap_or(seed_status)
            }
        };
        let created_at = existing
            .as_ref()
            .map(|meta| meta.created_at.clone())
            .filter(|created| !created.is_empty())
            .unwrap_or_else(|| now.clone());
        let dream_updated_at = dream_updated_at
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|meta| meta.dream_updated_at.clone())
            });
        let absolute_path = self.absolute_path_string(PROJECT_META_FILE)?;
        let record = ProjectMetaRecord {
            project_id: CURRENT_PROJECT_ID.to_string(),
            project_name: name,
            description: resolved_description,
            status: resolved_status,
            source_kind: None,
            source_workspace_path: None,
            source_project_id: None,
            created_at,
            updated_at: now,
            dream_updated_at,
            relative_path: PROJECT_META_FILE.to_string(),
            absolute_path,
        };
        self.write_project_meta_file(&record)?;
        self.repair_manifest()?;
        Ok(record)
    }

    fn write_project_meta_file(&self, record: &ProjectMetaRecord) -> Result<()> {
        let rendered = render_project_meta(record);
        self.write_file(PROJECT_META_FILE, &format!("{}\n", rendered.trim()))
    }

    fn ensure_project_meta_seeded(&mut self, candidate: Option<&MemoryCandidate>) -> Result<bool> {
        if self.get_project_meta()?.is_some() {
            return Ok(false);
        }
        let (project_name, description) = match candidate {
            Some(candidate) => (
                if candidate.kind == MemoryRecordType::Project {
                    Some(candidate.name.as_str())
                } else {
                    None
                },
                Some(candidate.description.as_str()),
            ),
            None => (None, None),
        };
        self.upsert_project_meta_internal(project_name, description, None, None)?;
        Ok(true)
    }

    fn maybe_update_seed_project_meta(&mut self, candidate: &MemoryCandidate) -> Result<()> {
        let Some(meta) = self.get_project_meta()? else {
            return Ok(());
        };
        let normalized_name = normalize_whitespace(&meta.project_name).to_lowercase();
        let normalized_description = normalize_whitespace(&meta.description).to_lowercase();
        let auto_seed_like = normalized_name == DEFAULT_PROJECT_NAME.to_lowercase()
            || normalized_description.ends_with("workspace memory")
            || normalized_description == DEFAULT_PROJECT_DESCRIPTION.to_lowercase();
        if !auto_seed_like {
            return Ok(());
        }
        let description = if normalize_whitespace(&candidate.description).is_empty() {
            None
        } else {
            Some(candidate.description.as_str())
        };
        self.upsert_project_meta_internal(
            Some(&candidate.name),
            description,
            Some(&meta.status),
            None,
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Upsert pipeline
    // -----------------------------------------------------------------

    fn same_origin_entry(
        &self,
        candidate: &MemoryCandidate,
    ) -> Result<Option<MemoryManifestEntry>> {
        let entries = self.collect_all_entries()?;
        Ok(entries
            .into_iter()
            .find(|entry| same_origin(entry, candidate)))
    }

    fn next_relative_path(&self, candidate: &MemoryCandidate) -> Result<String> {
        let now = now_iso();
        let name = normalize_whitespace(&candidate.name);
        let description = normalize_whitespace(&candidate.description);
        match candidate.kind {
            MemoryRecordType::User => {
                let seed = format!(
                    "user:{}:{}:{}:{}",
                    name,
                    description,
                    candidate.captured_at.clone().unwrap_or_default(),
                    candidate
                        .source_session_key
                        .clone()
                        .unwrap_or_else(|| now.clone())
                );
                Ok(format!(
                    "{}/{}",
                    USER_NOTES_DIR,
                    bounded_file_name(&slugify(&name), &format!("-{}.md", hash10(&seed)))
                ))
            }
            MemoryRecordType::Feedback | MemoryRecordType::Project => {
                let directory = if candidate.kind == MemoryRecordType::Feedback {
                    FEEDBACK_DIR
                } else {
                    PROJECT_DIR
                };
                let seed = format!(
                    "{}:{}:{}:{}:{}:{}",
                    candidate.kind.as_str(),
                    candidate.project_id.clone().unwrap_or_default(),
                    name,
                    description,
                    candidate.captured_at.clone().unwrap_or_default(),
                    candidate
                        .source_session_key
                        .clone()
                        .unwrap_or_else(|| now.clone())
                );
                Ok(format!(
                    "{}/{}",
                    directory,
                    bounded_file_name(&slugify(&name), &format!("-{}.md", hash10(&seed)))
                ))
            }
            MemoryRecordType::GeneralProjectMeta => {
                let project_id =
                    normalize_whitespace(candidate.project_id.as_deref().unwrap_or(""));
                let fallback_id = hash10(&format!("{}:{}", name, now));
                let short_id: String = if project_id.is_empty() {
                    fallback_id
                } else {
                    project_id.chars().take(12).collect()
                };
                Ok(format!(
                    "{}/{}",
                    GENERAL_PROJECTS_DIR,
                    bounded_file_name(&slugify(&name), &format!("-{short_id}.md"))
                ))
            }
        }
    }

    // -----------------------------------------------------------------
    // Snapshot / stage helpers
    // -----------------------------------------------------------------

    fn last_dream_root(&self) -> PathBuf {
        self.roots.workspace_dir.join(LAST_DREAM_DIR)
    }

    fn read_last_dream_metadata(&self) -> Result<Option<Value>> {
        let path = self.last_dream_root().join(LAST_DREAM_METADATA_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MemoryError::io(&path, error)),
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&raw).map_err(|error| MemoryError::Corrupt {
            path: path.clone(),
            message: format!("invalid last_dream metadata: {error}"),
        })?;
        Ok(Some(value))
    }

    fn capture_live_snapshot(&self) -> Result<SnapshotTree> {
        Ok(SnapshotTree {
            memory: read_tree(&self.roots.memory_dir)?,
            global: read_tree(&self.roots.global_dir)?,
        })
    }

    /// Write a snapshot tree + metadata to `last_dream/` atomically.
    fn install_last_dream_snapshot(&self, tree: &SnapshotTree, metadata: &Value) -> Result<()> {
        let root = self.last_dream_root();
        let staging = unique_sibling(&root, "snapshot");
        let result = (|| -> Result<()> {
            fs::create_dir_all(&staging).map_err(|error| MemoryError::io(&staging, error))?;
            write_tree(&staging.join("workspace"), &tree.memory)?;
            write_tree(&staging.join("global"), &tree.global)?;
            let mut metadata_text = serde_json::to_string_pretty(metadata)?;
            metadata_text.push('\n');
            let metadata_path = staging.join(LAST_DREAM_METADATA_FILE);
            fs::write(&metadata_path, metadata_text)
                .map_err(|error| MemoryError::io(&metadata_path, error))?;
            replace_directory(&root, &staging)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn replace_live_roots(&self, stage_roots: &StoreRoots, before: &SnapshotTree) -> Result<()> {
        let memory_backup = unique_sibling(&self.roots.memory_dir, "backup");
        let global_backup = unique_sibling(&self.roots.global_dir, "backup");
        if let Some(parent) = self.roots.global_dir.parent() {
            fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
        if let Some(parent) = self.roots.memory_dir.parent() {
            fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
        let mut global_moved = false;
        let mut memory_moved = false;
        let swap_result = (|| -> Result<()> {
            if self.roots.global_dir.exists() {
                fs::rename(&self.roots.global_dir, &global_backup)
                    .map_err(|error| MemoryError::io(&self.roots.global_dir, error))?;
                global_moved = true;
            }
            fs::rename(&stage_roots.global_dir, &self.roots.global_dir)
                .map_err(|error| MemoryError::io(&stage_roots.global_dir, error))?;
            if self.roots.memory_dir.exists() {
                fs::rename(&self.roots.memory_dir, &memory_backup)
                    .map_err(|error| MemoryError::io(&self.roots.memory_dir, error))?;
                memory_moved = true;
            }
            fs::rename(&stage_roots.memory_dir, &self.roots.memory_dir)
                .map_err(|error| MemoryError::io(&stage_roots.memory_dir, error))?;
            Ok(())
        })();
        if let Err(error) = swap_result {
            let _ = global_moved;
            let _ = memory_moved;
            // Restore the pre-commit bytes from the in-memory snapshot.
            let _ = write_tree(&self.roots.memory_dir, &before.memory);
            let _ = write_tree(&self.roots.global_dir, &before.global);
            let _ = fs::remove_dir_all(&memory_backup);
            let _ = fs::remove_dir_all(&global_backup);
            return Err(error);
        }
        let _ = fs::remove_dir_all(&memory_backup);
        let _ = fs::remove_dir_all(&global_backup);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MemoryStore implementation
// ---------------------------------------------------------------------------

impl MemoryStore for FileMemoryStore {
    fn roots(&self) -> &StoreRoots {
        &self.roots
    }

    fn ensure_layout(&mut self) -> Result<()> {
        self.ensure_directories()?;
        self.repair_manifest_internal()?;
        Ok(())
    }

    fn list_entries(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
        let mut entries = self.collect_all_entries()?;
        if !options.kinds.is_empty() {
            entries.retain(|entry| options.kinds.contains(&entry.frontmatter.kind));
        }
        if let Some(query) = options.query.as_deref() {
            let normalized_query = normalize_whitespace(query).to_lowercase();
            if !normalized_query.is_empty() {
                entries.retain(|entry| {
                    let haystack = format!(
                        "{} {} {}",
                        entry.frontmatter.name, entry.frontmatter.description, entry.relative_path
                    )
                    .to_lowercase();
                    haystack.contains(&normalized_query)
                });
            }
        }
        if !options.include_deprecated {
            entries.retain(|entry| !entry.frontmatter.deprecated);
        }
        let offset = options.offset;
        let items = entries.into_iter().skip(offset);
        Ok(match options.limit {
            Some(limit) => items.take(limit).collect(),
            None => items.collect(),
        })
    }

    fn get_records(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
        let mut records: Vec<MemoryFileRecord> = Vec::new();
        for id in ids {
            if let Some(record) = self.read_record(id, max_lines)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn upsert_candidate(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        if candidate.kind != MemoryRecordType::User {
            self.ensure_project_meta_seeded(Some(candidate))?;
        }
        // In single-project mode the candidate's project id is ignored; the
        // frontmatter always carries `current_project` (build_frontmatter).
        let existing = self.same_origin_entry(candidate)?;
        let frontmatter = build_frontmatter(candidate, existing.as_ref(), &now_iso());
        let body = build_record_body(candidate);
        let relative_path = match &existing {
            Some(entry) => entry.relative_path.clone(),
            None => self.next_relative_path(candidate)?,
        };
        let entry = self.write_record(&relative_path, &frontmatter, &body)?;
        if candidate.kind == MemoryRecordType::Project {
            self.maybe_update_seed_project_meta(candidate)?;
        }
        Ok(entry)
    }

    fn upsert_user_profile(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        let existing = self.read_entry(USER_PROFILE_RELATIVE_PATH)?;
        let mut effective = candidate.clone();
        effective.kind = MemoryRecordType::User;
        effective.scope = MemoryScope::Global;
        if normalize_whitespace(&effective.name).is_empty() {
            effective.name = DEFAULT_USER_PROFILE_NAME.to_string();
        }
        let frontmatter = build_frontmatter(&effective, existing.as_ref(), &now_iso());
        let body = build_record_body(&effective);
        self.write_record(USER_PROFILE_RELATIVE_PATH, &frontmatter, &body)
    }

    fn rebuild_manifest(&mut self) -> Result<bool> {
        self.repair_manifest_internal()
    }

    fn manifest_text(&self) -> Result<Option<String>> {
        let path = self.roots.memory_dir.join(MANIFEST_FILE);
        match fs::read_to_string(&path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MemoryError::io(&path, error)),
        }
    }

    fn get_project_meta(&self) -> Result<Option<ProjectMetaRecord>> {
        let path = self.roots.memory_dir.join(PROJECT_META_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MemoryError::io(&path, error)),
        };
        let values = parse_flat_fields(&raw);
        let project_name = normalize_whitespace(
            values
                .get("project_name")
                .map(String::as_str)
                .unwrap_or_default(),
        );
        if project_name.is_empty() {
            return Ok(None);
        }
        let parsed = parse_markdown(&raw)?;
        let frontmatter_updated_at = parsed
            .as_ref()
            .map(|parsed| parsed.frontmatter.updated_at.clone())
            .filter(|value| !value.is_empty());
        let description = normalize_description(
            values
                .get("description")
                .map(String::as_str)
                .unwrap_or_default(),
            &project_name,
        );
        let status = {
            let value =
                normalize_whitespace(values.get("status").map(String::as_str).unwrap_or_default());
            if value.is_empty() {
                DEFAULT_PROJECT_STATUS.to_string()
            } else {
                value
            }
        };
        let updated_at = values
            .get("updated_at")
            .map(|value| normalize_whitespace(value))
            .filter(|value| !value.is_empty())
            .or(frontmatter_updated_at)
            .unwrap_or_else(now_iso);
        let created_at = values
            .get("created_at")
            .map(|value| normalize_whitespace(value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| updated_at.clone());
        let project_id = values
            .get("project_id")
            .map(|value| normalize_whitespace(value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| CURRENT_PROJECT_ID.to_string());
        Ok(Some(ProjectMetaRecord {
            project_id,
            project_name,
            description,
            status,
            source_kind: values
                .get("source_kind")
                .cloned()
                .filter(|value| !value.is_empty()),
            source_workspace_path: values
                .get("source_workspace_path")
                .cloned()
                .filter(|value| !value.is_empty()),
            source_project_id: values
                .get("source_project_id")
                .cloned()
                .filter(|value| !value.is_empty()),
            created_at,
            updated_at,
            dream_updated_at: values
                .get("dream_updated_at")
                .cloned()
                .filter(|value| !value.is_empty()),
            relative_path: PROJECT_META_FILE.to_string(),
            absolute_path: path.to_string_lossy().to_string(),
        }))
    }

    fn set_project_meta(&mut self, meta: &ProjectMetaRecord) -> Result<()> {
        let mut record = meta.clone();
        if normalize_whitespace(&record.project_id).is_empty() {
            record.project_id = CURRENT_PROJECT_ID.to_string();
        }
        if normalize_whitespace(&record.project_name).is_empty() {
            record.project_name = DEFAULT_PROJECT_NAME.to_string();
        }
        if normalize_whitespace(&record.description).is_empty() {
            record.description = normalize_description("", &record.project_name);
        }
        if normalize_whitespace(&record.status).is_empty() {
            record.status = DEFAULT_PROJECT_STATUS.to_string();
        }
        if record.updated_at.is_empty() {
            record.updated_at = now_iso();
        }
        if record.created_at.is_empty() {
            record.created_at = record.updated_at.clone();
        }
        record.relative_path = PROJECT_META_FILE.to_string();
        record.absolute_path = self.absolute_path_string(PROJECT_META_FILE)?;
        self.write_project_meta_file(&record)?;
        self.repair_manifest()?;
        Ok(())
    }

    fn list_project_metas(&self) -> Result<Vec<ProjectMetaRecord>> {
        let mut metas: Vec<ProjectMetaRecord> = Vec::new();
        if let Some(meta) = self.get_project_meta()? {
            metas.push(meta);
        }
        for entry in self.collect_all_entries()? {
            if entry.frontmatter.kind != MemoryRecordType::GeneralProjectMeta {
                continue;
            }
            if let Some(record) = self.read_record(&entry.relative_path, usize::MAX)? {
                if let Some(meta) = general_meta_record_from_file(&record) {
                    metas.push(meta);
                }
            }
        }
        Ok(metas)
    }

    fn get_user_summary(&self) -> Result<MemoryUserSummary> {
        let mut identity_background: Vec<String> = Vec::new();
        let mut files: Vec<MemoryFileRecord> = Vec::new();
        if let Some(profile) = self.read_record(USER_PROFILE_RELATIVE_PATH, usize::MAX)? {
            let sections = parse_markdown_sections(&profile.content);
            identity_background = parse_fact_section(sections.get("身份背景").map(Vec::as_slice));
            files.push(profile);
        }
        for entry in self.collect_all_entries()? {
            if entry.frontmatter.kind != MemoryRecordType::User {
                continue;
            }
            if entry.relative_path == USER_PROFILE_RELATIVE_PATH {
                continue;
            }
            if let Some(record) = self.read_record(&entry.relative_path, usize::MAX)? {
                files.push(record);
            }
        }
        Ok(MemoryUserSummary {
            identity_background,
            files,
        })
    }

    fn overview(&self, last_dream_at: Option<&str>) -> Result<MemoryOverview> {
        let entries = self.collect_all_entries()?;
        let active: Vec<&MemoryManifestEntry> = entries
            .iter()
            .filter(|entry| !entry.frontmatter.deprecated)
            .collect();
        let project_memories = active
            .iter()
            .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
            .count();
        let feedback_memories = active
            .iter()
            .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Feedback)
            .count();
        let user_profiles = active
            .iter()
            .filter(|entry| entry.frontmatter.kind == MemoryRecordType::User)
            .count();
        let deprecated_files = entries
            .iter()
            .filter(|entry| entry.frontmatter.deprecated)
            .count();
        // Parity: `FileMemoryStore.getOverview` only sees workspace records, so
        // global user profile/note writes never count as dream changes.
        let workspace_active: Vec<&&MemoryManifestEntry> = active
            .iter()
            .filter(|entry| !entry.relative_path.starts_with("global/"))
            .collect();
        let changed_files_since_last_dream = match last_dream_at {
            Some(last_dream_at) if !last_dream_at.is_empty() => workspace_active
                .iter()
                .filter(|entry| entry.frontmatter.updated_at.as_str() > last_dream_at)
                .count(),
            _ => workspace_active.len(),
        };
        let workspace_name = self
            .roots
            .workspace_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "default".to_string());
        Ok(MemoryOverview {
            total_files: active.len(),
            project_memories,
            feedback_memories,
            user_profiles,
            pending_sessions: 0,
            pending_dialogue_turns: 0,
            last_indexed_at: None,
            last_dream_at: last_dream_at.map(str::to_string),
            last_dream_status: None,
            last_dream_summary: None,
            changed_files_since_last_dream,
            workspace_mode: WorkspaceMode::Single,
            workspace_name,
            project_meta: self.get_project_meta()?,
            deprecated_files,
        })
    }

    fn edit_entry(
        &mut self,
        id: &str,
        name: &str,
        description: &str,
        fields: Option<&MemoryEntryEditFields>,
    ) -> Result<MemoryManifestEntry> {
        let Some((frontmatter, body)) = self.read_full_document(id)? else {
            return Err(MemoryError::NotFound(id.to_string()));
        };
        let existing_candidate = candidate_from_document(&frontmatter, &body);
        let mut next = existing_candidate.clone();
        let resolved_name = normalize_whitespace(name);
        next.name = if resolved_name.is_empty() {
            existing_candidate.name.clone()
        } else {
            resolved_name
        };
        next.description = normalize_description(description, &existing_candidate.description);
        if let Some(fields) = fields {
            next.body.clear();
            if let Some(stage) = fields.stage.as_deref() {
                next.stage = Some(normalize_whitespace(stage));
            }
            if !fields.decisions.is_empty() {
                next.decisions = unique_strings(&fields.decisions);
            }
            if !fields.constraints.is_empty() {
                next.constraints = unique_strings(&fields.constraints);
            }
            if !fields.next_steps.is_empty() {
                next.next_steps = unique_strings(&fields.next_steps);
            }
            if !fields.blockers.is_empty() {
                next.blockers = unique_strings(&fields.blockers);
            }
            if !fields.timeline.is_empty() {
                next.timeline = unique_strings(&fields.timeline);
            }
            if !fields.notes.is_empty() {
                next.notes = unique_strings(&fields.notes);
            }
            if let Some(rule) = fields.rule.as_deref() {
                next.rule = Some(normalize_whitespace(rule));
            }
            if let Some(why) = fields.why.as_deref() {
                next.why = Some(normalize_whitespace(why));
            }
            if let Some(how_to_apply) = fields.how_to_apply.as_deref() {
                next.how_to_apply = Some(normalize_whitespace(how_to_apply));
            }
        }
        let mut frontmatter = frontmatter;
        frontmatter.name = next.name.clone();
        frontmatter.description = candidate_description(&next);
        frontmatter.updated_at = now_iso();
        let body = build_record_body(&next);
        self.write_record(id, &frontmatter, &body)
    }

    fn delete_entries(&mut self, ids: &[String]) -> Result<usize> {
        let mut removed = 0;
        for id in ids {
            let absolute = match self.resolve_relative(id) {
                Ok(path) => path,
                Err(_) => continue,
            };
            match fs::remove_file(&absolute) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(MemoryError::io(&absolute, error)),
            }
        }
        if removed > 0 {
            self.repair_manifest()?;
        }
        Ok(removed)
    }

    fn set_deprecated(&mut self, ids: &[String], deprecated: bool) -> Result<usize> {
        let mut changed = 0;
        for id in ids {
            let Some((mut frontmatter, body)) = self.read_full_document(id)? else {
                continue;
            };
            frontmatter.deprecated = deprecated;
            frontmatter.updated_at = now_iso();
            self.write_file(id, &render_markdown(&frontmatter, &body))?;
            changed += 1;
        }
        if changed > 0 {
            self.repair_manifest()?;
        }
        Ok(changed)
    }

    fn clear(&mut self, scope: ClearMemoryScope) -> Result<MemoryTransferCounts> {
        let entries = self.collect_all_entries()?;
        let scoped_entries: Vec<&MemoryManifestEntry> = entries
            .iter()
            .filter(|entry| {
                scope == ClearMemoryScope::AllMemory || !entry.relative_path.starts_with("global/")
            })
            .collect();
        let memory_tree = read_tree(&self.roots.memory_dir)?;
        let global_tree = read_tree(&self.roots.global_dir)?;
        let counts = MemoryTransferCounts {
            managed_files: match scope {
                ClearMemoryScope::CurrentProject => memory_tree.len(),
                ClearMemoryScope::AllMemory => memory_tree.len() + global_tree.len(),
            },
            memory_files: scoped_entries.len(),
            project: scoped_entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
                .count(),
            feedback: scoped_entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Feedback)
                .count(),
            user: scoped_entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::User)
                .count(),
            project_metas: self.list_project_metas()?.len(),
        };
        match scope {
            ClearMemoryScope::CurrentProject => {
                remove_directory_contents(&self.roots.memory_dir)?;
                // Parity: clearing the workspace invalidates any rollback.
                let _ = fs::remove_dir_all(self.last_dream_root());
            }
            ClearMemoryScope::AllMemory => {
                remove_directory_contents(&self.roots.memory_dir)?;
                remove_directory_contents(&self.roots.global_dir)?;
                let _ = fs::remove_dir_all(self.last_dream_root());
            }
        }
        self.ensure_directories()?;
        Ok(counts)
    }

    fn export_bundle(&self) -> Result<MemoryExportBundle> {
        let mut relative_paths: Vec<String> = Vec::new();
        if self.get_project_meta()?.is_some() {
            relative_paths.push(PROJECT_META_FILE.to_string());
        }
        for entry in self.collect_all_entries()? {
            relative_paths.push(entry.relative_path.clone());
        }
        let mut files: Vec<MemorySnapshotFileRecord> = Vec::new();
        for relative_path in relative_paths {
            let absolute = self.resolve_relative(&relative_path)?;
            if let Ok(content) = fs::read_to_string(&absolute) {
                files.push(MemorySnapshotFileRecord {
                    relative_path,
                    content,
                });
            }
        }
        Ok(MemoryExportBundle {
            format_version: MEMORY_EXPORT_FORMAT_VERSION.to_string(),
            metadata: MemoryBundleMetadata {
                exported_at: now_iso(),
                ..Default::default()
            },
            files,
        })
    }

    fn import_bundle(&mut self, bundle: &MemoryExportBundle) -> Result<MemoryImportResult> {
        if bundle.format_version != MEMORY_EXPORT_FORMAT_VERSION {
            return Err(MemoryError::InvalidInput(format!(
                "unsupported memory bundle format {} (expected {})",
                bundle.format_version, MEMORY_EXPORT_FORMAT_VERSION
            )));
        }
        let mut memory_files: Vec<(String, String)> = Vec::new();
        let mut global_files: Vec<(String, String)> = Vec::new();
        for record in &bundle.files {
            let relative_path = record.relative_path.trim();
            // Validate first: an escaping id must abort the whole import.
            self.resolve_relative(relative_path)?;
            if relative_path == MANIFEST_FILE {
                continue; // derived document; regenerated below
            }
            if relative_path.starts_with("global/") {
                global_files.push((relative_path.to_string(), record.content.clone()));
            } else {
                memory_files.push((relative_path.to_string(), record.content.clone()));
            }
        }
        let memory_bytes = dedupe_files(memory_files);
        // Global bundle ids carry the exposed `global/` prefix; the physical
        // tree under `global_dir` does not.
        let global_bytes: Vec<(String, Vec<u8>)> = dedupe_files(global_files)
            .into_iter()
            .map(|(relative_path, content)| {
                (
                    relative_path
                        .strip_prefix("global/")
                        .unwrap_or(&relative_path)
                        .to_string(),
                    content,
                )
            })
            .collect();
        write_tree(&self.roots.memory_dir, &memory_bytes)?;
        if !global_bytes.is_empty() {
            write_tree(&self.roots.global_dir, &global_bytes)?;
        }
        let _ = fs::remove_dir_all(self.last_dream_root());
        self.ensure_layout()?;
        let entries = self.collect_all_entries()?;
        let imported = MemoryTransferCounts {
            managed_files: memory_bytes.len() + global_bytes.len() + 1,
            memory_files: entries.len(),
            project: entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Project)
                .count(),
            feedback: entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::Feedback)
                .count(),
            user: entries
                .iter()
                .filter(|entry| entry.frontmatter.kind == MemoryRecordType::User)
                .count(),
            project_metas: self.list_project_metas()?.len(),
        };
        Ok(MemoryImportResult {
            format_version: MEMORY_EXPORT_FORMAT_VERSION.to_string(),
            imported,
            imported_at: now_iso(),
            warnings: Vec::new(),
            metadata: bundle.metadata.clone(),
        })
    }

    fn create_stage(&self, tag: &str) -> Result<Box<dyn MemoryStore>> {
        let id = unique_id();
        let safe_tag: String = tag
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let safe_tag = if safe_tag.is_empty() {
            "stage".to_string()
        } else {
            safe_tag
        };
        let global_parent = self.roots.global_dir.parent().ok_or_else(|| {
            MemoryError::InvalidInput("global_dir must have a parent directory for staging".into())
        })?;
        let stage_workspace = self
            .roots
            .workspace_dir
            .join(format!(".{safe_tag}-stage-{id}"));
        let stage_memory = stage_workspace.join("memory");
        let stage_global = global_parent.join(format!(".{safe_tag}-stage-{id}-global"));
        let mut stage = FileMemoryStore {
            roots: StoreRoots {
                memory_dir: stage_memory,
                global_dir: stage_global.clone(),
                workspace_dir: stage_workspace.clone(),
            },
            staged: true,
        };
        fs::create_dir_all(&stage.roots.memory_dir)
            .map_err(|error| MemoryError::io(&stage.roots.memory_dir, error))?;
        write_tree(&stage.roots.memory_dir, &read_tree(&self.roots.memory_dir)?)?;
        write_tree(&stage.roots.global_dir, &read_tree(&self.roots.global_dir)?)?;
        stage.ensure_layout()?;
        Ok(Box::new(stage))
    }

    fn commit_stage(
        &mut self,
        stage: &dyn MemoryStore,
        before_state: &DreamRuntimeState,
        after_state: &DreamRuntimeState,
    ) -> Result<()> {
        let stage_roots = stage.roots().clone();
        if stage_roots.memory_dir == self.roots.memory_dir
            || stage_roots.global_dir == self.roots.global_dir
        {
            return Err(MemoryError::InvalidInput(
                "commit_stage requires a distinct staged store".into(),
            ));
        }
        let before = self.capture_live_snapshot()?;
        let after = SnapshotTree {
            memory: read_tree(&stage_roots.memory_dir)?,
            global: read_tree(&stage_roots.global_dir)?,
        };
        let captured_at = after_state.last_dream_at.clone().unwrap_or_else(now_iso);
        let source_workspace_dir = self.roots.workspace_dir.to_string_lossy().to_string();
        let metadata = json!({
            "version": 1,
            "capturedAt": captured_at,
            "sourceAction": "dream",
            "sourceWorkspaceDir": source_workspace_dir,
            "summary": after_state.last_dream_summary,
            "before": {
                "workspaceVersion": snapshot_version(&before.memory),
                "globalVersion": snapshot_version(&before.global),
                "counts": transfer_counts_json(&before),
                "runtimeState": before_state,
            },
            "after": {
                "workspaceVersion": snapshot_version(&after.memory),
                "globalVersion": snapshot_version(&after.global),
                "counts": transfer_counts_json(&after),
                "runtimeState": after_state,
            },
        });
        self.install_last_dream_snapshot(&before, &metadata)?;
        self.replace_live_roots(&stage_roots, &before)?;
        // The staged files now live at the live roots; clean up empty stage
        // parents (best effort).
        if let Some(parent) = stage_roots.memory_dir.parent() {
            let _ = fs::remove_dir(parent);
        }
        self.repair_manifest_internal()?;
        Ok(())
    }

    fn dispose_stage(&self, stage: Box<dyn MemoryStore>) {
        let roots = stage.roots().clone();
        drop(stage);
        if roots.memory_dir != self.roots.memory_dir {
            if let Some(parent) = roots.memory_dir.parent() {
                if parent
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
                {
                    let _ = fs::remove_dir_all(parent);
                }
            }
        }
        if roots.global_dir != self.roots.global_dir {
            let _ = fs::remove_dir_all(&roots.global_dir);
        }
    }

    fn rollback_last_dream(
        &mut self,
        current_state: &DreamRuntimeState,
    ) -> Result<DreamRollbackResult> {
        let Some(metadata) = self.read_last_dream_metadata()? else {
            return Err(MemoryError::InvalidInput(
                "No last Dream snapshot is available for rollback.".into(),
            ));
        };
        let snapshot_root = self.last_dream_root();
        let before = SnapshotTree {
            memory: read_tree(&snapshot_root.join("workspace"))?,
            global: read_tree(&snapshot_root.join("global"))?,
        };
        let current = self.capture_live_snapshot()?;
        let after_workspace = metadata
            .pointer("/after/workspaceVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let after_global = metadata
            .pointer("/after/globalVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if snapshot_version(&current.memory) != after_workspace
            || snapshot_version(&current.global) != after_global
        {
            return Err(MemoryError::InvalidInput(
                "Current memory state no longer matches the last Dream snapshot, so rollback is unavailable."
                    .into(),
            ));
        }
        let rolled_back_at = now_iso();
        let reverse_metadata = json!({
            "version": 1,
            "capturedAt": rolled_back_at,
            "sourceAction": "rollback",
            "sourceWorkspaceDir": self.roots.workspace_dir.to_string_lossy(),
            "summary": current_state.last_dream_summary,
            "before": {
                "workspaceVersion": snapshot_version(&current.memory),
                "globalVersion": snapshot_version(&current.global),
                "counts": transfer_counts_json(&current),
                "runtimeState": current_state,
            },
            "after": metadata["before"],
        });
        self.install_last_dream_snapshot(&current, &reverse_metadata)?;
        let mut stage = self.create_stage("rollback")?;
        let restored = (|| -> Result<()> {
            write_tree(&stage.roots().memory_dir, &before.memory)?;
            write_tree(&stage.roots().global_dir, &before.global)?;
            stage.ensure_layout()?;
            self.replace_live_roots(stage.roots(), &current)
        })();
        self.dispose_stage(stage);
        restored?;
        let runtime = metadata.pointer("/before/runtimeState");
        let last_dream_at = runtime
            .and_then(|runtime| runtime.pointer("/lastDreamAt"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let last_dream_status = runtime
            .and_then(|runtime| runtime.pointer("/lastDreamStatus"))
            .cloned()
            .and_then(|value| serde_json::from_value::<DreamPipelineStatus>(value).ok());
        let last_dream_summary = runtime
            .and_then(|runtime| runtime.pointer("/lastDreamSummary"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let snapshot_captured_at = metadata
            .get("capturedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(DreamRollbackResult {
            rolled_back_at,
            snapshot_captured_at,
            last_dream_at,
            last_dream_status,
            last_dream_summary,
        })
    }

    fn last_dream_snapshot(&self) -> Result<Option<Value>> {
        let Some(metadata) = self.read_last_dream_metadata()? else {
            return Ok(None);
        };
        let snapshot_root = self.last_dream_root();
        if !snapshot_root.join("workspace").exists() || !snapshot_root.join("global").exists() {
            return Ok(None);
        }
        Ok(Some(metadata))
    }
}

// ---------------------------------------------------------------------------
// Snapshot tree helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct SnapshotTree {
    memory: Vec<(String, Vec<u8>)>,
    global: Vec<(String, Vec<u8>)>,
}

/// Content version used by the dream snapshot/rollback guard.
///
/// Parity (`sqlite.ts:448-456`): `sha1(JSON.stringify(sorted
/// [{relativePath, content}] without MEMORY.md))[:10]`.
fn snapshot_version(files: &[(String, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(String, Vec<u8>)> = files
        .iter()
        .filter(|(relative_path, _)| relative_path != MANIFEST_FILE)
        .collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    #[derive(serde::Serialize)]
    struct SnapshotFileRef<'a> {
        #[serde(rename = "relativePath")]
        relative_path: &'a str,
        content: String,
    }
    let references: Vec<SnapshotFileRef<'_>> = sorted
        .into_iter()
        .map(|(relative_path, content)| SnapshotFileRef {
            relative_path,
            content: String::from_utf8_lossy(content).to_string(),
        })
        .collect();
    let serialized = serde_json::to_string(&references).unwrap_or_default();
    let mut hasher = Sha1::new();
    hasher.update(serialized.as_bytes());
    format_hash10(&hex::encode(hasher.finalize()))
}

fn transfer_counts_json(tree: &SnapshotTree) -> Value {
    let mut project = 0;
    let mut feedback = 0;
    let mut user = 0;
    let mut memory_files = 0;
    for (_, content) in tree.memory.iter().chain(tree.global.iter()) {
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        let Ok(Some(parsed)) = parse_markdown(text) else {
            continue;
        };
        memory_files += 1;
        match parsed.frontmatter.kind {
            MemoryRecordType::Project => project += 1,
            MemoryRecordType::Feedback => feedback += 1,
            MemoryRecordType::User => user += 1,
            MemoryRecordType::GeneralProjectMeta => {}
        }
    }
    let project_metas = if tree
        .memory
        .iter()
        .any(|(path, _)| path == PROJECT_META_FILE)
    {
        1
    } else {
        0
    };
    json!({
        "managedFiles": tree.memory.len() + tree.global.len(),
        "memoryFiles": memory_files,
        "project": project,
        "feedback": feedback,
        "user": user,
        "tmp": 0,
        "projectMetas": project_metas,
    })
}

fn read_tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    read_tree_inner(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn read_tree_inner(
    base: &Path,
    directory: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let read_dir = fs::read_dir(directory).map_err(|error| MemoryError::io(directory, error))?;
    for item in read_dir {
        let item = item.map_err(|error| MemoryError::io(directory, error))?;
        let path = item.path();
        let file_type = item
            .file_type()
            .map_err(|error| MemoryError::io(&path, error))?;
        if file_type.is_dir() {
            read_tree_inner(base, &path, files)?;
        } else if file_type.is_file() {
            let name = item.file_name().to_string_lossy().to_string();
            if name.contains(".tmp-") {
                continue;
            }
            let relative = path
                .strip_prefix(base)
                .map_err(|_| MemoryError::PathEscape(path.display().to_string()))?;
            let relative = relative
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(part.to_string_lossy().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            let content = fs::read(&path).map_err(|error| MemoryError::io(&path, error))?;
            files.push((relative, content));
        }
    }
    Ok(())
}

fn write_tree(root: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    if root.exists() {
        fs::remove_dir_all(root).map_err(|error| MemoryError::io(root, error))?;
    }
    fs::create_dir_all(root).map_err(|error| MemoryError::io(root, error))?;
    for (relative_path, content) in files {
        let target = safe_join(root, relative_path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
        fs::write(&target, content).map_err(|error| MemoryError::io(&target, error))?;
    }
    Ok(())
}

fn safe_join(root: &Path, relative_path: &str) -> Result<PathBuf> {
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(MemoryError::PathEscape(relative_path.to_string()));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(MemoryError::PathEscape(relative_path.to_string()));
        }
    }
    Ok(root.join(path))
}

/// Replace `target` with `staged` using a sibling backup (atomic swap per dir).
fn replace_directory(target: &Path, staged: &Path) -> Result<()> {
    let backup = unique_sibling(target, "backup");
    let had_target = target.exists();
    if had_target {
        fs::rename(target, &backup).map_err(|error| MemoryError::io(target, error))?;
    }
    match fs::rename(staged, target) {
        Ok(()) => {
            if had_target {
                let _ = fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(error) => {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            Err(MemoryError::io(target, error))
        }
    }
}

fn remove_directory_contents(directory: &Path) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let read_dir = fs::read_dir(directory).map_err(|error| MemoryError::io(directory, error))?;
    for item in read_dir {
        let item = item.map_err(|error| MemoryError::io(directory, error))?;
        let path = item.path();
        let file_type = item
            .file_type()
            .map_err(|error| MemoryError::io(&path, error))?;
        if file_type.is_dir() {
            fs::remove_dir_all(&path).map_err(|error| MemoryError::io(&path, error))?;
        } else {
            fs::remove_file(&path).map_err(|error| MemoryError::io(&path, error))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Markdown body builders (parity with file-memory.ts §1.9)
// ---------------------------------------------------------------------------

fn build_user_body(candidate: &MemoryCandidate) -> String {
    let mut identity_background: Vec<String> = Vec::new();
    identity_background.extend(split_fact_text(&candidate.profile.join("；")));
    identity_background.extend(split_fact_text(
        &candidate.summary.clone().unwrap_or_default(),
    ));
    identity_background.extend(split_fact_text(&candidate.description));
    identity_background.extend(candidate.relationships.iter().cloned());
    let identity_background = unique_strings(&identity_background);
    let mut lines: Vec<String> = Vec::new();
    if !identity_background.is_empty() {
        lines.push("## 身份背景".to_string());
        for item in &identity_background {
            lines.push(format!("- {item}"));
        }
        lines.push(String::new());
    }
    if lines.is_empty() {
        lines.push("## 身份背景".to_string());
        lines.push("- 暂无稳定用户画像信息。".to_string());
        lines.push(String::new());
    }
    format!("{}\n", lines.join("\n").trim())
}

fn build_project_body(candidate: &MemoryCandidate) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(stage) = candidate.stage.as_deref().map(normalize_whitespace) {
        if !stage.is_empty() {
            lines.push("## Current Stage".to_string());
            lines.push(stage);
            lines.push(String::new());
        }
    }
    let sections: [(&str, &Vec<String>); 6] = [
        ("Decisions", &candidate.decisions),
        ("Constraints", &candidate.constraints),
        ("Next Steps", &candidate.next_steps),
        ("Blockers", &candidate.blockers),
        ("Timeline", &candidate.timeline),
        ("Notes", &candidate.notes),
    ];
    for (title, values) in sections {
        let normalized = unique_strings(values);
        if normalized.is_empty() {
            continue;
        }
        lines.push(format!("## {title}"));
        for item in &normalized {
            lines.push(format!("- {item}"));
        }
        lines.push(String::new());
    }
    if let Some(summary) = candidate.summary.as_deref().map(normalize_whitespace) {
        if !summary.is_empty() {
            lines.push("## Summary".to_string());
            lines.push(summary);
            lines.push(String::new());
        }
    }
    format!("{}\n", lines.join("\n").trim())
}

fn build_feedback_body(candidate: &MemoryCandidate) -> String {
    let rule = first_non_empty(&[
        candidate.rule.as_deref(),
        Some(candidate.description.as_str()),
        candidate.summary.as_deref(),
        Some(&candidate.name),
    ]);
    let mut lines: Vec<String> = vec![
        "## Rule".to_string(),
        normalize_whitespace(&rule),
        String::new(),
    ];
    if let Some(why) = candidate.why.as_deref().map(normalize_whitespace) {
        if !why.is_empty() {
            lines.push("## Why".to_string());
            lines.push(why);
            lines.push(String::new());
        }
    }
    if let Some(how_to_apply) = candidate.how_to_apply.as_deref().map(normalize_whitespace) {
        if !how_to_apply.is_empty() {
            lines.push("## How To Apply".to_string());
            lines.push(how_to_apply);
            lines.push(String::new());
        }
    }
    let notes = unique_strings(&candidate.notes);
    if !notes.is_empty() {
        lines.push("## Notes".to_string());
        for note in &notes {
            lines.push(format!("- {note}"));
        }
        lines.push(String::new());
    }
    format!("{}\n", lines.join("\n").trim())
}

fn build_general_project_meta_body(candidate: &MemoryCandidate) -> String {
    let mut lines: Vec<String> = vec![
        "## Summary".to_string(),
        normalize_description(&candidate.description, &candidate.name),
        String::new(),
        "## Status".to_string(),
        candidate
            .stage
            .as_deref()
            .map(normalize_whitespace)
            .filter(|status| !status.is_empty())
            .unwrap_or_else(|| DEFAULT_PROJECT_STATUS.to_string()),
        String::new(),
    ];
    let source_notes = unique_strings(&[
        if candidate.source_kind.as_deref() == Some("workspace_external_mirror") {
            "This project mirrors a read-only external workspace project.".to_string()
        } else {
            String::new()
        },
        candidate
            .source_workspace_path
            .as_deref()
            .map(|path| format!("Source workspace: {path}"))
            .unwrap_or_default(),
        candidate
            .source_project_id
            .as_deref()
            .map(|id| format!("Source project id: {id}"))
            .unwrap_or_default(),
    ]);
    if !source_notes.is_empty() {
        lines.push("## Source".to_string());
        for note in &source_notes {
            lines.push(format!("- {note}"));
        }
        lines.push(String::new());
    }
    format!("{}\n", lines.join("\n").trim())
}

fn build_record_body(candidate: &MemoryCandidate) -> String {
    if !normalize_whitespace(&candidate.body).is_empty() {
        return format!("{}\n", candidate.body.trim());
    }
    match candidate.kind {
        MemoryRecordType::GeneralProjectMeta => build_general_project_meta_body(candidate),
        MemoryRecordType::User => build_user_body(candidate),
        MemoryRecordType::Feedback => build_feedback_body(candidate),
        MemoryRecordType::Project => build_project_body(candidate),
    }
}

fn candidate_description(candidate: &MemoryCandidate) -> String {
    match candidate.kind {
        MemoryRecordType::User => {
            let fallback = if candidate.profile.is_empty() {
                candidate
                    .summary
                    .clone()
                    .unwrap_or_else(|| candidate.name.clone())
            } else {
                candidate.profile.join("；")
            };
            normalize_description(&candidate.description, &fallback)
        }
        MemoryRecordType::Feedback => {
            let fallback = first_non_empty(&[
                candidate.rule.as_deref(),
                candidate.summary.as_deref(),
                Some(&candidate.name),
            ]);
            normalize_description(&candidate.description, &fallback)
        }
        _ => {
            let fallback = first_non_empty(&[
                candidate.summary.as_deref(),
                candidate.stage.as_deref(),
                candidate.blockers.first().map(String::as_str),
                candidate.decisions.first().map(String::as_str),
                Some(&candidate.name),
            ]);
            normalize_description(&candidate.description, &fallback)
        }
    }
}

fn build_frontmatter(
    candidate: &MemoryCandidate,
    existing: Option<&MemoryManifestEntry>,
    now: &str,
) -> MemoryFileFrontmatter {
    let scope = if candidate.kind == MemoryRecordType::User {
        MemoryScope::Global
    } else {
        candidate.scope
    };
    let project_id = if scope == MemoryScope::Project {
        let candidate_id = normalize_whitespace(candidate.project_id.as_deref().unwrap_or(""));
        let existing_id = existing
            .map(|entry| {
                normalize_whitespace(entry.frontmatter.project_id.as_deref().unwrap_or(""))
            })
            .unwrap_or_default();
        Some(if !candidate_id.is_empty() {
            candidate_id
        } else if !existing_id.is_empty() {
            existing_id
        } else {
            CURRENT_PROJECT_ID.to_string()
        })
    } else {
        None
    };
    let source_kind = if candidate.kind == MemoryRecordType::GeneralProjectMeta {
        candidate
            .source_kind
            .clone()
            .or_else(|| existing.and_then(|entry| entry.frontmatter.source_kind.clone()))
    } else {
        existing.and_then(|entry| entry.frontmatter.source_kind.clone())
    };
    let source_workspace_path = if candidate.kind == MemoryRecordType::GeneralProjectMeta {
        candidate
            .source_workspace_path
            .clone()
            .or_else(|| existing.and_then(|entry| entry.frontmatter.source_workspace_path.clone()))
    } else {
        existing.and_then(|entry| entry.frontmatter.source_workspace_path.clone())
    };
    let source_project_id = if candidate.kind == MemoryRecordType::GeneralProjectMeta {
        candidate
            .source_project_id
            .clone()
            .or_else(|| existing.and_then(|entry| entry.frontmatter.source_project_id.clone()))
    } else {
        existing.and_then(|entry| entry.frontmatter.source_project_id.clone())
    };
    MemoryFileFrontmatter {
        name: {
            let candidate_name = normalize_whitespace(&candidate.name);
            let existing_name = existing
                .map(|entry| normalize_whitespace(&entry.frontmatter.name))
                .unwrap_or_default();
            if !candidate_name.is_empty() {
                candidate_name
            } else if !existing_name.is_empty() {
                existing_name
            } else {
                "memory-item".to_string()
            }
        },
        description: {
            let candidate_description = candidate_description(candidate);
            let existing_description = existing
                .map(|entry| normalize_whitespace(&entry.frontmatter.description))
                .unwrap_or_default();
            if !candidate_description.is_empty() {
                candidate_description
            } else if !existing_description.is_empty() {
                existing_description
            } else {
                "memory-item".to_string()
            }
        },
        kind: candidate.kind,
        scope,
        project_id,
        source_kind,
        source_workspace_path,
        source_project_id,
        updated_at: now.to_string(),
        dream_updated_at: existing.and_then(|entry| entry.frontmatter.dream_updated_at.clone()),
        captured_at: candidate
            .captured_at
            .clone()
            .or_else(|| existing.and_then(|entry| entry.frontmatter.captured_at.clone())),
        source_session_key: candidate
            .source_session_key
            .clone()
            .or_else(|| existing.and_then(|entry| entry.frontmatter.source_session_key.clone())),
        deprecated: existing
            .map(|entry| entry.frontmatter.deprecated)
            .unwrap_or(false),
        dream_attempts: existing
            .map(|entry| entry.frontmatter.dream_attempts)
            .unwrap_or(0),
    }
}

fn same_origin(entry: &MemoryManifestEntry, candidate: &MemoryCandidate) -> bool {
    let Some(captured_at) = candidate.captured_at.as_deref() else {
        return false;
    };
    let Some(source_session_key) = candidate.source_session_key.as_deref() else {
        return false;
    };
    entry.frontmatter.captured_at.as_deref() == Some(captured_at)
        && entry.frontmatter.source_session_key.as_deref() == Some(source_session_key)
        && entry.frontmatter.kind == candidate.kind
}

// ---------------------------------------------------------------------------
// Structured markdown parsing (edit path)
// ---------------------------------------------------------------------------

fn parse_markdown_sections(body: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    use std::collections::BTreeMap;
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut bucket: Vec<String> = Vec::new();
    for line in body.trim().replace("\r\n", "\n").split('\n') {
        if let Some(heading) = parse_heading(line) {
            if let Some(name) = current.take() {
                sections.insert(name, std::mem::take(&mut bucket));
            }
            current = Some(heading);
            continue;
        }
        if current.is_some() {
            bucket.push(line.to_string());
        }
    }
    if let Some(name) = current {
        sections.insert(name, bucket);
    }
    sections
}

fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("##")?;
    // `^##\s+(.+?)\s*$`: the heading text is trimmed and lower-cased.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_lowercase())
}

fn parse_list_section(lines: Option<&[String]>) -> Vec<String> {
    let Some(lines) = lines else {
        return Vec::new();
    };
    let values: Vec<String> = lines
        .iter()
        .map(|line| {
            line.trim_start_matches(|character: char| character == '-' || character.is_whitespace())
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect();
    unique_strings(&values)
}

fn parse_paragraph_section(lines: Option<&[String]>) -> String {
    let Some(lines) = lines else {
        return String::new();
    };
    normalize_whitespace(lines.join(" ").trim())
}

fn parse_fact_section(lines: Option<&[String]>) -> Vec<String> {
    let Some(lines) = lines else {
        return Vec::new();
    };
    let mut facts: Vec<String> = Vec::new();
    for line in lines {
        let stripped = line
            .trim_start_matches(|character: char| character == '-' || character.is_whitespace())
            .trim();
        if stripped.is_empty() {
            continue;
        }
        if line.trim_start().starts_with('-') {
            facts.push(stripped.to_string());
        } else {
            facts.extend(split_fact_text(stripped));
        }
    }
    unique_strings(&facts)
}

fn split_fact_text(text: &str) -> Vec<String> {
    let normalized = text.replace('\r', "\n");
    let mut facts: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in normalized.chars() {
        if character == '\n'
            || matches!(character, '，' | ',' | '；' | ';' | '。' | '.' | '!' | '?')
        {
            let piece = normalize_whitespace(&current);
            if piece.chars().count() >= 2 {
                facts.push(piece);
            }
            current.clear();
        } else {
            current.push(character);
        }
    }
    let piece = normalize_whitespace(&current);
    if piece.chars().count() >= 2 {
        facts.push(piece);
    }
    unique_strings(&facts)
}

fn candidate_from_document(frontmatter: &MemoryFileFrontmatter, body: &str) -> MemoryCandidate {
    let sections = parse_markdown_sections(body);
    let mut candidate = MemoryCandidate {
        kind: frontmatter.kind,
        scope: if frontmatter.kind == MemoryRecordType::User {
            MemoryScope::Global
        } else {
            frontmatter.scope
        },
        project_id: frontmatter.project_id.clone(),
        source_kind: frontmatter.source_kind.clone(),
        source_workspace_path: frontmatter.source_workspace_path.clone(),
        source_project_id: frontmatter.source_project_id.clone(),
        name: frontmatter.name.clone(),
        description: frontmatter.description.clone(),
        body: body.to_string(),
        captured_at: frontmatter.captured_at.clone(),
        source_session_key: frontmatter.source_session_key.clone(),
        ..Default::default()
    };
    match frontmatter.kind {
        MemoryRecordType::GeneralProjectMeta => {
            candidate.stage = Some(parse_paragraph_section(
                sections.get("status").map(Vec::as_slice),
            ));
        }
        MemoryRecordType::User => {
            let identity_facts = parse_fact_section(sections.get("身份背景").map(Vec::as_slice));
            candidate.profile = identity_facts.clone();
            candidate.relationships = identity_facts;
        }
        MemoryRecordType::Feedback => {
            candidate.rule = Some(parse_paragraph_section(
                sections.get("rule").map(Vec::as_slice),
            ));
            candidate.why = Some(parse_paragraph_section(
                sections.get("why").map(Vec::as_slice),
            ));
            candidate.how_to_apply = Some(parse_paragraph_section(
                sections.get("how to apply").map(Vec::as_slice),
            ));
            candidate.notes = parse_list_section(sections.get("notes").map(Vec::as_slice));
        }
        MemoryRecordType::Project => {
            candidate.stage = Some(parse_paragraph_section(
                sections.get("current stage").map(Vec::as_slice),
            ));
            candidate.decisions = parse_list_section(sections.get("decisions").map(Vec::as_slice));
            candidate.constraints =
                parse_list_section(sections.get("constraints").map(Vec::as_slice));
            candidate.next_steps =
                parse_list_section(sections.get("next steps").map(Vec::as_slice));
            candidate.blockers = parse_list_section(sections.get("blockers").map(Vec::as_slice));
            candidate.timeline = parse_list_section(sections.get("timeline").map(Vec::as_slice));
            candidate.notes = parse_list_section(sections.get("notes").map(Vec::as_slice));
            candidate.summary = Some(parse_paragraph_section(
                sections.get("summary").map(Vec::as_slice),
            ));
        }
    }
    candidate
}

fn general_meta_record_from_file(record: &MemoryFileRecord) -> Option<ProjectMetaRecord> {
    if record.entry.frontmatter.kind != MemoryRecordType::GeneralProjectMeta {
        return None;
    }
    let sections = parse_markdown_sections(&record.content);
    Some(ProjectMetaRecord {
        project_id: record
            .entry
            .frontmatter
            .project_id
            .clone()
            .unwrap_or_else(|| CURRENT_PROJECT_ID.to_string()),
        project_name: record.entry.frontmatter.name.clone(),
        description: normalize_description(
            &record.entry.frontmatter.description,
            &record.entry.frontmatter.name,
        ),
        status: {
            let status = parse_paragraph_section(sections.get("status").map(Vec::as_slice));
            if status.is_empty() {
                DEFAULT_PROJECT_STATUS.to_string()
            } else {
                status
            }
        },
        source_kind: record.entry.frontmatter.source_kind.clone(),
        source_workspace_path: record.entry.frontmatter.source_workspace_path.clone(),
        source_project_id: record.entry.frontmatter.source_project_id.clone(),
        created_at: record
            .entry
            .frontmatter
            .captured_at
            .clone()
            .unwrap_or_else(|| record.entry.frontmatter.updated_at.clone()),
        updated_at: record.entry.frontmatter.updated_at.clone(),
        dream_updated_at: record.entry.frontmatter.dream_updated_at.clone(),
        relative_path: record.entry.relative_path.clone(),
        absolute_path: record.entry.absolute_path.clone(),
    })
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// `sha1(seed)` first 10 lowercase hex chars (`hashText` contract).
pub fn hash10(seed: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(seed.as_bytes());
    format_hash10(&hex::encode(hasher.finalize()))
}

/// `slugify` from `file-memory.ts:94-100`.
///
/// Lowercase; keep `[a-z0-9]` and CJK unified ideographs (`U+4E00..=U+9FFF`),
/// collapse every other character into a single `-`, trim leading/trailing
/// `-`; falls back to `memory-item`.
pub fn slugify(value: &str) -> String {
    let normalized = normalize_whitespace(value).to_lowercase();
    let mut slug = String::with_capacity(normalized.len());
    for character in normalized.chars() {
        let keep = character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || ('\u{4E00}'..='\u{9FFF}').contains(&character);
        if keep {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "memory-item".to_string()
    } else {
        slug
    }
}

/// Trim/render content for recall: at most `max_lines` lines, with a `\n...`
/// suffix when truncated (`file-memory.ts:119-124`).
fn trim_content_lines(content: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let normalized = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }
    format!("{}\n...", lines[..max_lines].join("\n"))
}

/// Manifest preview: strip leading `#+ ` markers, collapse whitespace, and
/// truncate to 220 chars with `...` (`file-memory.ts:126-130`).
fn preview_content(content: &str) -> String {
    let mut stripped = String::with_capacity(content.len());
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        let mut hashes = 0;
        while let Some('#') = chars.clone().next() {
            chars.next();
            hashes += 1;
        }
        if hashes > 0 && chars.next().map(char::is_whitespace).unwrap_or(false) {
            stripped.push_str(chars.as_str());
        } else {
            stripped.push_str(line);
        }
        stripped.push('\n');
    }
    let normalized = normalize_whitespace(&stripped);
    if normalized.chars().count() <= PREVIEW_MAX_CHARS {
        return normalized;
    }
    let mut preview: String = normalized.chars().take(PREVIEW_MAX_CHARS).collect();
    preview.push_str("...");
    preview
}

fn unique_strings(values: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut result: Vec<String> = Vec::new();
    for value in values {
        let normalized = normalize_whitespace(value);
        if normalized.is_empty() || seen.contains(&normalized) {
            continue;
        }
        seen.push(normalized.clone());
        result.push(normalized);
        if result.len() >= UNIQUE_STRINGS_MAX {
            break;
        }
    }
    result
}

fn first_non_empty(values: &[Option<&str>]) -> String {
    for value in values.iter().flatten() {
        let normalized = normalize_whitespace(value);
        if !normalized.is_empty() {
            return normalized;
        }
    }
    String::new()
}

fn sort_entries(entries: &mut [MemoryManifestEntry]) {
    entries.sort_by(|left, right| {
        right
            .frontmatter
            .updated_at
            .cmp(&left.frontmatter.updated_at)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
}

fn dedupe_files(files: Vec<(String, String)>) -> Vec<(String, Vec<u8>)> {
    let mut deduped: Vec<(String, Vec<u8>)> = Vec::new();
    for (relative_path, content) in files {
        if let Some(existing) = deduped.iter_mut().find(|(path, _)| path == &relative_path) {
            existing.1 = content.into_bytes();
        } else {
            deduped.push((relative_path, content.into_bytes()));
        }
    }
    deduped.sort_by(|left, right| left.0.cmp(&right.0));
    deduped
}

fn render_project_meta(record: &ProjectMetaRecord) -> String {
    let mut lines: Vec<String> = vec![
        "---".to_string(),
        format!("project_id: {}", record.project_id),
        format!("project_name: {}", record.project_name),
        format!("description: {}", record.description),
        format!("status: {}", record.status),
    ];
    for (key, value) in [
        ("source_kind", record.source_kind.as_deref()),
        (
            "source_workspace_path",
            record.source_workspace_path.as_deref(),
        ),
        ("source_project_id", record.source_project_id.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            lines.push(format!("{key}: {value}"));
        }
    }
    lines.push(format!("created_at: {}", record.created_at));
    lines.push(format!("updated_at: {}", record.updated_at));
    if let Some(dream_updated_at) = record
        .dream_updated_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("dream_updated_at: {dream_updated_at}"));
    }
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("## Summary".to_string());
    lines.push(record.description.clone());
    lines.join("\n")
}

fn now_iso() -> String {
    crate::ports::format_iso(chrono::Utc::now())
}

fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{counter}")
}

fn unique_sibling(path: &Path, tag: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "memory".to_string());
    // The target name may already be close to the limit; shorten the copy of
    // it so the sibling stays creatable.
    let suffix = format!(".{tag}-{}", unique_id());
    let budget = MAX_FILE_NAME_BYTES.saturating_sub(1 + suffix.len());
    path.with_file_name(format!(".{}{suffix}", truncate_bytes(&name, budget)))
}

/// `<slug><suffix>`, with the slug cut so the name fits in
/// [`MAX_FILE_NAME_BYTES`]. Names that already fit are unchanged.
fn bounded_file_name(slug: &str, suffix: &str) -> String {
    let budget = MAX_FILE_NAME_BYTES.saturating_sub(suffix.len());
    let slug = truncate_bytes(slug, budget).trim_end_matches('-');
    format!("{slug}{suffix}")
}

/// Cut `value` to at most `max_bytes` bytes on a character boundary.
fn truncate_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent).map_err(|error| MemoryError::io(&parent, error))?;
    let temp = unique_sibling(path, "tmp");
    fs::write(&temp, contents).map_err(|error| MemoryError::io(&temp, error))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(MemoryError::io(path, error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_bytes_backs_off_to_a_character_boundary() {
        assert_eq!(truncate_bytes("abc", 5), "abc");
        // "长" is 3 bytes: a 4-byte budget keeps one character, not a fragment.
        assert_eq!(truncate_bytes("长长", 4), "长");
        assert_eq!(truncate_bytes("长", 2), "");
    }

    #[test]
    fn bounded_file_name_keeps_fitting_names_and_trims_a_dangling_dash() {
        assert_eq!(
            bounded_file_name("boreal", "-0123456789.md"),
            "boreal-0123456789.md"
        );
        let suffix = "-0123456789.md";
        let budget = MAX_FILE_NAME_BYTES - suffix.len();
        // The cut lands right after a '-', which must not double up with the suffix.
        let slug = format!("{}-{}", "a".repeat(budget - 1), "b".repeat(10));
        let name = bounded_file_name(&slug, suffix);
        assert_eq!(name, format!("{}{suffix}", "a".repeat(budget - 1)));
        assert!(name.len() <= MAX_FILE_NAME_BYTES);
    }
}
