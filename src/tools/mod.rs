use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

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

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Arc<dyn DynTool>>,
    /// Shared working-note store. Clones share the same note (Arc), so the
    /// `note` tool and the agent loop see the same value.
    pub notes: note::NoteStore,
    /// Live conversation snapshot, refreshed by the agent loop before tool
    /// execution; the `architect` tool forwards it to its reviewer model.
    pub transcript: architect::Transcript,
    /// Per-task architect consult counter (reset at the start of each run).
    pub architect_calls: Arc<Mutex<u32>>,
    /// Runtime on/off for the architect tool (Settings screen). Shared with the
    /// tool so toggling takes effect without re-registering.
    pub architect_enabled: Arc<AtomicBool>,
    /// Shared background-job store (`bash` with `background: true`). The bash
    /// tool spawns into it, `job_status` reads it, the UI polls it for the
    /// status line and completion notifications.
    pub jobs: jobs::JobStore,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: TypedTool + 'static>(&mut self, tool: T) {
        self.tools.insert(T::name(&tool), Arc::new(tool));
    }

    /// Register an already-boxed tool. Used for runtime-discovered tools (e.g.
    /// MCP) that implement `DynTool` directly rather than via `TypedTool`.
    pub fn register_dyn(&mut self, tool: Arc<dyn DynTool>) {
        self.tools.insert(tool.name(), tool);
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        self.tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?
            .call(args)
            .await
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

    /// Estimated context cost of MCP tools: `(count, tokens)`. MCP tool schemas
    /// ride in the system payload every turn, so they spend input budget the
    /// same as the prompt — worth surfacing (`-uW5-TaVXu4`: MCP bloats context).
    /// Mirrors the `{name, description, input_schema}` wire shape sent to the API.
    pub fn mcp_schema_cost(&self) -> (usize, usize) {
        self.iter()
            .filter(|t| t.name().starts_with("mcp__"))
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.schema(),
                })
                .to_string()
                .len()
                    / truncate::CHARS_PER_TOKEN_ESTIMATE
            })
            .fold((0, 0), |(n, tok), t| (n + 1, tok + t))
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

pub mod architect;
pub mod bash;
pub mod code_map;
#[cfg(feature = "rag")]
pub mod doc_search;
pub mod edit;
pub mod freshness;
pub mod glob;
pub mod grep;
pub mod historia;
pub mod jobs;
pub mod load_skill;
pub mod note;
pub mod read;
pub mod truncate;
pub mod undo;
pub mod verify;
pub mod web_fetch;
pub mod web_search;
pub mod write;

pub use architect::{Architect, ArchitectTool};
pub use bash::BashTool;
pub use code_map::CodeMapTool;
#[cfg(feature = "rag")]
pub use doc_search::DocSearchTool;
pub use edit::EditTool;
pub use freshness::ReadStamps;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use historia::HistoriaTool;
pub use jobs::{JobStatusTool, JobStore};
pub use load_skill::LoadSkillTool;
pub use note::{NoteStore, NoteTool};
pub use read::ReadTool;
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
}
