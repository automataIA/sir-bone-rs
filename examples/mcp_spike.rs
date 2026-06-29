//! Throwaway spike: verify rmcp 1.7 client API against the real crate.
//! Compile-check needs no server. Runtime roundtrip needs Node/npx + a server.
//!
//!   cargo build --example mcp_spike          # API verification (no server)
//!   cargo run --example mcp_spike            # roundtrip (spawns Context7)

use anyhow::Result;
use rmcp::{
    model::CallToolRequestParams,
    service::ServiceExt,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

#[tokio::main]
async fn main() -> Result<()> {
    let program = "npx";
    let args = ["-y", "@upstash/context7-mcp@latest"];

    let service = ()
        .serve(TokioChildProcess::new(Command::new(program).configure(
            |cmd| {
                cmd.args(args);
            },
        ))?)
        .await?;

    let tools = service.list_tools(Default::default()).await?;
    println!("tools: {}", tools.tools.len());
    for t in &tools.tools {
        println!("  - {}: {}", t.name, t.description.as_deref().unwrap_or(""));
    }

    if let Some(first) = tools.tools.first() {
        let req = CallToolRequestParams::new(first.name.clone());
        let result = service.call_tool(req).await?;
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();
        println!(
            "call result: {}",
            text.chars().take(200).collect::<String>()
        );
    }

    service.cancel().await?;
    Ok(())
}
