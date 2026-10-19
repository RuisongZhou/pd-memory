//! Frontmatter/Markdown format parity tests (task-1, dev A).

use pd_memory_core::frontmatter::{parse_flat_fields, parse_markdown, render_markdown};
use pd_memory_core::types::{MemoryFileFrontmatter, MemoryRecordType, MemoryScope};

#[test]
fn parses_and_renders_roundtrip_with_special_characters() {
    let frontmatter = MemoryFileFrontmatter {
        name: "a: b \"quoted\"".into(),
        description: "中文描述；包含: 冒号 and  spaces".into(),
        kind: MemoryRecordType::Project,
        scope: MemoryScope::Project,
        project_id: Some("current_project".into()),
        source_kind: None,
        source_workspace_path: Some("/Users/me/proj".into()),
        source_project_id: None,
        updated_at: "2026-10-18T09:13:02.114Z".into(),
        dream_updated_at: Some("2026-10-19T00:00:00.000Z".into()),
        captured_at: Some("2026-10-18T09:12:33.412Z".into()),
        source_session_key: Some("sess-7f3a".into()),
        deprecated: true,
        dream_attempts: 2,
    };
    let body = "## Summary\n\nBoreal 是一个本地知识库整理工具。";
    let rendered = render_markdown(&frontmatter, body);
    // Closing fence is followed by exactly one newline, then the trimmed body.
    assert!(rendered.starts_with("---\nname: a: b \"quoted\"\n"));
    assert!(rendered.ends_with("---\n## Summary\n\nBoreal 是一个本地知识库整理工具。\n"));
    let parsed = parse_markdown(&rendered).unwrap().unwrap();
    assert_eq!(parsed.frontmatter.name, "a: b \"quoted\"");
    assert_eq!(
        parsed.frontmatter.description,
        "中文描述；包含: 冒号 and spaces"
    );
    assert_eq!(
        parsed.frontmatter.project_id.as_deref(),
        Some("current_project")
    );
    assert_eq!(
        parsed.frontmatter.captured_at.as_deref(),
        Some("2026-10-18T09:12:33.412Z")
    );
    assert!(parsed.frontmatter.deprecated);
    assert_eq!(parsed.frontmatter.dream_attempts, 2);
    // The reader keeps the trailing newline from disk (callers trim it when
    // rendering recall content), matching `parseFrontmatterBlock`.
    assert_eq!(
        parsed.body,
        "## Summary\n\nBoreal 是一个本地知识库整理工具。\n"
    );
}

#[test]
fn rejects_invalid_type_or_scope_and_malformed_fences() {
    let raw = "---\nname: x\ntype: bogus\nscope: project\n---\nbody";
    assert!(parse_markdown(raw).unwrap().is_none());
    let raw = "---\nname: x\ntype: project\nscope: globalish\n---\nbody";
    assert!(parse_markdown(raw).unwrap().is_none());
    // CRLF is not accepted (the fence match is byte-exact).
    let raw = "---\r\nname: x\r\ntype: project\r\nscope: project\r\n---\r\nbody";
    assert!(parse_markdown(raw).unwrap().is_none());
    // No closing fence.
    assert!(
        parse_markdown("---\nname: x\ntype: project\nscope: project\n")
            .unwrap()
            .is_none()
    );
    // Unknown extra keys are ignored; missing updated_at falls back to now.
    let raw = "---\nname: x\ntype: user\nscope: global\nfoo: bar\n---\nbody";
    let parsed = parse_markdown(raw).unwrap().unwrap();
    assert_eq!(parsed.frontmatter.name, "x");
    assert_eq!(parsed.frontmatter.updated_at.len(), 24);
    assert!(parsed.frontmatter.updated_at.ends_with('Z'));
}

#[test]
fn parses_flat_fields_of_project_meta() {
    let raw = "---\nproject_id: current_project\nproject_name: Boreal\ndescription: a: b\nstatus: in_progress\n---\n\n## Summary\nBoreal\n";
    let fields = parse_flat_fields(raw);
    assert_eq!(
        fields.get("project_name").map(String::as_str),
        Some("Boreal")
    );
    assert_eq!(fields.get("description").map(String::as_str), Some("a: b"));
    assert!(parse_flat_fields("no frontmatter").is_empty());
}

#[test]
fn reference_edge_cases() {
    // First colon splits; whitespace is trimmed; repeated keys: last wins.
    let raw = "---\nname:foo\nname :  bar  \ndescription: a: b\ndeprecated: TRUE\ndream_attempts: 3abc\ntype: feedback\nscope: project\n---\n";
    let parsed = parse_markdown(raw).unwrap().unwrap();
    assert_eq!(parsed.frontmatter.name, "bar");
    assert_eq!(parsed.frontmatter.description, "a: b");
    assert!(!parsed.frontmatter.deprecated); // case-sensitive
    assert_eq!(parsed.frontmatter.dream_attempts, 3); // parseInt prefix
                                                      // `: bar` and continuation lines without a colon are ignored.
    let raw = "---\n: nope\nname: x\n  continued\n type: project\nscope: project\n---\n";
    let parsed = parse_markdown(raw).unwrap().unwrap();
    assert_eq!(parsed.frontmatter.name, "x");
}
