//! Dream consolidation: cluster -> merge -> project meta -> user profile (dev B2).
//!
//! Port of `dream-review.ts` for the single-workspace mode this SDK build
//! supports (see behavior spec §9.2: `general` multi-project mode is out of
//! scope). Runs on a staged copy; the engine commits via
//! [`crate::ports::MemoryStore::commit_stage`], which also installs the rollback
//! snapshot. Dream never touches the live store from here.
//!
//! Pipeline:
//! 1. no-op check (success without committing a staged tree);
//! 2. per kind (project/feedback): cluster <=200 headers with the model, then
//!    merge each accepted cluster (<=8 files) into one rewritten note;
//! 3. calibrate `project.meta.md` conservatively from <=5 project + <=5 feedback
//!    evidence notes;
//! 4. rewrite the global user profile from <=200 notes / 120k chars and absorb
//!    the notes only when the profile body actually changed.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::MemoryConfig;
use crate::error::Result;
use crate::extraction::complete_json;
use crate::ports::{Clock, MemoryStore, ModelProvider, Telemetry};
use crate::types::*;

/// Dream prompt schema names (frozen contract with adapters and E2E mocks).
pub mod schemas {
    /// Cluster planning: `{"summary", "clusters":[{"member_relative_paths","reason"}]}`.
    pub const CLUSTER: &str = "dream_cluster";
    /// Cluster merge/refine: `{"summary","name","description","markdown"}`.
    pub const MERGE: &str = "dream_merge";
    /// User profile rewrite: `{"identity_background_markdown": "- ..."}`.
    pub const USER_PROFILE: &str = "dream_user_profile";
    /// Project meta review: `{"should_update", "reason", "project_name", "description", "status"}`.
    pub const PROJECT_META: &str = "dream_project_meta";
}

/// Snapshot metadata written by `commit_stage` (kept in sync with A's impl).
pub const LAST_DREAM_DIR: &str = "last_dream";
/// Snapshot metadata file inside [`LAST_DREAM_DIR`].
pub const LAST_DREAM_METADATA_FILE: &str = "metadata.json";

/// Maximum headers scanned per category, matching `dream-review.ts`.
pub const DREAM_HEADER_SCAN_LIMIT: usize = 200;
/// Maximum files a single LLM cluster may contain.
pub const DREAM_CLUSTER_MAX_FILES: usize = 8;
/// Evidence project notes fed to the project-meta review.
pub const DREAM_META_PROJECT_CONTEXT_LIMIT: usize = 5;
/// Evidence feedback notes fed to the project-meta review.
pub const DREAM_META_FEEDBACK_CONTEXT_LIMIT: usize = 5;
/// Maximum user notes considered for the profile rewrite.
pub const DREAM_USER_NOTE_MAX_FILES: usize = 200;
/// Character budget for user notes considered for the profile rewrite.
pub const DREAM_USER_NOTE_CHAR_BUDGET: usize = 120_000;
/// Fixed relative path of the rewritten global user profile.
pub const DREAM_USER_PROFILE_RELATIVE_PATH: &str = "UserIdentity/user-profile.md";
/// Directory prefix (internal id form) of append-only user notes.
pub const DREAM_USER_NOTE_DIR: &str = "UserIdentityNotes/";
/// Fixed candidate name of the global user profile file.
pub const DREAM_USER_PROFILE_NAME: &str = "user-profile";
/// Maximum profile facts rendered into the rewritten profile candidate.
pub const DREAM_PROFILE_FACTS_MAX: usize = 20;

/// Maximum member ids parsed out of one model cluster before validation.
const DREAM_CLUSTER_MEMBER_LIMIT: usize = 32;
/// Cluster reason length cap.
const DREAM_REASON_MAX_CHARS: usize = 320;
/// Refined note name length cap.
const DREAM_REFINED_NAME_MAX_CHARS: usize = 120;
/// Refined note description length cap.
const DREAM_REFINED_DESCRIPTION_MAX_CHARS: usize = 320;
/// Existing profile markdown fed to the rewrite prompt.
const DREAM_USER_PROFILE_EXISTING_MAX_CHARS: usize = 3_200;
/// Single user note markdown fed to the rewrite prompt.
const DREAM_USER_NOTE_MARKDOWN_MAX_CHARS: usize = 1_400;
/// Single user note description fed to the rewrite prompt.
const DREAM_USER_NOTE_DESCRIPTION_MAX_CHARS: usize = 180;
/// Full-text read depth for dream evidence (TS reads 5000 lines).
const DREAM_RECORD_READ_MAX_LINES: usize = 5_000;
/// Project id used in single-workspace mode.
const DREAM_CURRENT_PROJECT_ID: &str = "current_project";
/// Fixed relative path of the current project metadata file.
const DREAM_PROJECT_META_FILE: &str = "project.meta.md";
/// Default project status when no metadata exists yet.
const DREAM_DEFAULT_PROJECT_STATUS: &str = "in_progress";
/// Default project name when no metadata exists and no note names a project.
const DREAM_DEFAULT_PROJECT_NAME: &str = "Current Project";
/// Default project description when no metadata exists.
const DREAM_DEFAULT_PROJECT_DESCRIPTION: &str = "Current project memory.";
/// LLM timeouts, matching `llm-extraction.ts` constants.
const CLUSTER_PLAN_TIMEOUT_MS: u64 = 180_000;
const CLUSTER_REFINE_TIMEOUT_MS: u64 = 180_000;
const PROJECT_META_REVIEW_TIMEOUT_MS: u64 = 120_000;
const USER_PROFILE_REWRITE_TIMEOUT_MS: u64 = 45_000;

// ---------------------------------------------------------------------------
// Prompts (re-authored from `llm-extraction.ts`; wording kept faithful because
// consolidation quality is product behaviour)
// ---------------------------------------------------------------------------

fn build_cluster_plan_system_prompt(kind: MemoryRecordType) -> String {
    let kind_label = if kind == MemoryRecordType::Feedback {
        "Feedback"
    } else {
        "Project"
    };
    let category_description = if kind == MemoryRecordType::Feedback {
        "Feedback memory files capture durable collaboration rules, delivery rules, style rules, title/body template rules, and confirmed output constraints."
    } else {
        "Project memory files capture durable project facts such as project definition, scope, goals, blockers, risks, and important progress."
    };
    format!(
        r#"You are the {kind_label} Dream cluster planner for a file-memory system.

Your job is to inspect lightweight header information only and decide which files should be refined together.

Rules:
- Use only the supplied header metadata as evidence.
- Do not assume full file contents beyond what the header says.
- {category_description}
- Return mutually exclusive candidate clusters only.
- A file may appear in at most one cluster.
- Only create a cluster when at least two files likely overlap, conflict, or should be merged into one cleaner memory file.
- If files are distinct and should remain separate, leave them out of clusters.
- Files belonging to the same current project is not, by itself, a merge reason.
- Shared workspace, shared project membership, shared domain, or shared topic is not enough unless the headers show concrete semantic overlap, fact conflict, rule duplication, or obvious consolidation value.
- Each cluster reason must name the specific overlap, conflict, repeated rule, repeated fact, or consolidation topic that justifies refinement.
- Keep reasons concise and specific.
- Natural-language output should follow the dominant language already visible in the supplied headers.
- Return valid JSON only.

Use this exact JSON shape:
{{
  "summary": "short planning summary",
  "clusters": [
    {{
      "member_relative_paths": ["Project/a.md", "Project/b.md"],
      "reason": "why these files should be refined together"
    }}
  ]
}}"#
    )
}

fn build_cluster_refine_system_prompt(kind: MemoryRecordType) -> String {
    let kind_label = if kind == MemoryRecordType::Feedback {
        "Feedback"
    } else {
        "Project"
    };
    let category_instruction = if kind == MemoryRecordType::Feedback {
        "Produce exactly one feedback memory file.\n- Keep durable collaboration rules only: delivery order, output structure, style constraints, title/body template guidance, and confirmed review preferences.\n- Prefer readable markdown headings such as ## Rule, ## Why, ## How To Apply, ## Notes when useful."
    } else {
        "Produce exactly one project memory file.\n- Keep durable project facts only: what the project is, stable scope, goals, important progress, blockers, risks, and important decisions.\n- Do not reduce the file to a vague status line.\n- Prefer readable markdown headings such as ## Summary, ## Current Stage, ## Constraints, ## Blockers, ## Next Steps, ## Timeline, ## Notes when useful."
    };
    format!(
        r#"You are the {kind_label} Dream refine engine for a file-memory system.

Your job is to merge one cluster of existing memory files into exactly one cleaner memory file.

Rules:
- Use only the supplied full file contents as evidence.
- Resolve overlap, deduplicate repeated details, and keep the most useful durable facts.
- Do not invent new facts.
- Output exactly one refined file.
- The visible output language must follow the dominant language already present in the supplied files. If the supplied files are mixed, prefer the dominant language of the cluster.
- Apply this language rule consistently to the title/name, description, markdown headings, and markdown body text.
- {category_instruction}
- Return valid JSON only.

Use this exact JSON shape:
{{
  "summary": "short refine summary",
  "name": "refined file title",
  "description": "one-line description",
  "markdown": "full markdown body"
}}"#
    )
}

/// System prompt for project metadata calibration.
const PROJECT_META_REVIEW_SYSTEM_PROMPT: &str = r#"You are the Dream project metadata reviewer for a file-memory system.

Your job is to decide whether the current project metadata is clearly incorrect or outdated after project/feedback refinement.

Rules:
- Use only the supplied current metadata and the recent project/feedback files as evidence.
- Be conservative. Keep the current metadata unless the supplied evidence clearly supports a change.
- You may update only:
  - project_name
  - description
  - status
- Do not rewrite metadata just to paraphrase it.
- Natural-language output fields must follow the dominant language already present in the supplied project/feedback files.
- Return valid JSON only.

Use this exact JSON shape:
{
  "should_update": false,
  "reason": "why metadata should or should not change",
  "project_name": "final project name",
  "description": "final description",
  "status": "in_progress"
}"#;

/// System prompt for the global user profile rewrite.
const USER_PROFILE_REWRITE_SYSTEM_PROMPT: &str = r###"You rewrite the single "身份背景" section of a global user profile for a conversational memory system.

Rules:
- Return JSON only.
- The existing profile markdown is the previous draft. The incoming user notes are the newest evidence.
- Rewrite the section from scratch. Do not append blindly, and do not keep duplicate or near-duplicate facts just because they already exist.
- Keep only durable personal identity/background information that should persist across future sessions.
- If old profile content conflicts with newer, clearer incoming evidence, prefer the newer evidence and rewrite the section accordingly.
- If the incoming evidence only describes reply preferences, formatting habits, style choices, language choices, file/tool boundaries, or project collaboration rules, do not include them in the rewritten section.
- Do not include project progress, project-specific collaboration rules, deadlines, blockers, or temporary tasks.
- Keep the language aligned with the user's language in the incoming content.
- "identity_background_markdown" must contain only the markdown content that belongs under the "## 身份背景" heading.
- Do not include the heading itself.
- Prefer concise bullet-list markdown when possible.

Use this exact JSON shape:
{
  "identity_background_markdown": "- ..."
}"###;

// ---------------------------------------------------------------------------
// Prompt payloads (struct field order matches the TS object literals)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ClusterPlanPrompt<'a> {
    category: &'a str,
    headers: Vec<ClusterHeader<'a>>,
}

#[derive(Debug, Serialize)]
struct ClusterHeader<'a> {
    relative_path: &'a str,
    name: String,
    description: String,
    updated_at: &'a str,
}

#[derive(Debug, Serialize)]
struct RefinePrompt<'a> {
    category: &'a str,
    records: Vec<RefineRecord<'a>>,
}

#[derive(Debug, Serialize)]
struct RefineRecord<'a> {
    entry_id: &'a str,
    relative_path: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    name: &'a str,
    description: String,
    updated_at: &'a str,
    captured_at: &'a str,
    source_session_key: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct MetaReviewPrompt<'a> {
    current_project_meta: MetaPrompt<'a>,
    recent_project_files: Vec<MetaFile<'a>>,
    recent_feedback_files: Vec<MetaFile<'a>>,
}

#[derive(Debug, Serialize)]
struct MetaPrompt<'a> {
    project_id: &'a str,
    project_name: &'a str,
    description: String,
    status: &'a str,
    updated_at: &'a str,
    dream_updated_at: &'a str,
}

#[derive(Debug, Serialize)]
struct MetaFile<'a> {
    relative_path: &'a str,
    name: &'a str,
    description: String,
    updated_at: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct UserProfilePrompt<'a> {
    existing_profile_markdown: Option<String>,
    incoming_user_notes: Vec<IncomingUserNote<'a>>,
}

#[derive(Debug, Serialize)]
struct IncomingUserNote<'a> {
    description: String,
    note_markdown: String,
    captured_at: &'a str,
    source_session_key: &'a str,
}

// ---------------------------------------------------------------------------
// Internal result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DreamCluster {
    member_ids: Vec<String>,
    #[allow(dead_code)]
    reason: String,
}

#[derive(Debug, Default)]
struct CategoryDream {
    planned_clusters: usize,
    refined_clusters: usize,
    deleted_files: usize,
    dropped_warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct RefinedFile {
    name: String,
    description: String,
    markdown: String,
}

/// Run dream on the staged store and return the outcome (no commit).
///
/// `Skipped` means there was nothing to consolidate; `Success` means the staged
/// tree changed and the engine should commit it.
pub fn run_dream(
    _config: &MemoryConfig,
    model: &dyn ModelProvider,
    clock: &dyn Clock,
    telemetry: &dyn Telemetry,
    stage: &mut dyn MemoryStore,
    trigger: DreamTrigger,
) -> Result<DreamRunResult> {
    let started_at = clock.now_iso();
    let project_entries = list_kind(stage, MemoryRecordType::Project)?;
    let feedback_entries = list_kind(stage, MemoryRecordType::Feedback)?;
    let workspace_entries: Vec<MemoryManifestEntry> = project_entries
        .iter()
        .chain(feedback_entries.iter())
        .cloned()
        .collect();
    let user_entries = list_kind(stage, MemoryRecordType::User)?;
    let (profile_entry, user_note_entries) = split_user_entries(&user_entries);
    let workspace_records = load_records(stage, &workspace_entries)?;
    let user_note_records = load_records(stage, &user_note_entries)?;
    let profile_record = load_profile_record(stage, profile_entry.as_ref(), &user_entries)?;
    let reviewed_files = workspace_entries.len() + user_note_records.len();

    // The upstream runner seeds metadata on its staged store even when the
    // only evidence is global identity. No-op stages are still discarded.
    if stage.get_project_meta()?.is_none() {
        stage.set_project_meta(&seed_project_meta(
            &project_entries,
            &feedback_entries,
            &started_at,
        ))?;
    }

    // --- no-op check ------------------------------------------------------
    if workspace_entries.is_empty() && user_note_records.is_empty() && profile_record.is_none() {
        let summary =
            "No file-based memory exists yet, so Dream had nothing to organize.".to_string();
        let result = DreamRunResult {
            summary: summary.clone(),
            trigger,
            status: DreamPipelineStatus::Success,
            is_no_op: true,
            started_at,
            finished_at: clock.now_iso(),
            ..Default::default()
        };
        telemetry.event(
            "memory.dream",
            json!({
                "trigger": trigger,
                "status": result.status,
                "summary": summary,
                "no_op": true,
            }),
        );
        return Ok(result);
    }

    // --- 2. cluster + merge per category ---------------------------------
    let mut project_dream = run_category_dream(
        model,
        stage,
        MemoryRecordType::Project,
        &project_entries,
        &workspace_records,
    )?;
    let mut feedback_dream = run_category_dream(
        model,
        stage,
        MemoryRecordType::Feedback,
        &feedback_entries,
        &workspace_records,
    )?;

    // --- 3. project meta calibration -------------------------------------
    let meta_updated = run_project_meta_review(model, clock, stage)?;

    // --- 4. user profile rewrite -----------------------------------------
    let (profile_updated, absorbed_notes) =
        run_user_profile_rewrite(model, stage, &user_note_records, profile_record.as_ref())?;
    if !profile_updated {
        // Notes were kept; make the reason visible to the caller.
        log::debug!("[pd-memory] dream kept user notes because the profile did not change");
    }

    // --- 5. manifest repair ----------------------------------------------
    stage.rebuild_manifest()?;

    // --- 6. result --------------------------------------------------------
    let finished_at = clock.now_iso();
    let rewritten_projects = project_dream.refined_clusters + feedback_dream.refined_clusters;
    let deleted_files = project_dream.deleted_files + feedback_dream.deleted_files + absorbed_notes;
    let duplicate_topic_count = project_dream.planned_clusters + feedback_dream.planned_clusters;
    let conflict_topic_count =
        project_dream.dropped_warnings.len() + feedback_dream.dropped_warnings.len();
    let did_work = rewritten_projects > 0 || deleted_files > 0 || profile_updated || meta_updated;
    let summary = build_summary(
        rewritten_projects,
        deleted_files,
        profile_updated,
        meta_updated,
    );
    let result = DreamRunResult {
        reviewed_files,
        rewritten_projects,
        deleted_projects: 0,
        deleted_files,
        profile_updated,
        duplicate_topic_count,
        conflict_topic_count,
        summary: summary.clone(),
        trigger,
        status: DreamPipelineStatus::Success,
        is_no_op: !did_work,
        started_at,
        finished_at,
    };
    telemetry.event(
        "memory.dream",
        json!({
            "trigger": trigger,
            "status": result.status,
            "summary": summary,
            "reviewed_files": reviewed_files,
            "rewritten_projects": rewritten_projects,
            "deleted_files": deleted_files,
            "profile_updated": profile_updated,
            "meta_updated": meta_updated,
            "duplicate_topic_count": duplicate_topic_count,
            "conflict_topic_count": conflict_topic_count,
        }),
    );
    // Keep the per-category warnings inspectable without failing the run.
    for warning in project_dream
        .dropped_warnings
        .drain(..)
        .chain(feedback_dream.dropped_warnings.drain(..))
    {
        log::debug!("[pd-memory] dream warning: {warning}");
    }
    Ok(result)
}

fn build_summary(
    rewritten_projects: usize,
    deleted_files: usize,
    profile_updated: bool,
    meta_updated: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if rewritten_projects > 0 {
        parts.push(format!("Refined {rewritten_projects} dream clusters."));
    }
    if deleted_files > 0 {
        parts.push(format!("Deleted {deleted_files} absorbed source files."));
    }
    if profile_updated {
        parts.push("Updated the global user profile.".to_string());
    }
    if meta_updated {
        parts.push("Reviewed and updated the project metadata record.".to_string());
    }
    if parts.is_empty() {
        "Dream completed without requiring any memory changes.".to_string()
    } else {
        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// Category dream: cluster planning + merge
// ---------------------------------------------------------------------------

fn run_category_dream(
    model: &dyn ModelProvider,
    stage: &mut dyn MemoryStore,
    kind: MemoryRecordType,
    entries: &[MemoryManifestEntry],
    workspace_records: &[MemoryFileRecord],
) -> Result<CategoryDream> {
    let mut result = CategoryDream::default();
    let mut sorted_entries: Vec<MemoryManifestEntry> = entries.to_vec();
    sort_entries(&mut sorted_entries);
    sorted_entries.truncate(DREAM_HEADER_SCAN_LIMIT);
    if sorted_entries.len() < 2 {
        return Ok(result);
    }

    let prompt = ClusterPlanPrompt {
        category: kind.as_str(),
        headers: sorted_entries
            .iter()
            .map(|entry| ClusterHeader {
                relative_path: &entry.relative_path,
                name: truncate_for_prompt(&entry.frontmatter.name, 120),
                description: truncate_for_prompt(&entry.frontmatter.description, 220),
                updated_at: &entry.frontmatter.updated_at,
            })
            .collect(),
    };
    let user_prompt = serde_json::to_string_pretty(&prompt)?;
    let plan = match complete_json(
        model,
        schemas::CLUSTER,
        &build_cluster_plan_system_prompt(kind),
        &user_prompt,
        CLUSTER_PLAN_TIMEOUT_MS,
        None,
    ) {
        Ok(value) => value,
        Err(error) => {
            log::warn!(
                "[pd-memory] dream {} cluster planning failed: {error}",
                kind.as_str()
            );
            result
                .dropped_warnings
                .push(format!("Cluster planning failed: {error}"));
            return Ok(result);
        }
    };
    let allowed: HashSet<String> = sorted_entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect();
    let (clusters, dropped) =
        normalize_and_validate_clusters(&plan, &allowed, DREAM_CLUSTER_MAX_FILES);
    result.dropped_warnings.extend(dropped);
    result.planned_clusters = clusters.len();

    for cluster in clusters {
        let mut members: Vec<MemoryFileRecord> = cluster
            .member_ids
            .iter()
            .filter_map(|id| find_record(workspace_records, id).cloned())
            .collect();
        if members.len() < 2 {
            result.dropped_warnings.push(format!(
                "Skipped {} cluster because fewer than 2 files could be loaded: {}",
                kind.as_str(),
                cluster.member_ids.join(", ")
            ));
            continue;
        }
        sort_records(&mut members);
        let refine_prompt = RefinePrompt {
            category: kind.as_str(),
            records: members
                .iter()
                .map(|record| RefineRecord {
                    entry_id: &record.entry.relative_path,
                    relative_path: &record.entry.relative_path,
                    kind: record.entry.frontmatter.kind.as_str(),
                    name: &record.entry.frontmatter.name,
                    description: truncate_for_prompt(&record.entry.frontmatter.description, 220),
                    updated_at: &record.entry.frontmatter.updated_at,
                    captured_at: record
                        .entry
                        .frontmatter
                        .captured_at
                        .as_deref()
                        .unwrap_or(""),
                    source_session_key: record
                        .entry
                        .frontmatter
                        .source_session_key
                        .as_deref()
                        .unwrap_or(""),
                    content: &record.content,
                })
                .collect(),
        };
        let user_prompt = serde_json::to_string_pretty(&refine_prompt)?;
        let refined = match complete_json(
            model,
            schemas::MERGE,
            &build_cluster_refine_system_prompt(kind),
            &user_prompt,
            CLUSTER_REFINE_TIMEOUT_MS,
            None,
        ) {
            Ok(value) => value,
            Err(error) => {
                log::warn!(
                    "[pd-memory] dream {} cluster merge failed: {error}",
                    kind.as_str()
                );
                result
                    .dropped_warnings
                    .push(format!("Cluster merge failed: {error}"));
                continue;
            }
        };
        let Some(refined) = parse_refined_file(&refined) else {
            result.dropped_warnings.push(format!(
                "Dropped {} cluster because the model returned no usable refined file: {}",
                kind.as_str(),
                cluster.member_ids.join(", ")
            ));
            continue;
        };
        let newest = &members[0];
        let candidate = MemoryCandidate {
            kind,
            scope: MemoryScope::Project,
            name: refined.name,
            description: refined.description,
            body: format!("{}\n", refined.markdown.trim()),
            captured_at: newest.entry.frontmatter.captured_at.clone(),
            source_session_key: newest.entry.frontmatter.source_session_key.clone(),
            ..Default::default()
        };
        // TS parity: the single-project reference calls runCategoryDream without
        // project metadata, so the refined candidate carries no project_id. The
        // store still writes `project_id: current_project` into frontmatter, but
        // the filename hash seed uses the empty project_id segment.
        // TS parity: delete every member first, then write the merged note.
        stage.delete_entries(&cluster.member_ids)?;
        stage.upsert_candidate(&candidate)?;
        result.refined_clusters += 1;
        result.deleted_files += cluster.member_ids.len();
    }
    Ok(result)
}

fn normalize_and_validate_clusters(
    plan: &Value,
    allowed: &HashSet<String>,
    max_files: usize,
) -> (Vec<DreamCluster>, Vec<String>) {
    let mut accepted: Vec<DreamCluster> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let Some(items) = plan.get("clusters").and_then(Value::as_array) else {
        return (accepted, dropped);
    };
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let raw_members = object
            .get("member_relative_paths")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut members: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for member in raw_members.iter().filter_map(Value::as_str) {
            let member = normalize_whitespace(member);
            if member.is_empty() || !allowed.contains(&member) {
                continue;
            }
            if seen.insert(member.clone()) {
                members.push(member);
            }
            if members.len() >= DREAM_CLUSTER_MEMBER_LIMIT {
                break;
            }
        }
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .map(|value| truncate_raw(value, DREAM_REASON_MAX_CHARS))
            .unwrap_or_default();
        let label = if reason.is_empty() {
            members.join(", ")
        } else {
            reason.clone()
        };
        if members.len() < 2 {
            dropped.push(format!(
                "Dropped cluster because it had fewer than 2 valid files: {label}"
            ));
            continue;
        }
        if members.len() > max_files {
            dropped.push(format!(
                "Dropped cluster because it exceeded the {max_files}-file limit: {}",
                members.join(", ")
            ));
            continue;
        }
        if members.iter().any(|member| used.contains(member)) {
            dropped.push(format!(
                "Dropped overlapping cluster: {}",
                members.join(", ")
            ));
            continue;
        }
        if is_generic_same_project_reason(&reason) {
            dropped.push(format!(
                "Dropped low-quality cluster because the reason only referenced same-project membership without concrete overlap/conflict: {}",
                members.join(", ")
            ));
            continue;
        }
        for member in &members {
            used.insert(member.clone());
        }
        accepted.push(DreamCluster {
            member_ids: members,
            reason,
        });
    }
    (accepted, dropped)
}

fn is_generic_same_project_reason(reason: &str) -> bool {
    let normalized = normalize_whitespace(reason).to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    const SAME_PROJECT_PHRASES: &[&str] = &[
        "same current project",
        "same project",
        "same workspace",
        "belong to the same project",
        "同一个项目",
        "同一项目",
        "都属于同一个项目",
        "都属于同一项目",
        "属于当前项目",
        "同一个工作区",
    ];
    const SEMANTIC_PHRASES: &[&str] = &[
        "overlap",
        "duplicate",
        "redundant",
        "redundancy",
        "conflict",
        "inconsistent",
        "merge",
        "consolidat",
        "same rule",
        "same constraint",
        "same risk",
        "same blocker",
        "same goal",
        "same stage",
        "same definition",
        "same style",
        "same audience",
        "重复",
        "冗余",
        "冲突",
        "重叠",
        "可合并",
        "可统一",
        "内容重合",
        "语义重合",
        "事实重合",
        "相同规则",
        "相同约束",
        "相同风险",
        "相同阻塞",
        "相同目标",
        "相同阶段",
        "相同定义",
        "相同风格",
        "相同受众",
    ];
    (SAME_PROJECT_PHRASES
        .iter()
        .any(|phrase| normalized.contains(phrase))
        || normalized
            .split_once("同属")
            .is_some_and(|(_, rest)| rest.contains("项目")))
        && !SEMANTIC_PHRASES
            .iter()
            .any(|phrase| normalized.contains(phrase))
}

fn parse_refined_file(value: &Value) -> Option<RefinedFile> {
    let name = truncate_for_prompt(value.get("name").and_then(Value::as_str).unwrap_or(""), 120);
    let description = truncate_for_prompt(
        value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
        320,
    );
    let markdown = value
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() || description.is_empty() || markdown.is_empty() {
        return None;
    }
    Some(RefinedFile {
        name: truncate_raw(&name, DREAM_REFINED_NAME_MAX_CHARS),
        description: truncate_raw(&description, DREAM_REFINED_DESCRIPTION_MAX_CHARS),
        markdown,
    })
}

// ---------------------------------------------------------------------------
// Project meta calibration
// ---------------------------------------------------------------------------

fn run_project_meta_review(
    model: &dyn ModelProvider,
    clock: &dyn Clock,
    stage: &mut dyn MemoryStore,
) -> Result<bool> {
    // Re-list after cluster merges so the evidence matches the staged tree.
    let project_entries = list_kind(stage, MemoryRecordType::Project)?;
    let feedback_entries = list_kind(stage, MemoryRecordType::Feedback)?;
    if project_entries.is_empty() && feedback_entries.is_empty() {
        return Ok(false);
    }
    let recent_projects: Vec<MemoryManifestEntry> = project_entries
        .into_iter()
        .take(DREAM_META_PROJECT_CONTEXT_LIMIT)
        .collect();
    let recent_feedback: Vec<MemoryManifestEntry> = feedback_entries
        .into_iter()
        .take(DREAM_META_FEEDBACK_CONTEXT_LIMIT)
        .collect();
    let evidence_entries: Vec<MemoryManifestEntry> = recent_projects
        .iter()
        .chain(recent_feedback.iter())
        .cloned()
        .collect();
    let evidence_records = load_records(stage, &evidence_entries)?;
    let current_meta = stage.get_project_meta().unwrap_or(None);
    let now = clock.now_iso();
    let current_meta = match current_meta {
        Some(meta) => meta,
        None => seed_project_meta(&recent_projects, &recent_feedback, &now),
    };
    if evidence_records.is_empty() {
        return Ok(false);
    }
    let project_records: Vec<MetaFile<'_>> = recent_projects
        .iter()
        .filter_map(|entry| find_record(&evidence_records, &entry.relative_path))
        .map(|record| MetaFile {
            relative_path: &record.entry.relative_path,
            name: &record.entry.frontmatter.name,
            description: truncate_for_prompt(&record.entry.frontmatter.description, 220),
            updated_at: &record.entry.frontmatter.updated_at,
            content: &record.content,
        })
        .collect();
    let feedback_records: Vec<MetaFile<'_>> = recent_feedback
        .iter()
        .filter_map(|entry| find_record(&evidence_records, &entry.relative_path))
        .map(|record| MetaFile {
            relative_path: &record.entry.relative_path,
            name: &record.entry.frontmatter.name,
            description: truncate_for_prompt(&record.entry.frontmatter.description, 220),
            updated_at: &record.entry.frontmatter.updated_at,
            content: &record.content,
        })
        .collect();
    let prompt = MetaReviewPrompt {
        current_project_meta: MetaPrompt {
            project_id: &current_meta.project_id,
            project_name: &current_meta.project_name,
            description: truncate_for_prompt(&current_meta.description, 220),
            status: &current_meta.status,
            updated_at: &current_meta.updated_at,
            dream_updated_at: current_meta.dream_updated_at.as_deref().unwrap_or(""),
        },
        recent_project_files: project_records,
        recent_feedback_files: feedback_records,
    };
    let user_prompt = serde_json::to_string_pretty(&prompt)?;
    let review = match complete_json(
        model,
        schemas::PROJECT_META,
        PROJECT_META_REVIEW_SYSTEM_PROMPT,
        &user_prompt,
        PROJECT_META_REVIEW_TIMEOUT_MS,
        None,
    ) {
        Ok(value) => value,
        Err(error) => {
            log::warn!("[pd-memory] dream project meta review failed: {error}");
            return Ok(false);
        }
    };
    let should_update = normalize_bool(review.get("should_update"), false);
    if !should_update {
        return Ok(false);
    }
    let project_name = match review.get("project_name").and_then(Value::as_str) {
        Some(value) if !normalize_whitespace(value).is_empty() => {
            truncate_raw(&normalize_whitespace(value), 120)
        }
        _ => current_meta.project_name.clone(),
    };
    let description = match review.get("description").and_then(Value::as_str) {
        Some(value) if !normalize_whitespace(value).is_empty() => {
            truncate_raw(&normalize_whitespace(value), 320)
        }
        _ => current_meta.description.clone(),
    };
    // TS parity quirk: a present-but-empty status becomes "active".
    let status = match review.get("status").and_then(Value::as_str) {
        Some(value) => {
            let normalized = normalize_whitespace(value);
            if normalized.is_empty() {
                "active".to_string()
            } else {
                truncate_raw(&normalized, 80)
            }
        }
        None => current_meta.status.clone(),
    };
    if project_name == current_meta.project_name
        && description == current_meta.description
        && status == current_meta.status
    {
        return Ok(false);
    }
    let next_meta = ProjectMetaRecord {
        project_id: current_meta.project_id.clone(),
        project_name,
        description,
        status,
        source_kind: current_meta.source_kind.clone(),
        source_workspace_path: current_meta.source_workspace_path.clone(),
        source_project_id: current_meta.source_project_id.clone(),
        created_at: current_meta.created_at.clone(),
        updated_at: now.clone(),
        dream_updated_at: Some(now),
        relative_path: if current_meta.relative_path.is_empty() {
            DREAM_PROJECT_META_FILE.to_string()
        } else {
            current_meta.relative_path.clone()
        },
        absolute_path: current_meta.absolute_path.clone(),
    };
    stage.set_project_meta(&next_meta)?;
    Ok(true)
}

fn seed_project_meta(
    project_entries: &[MemoryManifestEntry],
    feedback_entries: &[MemoryManifestEntry],
    now: &str,
) -> ProjectMetaRecord {
    let seed = project_entries.first().or_else(|| feedback_entries.first());
    let project_name = seed
        .map(|entry| normalize_whitespace(&entry.frontmatter.name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DREAM_DEFAULT_PROJECT_NAME.to_string());
    let description = seed
        .map(|entry| normalize_whitespace(&entry.frontmatter.description))
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| DREAM_DEFAULT_PROJECT_DESCRIPTION.to_string());
    ProjectMetaRecord {
        project_id: DREAM_CURRENT_PROJECT_ID.to_string(),
        project_name,
        description,
        status: DREAM_DEFAULT_PROJECT_STATUS.to_string(),
        source_kind: None,
        source_workspace_path: None,
        source_project_id: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        dream_updated_at: None,
        relative_path: DREAM_PROJECT_META_FILE.to_string(),
        absolute_path: String::new(),
    }
}

// ---------------------------------------------------------------------------
// User profile rewrite
// ---------------------------------------------------------------------------

fn run_user_profile_rewrite(
    model: &dyn ModelProvider,
    stage: &mut dyn MemoryStore,
    user_note_records: &[MemoryFileRecord],
    profile_record: Option<&MemoryFileRecord>,
) -> Result<(bool, usize)> {
    let selected = select_user_note_window(user_note_records);
    if selected.is_empty() {
        return Ok((false, 0));
    }
    let existing_profile_markdown = profile_record
        .map(|record| truncate_raw(&record.content, DREAM_USER_PROFILE_EXISTING_MAX_CHARS));
    let prompt = UserProfilePrompt {
        existing_profile_markdown,
        incoming_user_notes: selected
            .iter()
            .map(|record| IncomingUserNote {
                description: truncate_for_prompt(
                    &record.entry.frontmatter.description,
                    DREAM_USER_NOTE_DESCRIPTION_MAX_CHARS,
                ),
                note_markdown: truncate_raw(&record.content, DREAM_USER_NOTE_MARKDOWN_MAX_CHARS),
                captured_at: record
                    .entry
                    .frontmatter
                    .captured_at
                    .as_deref()
                    .unwrap_or(""),
                source_session_key: record
                    .entry
                    .frontmatter
                    .source_session_key
                    .as_deref()
                    .unwrap_or(""),
            })
            .collect(),
    };
    let user_prompt = serde_json::to_string_pretty(&prompt)?;
    let rewritten = match complete_json(
        model,
        schemas::USER_PROFILE,
        USER_PROFILE_REWRITE_SYSTEM_PROMPT,
        &user_prompt,
        USER_PROFILE_REWRITE_TIMEOUT_MS,
        None,
    ) {
        Ok(value) => value,
        Err(error) => {
            log::warn!("[pd-memory] dream user profile rewrite failed: {error}");
            return Ok((false, 0));
        }
    };
    let section_value = rewritten
        .get("identity_background_markdown")
        .or_else(|| rewritten.get("identity_background"));
    let Some(section) = section_value.and_then(normalize_identity_background) else {
        log::warn!("[pd-memory] dream user profile rewrite returned no usable section");
        return Ok((false, 0));
    };
    if section.trim().is_empty() {
        return Ok((false, 0));
    }
    let body = format!("## 身份背景\n{}\n", section.trim());
    let previous_body = profile_record
        .map(|record| record.content.trim().to_string())
        .unwrap_or_default();
    if previous_body == body.trim() {
        log::debug!("[pd-memory] dream profile body unchanged; keeping absorbed user notes");
        return Ok((false, 0));
    }
    let facts = split_profile_facts(&strip_markdown_syntax(&body));
    let description = truncate_raw(
        facts.first().map(String::as_str).unwrap_or("User profile"),
        120,
    );
    let latest = selected.last();
    let candidate = MemoryCandidate {
        kind: MemoryRecordType::User,
        scope: MemoryScope::Global,
        name: DREAM_USER_PROFILE_NAME.to_string(),
        description,
        body,
        captured_at: latest.and_then(|record| record.entry.frontmatter.captured_at.clone()),
        source_session_key: latest
            .and_then(|record| record.entry.frontmatter.source_session_key.clone()),
        profile: facts.clone(),
        relationships: facts,
        ..Default::default()
    };
    // The profile has a fixed path and is replaced wholesale (trait method,
    // behavior spec §5.7) instead of being appended to `UserIdentityNotes/`.
    stage.upsert_user_profile(&candidate)?;
    let absorbed: Vec<String> = selected
        .iter()
        .map(|record| record.entry.relative_path.clone())
        .filter(|relative_path| {
            is_user_note_path(relative_path) && !is_user_profile_path(relative_path)
        })
        .collect();
    let absorbed_count = absorbed.len();
    if absorbed_count > 0 {
        stage.delete_entries(&absorbed)?;
    }
    Ok((true, absorbed_count))
}

fn select_user_note_window(records: &[MemoryFileRecord]) -> Vec<MemoryFileRecord> {
    let mut sorted: Vec<&MemoryFileRecord> = records.iter().collect();
    sorted.sort_by(|left, right| {
        right
            .entry
            .frontmatter
            .updated_at
            .cmp(&left.entry.frontmatter.updated_at)
            .then_with(|| left.entry.relative_path.cmp(&right.entry.relative_path))
    });
    let mut selected: Vec<MemoryFileRecord> = Vec::new();
    let mut characters = 0usize;
    for record in sorted {
        if selected.len() >= DREAM_USER_NOTE_MAX_FILES {
            break;
        }
        let next = characters + record.content.chars().count();
        if next > DREAM_USER_NOTE_CHAR_BUDGET {
            break;
        }
        selected.push(record.clone());
        characters = next;
    }
    selected
}

fn normalize_identity_background(value: &Value) -> Option<String> {
    let section = match value {
        Value::String(text) => normalize_identity_background_markdown(text),
        Value::Array(items) => {
            let mut unique: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for item in items.iter().filter_map(Value::as_str) {
                let fact = strip_markdown_syntax(item);
                if fact.is_empty() || !seen.insert(fact.clone()) {
                    continue;
                }
                unique.push(fact);
                if unique.len() >= DREAM_PROFILE_FACTS_MAX {
                    break;
                }
            }
            unique
                .iter()
                .map(|fact| format!("- {fact}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    let trimmed = section.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_identity_background_markdown(text: &str) -> String {
    let normalized = text.replace('\r', "\n");
    let normalized = strip_leading_fence(&normalized);
    let normalized = strip_trailing_fence(&normalized);
    let normalized = normalized.trim();
    strip_identity_background_heading(normalized)
}

fn strip_leading_fence(text: &str) -> String {
    let trimmed = text.trim_start();
    let lower = trimmed.to_lowercase();
    if !(lower.starts_with("```markdown") || lower.starts_with("```")) {
        return text.to_string();
    }
    match trimmed.find('\n') {
        Some(index) => trimmed[index + 1..].to_string(),
        None => String::new(),
    }
}

fn strip_trailing_fence(text: &str) -> String {
    let trimmed = text.trim_end();
    match trimmed.strip_suffix("```") {
        Some(stripped) => stripped.to_string(),
        None => text.to_string(),
    }
}

fn strip_identity_background_heading(text: &str) -> String {
    let trimmed = text.trim_start();
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&hashes) {
        return text.to_string();
    }
    let rest = trimmed[hashes..].trim_start();
    let Some(after_heading) = rest.strip_prefix("身份背景") else {
        return text.to_string();
    };
    if let Some(newline) = after_heading.find('\n') {
        if after_heading[..newline].trim().is_empty() {
            return after_heading[newline + 1..].trim_start().to_string();
        }
    } else if after_heading.trim().is_empty() {
        return String::new();
    }
    text.to_string()
}

fn split_profile_facts(text: &str) -> Vec<String> {
    const SEPARATORS: &[char] = &['\n', '，', ',', '；', ';', '。', '.', '!', '?'];
    let mut facts: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = String::new();
    for character in text.chars() {
        if SEPARATORS.contains(&character) {
            push_profile_fact(&mut facts, &mut seen, &current);
            current.clear();
        } else {
            current.push(character);
        }
    }
    push_profile_fact(&mut facts, &mut seen, &current);
    facts
}

fn push_profile_fact(facts: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str) {
    if facts.len() >= DREAM_PROFILE_FACTS_MAX {
        return;
    }
    let fact = normalize_whitespace(raw);
    if fact.chars().count() >= 2 && seen.insert(fact.clone()) {
        facts.push(fact);
    }
}

fn strip_markdown_syntax(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    for line in text.replace('\r', "\n").split('\n') {
        let mut content = line.trim_start();
        let hashes = content
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if (1..=6).contains(&hashes) {
            content = &content[hashes..];
        }
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = content.strip_prefix(bullet) {
                content = rest;
                break;
            }
        }
        stripped.push_str(content);
        stripped.push('\n');
    }
    normalize_whitespace(&stripped.replace('`', "").replace("**", "").replace('*', ""))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn list_kind(store: &dyn MemoryStore, kind: MemoryRecordType) -> Result<Vec<MemoryManifestEntry>> {
    let mut entries = store.list_entries(&MemoryListOptions {
        kinds: vec![kind],
        include_deprecated: false,
        ..Default::default()
    })?;
    entries.retain(|entry| entry.frontmatter.kind == kind && !entry.frontmatter.deprecated);
    sort_entries(&mut entries);
    Ok(entries)
}

fn load_records(
    store: &dyn MemoryStore,
    entries: &[MemoryManifestEntry],
) -> Result<Vec<MemoryFileRecord>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect();
    store.get_records(&ids, DREAM_RECORD_READ_MAX_LINES)
}

fn load_profile_record(
    store: &dyn MemoryStore,
    profile_entry: Option<&MemoryManifestEntry>,
    user_entries: &[MemoryManifestEntry],
) -> Result<Option<MemoryFileRecord>> {
    if let Some(entry) = profile_entry {
        let records = store.get_records(
            std::slice::from_ref(&entry.relative_path),
            DREAM_RECORD_READ_MAX_LINES,
        )?;
        if let Some(record) = records.into_iter().next() {
            return Ok(Some(record));
        }
    }
    // Fallback: some stores expose the profile only through `get_user_summary`.
    let summary = store.get_user_summary().unwrap_or_default();
    if let Some(record) = summary.files.into_iter().next() {
        return Ok(Some(record));
    }
    let profile_from_entries = user_entries
        .iter()
        .find(|entry| is_user_profile_path(&entry.relative_path));
    if let Some(entry) = profile_from_entries {
        let records = store.get_records(
            std::slice::from_ref(&entry.relative_path),
            DREAM_RECORD_READ_MAX_LINES,
        )?;
        return Ok(records.into_iter().next());
    }
    Ok(None)
}

fn split_user_entries(
    user_entries: &[MemoryManifestEntry],
) -> (Option<MemoryManifestEntry>, Vec<MemoryManifestEntry>) {
    let mut profile: Option<MemoryManifestEntry> = None;
    let mut notes: Vec<MemoryManifestEntry> = Vec::new();
    for entry in user_entries {
        if is_user_profile_path(&entry.relative_path) {
            profile = Some(entry.clone());
        } else if is_user_note_path(&entry.relative_path) {
            notes.push(entry.clone());
        }
    }
    (profile, notes)
}

fn is_user_profile_path(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    normalized == DREAM_USER_PROFILE_RELATIVE_PATH
        || normalized == format!("global/{DREAM_USER_PROFILE_RELATIVE_PATH}")
}

fn is_user_note_path(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    normalized.starts_with(DREAM_USER_NOTE_DIR)
        || normalized.starts_with(&format!("global/{DREAM_USER_NOTE_DIR}"))
}

fn find_record<'a>(
    records: &'a [MemoryFileRecord],
    relative_path: &str,
) -> Option<&'a MemoryFileRecord> {
    records
        .iter()
        .find(|record| same_memory_id(&record.entry.relative_path, relative_path))
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

fn sort_entries(entries: &mut [MemoryManifestEntry]) {
    entries.sort_by(|left, right| {
        right
            .frontmatter
            .updated_at
            .cmp(&left.frontmatter.updated_at)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
}

fn sort_records(records: &mut [MemoryFileRecord]) {
    records.sort_by(|left, right| {
        right
            .entry
            .frontmatter
            .updated_at
            .cmp(&left.entry.frontmatter.updated_at)
            .then_with(|| left.entry.relative_path.cmp(&right.entry.relative_path))
    });
}

fn normalize_bool(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) if value.trim().eq_ignore_ascii_case("true") => true,
        Some(Value::String(value)) if value.trim().eq_ignore_ascii_case("false") => false,
        _ => fallback,
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `truncateForPrompt`: normalise whitespace, then cut (no ellipsis).
fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    truncate_raw(&normalize_whitespace(value), max_chars)
}

/// `truncate`: cut to `max_chars` code points, then trim (no ellipsis).
fn truncate_raw(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_same_project_reasons_are_rejected() {
        assert!(is_generic_same_project_reason(
            "These files are in the same project."
        ));
        assert!(is_generic_same_project_reason("都属于同一个项目"));
        assert!(!is_generic_same_project_reason(""));
        assert!(!is_generic_same_project_reason("同属一个工作组"));
        assert!(!is_generic_same_project_reason(
            "same project with redundancy"
        ));
        assert!(!is_generic_same_project_reason(
            "Both duplicates describe the same delivery rule with an overlapping constraint."
        ));
        assert!(!is_generic_same_project_reason(
            "同属一个项目，内容重叠可合并"
        ));
    }

    #[test]
    fn cluster_validation_drops_bad_clusters() {
        let mut allowed: HashSet<String> = HashSet::new();
        for id in [
            "Project/a.md",
            "Project/b.md",
            "Project/c.md",
            "Project/d.md",
        ] {
            allowed.insert(id.to_string());
        }
        let plan = json!({
            "clusters": [
                {"member_relative_paths": ["Project/a.md", "Project/b.md"], "reason": "duplicate facts overlap"},
                {"member_relative_paths": ["Project/c.md", "Project/unknown.md"], "reason": "duplicate rule"},
                {"member_relative_paths": ["Project/a.md", "Project/d.md"], "reason": "overlapping notes"},
                {"member_relative_paths": ["Project/c.md", "Project/d.md"], "reason": "same project"},
            ]
        });
        let (accepted, dropped) = normalize_and_validate_clusters(&plan, &allowed, 8);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].member_ids, vec!["Project/a.md", "Project/b.md"]);
        assert_eq!(dropped.len(), 3);
    }

    #[test]
    fn identity_background_section_is_normalised() {
        let value = json!("```markdown\n## 身份背景\n- 算法工程师\n```");
        let section = normalize_identity_background(&value).expect("section");
        assert_eq!(section, "- 算法工程师");
    }

    #[test]
    fn profile_facts_are_split_and_deduped() {
        let facts = split_profile_facts("算法工程师，长期做推荐系统。算法工程师；有一个女儿");
        assert_eq!(facts[0], "算法工程师");
        assert_eq!(facts.len(), 3);
    }
}
