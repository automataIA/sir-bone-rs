//! Agent core: the `AgentContext`, the `LlmClient` trait, and the public entry
//! points (`run`, `compact`, `localize`, `estimate_context_tokens`). The state
//! machine, permission policy, compaction, and token estimation each live in a
//! submodule; this module owns only the shared types and re-exports.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::permissions::PermissionConfig;
use crate::tools::ToolRegistry;
use crate::types::{AgentState, EventTx, Message};

mod compact;
mod grounding;
mod policy;
mod state;
mod tokens;

pub use compact::compact;
pub use grounding::{facts, facts_block, prompt_context};
pub use state::{approved_plan_note, localize, run};
pub use tokens::estimate_context_tokens;

#[cfg(test)]
pub(crate) use policy::{decide, nudge_if_stuck, stuck_tool, STUCK_THRESHOLD};
#[cfg(test)]
pub(crate) use state::{is_chit_chat, plan_lanes};
#[cfg(test)]
pub(crate) use tokens::should_compact;

/// How many recent messages to keep during compaction.
pub(crate) const COMPACTION_KEEP_RECENT_DEFAULT: usize = 6;

/// Two-way bridge for interactive destructive-command confirmation.
/// The agent sends the command string on `ask`; the UI responds with
/// true (allow) or false (deny) on `reply`.
pub struct ConfirmBridge {
    pub ask: mpsc::Sender<String>,
    pub reply: mpsc::Receiver<bool>,
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub assistant_message: Message,
    pub state: AgentState,
    /// Real token usage the provider reported for this turn (zero if none).
    pub usage: crate::types::TokenUsage,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn run_turn(
        &self,
        messages: &[&Message],
        registry: &ToolRegistry,
        events: &EventTx,
        cancel: &CancellationToken,
    ) -> Result<TurnResult>;

    /// Available model ids from the provider. Default: unsupported (lets the
    /// user still switch by name via `set_model`).
    async fn list_models(&self) -> Result<Vec<String>> {
        anyhow::bail!("model listing not supported by this provider")
    }

    /// Switch the active model at runtime. Default: no-op.
    fn set_model(&self, _model: String) {}

    /// Set the extended-thinking budget at runtime (None = off). Default: no-op
    /// (providers without extended thinking ignore it).
    fn set_thinking_budget(&self, _budget: Option<u32>) {}

    /// Current extended-thinking budget, if any. Default: None.
    fn thinking_budget(&self) -> Option<u32> {
        None
    }

    /// Exact input-token count for the given payload (system + tools + messages)
    /// from the provider. Default: unsupported (callers fall back to an estimate).
    async fn count_tokens(&self, _messages: &[&Message], _registry: &ToolRegistry) -> Result<u64> {
        anyhow::bail!("token counting not supported by this provider")
    }

    /// Context window (max input tokens) of the active model, from the provider
    /// when discoverable (cached per model). `SIRBONE_CONTEXT_WINDOW` overrides.
    /// Default: unknown (callers fall back to a conservative constant).
    async fn context_window(&self) -> Option<u32> {
        None
    }
}

/// Switch a client's model and remember the choice for this project.
pub fn switch_model(client: &dyn LlmClient, cwd: &std::path::Path, model: String) {
    client.set_model(model.clone());
    let mut meta = crate::project_store::load_meta(cwd);
    meta.model = Some(model);
    let _ = crate::project_store::save_meta(cwd, &mut meta);
}

pub struct AgentContext {
    pub model: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: ToolRegistry,
    pub client: Arc<dyn LlmClient>,
    pub events: EventTx,
    pub cancel: CancellationToken,
    pub context_window: Option<usize>,
    /// Set in interactive mode to prompt user before destructive bash commands.
    /// None = auto-deny destructive commands.
    pub confirm: Option<ConfirmBridge>,
    /// How many recent messages to keep during compaction (default 6).
    pub compaction_keep_recent: Option<usize>,
    /// Permission policy: allow/soft-deny globs plus optional NL classifier.
    pub permissions: PermissionConfig,
    /// Shadow-git workspace snapshots; one per run, taken lazily before the
    /// first mutating tool call. None = disabled (localize, compaction, tests).
    pub snapshots: Option<Arc<crate::snapshot::Snapshots>>,
    /// Deterministic lifecycle hooks (config `hooks`): a `pre_tool_use` exit-code
    /// gate, the `post_tool_use` auto-checks (legacy `post_edit_check`) whose
    /// failures ride the last tool result, and a `stop` hook.
    pub hooks: crate::checks::Hooks,
    /// Verification oracle (`--oracle`): after Done, run the project's tests and
    /// loop on failure. None = disabled (the default, and all internal runs).
    pub oracle: Option<crate::oracle::Oracle>,
    /// Safety cap on LLM turns per `run()` (opt-in via `SIRBONE_MAX_STEPS`). When
    /// the count is exceeded the loop stops with an Info notice. None = unbounded
    /// (the default; the LLM alone decides when to stop).
    pub max_steps: Option<usize>,
    /// Token spend cap (config `spend_cap`, Feature C). `None` = disabled. When
    /// `tokens_spent` reaches it, the loop stops with a Notice (not an Error).
    pub spend_cap: Option<u64>,
    /// Cumulative real input+output tokens this session (falls back to the
    /// estimate when the provider reports no usage). Drives the cap and the
    /// `tok N/M` status-bar indicator.
    pub tokens_spent: u64,
}

/// Opt-in AFK safety bound: cap on LLM turns from `SIRBONE_MAX_STEPS`. Absent or
/// unparsable = None = unbounded (current behaviour).
pub fn env_max_steps() -> Option<usize> {
    std::env::var("SIRBONE_MAX_STEPS").ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::permissions::{self, Decision};
    use crate::types::{extract_text, AgentEvent, ContentBlock, Role, ToolCall};
    use tokio::sync::mpsc;

    struct FauxClient {
        turns: Vec<TurnResult>,
        idx: AtomicUsize,
    }

    impl FauxClient {
        fn new(turns: Vec<TurnResult>) -> Arc<Self> {
            Arc::new(Self {
                turns,
                idx: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl LlmClient for FauxClient {
        async fn run_turn(
            &self,
            _messages: &[&Message],
            _registry: &ToolRegistry,
            events: &EventTx,
            _cancel: &CancellationToken,
        ) -> Result<TurnResult> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            let result = self
                .turns
                .get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("faux: no more turns"))?;
            for block in &result.assistant_message.content {
                if let ContentBlock::Text { text } = block {
                    events.send(AgentEvent::TextChunk(text.clone())).await.ok();
                }
            }
            events.send(AgentEvent::TurnEnd).await.ok();
            Ok(result)
        }
    }

    fn make_text_turn(text: &str) -> TurnResult {
        TurnResult {
            assistant_message: Message::assistant(text),
            state: AgentState::Done,
            usage: Default::default(),
        }
    }

    fn make_tool_turn(tool_id: &str, tool_name: &str, args: serde_json::Value) -> TurnResult {
        TurnResult {
            assistant_message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: tool_id.into(),
                    name: tool_name.into(),
                    input: args.clone(),
                }],
            },
            usage: Default::default(),
            state: AgentState::ToolCalling(vec![ToolCall {
                id: tool_id.into(),
                name: tool_name.into(),
                arguments: args,
            }]),
        }
    }

    fn make_ctx(client: Arc<dyn LlmClient>, tools: ToolRegistry, tx: EventTx) -> AgentContext {
        AgentContext {
            model: "test".into(),
            system_prompt: None,
            messages: vec![Message::user("do the task")],
            tools,
            client,
            events: tx,
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

    #[tokio::test]
    async fn text_only_turn() {
        let (tx, mut rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![make_text_turn("hello from agent")]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);

        run(&mut ctx).await.unwrap();

        let mut texts = vec![];
        rx.close();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextChunk(s) = ev {
                texts.push(s);
            }
        }
        assert!(texts.iter().any(|s| s.contains("hello from agent")));
        assert_eq!(ctx.messages.len(), 2);
    }

    #[tokio::test]
    async fn tool_call_turn() {
        use crate::tools::BashTool;

        let (tx, _rx) = mpsc::channel(64);
        let mut registry = ToolRegistry::new();
        registry.register(BashTool::default());
        let client = FauxClient::new(vec![
            make_tool_turn("call1", "bash", serde_json::json!({"command": "echo hi"})),
            make_text_turn("done"),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        ctx.messages = vec![Message::user("run echo")];

        run(&mut ctx).await.unwrap();
        assert_eq!(ctx.messages.len(), 4);
    }

    #[tokio::test]
    async fn max_steps_caps_the_loop() {
        use crate::tools::BashTool;

        let (tx, mut rx) = mpsc::channel(256);
        let mut registry = ToolRegistry::new();
        registry.register(BashTool::default());
        // More tool-call turns than the cap allows: the loop must stop on the
        // cap, not by exhausting the script (which would error).
        let client = FauxClient::new(vec![
            make_tool_turn("c1", "bash", serde_json::json!({"command": "echo 1"})),
            make_tool_turn("c2", "bash", serde_json::json!({"command": "echo 2"})),
            make_tool_turn("c3", "bash", serde_json::json!({"command": "echo 3"})),
            make_tool_turn("c4", "bash", serde_json::json!({"command": "echo 4"})),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        ctx.messages = vec![Message::user("loop")];
        ctx.max_steps = Some(2);

        run(&mut ctx).await.unwrap();

        rx.close();
        let mut capped = false;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Notice { text, .. } = ev {
                capped |= text.contains("Max steps reached (2)");
            }
        }
        assert!(capped, "expected the max-steps notice to stop the loop");
    }

    #[tokio::test]
    async fn post_edit_check_failure_rides_tool_result() {
        use crate::tools::{UndoStore, WriteTool};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let (tx, _rx) = mpsc::channel(64);
        let mut registry = ToolRegistry::new();
        registry.register(WriteTool {
            undo: UndoStore::default(),
            stamps: crate::tools::ReadStamps::default(),
        });
        let client = FauxClient::new(vec![
            make_tool_turn(
                "call1",
                "write",
                serde_json::json!({"path": path.to_str().unwrap(), "content": "fn x() {}"}),
            ),
            make_text_turn("done"),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        ctx.hooks.post = crate::checks::PostEditChecks::from_value(Some(
            &serde_json::json!({"*.rs": "echo E0308 mismatched; exit 1"}),
        ));

        run(&mut ctx).await.unwrap();
        let injected = ctx.messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. }
                if content.contains("post-edit check failed") && content.contains("E0308"))
            })
        });
        assert!(injected, "check failure must ride the tool result");
    }

    #[tokio::test]
    async fn mutating_call_takes_one_snapshot() {
        use crate::tools::{UndoStore, WriteTool};

        let store = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let path = proj.path().join("a.txt");
        let (tx, _rx) = mpsc::channel(64);
        let mut registry = ToolRegistry::new();
        registry.register(WriteTool {
            undo: UndoStore::default(),
            stamps: crate::tools::ReadStamps::default(),
        });
        let client = FauxClient::new(vec![
            make_tool_turn(
                "c1",
                "write",
                serde_json::json!({"path": path.to_str().unwrap(), "content": "v1"}),
            ),
            make_tool_turn(
                "c2",
                "write",
                serde_json::json!({"path": path.to_str().unwrap(), "content": "v2"}),
            ),
            make_text_turn("done"),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        let snaps = Arc::new(crate::snapshot::Snapshots::at(
            store.path().join("snapshots.git"),
            proj.path().to_path_buf(),
        ));
        ctx.snapshots = Some(Arc::clone(&snaps));

        run(&mut ctx).await.unwrap();
        // Two mutating batches, one run: exactly one snapshot, taken BEFORE the
        // first write (so the snapshot tree does not contain a.txt).
        let entries = snaps.list(10).await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn destructive_bash_auto_denied() {
        use crate::tools::BashTool;

        let (tx, _rx) = mpsc::channel(64);
        let mut registry = ToolRegistry::new();
        registry.register(BashTool::default());
        let client = FauxClient::new(vec![
            make_tool_turn(
                "call1",
                "bash",
                serde_json::json!({"command": "rm -rf /tmp/test"}),
            ),
            make_text_turn("done"),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        ctx.messages = vec![Message::user("delete stuff")];
        ctx.confirm = None; // no bridge = auto-deny

        run(&mut ctx).await.unwrap();
        // tool_result should contain "blocked"
        let blocked = ctx.messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("blocked"))
            })
        });
        assert!(blocked);
    }

    #[tokio::test]
    async fn destructive_bash_approved_via_bridge() {
        use crate::tools::BashTool;

        let (tx, _rx) = mpsc::channel(64);
        let mut registry = ToolRegistry::new();
        registry.register(BashTool::default());
        let client = FauxClient::new(vec![
            make_tool_turn(
                "call1",
                "bash",
                serde_json::json!({"command": "rm -rf /tmp/pi_test_nonexistent"}),
            ),
            make_text_turn("done"),
        ]);
        let mut ctx = make_ctx(client, registry, tx);
        ctx.messages = vec![Message::user("delete stuff")];

        let (ask_tx, mut ask_rx) = mpsc::channel::<String>(1);
        let (reply_tx, reply_rx) = mpsc::channel::<bool>(1);
        ctx.confirm = Some(ConfirmBridge {
            ask: ask_tx,
            reply: reply_rx,
        });

        // Simulate user approving
        tokio::spawn(async move {
            ask_rx.recv().await.unwrap();
            reply_tx.send(true).await.unwrap();
        });

        run(&mut ctx).await.unwrap();
        let blocked = ctx.messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("blocked"))
            })
        });
        assert!(!blocked, "approved command should not be blocked");
    }

    #[test]
    fn lanes_serialize_same_path_mutations() {
        let tc = |name: &str, path: &str, id: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": path }),
        };
        let mut reg = ToolRegistry::new();
        reg.register(crate::tools::EditTool {
            undo: crate::tools::UndoStore::default(),
            stamps: crate::tools::ReadStamps::default(),
        });
        reg.register(crate::tools::WriteTool {
            undo: crate::tools::UndoStore::default(),
            stamps: crate::tools::ReadStamps::default(),
        });
        let lanes = plan_lanes(
            vec![
                tc("edit", "/tmp/a", "1"),
                tc("write", "/tmp/a", "2"), // same path -> shares lane 0
                tc("edit", "/tmp/b", "3"),  // different path -> own lane
            ],
            &reg,
        );
        // Two distinct files => two lanes; the /tmp/a lane holds both calls.
        assert_eq!(lanes.len(), 2);
        let a_lane = lanes.iter().find(|l| l.len() == 2).unwrap();
        assert_eq!(a_lane[0].id, "1");
        assert_eq!(a_lane[1].id, "2");
    }

    #[test]
    fn lanes_keep_non_mutating_parallel() {
        let tc = |name: &str, id: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({ "pattern": "x" }),
        };
        // grep/read declare no mutation_target -> each gets its own lane.
        let lanes = plan_lanes(
            vec![tc("grep", "1"), tc("grep", "2"), tc("read", "3")],
            &ToolRegistry::new(),
        );
        // No mutating calls -> every call gets its own lane.
        assert_eq!(lanes.len(), 3);
        assert!(lanes.iter().all(|l| l.len() == 1));
    }

    #[test]
    fn chit_chat_detects_greetings_not_tasks() {
        // Greetings / meta-questions -> chit-chat (single conversational turn).
        for s in [
            "ciao",
            "Ciao!",
            "  HELLO ",
            "chi sei?",
            "grazie",
            "what can you do?",
        ] {
            assert!(is_chit_chat(s), "expected chit-chat: {s:?}");
        }
        // Real tasks (incl. short ones) -> not chit-chat, normal workflow.
        for s in [
            "crea il file X",
            "fix the parser",
            "ciao mondo, scrivi un test", // greeting word but a real task
            "",
            "read src/agent.rs",
        ] {
            assert!(!is_chit_chat(s), "expected task: {s:?}");
        }
    }

    #[test]
    fn compaction_threshold() {
        assert!(!should_compact(100, 128_000, 20, 6));
        assert!(should_compact(120_000, 128_000, 20, 6));
        assert!(!should_compact(111_999, 128_000, 20, 6));
        // Too few messages to compact — should return false even at high token usage
        assert!(!should_compact(120_000, 128_000, 5, 6));
    }

    #[test]
    fn destructive_patterns() {
        // Non-git destruction lives in is_destructive; git history/work
        // destruction is handled separately by GIT_GUARDRAILS (soft_deny).
        let d = permissions::is_destructive;
        assert!(d("rm -rf /tmp"));
        assert!(d("sudo rm file"));
        assert!(d("rm\n-rf /"));
        assert!(d("rm && echo done"));
        assert!(d("rm"));
        assert!(d("shred /etc/passwd"));
        assert!(d("dd if=/dev/zero of=/dev/sda"));
        assert!(!d("echo hello"));
        assert!(!d("cargo test"));
        assert!(!d("grep -r pattern ."));
        assert!(!d("git reset --hard HEAD")); // guardrails, not is_destructive
        assert!(!d("warm day")); // 'rm' inside a word must not fire
    }

    /// One failing bash call repeated `n` times -> message history.
    fn failing_runs(n: usize, args: serde_json::Value) -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..n {
            let id = format!("t{i}");
            msgs.push(Message::assistant_with_tools(vec![ToolCall {
                id: id.clone(),
                name: "bash".into(),
                arguments: args.clone(),
            }]));
            msgs.push(Message::tool_result(id, "Error: boom", true));
        }
        msgs
    }

    #[test]
    fn stuck_tool_fires_at_threshold() {
        let args = serde_json::json!({ "command": "cargo build" });
        // Below threshold: no nudge.
        assert!(stuck_tool(&failing_runs(STUCK_THRESHOLD - 1, args.clone())).is_none());
        // Exactly at threshold: fires once.
        assert_eq!(
            stuck_tool(&failing_runs(STUCK_THRESHOLD, args.clone())).as_deref(),
            Some("bash")
        );
        // Past threshold: does not re-fire.
        assert!(stuck_tool(&failing_runs(STUCK_THRESHOLD + 1, args)).is_none());
    }

    #[test]
    fn stuck_tool_ignores_success_and_different_args() {
        // A trailing success breaks the streak.
        let mut msgs = failing_runs(STUCK_THRESHOLD - 1, serde_json::json!({ "command": "x" }));
        msgs.push(Message::assistant_with_tools(vec![ToolCall {
            id: "ok".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "x" }),
        }]));
        msgs.push(Message::tool_result("ok", "done", false));
        assert!(stuck_tool(&msgs).is_none());

        // Differing arguments are not a repeat.
        let mut msgs = Vec::new();
        for i in 0..STUCK_THRESHOLD {
            let id = format!("d{i}");
            msgs.push(Message::assistant_with_tools(vec![ToolCall {
                id: id.clone(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": format!("cmd{i}") }),
            }]));
            msgs.push(Message::tool_result(id, "Error", true));
        }
        assert!(stuck_tool(&msgs).is_none());
    }

    // --- estimate_context_tokens: exact per-block-kind accounting ---

    #[test]
    fn estimate_context_tokens_sums_each_block_kind() {
        // CHARS_PER_TOKEN_ESTIMATE = 3. Lengths are multiples of 3 so that a
        // `/`→`%` mutation collapses a term to 0 and `/`→`*` blows it up — both
        // diverge from the true division.
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "a".repeat(9),
                }, // 9/3 = 3
                ContentBlock::Thinking {
                    thinking: "b".repeat(6),
                }, // 6/3 = 2
                ContentBlock::ToolUse {
                    id: "i".into(),
                    name: "n".into(),
                    input: serde_json::json!({ "k": "v" }), // {"k":"v"} = 9 -> 3
                },
                ContentBlock::ToolResult {
                    tool_use_id: "i".into(),
                    content: "c".repeat(12), // 12/3 = 4
                    is_error: false,
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "d".repeat(15),
                }, // 15/3 = 5
            ],
        }];
        assert_eq!(estimate_context_tokens(&msgs), 3 + 2 + 3 + 4 + 5);
    }

    // --- nudge_if_stuck: appends the strategy-change note, only when stuck ---

    #[test]
    fn nudge_if_stuck_appends_on_streak() {
        let mut msgs = failing_runs(STUCK_THRESHOLD, serde_json::json!({ "command": "x" }));
        nudge_if_stuck(&mut msgs);
        let ContentBlock::ToolResult { content, .. } = msgs.last().unwrap().content.last().unwrap()
        else {
            panic!("last block should be a tool result");
        };
        assert!(
            content.contains("Stop repeating"),
            "nudge text should be appended"
        );
    }

    #[test]
    fn nudge_if_stuck_noop_below_threshold() {
        let mut msgs = failing_runs(STUCK_THRESHOLD - 1, serde_json::json!({ "command": "x" }));
        nudge_if_stuck(&mut msgs);
        let ContentBlock::ToolResult { content, .. } = msgs.last().unwrap().content.last().unwrap()
        else {
            panic!();
        };
        assert!(
            !content.contains("Stop repeating"),
            "no nudge below the streak threshold"
        );
    }

    // --- compact: structure and boundary of the compaction window ---

    #[tokio::test]
    async fn compact_replaces_old_messages_with_summary() {
        let (tx, _rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![make_text_turn("SUMMARY-BODY")]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.messages = (0..10).map(|i| Message::user(format!("m{i}"))).collect();
        ctx.compaction_keep_recent = Some(6); // to_compact = 10 - 6 = 4

        compact(&mut ctx).await.unwrap();

        // summary + ack + 6 recent
        assert_eq!(ctx.messages.len(), 8);
        let head = extract_text(&ctx.messages[0].content);
        assert!(head.starts_with("[Previous conversation summary"));
        assert!(head.contains("SUMMARY-BODY"));
        // The 6 most-recent messages are preserved, in order, at the tail.
        assert!(extract_text(&ctx.messages[2].content).contains("m4"));
        assert!(extract_text(&ctx.messages[7].content).contains("m9"));
    }

    #[tokio::test]
    async fn compact_succeeds_at_exactly_two_to_compact() {
        // to_compact == 2 must still compact: pins the `< 2` boundary against
        // `<=` / `==` mutations (which would bail here).
        let (tx, _rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![make_text_turn("S")]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.messages = (0..8).map(|i| Message::user(format!("m{i}"))).collect();
        ctx.compaction_keep_recent = Some(6); // to_compact = 2

        compact(&mut ctx).await.unwrap();
        assert_eq!(ctx.messages.len(), 8); // 2 compacted -> summary+ack, + 6 recent
    }

    /// Captures the messages it is asked to run — lets a test inspect the exact
    /// summary request `compact` builds.
    struct CapturingClient {
        reply: String,
        seen: std::sync::Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl LlmClient for CapturingClient {
        async fn run_turn(
            &self,
            messages: &[&Message],
            _registry: &ToolRegistry,
            events: &EventTx,
            _cancel: &CancellationToken,
        ) -> Result<TurnResult> {
            *self.seen.lock().unwrap() = messages.iter().copied().cloned().collect();
            events.send(AgentEvent::TurnEnd).await.ok();
            Ok(TurnResult {
                assistant_message: Message::assistant(self.reply.clone()),
                state: AgentState::Done,
                usage: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn compact_grounds_files_and_truncates_long_results() {
        let (tx, _rx) = mpsc::channel(64);
        let client = Arc::new(CapturingClient {
            reply: "S".into(),
            seen: std::sync::Mutex::new(vec![]),
        });
        let mut reg = ToolRegistry::new();
        reg.register(crate::tools::EditTool {
            undo: crate::tools::UndoStore::default(),
            stamps: crate::tools::ReadStamps::default(),
        });
        let mut ctx = make_ctx(client.clone(), reg, tx);
        // Two edits to the SAME path (dedup) + a long tool result (truncation).
        ctx.messages = vec![
            Message::assistant_with_tools(vec![ToolCall {
                id: "e1".into(),
                name: "edit".into(),
                arguments: serde_json::json!({ "path": "/p" }),
            }]),
            Message::tool_result("e1", "X".repeat(400), false),
            Message::assistant_with_tools(vec![ToolCall {
                id: "e2".into(),
                name: "edit".into(),
                arguments: serde_json::json!({ "path": "/p" }),
            }]),
            Message::user("f3"),
            Message::user("f4"),
            Message::user("f5"),
        ];
        ctx.compaction_keep_recent = Some(2); // compact the first 4 (incl. edits + result)

        compact(&mut ctx).await.unwrap();

        let seen = client.seen.lock().unwrap();
        let request = extract_text(&seen[1].content); // [system, user]
                                                      // Files-modified preamble is grounded in the edit tool calls, deduped.
        assert!(
            request.contains("Files modified"),
            "should list mutated files"
        );
        assert_eq!(
            request.matches("- /p").count(),
            1,
            "the path is listed once"
        );
        // The 400-char tool result is elided to a 300-char head + ellipsis.
        assert!(
            request.contains('…'),
            "long tool result should be truncated"
        );
        assert!(
            !request.contains(&"X".repeat(400)),
            "full long result must not be inlined"
        );
    }

    // --- localize: returns the last non-empty assistant report ---

    #[tokio::test]
    async fn localize_returns_last_nonempty_report() {
        let client = FauxClient::new(vec![
            make_tool_turn("c1", "read", serde_json::json!({ "path": "/nonexistent" })),
            make_text_turn("REPORT: change foo.rs"),
        ]);
        let report = localize(
            client,
            "test",
            "find the bug",
            crate::tools::read_only_registry(),
            3,
        )
        .await;
        assert_eq!(report.as_deref(), Some("REPORT: change foo.rs"));
    }

    #[test]
    fn approved_plan_note_carries_spec_and_directive() {
        let note = approved_plan_note("**Goal** — x");
        assert!(
            note.contains("APPROVED PLAN"),
            "labels the note for the model"
        );
        assert!(note.contains("**Goal** — x"), "embeds the spec verbatim");
        assert!(
            note.contains("deviate"),
            "tells the model to flag deviations"
        );
    }

    // --- decide: permission routing ---

    #[tokio::test]
    async fn decide_allows_safe_bash_without_classifier() {
        let (tx, _rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![]); // must never be called
        let ctx = make_ctx(client, ToolRegistry::new(), tx);
        let (decision, _) = decide(&ctx, "bash", &serde_json::json!({"command": "echo hi"})).await;
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn decide_classifies_in_configured_environment() {
        let (tx, _rx) = mpsc::channel(64);
        // The classifier reply denies the command.
        let client = FauxClient::new(vec![make_text_turn(
            r#"{"decision":"deny","reason":"prod"}"#,
        )]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.permissions.environment = vec!["production database".into()];
        // Non-readonly, non-destructive bash in a configured env -> classifier runs.
        let (decision, _) =
            decide(&ctx, "bash", &serde_json::json!({"command": "dropdb app"})).await;
        assert_eq!(decision, Decision::Deny("prod".into()));
    }

    #[tokio::test]
    async fn decide_pre_hook_allow_skips_classifier() {
        let (tx, _rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![]); // classifier must never run
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.permissions.environment = vec!["production database".into()];
        ctx.hooks.pre = vec![crate::checks::PreHook {
            matcher: "bash".into(),
            command: "exit 0".into(),
        }];
        let (decision, _) =
            decide(&ctx, "bash", &serde_json::json!({"command": "dropdb app"})).await;
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn decide_pre_hook_deny_blocks_before_classifier() {
        let (tx, _rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.permissions.environment = vec!["production database".into()];
        ctx.hooks.pre = vec![crate::checks::PreHook {
            matcher: "*".into(),
            command: "echo blocked >&2; exit 2".into(),
        }];
        let (decision, _) =
            decide(&ctx, "bash", &serde_json::json!({"command": "dropdb app"})).await;
        assert!(
            matches!(&decision, Decision::Deny(r) if r.contains("blocked")),
            "{decision:?}"
        );
    }

    // --- spend cap (Feature C) ---

    #[tokio::test]
    async fn spend_cap_stops_before_turn_when_exceeded() {
        let (tx, mut rx) = mpsc::channel(64);
        let client = FauxClient::new(vec![]); // must never run a turn
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.spend_cap = Some(1000);
        ctx.tokens_spent = 1000; // already at the cap
        run(&mut ctx).await.unwrap();
        rx.close();
        let mut notice = None;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Notice { text, .. } = ev {
                notice = Some(text);
            }
        }
        assert!(notice.unwrap_or_default().contains("spend cap reached"));
    }

    #[tokio::test]
    async fn spend_cap_prefers_real_usage_and_emits() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut turn = make_text_turn("done");
        turn.usage = crate::types::TokenUsage {
            input: 1234,
            output: 6,
        };
        let client = FauxClient::new(vec![turn]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        ctx.spend_cap = Some(10_000_000);
        run(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.tokens_spent, 1240,
            "real usage is summed, not the estimate"
        );
        rx.close();
        let mut spent = None;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::SpendUsage { spent: s, cap } = ev {
                assert_eq!(cap, 10_000_000);
                spent = Some(s);
            }
        }
        assert_eq!(spent, Some(1240));
    }

    #[tokio::test]
    async fn spend_cap_disabled_does_not_accumulate() {
        let (tx, _rx) = mpsc::channel(64);
        let mut turn = make_text_turn("done");
        turn.usage = crate::types::TokenUsage {
            input: 999,
            output: 1,
        };
        let client = FauxClient::new(vec![turn]);
        let mut ctx = make_ctx(client, ToolRegistry::new(), tx);
        // spend_cap None (default) -> no accounting overhead, counter stays 0.
        run(&mut ctx).await.unwrap();
        assert_eq!(ctx.tokens_spent, 0);
    }

    // --- switch_model: persists the choice to the project store ---

    #[tokio::test]
    async fn switch_model_persists_choice() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let client = FauxClient::new(vec![]);
        switch_model(client.as_ref(), cwd.path(), "my-model".into());
        let meta = crate::project_store::load_meta(cwd.path());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(meta.model.as_deref(), Some("my-model"));
    }

    // --- LlmClient default methods are "unsupported" until overridden ---

    #[tokio::test]
    async fn llm_client_defaults_are_unsupported() {
        let client = FauxClient::new(vec![]); // overrides none of the defaults
        assert!(client.list_models().await.is_err());
        assert_eq!(client.thinking_budget(), None);
        assert!(client
            .count_tokens(&[], &ToolRegistry::new())
            .await
            .is_err());
    }

    // --- compact: prior-summary chaining + truncation boundary ---

    #[tokio::test]
    async fn compact_folds_in_a_prior_summary() {
        // messages[0] is a previous summary: compact must carry it forward and
        // mark the boundary, not re-summarize it as fresh content.
        let (tx, _rx) = mpsc::channel(64);
        let client = Arc::new(CapturingClient {
            reply: "S2".into(),
            seen: std::sync::Mutex::new(vec![]),
        });
        let mut ctx = make_ctx(client.clone(), ToolRegistry::new(), tx);
        ctx.messages = vec![
            Message::user("[Previous conversation summary — 3 messages compacted]\nOLD-CONTEXT"),
            Message::assistant("ack"),
            Message::user("new1"),
            Message::user("new2"),
            Message::user("keep1"),
            Message::user("keep2"),
        ];
        ctx.compaction_keep_recent = Some(2);

        compact(&mut ctx).await.unwrap();
        let request = extract_text(&client.seen.lock().unwrap()[1].content);
        assert!(
            request.contains("New messages since last summary"),
            "boundary marker present"
        );
    }

    #[tokio::test]
    async fn compact_does_not_treat_plain_first_message_as_summary() {
        // A plain first message must NOT be mistaken for a prior summary.
        let (tx, _rx) = mpsc::channel(64);
        let client = Arc::new(CapturingClient {
            reply: "S".into(),
            seen: std::sync::Mutex::new(vec![]),
        });
        let mut ctx = make_ctx(client.clone(), ToolRegistry::new(), tx);
        ctx.messages = (0..6).map(|i| Message::user(format!("plain{i}"))).collect();
        ctx.compaction_keep_recent = Some(2);

        compact(&mut ctx).await.unwrap();
        let request = extract_text(&client.seen.lock().unwrap()[1].content);
        assert!(!request.contains("New messages since last summary"));
    }

    #[tokio::test]
    async fn compact_keeps_300_char_result_untruncated() {
        // Exactly 300 chars sits on the `> 300` boundary: no ellipsis. Pins the
        // truncation threshold against a `>`→`>=` mutation.
        let (tx, _rx) = mpsc::channel(64);
        let client = Arc::new(CapturingClient {
            reply: "S".into(),
            seen: std::sync::Mutex::new(vec![]),
        });
        let mut ctx = make_ctx(client.clone(), ToolRegistry::new(), tx);
        ctx.messages = vec![
            Message::assistant_with_tools(vec![ToolCall {
                id: "t".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": "x" }),
            }]),
            Message::tool_result("t", "Y".repeat(300), false),
            Message::user("a"),
            Message::user("b"),
        ];
        ctx.compaction_keep_recent = Some(2);

        compact(&mut ctx).await.unwrap();
        let request = extract_text(&client.seen.lock().unwrap()[1].content);
        assert!(
            !request.contains('…'),
            "a 300-char result is not over the limit"
        );
    }
}
