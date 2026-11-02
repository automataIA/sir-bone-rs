//! Task set for the agent-level `grep:symbols` A/B (`bench/code_map_lines_agent.sh`).
//!
//! Same shape as the lost 2026-09-04 set: two real repos, 4 symbols per tier per
//! repo, gold from the production index (`declarations` + `find_references`), no
//! LLM judge. Only symbols with exactly one declaring file are eligible, so
//! `DEFINER=` has one right answer. Tiers by referencing-file count; the sample
//! inside a tier is evenly spaced over the sorted names, so it is deterministic
//! and not biased toward one end of the alphabet.
//!
//! `hard` (second argument) builds the localization set instead: one declaring
//! file, but at least `HARD_MIN_LINES` whole-word matching lines, so the
//! declaration is buried in (or cut from) a plain grep's 100-line answer. That
//! is the regime the `defined:` line exists for; the first set never reached it.
//! Only distinctive names (a `_` or a leading capital): a bare `iter`/`from`
//! "defined" in one file is an index artifact, not a question with an answer.
//!
//!   cargo run --release --example grep_symbols_ab_tasks -- <out.json> [hard]

use std::path::{Path, PathBuf};

use sirbone::structure::{self, Index};

const PER_TIER: usize = 4;
const HARD_MIN_LINES: usize = 100;
const HARD_PER_REPO: usize = 12;

fn tier_of(n_files: usize) -> Option<&'static str> {
    match n_files {
        2..=3 => Some("rare"),
        4..=12 => Some("mid"),
        13..=40 => Some("common"),
        _ => None,
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: grep_symbols_ab_tasks <out.json> [hard]"))?;
    let hard = std::env::args().nth(2).as_deref() == Some("hard");
    let repos: [(&str, PathBuf); 2] = [
        ("sirbone", PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        ("graphrag", PathBuf::from("/home/dio/graphrag-rs")),
    ];
    let mut tasks = Vec::new();
    for (repo, root) in &repos {
        let index = structure::update(root, Index::load(root));
        let mut names: Vec<String> = index
            .files
            .values()
            .flat_map(|c| c.data.defs.iter().cloned())
            .filter(|s| s.len() >= 4)
            .collect();
        names.sort_unstable();
        names.dedup();
        if hard {
            tasks.extend(hard_tasks(repo, root, &index, &names));
            continue;
        }
        for tier in ["rare", "mid", "common"] {
            let eligible: Vec<(String, PathBuf, Vec<PathBuf>)> = names
                .iter()
                .filter_map(|s| {
                    let defs = structure::declarations(&index, s);
                    let [definer] = defs.as_slice() else {
                        return None;
                    };
                    let refs = structure::find_references(root, s);
                    (tier_of(refs.len()) == Some(tier) && refs.contains(definer))
                        .then(|| (s.clone(), definer.clone(), refs))
                })
                .collect();
            let step = (eligible.len() / PER_TIER).max(1);
            for (i, (sym, definer, refs)) in
                eligible.iter().step_by(step).take(PER_TIER).enumerate()
            {
                tasks.push(serde_json::json!({
                    "id": format!("{repo}-{tier}-{i}"),
                    "symbol": sym,
                    "repo_path": root.display().to_string(),
                    "tier": tier,
                    "gold_definer": rel(root, definer),
                    "gold_refs": refs.iter().map(|p| rel(root, p)).collect::<Vec<_>>(),
                }));
            }
            eprintln!("{repo} {tier}: {} eligible", eligible.len());
        }
    }
    std::fs::write(&out, serde_json::to_string_pretty(&tasks)?)?;
    eprintln!("{} tasks → {out}", tasks.len());
    Ok(())
}

/// Single-definer symbols with at least `HARD_MIN_LINES` matching lines, evenly
/// spaced over the sorted eligible names.
fn hard_tasks(repo: &str, root: &Path, index: &Index, names: &[String]) -> Vec<serde_json::Value> {
    let eligible: Vec<(String, PathBuf, Vec<PathBuf>, usize)> = names
        .iter()
        .filter(|s| s.contains('_') || s.starts_with(|c: char| c.is_ascii_uppercase()))
        .filter_map(|s| {
            let defs = structure::declarations(index, s);
            let [definer] = defs.as_slice() else {
                return None;
            };
            let hits = structure::find_reference_lines(root, s, 1, false);
            let n_lines: usize = hits.iter().map(|h| h.lines.len() + h.extra).sum();
            (n_lines >= HARD_MIN_LINES).then(|| {
                let refs = hits.into_iter().map(|h| h.path).collect();
                (s.clone(), definer.clone(), refs, n_lines)
            })
        })
        .collect();
    eprintln!("{repo} hard: {} eligible", eligible.len());
    let step = (eligible.len() / HARD_PER_REPO).max(1);
    eligible
        .iter()
        .step_by(step)
        .take(HARD_PER_REPO)
        .enumerate()
        .map(|(i, (sym, definer, refs, n_lines))| {
            serde_json::json!({
                "id": format!("{repo}-hard-{i}"),
                "symbol": sym,
                "repo_path": root.display().to_string(),
                "tier": "hard",
                "match_lines": n_lines,
                "gold_definer": rel(root, definer),
                "gold_refs": refs.iter().map(|p| rel(root, p)).collect::<Vec<_>>(),
            })
        })
        .collect()
}
