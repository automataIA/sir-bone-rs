//! Exercises sirbone's own MCP path (not just rmcp): `load_servers` → adapter →
//! `register_dyn` → `ToolRegistry::execute`. Needs Node/npx + network.
//!
//!   D=$(mktemp -d); mkdir -p $D/.sirbone
//!   echo '{"mcpServers":{"context7":{"command":"npx","args":["-y","@upstash/context7-mcp@latest"]}}}' > $D/.sirbone/config.json
//!   HOME=$D cargo run --example mcp_integration

use sirbone::ToolRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut reg = ToolRegistry::new();
    let _handles = sirbone::mcp::load_servers(&mut reg).await;

    let names: Vec<&str> = reg.iter().map(|t| t.name()).collect();
    println!("registered tools: {names:?}");

    let name = "mcp__context7__resolve-library-id";
    assert!(names.contains(&name), "expected `{name}` to be registered");

    let out = reg
        .execute(
            name,
            serde_json::json!({ "libraryName": "react", "query": "hooks" }),
        )
        .await?;
    assert!(!out.is_empty(), "expected non-empty tool result");
    println!(
        "call result ({} bytes): {}",
        out.len(),
        out.chars().take(120).collect::<String>()
    );
    println!("OK — sirbone MCP integration path verified");
    Ok(())
}
