//! Pure data types copied from `src/agent/mod.rs` (the agent's `Prompt` family).
//!
//! Same pattern as `types.rs`: the #[path]-included render modules resolve
//! `crate::agent::*` against this shim, because the real `agent` module pulls
//! tokio/reqwest (neither builds for `wasm32`).
//!
//! keep in sync with src/agent/mod.rs — only the data types are copied;
//! `ConfirmBridge` and everything tokio-bound is intentionally omitted.

/// A decision requested from the user: a permission gate on a tool call, or a
/// free-form question raised by the `ask_user` tool. One primitive drives every
/// interactive surface (TUI dialog, REPL menu, VSCode webview).
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Short dialog title, e.g. "permission required" or the model's question.
    pub title: String,
    /// Optional body: the command to run, or extra context for a question.
    pub detail: Option<String>,
    /// Selectable options, in display order.
    pub options: Vec<String>,
    /// Append an "Other…" free-text row after the options.
    pub allow_free_text: bool,
    pub kind: PromptKind,
}

/// What a [`Prompt`] is gating.
#[derive(Debug, Clone)]
pub enum PromptKind {
    /// Permission gate on a tool call. `suggested_glob` pre-fills the editable
    /// "allow always" rule the UI offers (see `permissions::suggested_glob`).
    Permission { suggested_glob: String },
    /// A free-form question from the `ask_user` tool.
    Question,
    /// One to three independent questions presented as one human interaction.
    QuestionRound { questions: Vec<PromptQuestion> },
}

/// One question inside a [`PromptKind::QuestionRound`].
#[derive(Debug, Clone)]
pub struct PromptQuestion {
    pub id: String,
    pub title: String,
    pub detail: Option<String>,
    pub options: Vec<String>,
    pub allow_free_text: bool,
}

/// One selected value inside an aggregated question-round reply.
#[derive(Debug, Clone, Default)]
pub struct PromptAnswer {
    pub index: Option<usize>,
    pub text: Option<String>,
}

/// The user's answer to a [`Prompt`].
#[derive(Debug, Clone, Default)]
pub struct PromptReply {
    /// Chosen option index, or `None` when only free text was supplied.
    pub index: Option<usize>,
    /// Free text: the "Other" value, deny feedback, or an edited allow-glob.
    pub text: Option<String>,
    /// Answers to a question round, in the same order as its questions.
    pub answers: Vec<PromptAnswer>,
}
