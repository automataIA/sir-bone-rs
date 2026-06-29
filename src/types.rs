use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};

/// Lock acquisition that recovers from poisoning instead of panicking. A lock
/// is poisoned only if a holder panicked; the data here (notes, model name,
/// counters) stays valid, so recovering beats cascading the panic.
pub fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn read_or_recover<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

pub fn write_or_recover<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

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
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant_with_tools(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: tool_calls
                .into_iter()
                .map(|tc| ContentBlock::ToolUse {
                    id: tc.id,
                    name: tc.name,
                    input: tc.arguments,
                })
                .collect(),
        }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

pub fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TextChunk(String),
    ThinkingStart,
    ThinkingChunk(String),
    ToolCallStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCallEnd {
        id: String,
        name: String,
        result: String,
        is_error: bool,
    },
    TurnEnd,
    Cancelled,
    Error(String),
    /// Out-of-band status line that is *not* a failure — e.g. the verification
    /// oracle reporting green/retry. `level` lets the renderer colour it (green
    /// success, amber progress) instead of the red reserved for `Error`.
    Notice {
        text: String,
        level: NoticeLevel,
    },
    /// `used_tokens` is the full prompt size (uncached + cache reads/writes);
    /// `cached_tokens` is the slice of it served from the prompt cache at ~0.1×
    /// price — 0 across requests means a silent cache invalidator is at work.
    ContextUsage {
        used_tokens: u32,
        context_window: u32,
        cached_tokens: u32,
    },
    /// Context was compacted; carries the full post-compaction transcript
    /// (summary + ack + kept recent messages) so consumers can persist it.
    Compacted {
        messages: Vec<Message>,
    },
    /// A background job (bash `background: true`) finished. Emitted by the UI
    /// poller, not the agent loop.
    JobDone {
        id: u32,
        command: String,
        exit: Option<i32>,
        secs: u64,
    },
    /// Cumulative token spend for the session and the active cap, when the spend
    /// cap is enabled. Drives the `tok N/M` status-bar indicator. `cap` is the
    /// configured ceiling. Distinct from `ContextUsage` (a per-call snapshot).
    SpendUsage {
        spent: u64,
        cap: u64,
    },
    /// A workspace snapshot was taken for this run. The id is the full shadow-git
    /// commit id, usable with `/rollback <id>`.
    WorkspaceSnapshot {
        id: String,
        label: String,
    },
}

/// Real token usage for one LLM turn, used to accumulate session spend for the
/// spend cap. Zero when the provider returned no usage (then callers fall back
/// to the estimate). See [`crate::checks`]-adjacent plumbing in `agent::run`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input as u64 + self.output as u64
    }
}

/// Severity of an [`AgentEvent::Notice`], so the frontends can pick a colour.
#[derive(Debug, Clone, Copy)]
pub enum NoticeLevel {
    /// Something went right (green, ✓).
    Success,
    /// In-progress / cautionary status, no failure yet (amber).
    Info,
}

#[derive(Debug, Clone)]
pub enum AgentState {
    Idle,
    ToolCalling(Vec<ToolCall>),
    Done,
}

pub type EventTx = tokio::sync::mpsc::Sender<AgentEvent>;
pub type EventRx = tokio::sync::mpsc::Receiver<AgentEvent>;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn lock_or_recover_survives_poisoning() {
        let m = std::sync::Arc::new(Mutex::new(7u32));
        let m2 = std::sync::Arc::clone(&m);
        // Poison the mutex: panic while holding the guard.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock();
            panic!("poison");
        })
        .join();
        assert!(m.lock().is_err(), "mutex should be poisoned");
        assert_eq!(*lock_or_recover(&m), 7);
        *lock_or_recover(&m) = 9;
        assert_eq!(*lock_or_recover(&m), 9);
    }

    #[test]
    fn rwlock_recover_round_trip() {
        let l = RwLock::new(String::from("a"));
        *write_or_recover(&l) = "b".into();
        assert_eq!(*read_or_recover(&l), "b");
    }

    fn arb_value() -> impl Strategy<Value = serde_json::Value> {
        // A small string→string object — enough to exercise ToolUse inputs while
        // staying exactly round-trippable through JSON.
        proptest::collection::hash_map("[a-z_]{1,8}", "[a-zA-Z0-9 ._/-]{0,24}", 0..4)
            .prop_map(|m| serde_json::to_value(m).unwrap())
    }

    fn arb_block() -> impl Strategy<Value = ContentBlock> {
        prop_oneof![
            ".*".prop_map(|text| ContentBlock::Text { text }),
            ".*".prop_map(|thinking| ContentBlock::Thinking { thinking }),
            ("[a-z]{1,10}/[a-z]{1,10}", "[A-Za-z0-9+/=]{0,40}")
                .prop_map(|(media_type, data)| ContentBlock::Image { media_type, data }),
            ("[a-z]{1,10}", "[a-z_]{1,10}", arb_value())
                .prop_map(|(id, name, input)| ContentBlock::ToolUse { id, name, input }),
            ("[a-z]{1,10}", ".*", any::<bool>()).prop_map(|(tool_use_id, content, is_error)| {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                }
            }),
        ]
    }

    fn arb_message() -> impl Strategy<Value = Message> {
        let role = prop_oneof![
            Just(Role::System),
            Just(Role::User),
            Just(Role::Assistant),
            Just(Role::Tool),
        ];
        (role, proptest::collection::vec(arb_block(), 0..5))
            .prop_map(|(role, content)| Message { role, content })
    }

    proptest! {
        /// Any message survives a JSON serialize→deserialize round-trip intact.
        #[test]
        fn message_serde_roundtrip(m in arb_message()) {
            let json = serde_json::to_string(&m).unwrap();
            let back: Message = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(m, back);
        }

        /// `extract_text` concatenates exactly the `Text` blocks, in order.
        #[test]
        fn extract_text_concatenates_text_blocks(blocks in proptest::collection::vec(arb_block(), 0..6)) {
            let expected: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(extract_text(&blocks), expected);
        }
    }

    #[test]
    fn role_serde_round_trip() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn content_block_serde_round_trip() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::ToolUse {
                id: "id1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "id1".into(),
                content: "hi\n".into(),
                is_error: false,
            },
        ];
        for block in &blocks {
            let json = serde_json::to_string(block).unwrap();
            let back: ContentBlock = serde_json::from_str(&json).unwrap();
            assert_eq!(*block, back);
        }
    }

    #[test]
    fn message_helpers() {
        let m = Message::user("hello");
        assert_eq!(m.role, Role::User);
        assert_eq!(
            m.content,
            vec![ContentBlock::Text {
                text: "hello".into()
            }]
        );

        let m = Message::tool_result("id1", "output", false);
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "id1");
                assert_eq!(content, "output");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn message_serde_round_trip() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn tool_call_serde_round_trip() {
        let tc = ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
    }
}
