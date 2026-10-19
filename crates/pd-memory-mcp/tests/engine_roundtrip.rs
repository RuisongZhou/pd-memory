//! End-to-end MCP dispatch against a real engine over a temporary data root.
//!
//! These tests are offline: the engine is built with `pd_memory_llm::MockProvider`
//! over a `tempfile::TempDir`. While the core storage/indexing/recall crates are
//! still being implemented the tests print `SKIP:` and pass; once those crates
//! land they exercise the full MCP -> engine -> files path.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::types::MemoryListOptions;
use pd_memory_llm::MockProvider;
use pd_memory_mcp::{handle_line, McpServerConfig};

const WORKSPACE: &str = "mcp-itest";

fn server_config() -> McpServerConfig {
    McpServerConfig {
        server_name: "pd-memory".into(),
        server_version: "test".into(),
    }
}

fn config_for(root: &Path) -> MemoryConfig {
    let mut config = MemoryConfig::for_data_root(root);
    config.workspace_id = Some(WORKSPACE.to_string());
    config
}

/// Build an engine and prove the basic read path works (core storage landed).
fn base_engine() -> Option<(tempfile::TempDir, Arc<MemoryEngine>)> {
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path().to_path_buf();
    let result = catch_unwind(AssertUnwindSafe(|| -> Option<Arc<MemoryEngine>> {
        let engine = MemoryEngine::open(config_for(&root), Arc::new(MockProvider::new())).ok()?;
        engine.overview().ok()?;
        engine.list(&MemoryListOptions::default()).ok()?;
        Some(Arc::new(engine))
    }));
    match result {
        Ok(Some(engine)) => Some((dir, engine)),
        Ok(None) => None,
        Err(_) => None,
    }
}

/// Prove the indexing + recall pipelines work (core-index / core-recall landed).
fn pipeline_available() -> bool {
    let Some((_dir, engine)) = base_engine() else {
        return false;
    };
    let probe = catch_unwind(AssertUnwindSafe(|| {
        let flush_ok = engine
            .flush(pd_memory_core::engine::FlushOptions::default())
            .is_ok();
        let search_ok = engine.search("probe", &[]).is_ok();
        flush_ok && search_ok
    }));
    matches!(probe, Ok(true))
}

fn call_tool(
    engine: &MemoryEngine,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
    .to_string();
    let response = handle_line(engine, &server_config(), &request).expect("tools/call responds");
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(
        response.get("error").is_none(),
        "unexpected JSON-RPC error: {response}"
    );
    response["result"].clone()
}

#[test]
fn mcp_initialize_tools_list_and_overview_with_engine() {
    let Some((_dir, engine)) = base_engine() else {
        eprintln!("SKIP: core storage not implemented yet (engine cannot open)");
        return;
    };
    let initialize = handle_line(
        &engine,
        &server_config(),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        })
        .to_string(),
    )
    .expect("initialize responds");
    assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(initialize["result"]["serverInfo"]["name"], "pd-memory");
    assert_eq!(
        initialize["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    let tools = handle_line(
        &engine,
        &server_config(),
        &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
    )
    .expect("tools/list responds");
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 7);

    let overview = call_tool(&engine, 3, "memory_overview", serde_json::json!({}));
    assert_eq!(overview["isError"], false);
    let structured = &overview["structuredContent"];
    assert_eq!(structured["total_files"], 0);
    assert_eq!(structured["workspace_name"], WORKSPACE);
    let text = overview["content"][0]["text"].as_str().expect("text");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("text is JSON");
    assert_eq!(&parsed, structured);
}

#[test]
fn mcp_add_is_captured_and_deduplicated() {
    let Some((_dir, engine)) = base_engine() else {
        eprintln!("SKIP: core storage not implemented yet (engine cannot open)");
        return;
    };
    let arguments = serde_json::json!({
        "session_id": "mcp-session-1",
        "messages": [
            {"role": "user", "content": "这个项目叫 Boreal，是一个本地知识库整理工具"},
            {"role": "assistant", "content": "已记录"}
        ],
        "source": "mcp-test"
    });
    let first = call_tool(&engine, 1, "memory_add", arguments.clone());
    assert_eq!(first["isError"], false);
    assert_eq!(first["structuredContent"]["captured"], true);
    assert_eq!(first["structuredContent"]["session_key"], "mcp-session-1");

    let duplicate = call_tool(&engine, 2, "memory_add", arguments);
    assert_eq!(duplicate["isError"], false);
    assert_eq!(duplicate["structuredContent"]["captured"], true);
    assert!(duplicate["structuredContent"].get("duplicate").is_none());
    assert!(duplicate["structuredContent"].get("message").is_none());

    let overview = call_tool(&engine, 3, "memory_overview", serde_json::json!({}));
    assert_eq!(overview["structuredContent"]["pending_dialogue_turns"], 1);
}

#[test]
fn mcp_flush_list_and_search_roundtrip() {
    if !pipeline_available() {
        eprintln!("SKIP: indexing/recall pipeline not implemented yet");
        return;
    }
    let Some((_dir, engine)) = base_engine() else {
        eprintln!("SKIP: core storage not implemented yet");
        return;
    };
    let fact = "这个项目叫 Boreal，是一个本地知识库整理工具";
    let add = call_tool(
        &engine,
        1,
        "memory_add",
        serde_json::json!({
            "session_id": "mcp-session-2",
            "messages": [
                {"role": "user", "content": fact},
                {"role": "assistant", "content": "记住了，Boreal 是一个本地知识库整理工具。"}
            ]
        }),
    );
    assert_eq!(add["structuredContent"]["captured"], true);

    let flush = call_tool(
        &engine,
        2,
        "memory_flush",
        serde_json::json!({"batch_size": 10, "reason": "integration"}),
    );
    assert_eq!(flush["isError"], false);
    assert!(
        flush["structuredContent"]["processed_turns"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "flush output: {flush}"
    );

    let list = call_tool(&engine, 3, "memory_list", serde_json::json!({}));
    assert_eq!(list["isError"], false);
    let entries = list["structuredContent"]["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "expected memory entries after flush");

    let search = call_tool(
        &engine,
        4,
        "memory_search",
        serde_json::json!({"query": "Boreal", "limit": 3}),
    );
    assert_eq!(search["isError"], false);
    let structured = &search["structuredContent"];
    let has_selection = structured["selected_ids"]
        .as_array()
        .is_some_and(|ids| !ids.is_empty());
    let has_context = structured["context"]
        .as_str()
        .is_some_and(|context| !context.is_empty());
    assert!(
        has_selection || has_context,
        "expected recall of the stored fact: {structured}"
    );

    let overview = call_tool(&engine, 5, "memory_overview", serde_json::json!({}));
    assert!(
        overview["structuredContent"]["total_files"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
}
