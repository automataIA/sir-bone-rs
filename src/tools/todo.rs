use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::TypedTool;
use crate::types::lock_or_recover;

/// Bound on list size — a step list past this is a planning smell, and the cap
/// keeps the per-turn re-render and the schema payload trivially small.
const MAX_ITEMS: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    /// The step, imperative and short (e.g. "Add failing test for the parser").
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Deserialize, JsonSchema)]
pub struct TodoInput {
    /// The full updated list (REPLACES the previous one). Keep at most one
    /// item in_progress at a time; mark items completed as soon as they are.
    pub todos: Vec<TodoItem>,
}

/// Shared step list. Lives in the `ToolRegistry` like [`super::note::NoteStore`],
/// so it survives context compaction and every UI (TUI, REPL, ACP) can read the
/// current plan state without parsing tool events.
#[derive(Clone, Default)]
pub struct TodoStore(Arc<Mutex<Vec<TodoItem>>>);

impl TodoStore {
    pub fn get(&self) -> Vec<TodoItem> {
        lock_or_recover(&self.0).clone()
    }
    pub fn set(&self, items: Vec<TodoItem>) {
        *lock_or_recover(&self.0) = items;
    }
}

/// One-line-per-step plain-text render, shared by the tool result (what the
/// model and headless mode see) and any UI without styled output.
pub fn render_plain(items: &[TodoItem]) -> String {
    items
        .iter()
        .map(|t| {
            let mark = match t.status {
                TodoStatus::Completed => "✔",
                TodoStatus::InProgress => "❯",
                TodoStatus::Pending => "☐",
            };
            format!("{mark} {}\n", t.content)
        })
        .collect()
}

pub struct TodoTool {
    pub store: TodoStore,
}

#[async_trait]
impl TypedTool for TodoTool {
    type Input = TodoInput;

    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Track your step list for multi-step tasks (3+ steps): break the task into \
         short imperative steps, mark exactly one in_progress while working on it, \
         and mark it completed as soon as it's done. Send the FULL list each call — \
         it replaces the previous one. The user sees this list as your live plan, \
         so keep it current; skip it for trivial single-step requests."
    }

    async fn run(&self, input: TodoInput) -> Result<String> {
        let mut items = input.todos;
        items.truncate(MAX_ITEMS);
        let done = items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let total = items.len();
        let body = render_plain(&items);
        self.store.set(items);
        Ok(format!("{body}{done}/{total} completed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    #[tokio::test]
    async fn replaces_caps_and_counts() {
        let store = TodoStore::default();
        let tool = TodoTool {
            store: store.clone(),
        };
        let out = tool
            .run(TodoInput {
                todos: vec![
                    item("a", TodoStatus::Completed),
                    item("b", TodoStatus::InProgress),
                    item("c", TodoStatus::Pending),
                ],
            })
            .await
            .unwrap();
        assert!(out.contains("1/3 completed"), "{out}");
        assert!(out.contains("✔ a") && out.contains("❯ b") && out.contains("☐ c"));
        assert_eq!(store.get().len(), 3);

        // Replace semantics, not append.
        tool.run(TodoInput {
            todos: vec![item("only", TodoStatus::Pending)],
        })
        .await
        .unwrap();
        assert_eq!(store.get().len(), 1);

        // Cap.
        tool.run(TodoInput {
            todos: (0..100)
                .map(|i| item(&i.to_string(), TodoStatus::Pending))
                .collect(),
        })
        .await
        .unwrap();
        assert_eq!(store.get().len(), MAX_ITEMS);
    }
}
