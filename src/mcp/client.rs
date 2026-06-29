//! Thin wrapper over an rmcp stdio client: spawn one MCP server as a child
//! process, list its tools, call them. Signatures verified against rmcp 1.7.

use anyhow::{Context, Result};
use rmcp::{
    model::{CallToolRequestParams, Tool},
    service::{RoleClient, RunningService, ServiceExt},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use std::time::Duration;
use tokio::process::Command;

/// Upper bound on the MCP handshake; a hung server is skipped, not blocking.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// A live connection to one MCP server over stdio. Dropping it terminates the
/// child process, so it must stay alive for as long as its tools are usable.
pub struct McpServer {
    service: RunningService<RoleClient, ()>,
}

impl McpServer {
    /// Spawn `program args...` as a child MCP server and complete the handshake.
    /// e.g. `spawn("npx", &["-y".into(), "@upstash/context7-mcp@latest".into()])`.
    /// The handshake is bounded by `HANDSHAKE_TIMEOUT` so a server that starts
    /// but hangs can't block startup forever.
    pub async fn spawn(program: &str, args: &[String]) -> Result<Self> {
        let args = args.to_vec();
        // stdout is the JSON-RPC pipe; the child's stderr is its own banner/logging
        // (e.g. Context7 prints "… running on stdio"). The builder's stdio settings
        // are applied at spawn and override anything set in `configure`, so stderr
        // must be silenced here on the builder — otherwise its default `inherit()`
        // leaks the banner into the TUI's first frame.
        let (transport, _stderr) =
            TokioChildProcess::builder(Command::new(program).configure(|cmd| {
                cmd.args(&args);
            }))
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("cannot spawn MCP server `{program}`"))?;
        let service = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
            .await
            .with_context(|| format!("MCP handshake timed out for `{program}`"))?
            .with_context(|| format!("MCP handshake failed for `{program}`"))?;
        Ok(Self { service })
    }

    /// Tools advertised by the server.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        Ok(self.service.list_tools(Default::default()).await?.tools)
    }

    /// Invoke a tool by its *server-side* name; `args` is a JSON object.
    /// Concatenates the text content blocks of the result.
    pub async fn call_tool(&self, name: String, args: serde_json::Value) -> Result<String> {
        let mut req = CallToolRequestParams::new(name);
        if let Some(obj) = args.as_object() {
            req = req.with_arguments(obj.clone());
        }
        let result = self.service.call_tool(req).await?;
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect();
        Ok(text)
    }
}
