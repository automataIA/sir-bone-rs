//! End-to-end exercise of the agent loop (`sirbone::run`) across the public API
//! boundary, driven by a scripted mock client — no network, no API key.

mod common;

use common::{ctx, text_turn, tool_turn, MockClient};
use sirbone::{AgentEvent, ContentBlock, ReadStamps, ToolRegistry, UndoStore, WriteTool};
use tokio::sync::mpsc;

/// Tool call → tool runs → final answer. The file is actually written and the
/// loop emits the tool-call lifecycle plus the closing text.
#[tokio::test]
async fn writes_file_then_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");

    let mut registry = ToolRegistry::new();
    registry.register(WriteTool {
        undo: UndoStore::default(),
        stamps: ReadStamps::default(),
    });

    let (tx, mut rx) = mpsc::channel(64);
    let client = MockClient::new(vec![
        tool_turn(
            "c1",
            "write",
            serde_json::json!({"path": path.to_str().unwrap(), "content": "hello"}),
        ),
        text_turn("done"),
    ]);
    let mut c = ctx(client, registry, tx);

    sirbone::run(&mut c).await.unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

    let mut saw_tool_start = false;
    let mut saw_tool_end = false;
    let mut texts = String::new();
    rx.close();
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::ToolCallStart { name, .. } if name == "write" => saw_tool_start = true,
            AgentEvent::ToolCallEnd { name, .. } if name == "write" => saw_tool_end = true,
            AgentEvent::TextChunk(s) => texts.push_str(&s),
            _ => {}
        }
    }
    assert!(saw_tool_start, "expected ToolCallStart for write");
    assert!(saw_tool_end, "expected ToolCallEnd for write");
    assert!(texts.contains("done"));
}

/// A failing tool does not abort the loop: the error is fed back as a tool
/// result and the agent still produces its final turn.
#[tokio::test]
async fn tool_error_is_recovered() {
    let mut registry = ToolRegistry::new();
    registry.register(WriteTool {
        undo: UndoStore::default(),
        stamps: ReadStamps::default(),
    });

    let (tx, _rx) = mpsc::channel(64);
    // Write to a path whose parent is a file, not a directory → tool errors.
    let client = MockClient::new(vec![
        tool_turn(
            "c1",
            "write",
            serde_json::json!({"path": "/dev/null/nope.txt", "content": "x"}),
        ),
        text_turn("recovered"),
    ]);
    let mut c = ctx(client, registry, tx);

    sirbone::run(&mut c).await.unwrap();

    // The conversation carries a tool result (error text) and a final answer.
    let has_tool_result = c.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    });
    assert!(has_tool_result, "tool error should surface as a ToolResult");
    let ends_with_answer = c
        .messages
        .last()
        .unwrap()
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("recovered")));
    assert!(
        ends_with_answer,
        "loop should reach the final answer after a tool error"
    );
}
