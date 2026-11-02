//! Offline production-tool probe: bytes, deterministic output and retained evidence.
//! Run: cargo run --locked --example code_map_navigation_probe -- /tmp/navigation.jsonl
//! This measures output, not agent task success, tokens or API cost.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sirbone::tools::code_map::{CodeMapInput, CodeMapTool};
use sirbone::tools::TypedTool;

fn declaration(root: &Path, path: &str, prefix: &str) -> Result<String> {
    let source = std::fs::read_to_string(root.join(path))?;
    let line = source
        .lines()
        .position(|line| line.starts_with(prefix))
        .with_context(|| format!("gold declaration missing: {path}: {prefix}"))?;
    Ok(format!("{path}:{}", line + 1))
}

fn record(case: &str, arm: &str, budget: Option<usize>, text: &str, gold: &[String]) -> Value {
    json!({
        "case": case,
        "arm": arm,
        "max_bytes": budget,
        "bytes": text.len(),
        "gold": gold,
        "evidence_hits": gold.iter().filter(|needle| text.contains(needle.as_str())).count(),
        "evidence_total": gold.len(),
        "omissions_reported": text.contains("[budget:"),
        "output": text,
    })
}

async fn measure(
    tool: &CodeMapTool,
    out: &mut std::fs::File,
    case: &str,
    mut input: Value,
    gold: &[String],
) -> Result<()> {
    for budget in [512, 2_000, 6_000, 12_000] {
        input["max_bytes"] = json!(budget);
        let first = tool
            .run(serde_json::from_value::<CodeMapInput>(input.clone())?)
            .await?;
        let second = tool
            .run(serde_json::from_value::<CodeMapInput>(input.clone())?)
            .await?;
        ensure!(first == second, "{case}: nondeterministic output");
        ensure!(first.len() <= budget, "{case}: exceeded {budget} bytes");
        let row = record(
            case,
            input["op"].as_str().unwrap_or(""),
            Some(budget),
            &first,
            gold,
        );
        writeln!(out, "{row}")?;
        println!(
            "{case:24} budget={budget:5} bytes={:5} evidence={}/{} omitted={}",
            first.len(),
            row["evidence_hits"],
            gold.len(),
            row["omissions_reported"]
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::env::args()
        .nth(1)
        .context("provide an output JSONL path")?;
    let mut out = std::fs::File::create(output)?;
    let tool = CodeMapTool { root: root.into() };
    for (symbol, path, prefix) in [
        ("usage_chain", "src/structure.rs", "pub fn usage_chain("),
        (
            "reverse_paths",
            "src/structure/impact.rs",
            "pub fn reverse_paths(",
        ),
        (
            "CodeMapTool",
            "src/tools/code_map.rs",
            "pub struct CodeMapTool {",
        ),
    ] {
        let gold = [declaration(root, path, prefix)?];
        measure(
            &tool,
            &mut out,
            symbol,
            json!({"op":"context", "symbol":symbol}),
            &gold,
        )
        .await?;
        let text = tool
            .run(serde_json::from_value(
                json!({"op":"find_references", "symbol":symbol}),
            )?)
            .await?;
        writeln!(
            out,
            "{}",
            record(symbol, "find_references", None, &text, &gold)
        )?;
        // Restrict the control to production Rust sources, matching these gold tasks.
        // Exit 1 is a legitimate empty result; other failures must not score as misses.
        let result = Command::new("rg")
            .args(["-n", "-w", "--type", "rust", "--", symbol, "src"])
            .current_dir(root)
            .output()
            .context("run the ripgrep control")?;
        ensure!(
            matches!(result.status.code(), Some(0 | 1)),
            "rg failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout)?;
        writeln!(out, "{}", record(symbol, "rg_src", None, &text, &gold))?;
    }
    measure(
        &tool,
        &mut out,
        "inspect_usage_chain",
        json!({"op":"inspect", "path":"src/structure.rs", "symbol":"usage_chain"}),
        &["::usage_chain".into(), " sort".into()],
    )
    .await?;
    measure(
        &tool,
        &mut out,
        "changed_inline_tests",
        json!({"op":"affected_tests", "files":["src/structure.rs"], "max_depth":0}),
        &["[test hop=0] src/structure.rs:".into()],
    )
    .await?;
    measure(
        &tool,
        &mut out,
        "transitive_test_candidate",
        json!({"op":"affected_tests", "files":["src/structure/impact.rs"], "max_depth":3}),
        &["src/tools/code_map/navigation.rs:".into()],
    )
    .await?;
    Ok(())
}
