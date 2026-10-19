//! Hand-written Markdown frontmatter parser / renderer (dev A).
//!
//! Parity target: `file-memory.ts` parses a small YAML subset without a YAML
//! dependency. Keep the exact same key handling (`snake_case`, `key: value`
//! first-colon split, boolean/number coercion) so both implementations can
//! read each other's files.
//!
//! This is *not* YAML: only `key: value` lines are understood. Multi-line
//! values, flow sequences and comments are ignored exactly like the reference.
//!
//! Deliberate deviation from the TypeScript reference: `deprecated: false` /
//! `dream_attempts: 0` are not rendered (they are the reader defaults anyway).
//! The reference omits them for fresh candidates and only renders them when
//! rewriting an existing record; the Rust domain type cannot distinguish
//! "absent" from "false/0", so the writer always picks the smaller form.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::types::{MemoryFileFrontmatter, MemoryRecordType, MemoryScope};

/// A parsed Markdown document: frontmatter + body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMarkdown {
    pub frontmatter: MemoryFileFrontmatter,
    pub body: String,
}

/// Opening fence, matched byte-for-byte (`\r\n` and BOM input fails, as in TS).
const OPENING_FENCE: &str = "---\n";
/// Closing fence including the newline that must precede it.
const CLOSING_FENCE: &str = "\n---\n";

/// Parse a Markdown file into frontmatter + body.
///
/// Returns `Ok(None)` when the document is not a memory file: no well-formed
/// frontmatter block, or an invalid/missing `type` / `scope`. Leading blank
/// lines of the body are stripped and a missing `updated_at` falls back to the
/// current time, both matching the reference.
pub fn parse_markdown(raw: &str) -> Result<Option<ParsedMarkdown>> {
    let Some((header, body)) = split_block(raw) else {
        return Ok(None);
    };
    let values = parse_header_values(header);

    let Some(kind) = values
        .get("type")
        .and_then(|value| MemoryRecordType::parse(value))
    else {
        return Ok(None);
    };
    let scope = match values.get("scope").map(String::as_str) {
        Some("global") => MemoryScope::Global,
        Some("project") => MemoryScope::Project,
        _ => return Ok(None),
    };

    let frontmatter = MemoryFileFrontmatter {
        name: values.get("name").cloned().unwrap_or_default(),
        description: values.get("description").cloned().unwrap_or_default(),
        kind,
        scope,
        project_id: non_empty(values.get("project_id")),
        source_kind: non_empty(values.get("source_kind")),
        source_workspace_path: non_empty(values.get("source_workspace_path")),
        source_project_id: non_empty(values.get("source_project_id")),
        updated_at: values.get("updated_at").cloned().unwrap_or_else(now_iso),
        dream_updated_at: non_empty(values.get("dream_updated_at")),
        captured_at: non_empty(values.get("captured_at")),
        source_session_key: non_empty(values.get("source_session_key")),
        deprecated: values
            .get("deprecated")
            .and_then(|value| parse_bool(value))
            .unwrap_or(false),
        dream_attempts: values
            .get("dream_attempts")
            .and_then(|value| parse_u32_prefix(value))
            .unwrap_or(0),
    };
    Ok(Some(ParsedMarkdown {
        frontmatter,
        body: body.to_string(),
    }))
}

/// Render frontmatter + body back into a Markdown document.
///
/// Byte-compatible with the reference writer: the closing `---` is followed by
/// exactly one newline and then the trimmed body (no blank separator line).
/// Values are normalised the same way (`name` collapses whitespace,
/// `description` falls back to `name`).
pub fn render_markdown(frontmatter: &MemoryFileFrontmatter, body: &str) -> String {
    let mut lines: Vec<String> = vec![
        "---".to_string(),
        format!("name: {}", normalize_whitespace(&frontmatter.name)),
        format!(
            "description: {}",
            normalize_description(&frontmatter.description, &frontmatter.name)
        ),
        format!("type: {}", frontmatter.kind.as_str()),
        format!("scope: {}", frontmatter.scope.as_str()),
    ];
    push_optional(&mut lines, "project_id", frontmatter.project_id.as_deref());
    push_optional(
        &mut lines,
        "source_kind",
        frontmatter.source_kind.as_deref(),
    );
    push_optional(
        &mut lines,
        "source_workspace_path",
        frontmatter.source_workspace_path.as_deref(),
    );
    push_optional(
        &mut lines,
        "source_project_id",
        frontmatter.source_project_id.as_deref(),
    );
    lines.push(format!("updated_at: {}", frontmatter.updated_at));
    push_optional(
        &mut lines,
        "dream_updated_at",
        frontmatter.dream_updated_at.as_deref(),
    );
    push_optional(
        &mut lines,
        "captured_at",
        frontmatter.captured_at.as_deref(),
    );
    push_optional(
        &mut lines,
        "source_session_key",
        frontmatter.source_session_key.as_deref(),
    );
    if frontmatter.deprecated {
        lines.push("deprecated: true".to_string());
    }
    if frontmatter.dream_attempts > 0 {
        lines.push(format!("dream_attempts: {}", frontmatter.dream_attempts));
    }
    lines.push("---".to_string());
    lines.push(String::new());

    // `join` already ends with a single newline because the last element is
    // the empty line after the closing fence (matches the TS template).
    let mut rendered = lines.join("\n");
    rendered.push_str(body.trim());
    rendered.push('\n');
    rendered
}

/// Parse the flat `key: value` block used by `project.meta.md`.
///
/// No `type` / `scope` validation: `project.meta.md` is a different schema.
/// Returns an empty map when the document has no well-formed frontmatter
/// block (the reference behaves the same way).
pub fn parse_flat_fields(raw: &str) -> BTreeMap<String, String> {
    let Some((header, _)) = split_block(raw) else {
        return BTreeMap::new();
    };
    parse_header_values(header)
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by `files.rs`)
// ---------------------------------------------------------------------------

/// Collapse every run of Unicode whitespace into a single space and trim.
///
/// Mirrors `normalizeWhitespace` (`file-memory.ts:86-88`).
pub(crate) fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `normalizeWhitespace(value) || normalizeWhitespace(fallback)`.
pub(crate) fn normalize_description(value: &str, fallback: &str) -> String {
    let normalized = normalize_whitespace(value);
    if normalized.is_empty() {
        normalize_whitespace(fallback)
    } else {
        normalized
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Split a document into its frontmatter header (without fences) and body.
fn split_block(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix(OPENING_FENCE)?;
    let close = rest.find(CLOSING_FENCE)?;
    let header = &rest[..close];
    // `close` points at the `\n` that starts the closing fence; skip the five
    // bytes of `\n---\n` and then strip the leading blank lines of the body.
    let body = rest[close + CLOSING_FENCE.len()..].trim_start_matches('\n');
    Some((header, body))
}

/// Parse `key: value` lines. The first colon splits; `: foo` is ignored.
fn parse_header_values(header: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in header.split('\n') {
        let Some(separator) = line.find(':') else {
            continue;
        };
        if separator == 0 {
            continue;
        }
        // `separator` is the byte offset of an ASCII colon, hence a valid UTF-8
        // boundary for both slices.
        let key = line[..separator].trim();
        let value = line[separator + 1..].trim();
        values.insert(key.to_string(), value.to_string());
    }
    values
}

fn non_empty(value: Option<&String>) -> Option<String> {
    value.filter(|item| !item.is_empty()).cloned()
}

fn push_optional(lines: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|item| !item.is_empty()) {
        lines.push(format!("{key}: {value}"));
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// `Number.parseInt(value, 10)` semantics: optional sign, ASCII digit prefix.
fn parse_int_prefix(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut negative = false;
    if bytes.first() == Some(&b'+') {
        index = 1;
    } else if bytes.first() == Some(&b'-') {
        index = 1;
        negative = true;
    }
    let start = index;
    let mut magnitude: i128 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(i128::from(bytes[index] - b'0'));
        index += 1;
    }
    if index == start {
        return None;
    }
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).ok()
}

fn parse_u32_prefix(value: &str) -> Option<u32> {
    parse_int_prefix(value).and_then(|parsed| u32::try_from(parsed).ok())
}

fn now_iso() -> String {
    crate::ports::format_iso(chrono::Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_special_characters_and_colons() {
        let raw = "---\nname: a: b\n description: 你好: 世界\n type: project\nscope: project\nupdated_at: t\n---\nbody";
        let parsed = parse_markdown(raw).unwrap().unwrap();
        assert_eq!(parsed.frontmatter.name, "a: b");
        assert_eq!(parsed.frontmatter.description, "你好: 世界");
        assert_eq!(parsed.frontmatter.kind, MemoryRecordType::Project);
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn rejects_crlf_and_invalid_type() {
        assert!(parse_markdown("---\r\nname: x\r\n---\r\nbody")
            .unwrap()
            .is_none());
        assert!(
            parse_markdown("---\nname: x\ntype: nope\nscope: project\n---\n")
                .unwrap()
                .is_none()
        );
        assert!(parse_markdown("---\ntype: project\n---\n")
            .unwrap()
            .is_none());
    }

    #[test]
    fn parses_reference_edge_cases() {
        let raw = "---\nname:foo\ndescription:\ndeprecated: TRUE\ndream_attempts: 3abc\ntype: user\nscope: global\n---\n\n\nbody";
        let parsed = parse_markdown(raw).unwrap().unwrap();
        assert_eq!(parsed.frontmatter.name, "foo");
        assert_eq!(parsed.frontmatter.description, "");
        assert!(!parsed.frontmatter.deprecated);
        assert_eq!(parsed.frontmatter.dream_attempts, 3);
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn renders_without_optional_defaults() {
        let frontmatter = MemoryFileFrontmatter {
            name: "Boreal ".into(),
            description: "  ".into(),
            kind: MemoryRecordType::Project,
            scope: MemoryScope::Project,
            project_id: Some(CURRENT_PROJECT_ID_FOR_TEST.into()),
            updated_at: "t".into(),
            ..Default::default()
        };
        let rendered = render_markdown(&frontmatter, "  ## Summary\nx  ");
        assert_eq!(
            rendered,
            "---\nname: Boreal\ndescription: Boreal\ntype: project\nscope: project\nproject_id: current_project\nupdated_at: t\n---\n## Summary\nx\n"
        );
        let parsed = parse_markdown(&rendered).unwrap().unwrap();
        assert_eq!(parsed.frontmatter.name, "Boreal");
        assert_eq!(parsed.body, "## Summary\nx\n");
    }

    const CURRENT_PROJECT_ID_FOR_TEST: &str = "current_project";
}
