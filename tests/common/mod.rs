//! Shared test helpers for integration tests: a scripted `LlmClient` mock and a
//! minimal `AgentContext` builder. Mirrors the in-crate `FauxClient` pattern but
//! lives at the public API boundary so integration tests exercise the real loop.
//!
//! Each integration test binary pulls this in via `mod common;` and uses only a
//! subset, so unused-helper warnings here are expected.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use sirbone::{
    AgentContext, AgentEvent, AgentState, ContentBlock, EventTx, LlmClient, Message,
    PermissionConfig, Role, ToolCall, ToolRegistry, TurnResult,
};
use tokio_util::sync::CancellationToken;

/// A scripted client: returns the pre-built `TurnResult`s in order, one per
/// `run_turn` call, emitting the same `TextChunk`/`TurnEnd` events the real
/// clients do.
pub struct MockClient {
    turns: Vec<TurnResult>,
    idx: AtomicUsize,
}

impl MockClient {
    pub fn new(turns: Vec<TurnResult>) -> Arc<Self> {
        Arc::new(Self {
            turns,
            idx: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl LlmClient for MockClient {
    async fn run_turn(
        &self,
        _messages: &[&Message],
        _registry: &ToolRegistry,
        events: &EventTx,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<TurnResult> {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let result = self
            .turns
            .get(i)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mock: no more turns (call {i})"))?;
        for block in &result.assistant_message.content {
            if let ContentBlock::Text { text } = block {
                events.send(AgentEvent::TextChunk(text.clone())).await.ok();
            }
        }
        events.send(AgentEvent::TurnEnd).await.ok();
        Ok(result)
    }
}

/// A final assistant turn carrying text, ending the loop.
pub fn text_turn(text: &str) -> TurnResult {
    TurnResult {
        assistant_message: Message::assistant(text),
        state: AgentState::Done,
        usage: Default::default(),
    }
}

/// A turn that requests a single tool call.
pub fn tool_turn(id: &str, name: &str, args: serde_json::Value) -> TurnResult {
    TurnResult {
        assistant_message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: args.clone(),
            }],
        },
        state: AgentState::ToolCalling(vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }]),
        usage: Default::default(),
    }
}

/// Minimal context: a single user message, no compaction, default permissions.
pub fn ctx(client: Arc<dyn LlmClient>, tools: ToolRegistry, events: EventTx) -> AgentContext {
    // Keep any per-project caches the agent loop writes out of the real
    // `~/.sirbone` (integration tests run with the lib compiled without cfg(test)).
    sirbone::project_store::set_projects_root_override(
        std::env::temp_dir().join("sirbone-integration-test-projects"),
    );
    AgentContext {
        model: "test".into(),
        system_prompt: None,
        messages: vec![Message::user("do the task")],
        tools,
        client,
        events,
        cancel: CancellationToken::new(),
        context_window: None,
        confirm: None,
        compaction_keep_recent: None,
        permissions: PermissionConfig::default(),
        snapshots: None,
        hooks: Default::default(),
        oracle: None,
        max_steps: None,
        spend_cap: None,
        tokens_spent: 0,
    }
}
