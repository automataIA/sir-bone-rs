//! Pure data types copied from `src/types.rs` (the agent's `types` module).
//!
//! The wasm demo can't depend on the `sirbone` lib (it pulls tokio/reqwest/rmcp,
//! none of which build for `wasm32`), so the render modules included via `#[path]`
//! resolve `crate::types::*` against this shim instead.
//!
//! keep in sync with src/types.rs — only the tokio `EventTx`/`EventRx` aliases and
//! the test module are intentionally omitted.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Thinking { thinking: String },
    Image { media_type: String, data: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![ContentBlock::Text { text: text.into() }] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: vec![ContentBlock::Text { text: text.into() }] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TextChunk(String),
    ThinkingStart,
    ThinkingChunk(String),
    ToolCallStart { name: String, input: serde_json::Value },
    ToolCallEnd { name: String, result: String, is_error: bool },
    TurnEnd,
    Cancelled,
    Error(String),
    Notice { text: String, level: NoticeLevel },
    ContextUsage { used_tokens: u32, context_window: u32, cached_tokens: u32 },
    Compacted { messages: Vec<Message> },
    JobDone { id: u32, command: String, exit: Option<i32>, secs: u64 },
}

#[derive(Debug, Clone, Copy)]
pub enum NoticeLevel {
    Success,
    Info,
}
