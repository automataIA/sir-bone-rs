//! Raw and routed `grep` output for the offline next-action-preservation probe
//! (bench/nap/). Reads `{"repo": dir, "args": {grep input}}` lines on stdin and
//! writes `{"raw": ..., "routed": ..., "full": ...}` per line: the same call
//! with `raw=true`, with the routing `grep` applies by default, and raw with no
//! result cap (a capped run keeps whichever matches rg's parallel walk met first).
//!
//!   cargo run --release --example nap_views < steps.jsonl > views.jsonl

use std::io::BufRead;

use sirbone::tools::{grep::GrepTool, DynTool};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    for line in std::io::stdin().lock().lines() {
        let step: serde_json::Value = serde_json::from_str(&line?)?;
        let repo = step["repo"].as_str().unwrap_or(".");
        std::env::set_current_dir(repo)?;
        let mut args = step["args"].clone();
        args["raw"] = true.into();
        let raw = GrepTool.call(args.clone()).await.unwrap_or_else(|e| e.to_string());
        args["raw"] = false.into();
        let routed = GrepTool.call(args.clone()).await.unwrap_or_else(|e| e.to_string());
        args["raw"] = true.into();
        args["max_results"] = 1_000_000.into();
        let full = GrepTool.call(args).await.unwrap_or_else(|e| e.to_string());
        println!("{}", serde_json::json!({"raw": raw, "routed": routed, "full": full}));
    }
    Ok(())
}
