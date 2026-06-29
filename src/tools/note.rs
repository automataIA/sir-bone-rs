use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;
use crate::types::lock_or_recover;

/// Cap on the persisted note. Replace-semantics keep it bounded by construction;
/// this only guards against a runaway single write.
const MAX_NOTE_CHARS: usize = 4000;

/// Shared, mutable working note. Lives in the `ToolRegistry` (and thus survives
/// context compaction, which only rewrites `messages`); `run_turn` re-injects it
/// each turn so it stays visible regardless of how old the task is.
#[derive(Clone, Default)]
pub struct NoteStore(Arc<Mutex<String>>);

impl NoteStore {
    pub fn get(&self) -> String {
        lock_or_recover(&self.0).clone()
    }
    pub fn set(&self, s: String) {
        *lock_or_recover(&self.0) = s;
    }
    /// Set the note only if currently empty — used to pre-seed (e.g. a
    /// localization report) without clobbering later model updates.
    pub fn seed(&self, s: String) {
        let mut g = lock_or_recover(&self.0);
        if g.trim().is_empty() {
            *g = s;
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteInput {
    /// The full updated note (this REPLACES the previous one).
    pub content: String,
}

pub struct NoteTool {
    pub store: NoteStore,
}

#[async_trait]
impl TypedTool for NoteTool {
    type Input = NoteInput;

    fn name(&self) -> &'static str {
        "note"
    }

    fn description(&self) -> &'static str {
        "Save your working notes — plan, root-cause hypothesis, files changed so far, \
         what's left. This REPLACES the previous note, so keep one concise living \
         summary (not an append log). The note persists across context compaction and \
         is shown every turn, so on long tasks update it as you make progress to avoid \
         losing track."
    }

    async fn run(&self, input: NoteInput) -> Result<String> {
        let content: String = input.content.chars().take(MAX_NOTE_CHARS).collect();
        self.store.set(content);
        Ok("note saved".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_and_caps() {
        let store = NoteStore::default();
        let tool = NoteTool {
            store: store.clone(),
        };
        tool.run(NoteInput {
            content: "first".into(),
        })
        .await
        .unwrap();
        assert_eq!(store.get(), "first");
        // replace semantics, not append
        tool.run(NoteInput {
            content: "second".into(),
        })
        .await
        .unwrap();
        assert_eq!(store.get(), "second");
        // cap
        tool.run(NoteInput {
            content: "x".repeat(5000),
        })
        .await
        .unwrap();
        assert_eq!(store.get().chars().count(), MAX_NOTE_CHARS);
    }
}
