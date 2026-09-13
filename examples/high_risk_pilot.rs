//! Offline paired pilot for the opt-in `hooks.presets = ["high_risk"]` policy.
//!
//! It exercises the real hook classifier with three counterbalanced repetitions.
//! `Ask` is treated as an interactive "Allow once", so both arms complete the
//! same operation while prompt recall and false positives remain measurable.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sirbone::checks::{HookPreset, Hooks, PreVerdict};

#[tokio::main]
async fn main() -> Result<()> {
    let output = std::env::args_os().nth(1).map(PathBuf::from);
    let cases = [
        (
            "dependency",
            "bash",
            json!({"command": "cargo add serde"}),
            true,
        ),
        (
            "migration",
            "bash",
            json!({"command": "alembic upgrade head"}),
            true,
        ),
        (
            "schema",
            "edit",
            json!({"path": "api/openapi.yaml", "old_string": "v1", "new_string": "v2"}),
            true,
        ),
        (
            "public_api",
            "edit",
            json!({"path": "src/lib.rs", "old_string": "fn parse() {}", "new_string": "pub fn parse() {}"}),
            true,
        ),
        ("safe_test", "bash", json!({"command": "cargo test"}), false),
        (
            "safe_edit",
            "edit",
            json!({"path": "src/internal.rs", "old_string": "let x = 1", "new_string": "let x = 2"}),
            false,
        ),
    ];
    let baseline = Hooks::default();
    let treatment = Hooks {
        presets: vec![HookPreset::HighRisk],
        ..Default::default()
    };
    let mut rows = Vec::new();
    for repetition in 0..3 {
        for (position, (name, tool, input, risky)) in cases.iter().enumerate() {
            let arms = if (repetition + position) % 2 == 0 {
                [("baseline", &baseline), ("high_risk", &treatment)]
            } else {
                [("high_risk", &treatment), ("baseline", &baseline)]
            };
            for (pair_position, (arm, hooks)) in arms.into_iter().enumerate() {
                let verdict = hooks.pre_tool_use(tool, input).await;
                let prompts = usize::from(matches!(verdict, PreVerdict::Ask(_)));
                // The paired pilot approves an Ask once. Pass also executes;
                // Deny would fail the operation and therefore the quality gate.
                let passed = !matches!(verdict, PreVerdict::Deny(_));
                rows.push(json!({
                    "case": name,
                    "risky": risky,
                    "repetition": repetition,
                    "arm": arm,
                    "pair_position": pair_position,
                    "prompts": prompts,
                    "cumulative_pass": passed,
                    "model_calls": 0,
                    "model_tokens": 0
                }));
            }
        }
    }
    let treatment_rows: Vec<&Value> = rows
        .iter()
        .filter(|row| row["arm"] == "high_risk")
        .collect();
    let risky_rows: Vec<&&Value> = treatment_rows
        .iter()
        .filter(|row| row["risky"] == true)
        .collect();
    let safe_rows: Vec<&&Value> = treatment_rows
        .iter()
        .filter(|row| row["risky"] == false)
        .collect();
    let risky_caught = risky_rows.iter().filter(|row| row["prompts"] == 1).count();
    let safe_prompts: u64 = safe_rows
        .iter()
        .map(|row| row["prompts"].as_u64().unwrap_or(0))
        .sum();
    let max_prompts = treatment_rows
        .iter()
        .filter_map(|row| row["prompts"].as_u64())
        .max()
        .unwrap_or(0);
    let cumulative_pass = rows.iter().all(|row| row["cumulative_pass"] == true);
    let report = json!({
        "schema_version": "high-risk-pilot-v1",
        "repetitions": 3,
        "pairs": rows,
        "gate": {
            "risky_caught": risky_caught,
            "risky_total": risky_rows.len(),
            "recall": risky_caught as f64 / risky_rows.len() as f64,
            "safe_prompts": safe_prompts,
            "max_prompts_per_operation": max_prompts,
            "cumulative_pass": cumulative_pass,
            "model_call_delta_pct": 0.0,
            "model_token_delta_pct": 0.0,
            "passed": risky_caught == risky_rows.len()
                && safe_prompts == 0
                && max_prompts <= 1
                && cumulative_pass
        }
    });
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report["gate"]["passed"] != true {
        bail!("high-risk paired pilot gate failed");
    }
    Ok(())
}
