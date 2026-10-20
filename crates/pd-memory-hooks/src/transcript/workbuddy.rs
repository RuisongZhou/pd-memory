//! WorkBuddy / CodeBuddy transcripts: Claude-style records (decoded by
//! [`super::claude`]) whose GUI user text wraps the typed query in host
//! `<system-reminder>` blocks.

/// User-typed text of one content string, or `None` for host context.
pub(crate) fn conversation_text(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    if !rest.starts_with("<system-reminder") {
        return Some(text);
    }
    // WorkBuddy GUI combines host reminders and <user_query> in one block.
    // Keep the user query, while excluding host instructions from memory.
    while rest.starts_with("<system-reminder") {
        rest = rest.split_once("</system-reminder>")?.1.trim_start();
    }
    rest.strip_prefix("<user_query>")?
        .trim_end()
        .strip_suffix("</user_query>")
        .map(str::trim)
        .filter(|query| !query.is_empty())
}

#[cfg(test)]
mod tests {
    use crate::transcript::claude::record_to_message;
    use crate::transcript::test_support::parse_lines;

    #[test]
    fn parses_workbuddy_desktop_transcript() {
        let lines = [
            r#"{"id":"1","type":"session-meta","sessionId":"s"}"#,
            r#"{"id":"2","type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"user-context\">ctx</system-reminder>"}]}"#,
            r#"{"id":"3","type":"message","role":"user","content":[{"type":"input_text","text":"这个项目叫 Boreal"}]}"#,
            r#"{"id":"4","type":"function_call","name":"Bash","arguments":"{}"}"#,
            r#"{"id":"5","type":"function_call_result","name":"Bash","output":{"type":"text","text":"ok"}}"#,
            r#"{"id":"6","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"收到，记住了"}]}"#,
        ];
        let messages = parse_lines(&lines, 10);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "这个项目叫 Boreal");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "收到，记住了");
    }

    #[test]
    fn extracts_workbuddy_gui_user_query_from_combined_context_block() {
        let query = "Azure Heron release code is Kestrel-9462; literal </user_query> stays.";
        let envelope = format!(
            "<system-reminder data-role=\"user-context\">host instructions</system-reminder>\n\
             <system-reminder data-role=\"additional-data\">host memory</system-reminder>\n\
             <user_query>{query}</user_query>"
        );
        for content in [
            serde_json::json!([{ "type": "input_text", "text": envelope }]),
            serde_json::json!(envelope),
            serde_json::json!({ "text": envelope }),
        ] {
            let record = serde_json::json!({
                "type": "message", "role": "user", "content": content
            });
            let message = record_to_message(&record).expect("GUI user query");
            assert_eq!(message.content, query);
        }
    }

    #[test]
    fn skips_workbuddy_context_without_a_complete_user_query() {
        for text in [
            "<system-reminder>host instructions</system-reminder>",
            "<system-reminder>host instructions</system-reminder><user_query>incomplete",
            "<system-reminder>incomplete<user_query>not user text</user_query>",
            "<system-reminder>host instructions</system-reminder><user_query> </user_query>",
        ] {
            let record = serde_json::json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": text }]
            });
            assert!(record_to_message(&record).is_none(), "{text}");
        }
    }

    #[test]
    fn parses_flat_workbuddy_cli_message_shape() {
        let lines = [
            r#"{"type":"message","role":"user","text":"flat text"}"#,
            r#"{"role":"assistant","content":"bare role content"}"#,
        ];
        let messages = parse_lines(&lines, 10);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "flat text");
        assert_eq!(messages[1].content, "bare role content");
    }
}
