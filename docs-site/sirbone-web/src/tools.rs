//! Pure data types copied from `src/tools/todo.rs` (the shared step list).
//!
//! Same pattern as `types.rs`: the #[path]-included render modules resolve
//! `crate::tools::*` against this shim, because the real `tools` module pulls
//! tokio/schemars (neither builds for `wasm32` — the `JsonSchema` derive is
//! intentionally dropped here).
//!
//! keep in sync with src/tools/todo.rs — only the data types are copied;
//! `TodoStore` and the tool machinery are intentionally omitted.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoItem {
    /// The step, imperative and short (e.g. "Add failing test for the parser").
    pub content: String,
    pub status: TodoStatus,
}
