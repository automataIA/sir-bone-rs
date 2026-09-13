//! Task generator for the `code_map` `path:line:content` agent A/B.
//!
//! Emits reference-completeness tasks with deterministic gold, taken from the
//! production index so the gold is the same thing `code_map` and `grep` are
//! judged against — no human labels, no LLM judge.
//!
//! One task = one declared symbol in one repo:
//!   gold_definers : file(s) declaring it (`structure::declarations`)
//!   gold_refs     : file(s) containing a whole-word match (`find_references`)
//!
//! Symbols with an ambiguous definer (declared in more than one file) are
//! skipped: the task asks the agent for *the* defining file, so a multi-definer
//! symbol has no single right answer and would score both arms as wrong.
//!
//! Usage:
//!   cargo run --release --example code_map_agent_tasks -- <out.json> [n_per_tier]
//!                                                        [min_refs] [max_refs]
//!
//! `min_refs`/`max_refs` bound the size of the reference set (default 2..=12).
//! The default range was chosen for the `code_map:lines` A/B, where anything
//! wider was "a transcription exercise" — but it also excludes every symbol for
//! which the 40-line budget fires, so the budget A/B needs its own wide set
//! (`… 14 45`), where the question *is* whether the tool hides files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;

use sirbone::structure::{self, declarations, discover, find_references, Index};

#[derive(Serialize)]
struct Task {
    id: String,
    repo: String,
    repo_path: String,
    symbol: String,
    tier: &'static str,
    gold_definer: String,
    gold_refs: Vec<String>,
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn tier_of(n: usize) -> &'static str {
    if n <= 3 {
        "rare"
    } else if n >= 9 {
        "common"
    } else {
        "mid"
    }
}

/// Whole-word mention count per declared symbol, one alternation regex per file.
fn mention_counts(root: &Path, symbols: &[String]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = symbols.iter().map(|s| (s.clone(), 0)).collect();
    if symbols.is_empty() {
        return counts;
    }
    let alt = symbols
        .iter()
        .map(|s| regex::escape(s))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(re) = Regex::new(&format!(r"\b(?:{alt})\b")) else {
        return counts;
    };
    for (path, _) in discover(root) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for s in re
            .find_iter(&content)
            .map(|m| m.as_str())
            .collect::<HashSet<_>>()
        {
            if let Some(c) = counts.get_mut(s) {
                *c += 1;
            }
        }
    }
    counts
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out_path = args.next().unwrap_or_else(|| "/tmp/cmab/tasks.json".into());
    let n_per_tier: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);
    let min_refs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let max_refs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);

    let repos: &[(&str, &str)] = &[
        ("sirbone", "/tmp/cmab/repo-sirbone"),
        ("graphrag", "/tmp/cmab/repo-graphrag"),
    ];

    let mut tasks: Vec<Task> = Vec::new();
    for (name, path) in repos {
        let root = PathBuf::from(path);
        anyhow::ensure!(root.exists(), "{path} not found — copy the repo first");
        let index = structure::update(&root, Index::load(&root));
        index.save(&root)?;

        let symbols: Vec<String> = {
            let mut s: HashSet<String> = HashSet::new();
            for c in index.files.values() {
                s.extend(c.data.defs.iter().cloned());
            }
            let mut v: Vec<String> = s.into_iter().collect();
            v.sort();
            v
        };
        let counts = mention_counts(&root, &symbols);

        // Bucket by tier; keep only unambiguous definers, and skip symbols whose
        // reference set is huge — the task would be a transcription exercise.
        let mut buckets: HashMap<&str, Vec<String>> = HashMap::new();
        for sym in &symbols {
            let defs = declarations(&index, sym);
            if defs.len() != 1 {
                continue;
            }
            buckets
                .entry(tier_of(*counts.get(sym).unwrap_or(&0)))
                .or_default()
                .push(sym.clone());
        }

        for tier in ["rare", "mid", "common"] {
            let Some(candidates) = buckets.get(tier) else {
                continue;
            };
            let mut taken = 0usize;
            // Constant-stride walk, not the first N: taking the head of a sorted
            // symbol list would sample one corner of the alphabet, and in this
            // codebase that means one or two modules.
            let stride = (candidates.len() / (n_per_tier * 8)).max(1);
            for sym in candidates.iter().step_by(stride) {
                if taken == n_per_tier {
                    break;
                }
                let refs = find_references(&root, sym);
                if refs.len() < min_refs || refs.len() > max_refs {
                    continue;
                }
                let definer = declarations(&index, sym)
                    .first()
                    .map(|p| rel(&root, p))
                    .expect("single definer checked above");
                tasks.push(Task {
                    id: format!("{name}-{tier}-{sym}"),
                    repo: (*name).to_string(),
                    repo_path: path.to_string(),
                    symbol: sym.clone(),
                    tier,
                    gold_definer: definer,
                    gold_refs: refs.iter().map(|p| rel(&root, p)).collect(),
                });
                taken += 1;
            }
            eprintln!("{name}/{tier}: {taken} task(s)");
        }
    }

    std::fs::write(&out_path, serde_json::to_string_pretty(&tasks)?)?;
    eprintln!("wrote {} task(s) to {out_path}", tasks.len());
    Ok(())
}
