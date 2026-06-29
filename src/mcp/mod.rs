//! Generic MCP client integration. Server *definitions* live in one global
//! catalog, `~/.sirbone/mcp.json` (`{ "mcpServers": { name: {...} } }`, the
//! Claude Desktop schema); each project's `config.json` only references servers
//! by name in its `mcp.enabled` allowlist. Allowlist semantics: a project runs
//! no MCP server until it opts one in. Each advertised tool is registered as a
//! `DynTool` named `mcp__<server>__<tool>` so the LLM sees it beside the natives.

pub mod client;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tools::{DynTool, ToolRegistry};
use client::McpServer;

/// One server entry from the `mcpServers` catalog map.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// When true, this server's tools auto-run; otherwise each call needs
    /// confirmation (the safe default for untrusted external tools).
    #[serde(default)]
    pub trust: bool,
}

/// Map of server name → trust flag, over the servers the project has enabled.
/// Backs the permission gate for `mcp__<server>__*` tools.
pub fn trust_map() -> HashMap<String, bool> {
    load_config()
        .into_iter()
        .map(|(k, v)| (k, v.trust))
        .collect()
}

/// The global MCP catalog file, `~/.sirbone/mcp.json`.
pub fn catalog_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".sirbone").join("mcp.json"))
}

/// Parse the `mcpServers` object out of a JSON file. Missing file or malformed
/// config yields an empty map — never an error.
fn read_mcp_servers(path: &std::path::Path) -> HashMap<String, McpServerConfig> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("mcpServers")?.clone()).ok())
        .unwrap_or_default()
}

/// Every server defined in the global catalog (`~/.sirbone/mcp.json`), regardless
/// of enablement. Used by the TUI picker to list choices. Missing file → empty.
pub fn read_catalog() -> HashMap<String, McpServerConfig> {
    catalog_path()
        .map(|p| read_mcp_servers(&p))
        .unwrap_or_default()
}

/// Servers the current project actually runs: catalog entries whose name is in
/// the project's `mcp.enabled` allowlist. Default-off — an empty allowlist (a
/// fresh project) runs nothing.
pub fn load_config() -> HashMap<String, McpServerConfig> {
    enabled_subset(read_catalog(), &crate::config::mcp_enabled())
}

/// Keep only catalog entries named in `enabled`. (pure)
fn enabled_subset(
    mut catalog: HashMap<String, McpServerConfig>,
    enabled: &[String],
) -> HashMap<String, McpServerConfig> {
    catalog.retain(|name, _| enabled.iter().any(|e| e == name));
    catalog
}

/// One-time migration to the catalog model: lift any inline `mcpServers` still
/// living in the global `config.json` into `~/.sirbone/mcp.json` (definitions
/// only — enablement stays per-project and off). No-op once the catalog already
/// holds them or there's nothing to migrate. Best-effort; warns on what it moved
/// so the user knows to enable servers per project (Settings → MCP).
pub fn migrate_inline_servers() {
    let Some(global) = crate::config::global_path() else {
        return;
    };
    let inline = read_mcp_servers(&global);
    if inline.is_empty() {
        return;
    }
    let Some(cat_path) = catalog_path() else {
        return;
    };
    let mut catalog = read_catalog();
    let mut moved: Vec<String> = Vec::new();
    for (name, cfg) in inline {
        if !catalog.contains_key(&name) {
            catalog.insert(name.clone(), cfg);
            moved.push(name);
        }
    }
    // Persist the catalog only if we added something; if it failed, bail before
    // stripping so we never drop the inline copy without a safe home for it.
    if !moved.is_empty() {
        let body = serde_json::json!({ "mcpServers": catalog });
        let Ok(text) = serde_json::to_string_pretty(&body) else {
            return;
        };
        if let Some(parent) = cat_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&cat_path, text).is_err() {
            return;
        }
    }
    // Every inline server is now in the catalog (already present or just written):
    // drop them — and the dead skills.disabled — from config.json so the file
    // reflects the new format. Runs even when nothing new was moved (e.g. a prior
    // run migrated the definitions but left the inline copy behind).
    crate::config::strip_obsolete_global_keys();
    if !moved.is_empty() {
        moved.sort();
        eprintln!(
            "MCP: migrated {} server(s) from config.json into ~/.sirbone/mcp.json ({}). \
             They are now disabled by default — enable per project in Settings → MCP.",
            moved.len(),
            moved.join(", ")
        );
    }
}

/// A single MCP tool exposed to the agent as a `DynTool`. Holds an `Arc` to its
/// server so the child process stays alive for as long as the tool is reachable.
struct McpToolAdapter {
    server: Arc<McpServer>,
    /// Tool name as seen by the LLM and registry key: `mcp__<server>__<tool>`.
    qualified_name: &'static str,
    description: &'static str,
    /// Server-side tool name used when forwarding the call.
    remote_name: String,
    schema: serde_json::Value,
}

#[async_trait]
impl DynTool for McpToolAdapter {
    fn name(&self) -> &'static str {
        self.qualified_name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }
    async fn call(&self, args: serde_json::Value) -> Result<String> {
        self.server.call_tool(self.remote_name.clone(), args).await
    }
}

/// Spawn every configured MCP server and build their tools, without touching a
/// registry. Returns `(tools, handles)`; the caller registers the tools and must
/// keep the handles alive for the whole session (dropping one kills its child
/// process). A server that fails to start is logged and skipped — never fatal.
/// Decoupled from the registry so startup can run this off the critical path
/// (`npx`-spawned servers cost seconds) and register the result just before the
/// first turn.
pub type McpLoad = (Vec<Arc<dyn DynTool>>, Vec<Arc<McpServer>>);

pub async fn collect_tools() -> McpLoad {
    let mut tools: Vec<Arc<dyn DynTool>> = Vec::new();
    let mut handles = Vec::new();
    for (name, cfg) in load_config() {
        let server = match McpServer::spawn(&cfg.command, &cfg.args).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("warning: MCP server `{name}` failed to start: {e}");
                continue;
            }
        };
        let remote_tools = match server.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: MCP server `{name}` list_tools failed: {e}");
                continue;
            }
        };
        for tool in remote_tools {
            let remote_name = tool.name.to_string();
            let qualified: &'static str =
                Box::leak(format!("mcp__{name}__{remote_name}").into_boxed_str());
            let description: &'static str = Box::leak(
                tool.description
                    .map(|d| d.to_string())
                    .unwrap_or_default()
                    .into_boxed_str(),
            );
            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            tools.push(Arc::new(McpToolAdapter {
                server: server.clone(),
                qualified_name: qualified,
                description,
                remote_name,
                schema,
            }));
        }
        handles.push(server);
    }
    (tools, handles)
}

/// Spawn + register in one step (blocking the caller until ready). Retained for
/// callers that don't need the background-load split. See [`collect_tools`].
pub async fn load_servers(registry: &mut ToolRegistry) -> Vec<Arc<McpServer>> {
    let (tools, handles) = collect_tools().await;
    for tool in tools {
        registry.register_dyn(tool);
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_servers_config() {
        let v = serde_json::json!({
            "mcpServers": {
                "context7": { "command": "npx", "args": ["-y", "@upstash/context7-mcp@latest"], "trust": true },
                "noargs": { "command": "mytool" }
            }
        });
        let map: HashMap<String, McpServerConfig> =
            serde_json::from_value(v.get("mcpServers").unwrap().clone()).unwrap();
        assert_eq!(map["context7"].command, "npx");
        assert_eq!(map["context7"].args, ["-y", "@upstash/context7-mcp@latest"]);
        assert!(map["context7"].trust); // explicit trust
        assert!(map["noargs"].args.is_empty()); // args defaults to empty
        assert!(!map["noargs"].trust); // trust defaults to false (untrusted)
    }

    #[test]
    fn read_mcp_servers_parses_mcp_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{ "mcpServers": { "local": { "command": "mytool", "trust": true } } }"#,
        )
        .unwrap();
        let map = read_mcp_servers(&path);
        assert_eq!(map["local"].command, "mytool");
        assert!(map["local"].trust);
        // Missing file → empty, never an error.
        assert!(read_mcp_servers(&dir.path().join("absent.json")).is_empty());
    }

    #[test]
    fn enabled_subset_keeps_only_allowlisted() {
        let catalog: HashMap<String, McpServerConfig> = serde_json::from_value(serde_json::json!({
            "context7": { "command": "npx" },
            "local":    { "command": "mytool" },
            "other":    { "command": "x" }
        }))
        .unwrap();
        let kept = enabled_subset(catalog, &["context7".into(), "local".into()]);
        assert_eq!(kept.len(), 2);
        assert!(kept.contains_key("context7"));
        assert!(kept.contains_key("local"));
        assert!(!kept.contains_key("other")); // not in allowlist → dropped
                                              // Empty allowlist (fresh project) runs nothing.
        let none = enabled_subset(
            serde_json::from_value(serde_json::json!({ "context7": { "command": "npx" } }))
                .unwrap(),
            &[],
        );
        assert!(none.is_empty());
    }
}
