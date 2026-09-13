pub mod ablate;
pub mod acp;
pub mod agent;
pub mod ai;
pub mod attachments;
pub mod best_of;
pub mod checks;
pub mod claude_md;
pub mod config;
pub mod highlight;
pub mod mcp;
pub mod oracle;
pub mod permissions;
pub mod project_store;
pub mod questions;
pub mod quota;
pub mod render;
pub mod session;
pub mod skills;
pub mod snapshot;
pub mod stream_rules;
pub mod structure;
pub mod system_prompt;
pub mod telemetry;
pub mod tools;
pub mod tui;
pub mod types;
pub mod verification_setup;

/// Version string shown by `--version` and `sirbone doctor`. A `bench_bypass`
/// build is a different product — it has no permission gate — so it says so
/// here rather than passing for a normal release.
pub const VERSION: &str = if cfg!(feature = "bench_bypass") {
    concat!(
        env!("CARGO_PKG_VERSION"),
        "+bench_bypass (NO PERMISSION GATE)"
    )
} else {
    env!("CARGO_PKG_VERSION")
};

pub use agent::{run, AgentContext, ConfirmBridge, LlmClient, TurnResult};
pub use ai::{AnthropicClient, CodexClient, OpenAiClient};
pub use permissions::{Decision, PermissionConfig};
pub use tools::{
    BashTool, DynTool, EditTool, GlobTool, GrepTool, ReadStamps, ReadTool, ToolRegistry, TypedTool,
    UndoStore, UndoTool, WebFetchTool, WriteTool,
};
pub use types::{
    AgentEvent, AgentState, ContentBlock, EventRx, EventTx, Message, NoticeLevel, Role, ToolCall,
};
