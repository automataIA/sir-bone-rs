use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

#[async_trait]
pub trait DynTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    async fn call(&self, args: serde_json::Value) -> Result<String>;

    /// File path this call mutates, if any (`None` for read-only / non-file
    /// tools). The permission pipeline uses this to gate writes into the trust
    /// root — centralizing it here means every file-writing tool is covered
    /// without a hardcoded name list (the bug: `write|edit|sed`-only checks let
    /// `save_skill` and future writers slip past). Default: `None`.
    fn mutation_target(&self, _args: &serde_json::Value) -> Option<PathBuf> {
        None
    }

    /// Short descriptor of the call's primary argument — the command for `bash`,
    /// the path for file tools — shown in the UI and permission prompts.
    /// Default: first of `command`/`path`/`file_path` in `args`, else `""`.
    fn inner_descriptor(&self, args: &serde_json::Value) -> String {
        args.get("command")
            .or_else(|| args.get("path"))
            .or_else(|| args.get("file_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

#[async_trait]
pub trait TypedTool: Send + Sync {
    type Input: DeserializeOwned + JsonSchema + Send;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    async fn run(&self, input: Self::Input) -> Result<String>;

    /// Override to declare a mutation target (see [`DynTool::mutation_target`]).
    fn mutation_target(&self, _args: &serde_json::Value) -> Option<PathBuf> {
        None
    }
}

#[async_trait]
impl<T: TypedTool> DynTool for T {
    fn name(&self) -> &'static str {
        T::name(self)
    }
    fn description(&self) -> &'static str {
        T::description(self)
    }
    fn schema(&self) -> serde_json::Value {
        schemars::schema_for!(T::Input).into()
    }
    async fn call(&self, args: serde_json::Value) -> Result<String> {
        let input: T::Input = serde_json::from_value(args)?;
        T::run(self, input).await
    }
    fn mutation_target(&self, args: &serde_json::Value) -> Option<PathBuf> {
        T::mutation_target(self, args)
    }
}

/// Adapter that precomputes a tool's JSON schema at registration.
/// `schemars::schema_for!` in the blanket impl re-derives the schema on every
/// request otherwise — static data recomputed on the per-turn critical path.
struct CachedSchemaTool {
    inner: Arc<dyn DynTool>,
    schema: serde_json::Value,
}

#[async_trait]
impl DynTool for CachedSchemaTool {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn description(&self) -> &'static str {
        self.inner.description()
    }
    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }
    async fn call(&self, args: serde_json::Value) -> Result<String> {
        self.inner.call(args).await
    }
    fn mutation_target(&self, args: &serde_json::Value) -> Option<PathBuf> {
        self.inner.mutation_target(args)
    }
    fn inner_descriptor(&self, args: &serde_json::Value) -> String {
        self.inner.inner_descriptor(args)
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Arc<dyn DynTool>>,
    /// Shared working-note store. Clones share the same note (Arc), so the
    /// `note` tool and the agent loop see the same value.
    pub notes: note::NoteStore,
    /// Shared background-job store (`bash` with `background: true`). The bash
    /// tool spawns into it, `job_status` reads it, the UI polls it for the
    /// status line and completion notifications.
    pub jobs: jobs::JobStore,
    /// Live step list maintained by the `todo` tool; UIs read it to render the
    /// model's current plan.
    pub todos: todo::TodoStore,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: TypedTool + 'static>(&mut self, tool: T) {
        let inner: Arc<dyn DynTool> = Arc::new(tool);
        let schema = inner.schema();
        self.tools
            .insert(inner.name(), Arc::new(CachedSchemaTool { inner, schema }));
    }

    /// Initialize the authoritative task contract and its small visual
    /// projection. The model can refine either, but does not need a setup turn.
    pub fn start_plan(&self, task: &str) {
        self.notes.start_plan(task);
        self.todos.set(vec![
            todo::TodoItem {
                content: "Ispezionare la richiesta".into(),
                status: todo::TodoStatus::InProgress,
            },
            todo::TodoItem {
                content: "Implementare la modifica".into(),
                status: todo::TodoStatus::Pending,
            },
            todo::TodoItem {
                content: "Verificare il risultato".into(),
                status: todo::TodoStatus::Pending,
            },
        ]);
    }

    /// Register an already-boxed tool. Used for runtime-discovered tools (e.g.
    /// MCP) that implement `DynTool` directly rather than via `TypedTool`.
    /// Filters here rather than via `apply_ablation`: MCP tools arrive after that
    /// pass, so a `SIRBONE_TOOLS` allowlist would otherwise leak them.
    pub fn register_dyn(&mut self, tool: Arc<dyn DynTool>) {
        if !crate::ablate::disabled_tool(tool.name()) {
            self.tools.insert(tool.name(), tool);
        }
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        // Embedder progress protocol: one stderr line per tool start/end, so a
        // spawning process (Juno's action bar) can show what the model is
        // doing mid-call. Opt-in via env so interactive runs stay clean.
        let watch = std::env::var_os("SIRBONE_TOOL_STDERR").is_some();
        if watch {
            eprintln!("tool-start: {name}");
        }
        let result = self
            .tools
            .get(name)
            .ok_or_else(|| self.unknown_tool_error(name))?
            .call(args)
            .await;
        if watch {
            eprintln!("tool-end: {name}");
        }
        result
    }

    /// Error for a call whose name is not in the registry, carrying the names
    /// that are. Streamed tool names arrive unvalidated on OpenAI-compatible
    /// endpoints, and some models leak their own delimiters into the field
    /// (`…<|tool_call_argument_begin|> web_search` was observed once from
    /// `mistral-small-latest`): a bare "unknown tool" leaves the model guessing,
    /// while the allowlist lets it re-issue the call in the same turn. The name
    /// is echoed truncated so a corrupted field cannot flood the transcript.
    fn unknown_tool_error(&self, name: &str) -> anyhow::Error {
        let shown: String = name.chars().take(60).collect();
        tracing::warn!(tool = %shown, "tool call with a name outside the registry");
        let mut known: Vec<&str> = self.tools.keys().copied().collect();
        known.sort_unstable();
        anyhow::anyhow!(
            "unknown tool: {shown}. Available tools: {}",
            known.join(", ")
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn DynTool> {
        self.tools.values().map(|b| b.as_ref())
    }

    /// The file path this call mutates, if the named tool declares one (see
    /// [`DynTool::mutation_target`]). None for read-only / non-file tools or an
    /// unknown name.
    pub fn mutation_target(&self, name: &str, args: &serde_json::Value) -> Option<PathBuf> {
        self.tools.get(name).and_then(|t| t.mutation_target(args))
    }

    /// Estimated context cost of one tool's wire entry, mirroring the
    /// `{name, description, input_schema}` shape sent to the API.
    fn schema_tokens(tool: &dyn DynTool) -> usize {
        serde_json::json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.schema(),
        })
        .to_string()
        .len()
            / truncate::CHARS_PER_TOKEN_ESTIMATE
    }

    fn schema_cost(&self, mcp: bool) -> (usize, usize) {
        self.iter()
            .filter(|t| t.name().starts_with("mcp__") == mcp)
            .map(Self::schema_tokens)
            .fold((0, 0), |(n, tok), t| (n + 1, tok + t))
    }

    /// Estimated context cost of MCP tools: `(count, tokens)`. MCP tool schemas
    /// ride in the system payload every turn, so they spend input budget the
    /// same as the prompt — worth surfacing (`-uW5-TaVXu4`: MCP bloats context).
    pub fn mcp_schema_cost(&self) -> (usize, usize) {
        self.schema_cost(true)
    }

    /// Same, for the native tools. They are the always-on half of the schema
    /// budget: unlike MCP servers nobody opts into them, so their cost is
    /// invisible until measured.
    pub fn native_schema_cost(&self) -> (usize, usize) {
        self.schema_cost(false)
    }

    /// Per-tool schema cost, dearest first. The ranking the ablation protocol
    /// needs: which tools are worth an A/B, ordered by what they cost every turn.
    pub fn schema_ranking(&self) -> Vec<(&str, usize)> {
        let mut rows: Vec<_> = self
            .iter()
            .map(|t| (t.name(), Self::schema_tokens(t)))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        rows
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Drop tools named in `SIRBONE_DISABLE` (feature-audit ablation). No-op when
    /// the env var is unset, so shipped runs are unaffected.
    pub fn apply_ablation(&mut self) {
        self.tools
            .retain(|name, _| !crate::ablate::disabled_tool(name));
    }
}

/// Read-only tool subset for the localization pre-pass (no edits/bash).
pub fn read_only_registry() -> ToolRegistry {
    let mut t = ToolRegistry::new();
    t.register(read::ReadTool::default());
    t.register(grep::GrepTool);
    t.register(glob::GlobTool);
    // code_map is read-only (symbol index / call graph; writes only in tests) and
    // is the purpose-built tool for grounding "where" — give the localization and
    // planning pre-passes structural lookup, not just text search.
    t.register(code_map::CodeMapTool {
        root: std::env::current_dir().unwrap_or_default(),
    });
    t.apply_ablation();
    t
}

pub mod ask_user;
pub mod bash;
pub mod code_map;
pub mod edit;
pub mod freshness;
pub mod glob;
pub mod grep;
pub mod historia;
pub mod jobs;
pub mod load_skill;
pub mod note;
pub mod patch;
pub mod read;
pub mod spill;
pub mod todo;
pub mod truncate;
pub mod undo;
pub mod verify;
pub mod web_fetch;
pub mod web_search;
pub mod write;

pub use ask_user::{AskUserRoundTool, AskUserTool};
pub use bash::BashTool;
pub use code_map::CodeMapTool;
pub use edit::EditTool;
pub use freshness::ReadStamps;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use historia::HistoriaTool;
pub use jobs::{JobStatusTool, JobStore};
pub use load_skill::LoadSkillTool;
pub use note::{NoteStore, NoteTool};
pub use patch::PatchTool;
pub use read::ReadTool;
pub use todo::{TodoItem, TodoStatus, TodoStore, TodoTool};
pub use truncate::{truncate_output, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
pub use undo::{UndoStore, UndoTool};
pub use verify::VerifyTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write::WriteTool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_registry_is_empty() {
        assert!(ToolRegistry::new().is_empty());
    }

    #[test]
    fn register_exposes_tool_via_iter() {
        let mut reg = ToolRegistry::new();
        reg.register(read::ReadTool::default());
        assert!(!reg.is_empty());
        assert!(reg.iter().any(|t| t.name() == "read"));
    }

    #[test]
    fn mcp_schema_cost_counts_only_mcp_tools() {
        struct FauxMcp;
        #[async_trait::async_trait]
        impl DynTool for FauxMcp {
            fn name(&self) -> &'static str {
                "mcp__srv__do"
            }
            fn description(&self) -> &'static str {
                "remote tool"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, _args: serde_json::Value) -> Result<String> {
                Ok(String::new())
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(read::ReadTool::default()); // non-MCP: ignored
        let (n0, _) = reg.mcp_schema_cost();
        assert_eq!(n0, 0);

        reg.register_dyn(std::sync::Arc::new(FauxMcp));
        let (n1, tok1) = reg.mcp_schema_cost();
        assert_eq!(n1, 1);
        assert!(tok1 > 0, "MCP schema should cost some tokens");

        // The native side is the complement, and the ranking covers both.
        let (native_n, native_tok) = reg.native_schema_cost();
        assert_eq!(native_n, 1); // read
        assert!(native_tok > 0);
        let ranking = reg.schema_ranking();
        assert_eq!(ranking.len(), 2);
        assert!(ranking[0].1 >= ranking[1].1, "ranking is dearest-first");
    }

    #[test]
    fn read_only_registry_has_the_safe_subset() {
        let reg = read_only_registry();
        let mut names: Vec<_> = reg.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        assert_eq!(names, ["code_map", "glob", "grep", "read"]);
    }

    #[test]
    fn native_file_mutators_declare_mutation_targets() {
        let path = serde_json::json!({"path": "src/lib.rs"});

        let mut reg = ToolRegistry::new();
        reg.register(write::WriteTool {
            undo: undo::UndoStore::default(),
            stamps: freshness::ReadStamps::default(),
        });
        reg.register(edit::EditTool {
            undo: undo::UndoStore::default(),
            stamps: freshness::ReadStamps::default(),
        });
        reg.register(undo::UndoTool {
            store: undo::UndoStore::default(),
        });

        for name in ["write", "edit", "undo"] {
            assert_eq!(
                reg.mutation_target(name, &path).as_deref(),
                Some(std::path::Path::new("src/lib.rs")),
                "{name} must expose its mutated path"
            );
        }
    }

    #[tokio::test]
    async fn execute_runs_a_registered_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();

        let mut reg = ToolRegistry::new();
        reg.register(read::ReadTool::default());
        let out = reg
            .execute("read", serde_json::json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(out.contains("alpha"));
    }

    #[tokio::test]
    async fn execute_unknown_tool_errors() {
        let reg = ToolRegistry::new();
        let err = reg
            .execute("nope", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    /// A corrupted streamed name must come back with the registry's names, and
    /// must not paste the whole corrupted field into the transcript.
    #[tokio::test]
    async fn unknown_tool_error_lists_registry_and_truncates_the_name() {
        let mut reg = ToolRegistry::new();
        reg.register(read::ReadTool::default());
        let garbage = format!("{}<|tool_call_argument_begin|> web_search", "x".repeat(200));
        let err = reg
            .execute(&garbage, serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Available tools: read"), "{err}");
        assert!(err.len() < 200, "{err}");
    }
}
