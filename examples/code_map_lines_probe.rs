//! Probe: does `path:line:content` output for `code_map find_references` pay for
//! itself? Deterministic, no LLM, no quota.
//!
//! Motivated by agentconnect.md/blog/grep-beat-lsp-harness: returning bare
//! locations instead of `path:line:content` measurably suppresses adoption and
//! forces follow-up file reads. `code_map find_references` today returns bare
//! paths, so this measures the two halves that CAN be measured offline:
//!
//!   cost            : output bytes the model must read.
//!   self_sufficient : does the output already contain the DECLARATION line of the
//!                     symbol? If yes, the "which file defines this" question is
//!                     answered without a follow-up `read`.
//!
//! Arms, paired on the same symbols/repos as the code_map Fase A bench:
//!   paths       — current behaviour: one relative path per referencing file.
//!   lines3      — naive enrichment: first ≤3 matching lines per file.
//!   declfirst   — enrichment that shows the declaration line first, then ≤2 more.
//!   rg_n        — `rg -n -w` (what grep already gives the model, for reference).
//!
//! Usage:
//!   cargo run --example code_map_lines_probe -- examples/code_map_lines_probe.jsonl

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;

use sirbone::structure::{self, declarations, discover, find_references, Index};

/// Per-file line cap for the enriched arms — the whole point is to stay cheap,
/// so an unbounded dump is not a candidate worth measuring.
const MAX_LINES_PER_FILE: usize = 3;
/// Whole-result line cap, mirroring what a shipped tool would need.
const MAX_LINES_TOTAL: usize = 40;
/// Rendered content lines are trimmed and clipped to this many chars.
const MAX_LINE_CHARS: usize = 160;
/// What the `read` tool would cap a single file at (`truncate::DEFAULT_MAX_BYTES`).
const READ_CAP_BYTES: usize = sirbone::tools::truncate::DEFAULT_MAX_BYTES;

#[derive(Serialize)]
struct Rec {
    repo: &'static str,
    symbol: String,
    tier: &'static str,
    arm: &'static str,
    bytes: usize,
    n_lines: usize,
    n_files: usize,
    self_sufficient: bool,
    truncated: bool,
}

fn read_corpus(root: &Path) -> Vec<(PathBuf, String)> {
    discover(root)
        .into_iter()
        .filter_map(|(p, _)| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .collect()
}

/// Whole-word mention count per declared symbol (one alternation regex per file).
fn mention_counts(corpus: &[(PathBuf, String)], symbols: &[String]) -> HashMap<String, usize> {
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
    for (_path, content) in corpus {
        let mut seen: HashSet<&str> = HashSet::new();
        for m in re.find_iter(content) {
            seen.insert(m.as_str());
        }
        for s in seen {
            if let Some(c) = counts.get_mut(s) {
                *c += 1;
            }
        }
    }
    counts
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

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Approximate declaration-line detector for the languages the index covers
/// (Rust / Python / TS-JS / Go). A probe heuristic, not the parser: it is used
/// only to score whether an arm's output already answers "where is this defined".
fn decl_re(symbol: &str) -> Regex {
    let s = regex::escape(symbol);
    let kw = "fn|func|struct|enum|trait|type|class|def|interface|const|static|let|var|mod|impl";
    let pat =
        format!(r"^\s*(?:export\s+|pub(?:\([^)]*\))?\s+|default\s+)*(?:async\s+)?(?:{kw})\s+{s}\b");
    Regex::new(&pat).expect("declaration pattern is a valid regex")
}

struct Rendered {
    body: String,
    n_lines: usize,
    self_sufficient: bool,
    truncated: bool,
}

/// Render the enriched output. `decl_first` prioritises the declaration line of
/// the symbol within each file instead of taking the first matches in order.
fn render_lines(
    root: &Path,
    files: &[PathBuf],
    symbol: &str,
    gold: &HashSet<PathBuf>,
    decl_first: bool,
) -> Rendered {
    let word = Regex::new(&format!(r"\b{}\b", regex::escape(symbol)))
        .expect("escaped symbol is a valid regex");
    let decl = decl_re(symbol);
    let mut body = String::new();
    let mut n_lines = 0usize;
    let mut self_sufficient = false;
    let mut truncated = false;

    for path in files {
        let Some(content) = std::fs::read_to_string(path).ok() else {
            continue;
        };
        let mut matches: Vec<(usize, &str)> = content
            .lines()
            .enumerate()
            .filter(|(_, l)| word.is_match(l))
            .map(|(i, l)| (i + 1, l))
            .collect();
        if decl_first {
            // Stable partition: declaration lines first, original order kept.
            matches.sort_by_key(|(_, l)| !decl.is_match(l));
        }
        let shown = matches.len().min(MAX_LINES_PER_FILE);
        for (lineno, text) in matches.iter().take(shown) {
            if n_lines >= MAX_LINES_TOTAL {
                truncated = true;
                break;
            }
            let clipped: String = text.trim().chars().take(MAX_LINE_CHARS).collect();
            body.push_str(&format!("{}:{lineno}:{clipped}\n", rel(root, path)));
            n_lines += 1;
            if gold.contains(path) && decl.is_match(text) {
                self_sufficient = true;
            }
        }
        if matches.len() > shown {
            body.push_str(&format!(
                "  … {} more match(es) in this file\n",
                matches.len() - shown
            ));
        }
        if truncated {
            break;
        }
    }
    Rendered {
        body,
        n_lines,
        self_sufficient,
        truncated,
    }
}

/// `rg -n -w SYMBOL` output as the model would see it. Computed in-process over
/// the same corpus rather than shelling out: `rg` is not always a real binary on
/// PATH (it can be a shell function), and a silently empty arm would look like a
/// free win for every other arm.
fn rg_lines(root: &Path, corpus: &[(PathBuf, String)], symbol: &str) -> String {
    let word = Regex::new(&format!(r"\b{}\b", regex::escape(symbol)))
        .expect("escaped symbol is a valid regex");
    corpus
        .iter()
        .flat_map(|(path, content)| {
            content
                .lines()
                .enumerate()
                .filter(|(_, l)| word.is_match(l))
                .map(move |(i, l)| format!("{}:{}:{}\n", rel(root, path), i + 1, l))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn emit(
    out: &mut std::fs::File,
    repo: &'static str,
    symbol: &str,
    tier: &'static str,
    arm: &'static str,
    bytes: usize,
    n_lines: usize,
    n_files: usize,
    self_sufficient: bool,
    truncated: bool,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "{}",
        serde_json::to_string(&Rec {
            repo,
            symbol: symbol.to_string(),
            tier,
            arm,
            bytes,
            n_lines,
            n_files,
            self_sufficient,
            truncated,
        })?
    )?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let graphrag = PathBuf::from("/home/dio/graphrag-rs");
    let repos: &[(&str, &Path)] = &[
        ("sir-bone-rs", here.as_path()),
        ("graphrag-rs", graphrag.as_path()),
    ];
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/code_map_lines_probe.jsonl".into());
    let mut out = std::fs::File::create(&out_path)?;
    let n_per_tier = 8usize;

    for (repo_name, root) in repos {
        if !root.exists() {
            eprintln!("SKIP {repo_name}: {} not found", root.display());
            continue;
        }
        let index = structure::update(root, Index::load(root));
        let symbols = {
            let mut s: HashSet<String> = HashSet::new();
            for c in index.files.values() {
                s.extend(c.data.defs.iter().cloned());
            }
            let mut v: Vec<String> = s.into_iter().collect();
            v.sort();
            v
        };
        let corpus = read_corpus(root);
        let counts = mention_counts(&corpus, &symbols);

        let mut buckets: HashMap<&str, Vec<String>> = HashMap::new();
        for sym in &symbols {
            buckets
                .entry(tier_of(*counts.get(sym).unwrap_or(&0)))
                .or_default()
                .push(sym.clone());
        }
        let mut sample: Vec<(String, &'static str)> = Vec::new();
        for tier in ["rare", "common", "mid"] {
            if let Some(v) = buckets.get(tier) {
                sample.extend(v.iter().take(n_per_tier).map(|s| (s.clone(), tier)));
            }
        }
        eprintln!(
            "{repo_name}: {} declared symbols, sampled {}",
            symbols.len(),
            sample.len()
        );

        for (sym, tier) in &sample {
            let gold: HashSet<PathBuf> = declarations(&index, sym).into_iter().collect();
            if gold.is_empty() {
                continue;
            }
            let files = find_references(root, sym);

            // arm: paths (current behaviour)
            let paths_body: String = files
                .iter()
                .map(|p| format!("{}\n", rel(root, p)))
                .collect();
            emit(
                &mut out,
                repo_name,
                sym,
                tier,
                "paths",
                paths_body.len(),
                files.len(),
                files.len(),
                false, // bare paths can never contain the declaration line
                false,
            )?;

            for (arm, decl_first) in [("lines3", false), ("declfirst", true)] {
                let r = render_lines(root, &files, sym, &gold, decl_first);
                emit(
                    &mut out,
                    repo_name,
                    sym,
                    tier,
                    arm,
                    r.body.len(),
                    r.n_lines,
                    files.len(),
                    r.self_sufficient,
                    r.truncated,
                )?;
            }

            // arm: paths_plus_read — what the `paths` arm actually costs end to end.
            // Bare paths answer nothing on their own, so the model must open the
            // definer file; charge that read (capped the way the read tool caps it).
            let read_bytes: usize = gold
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len() as usize))
                .map(|n| n.min(READ_CAP_BYTES))
                .sum();
            emit(
                &mut out,
                repo_name,
                sym,
                tier,
                "paths_plus_read",
                paths_body.len() + read_bytes,
                files.len(),
                files.len(),
                true, // the read does answer it — at full file cost
                false,
            )?;

            // arm: rg -n (reference point — what grep already returns)
            let rgo = rg_lines(root, &corpus, sym);
            let decl = decl_re(sym);
            let rg_self = rgo.lines().any(|l| {
                // rg format is path:line:content — check the content half only.
                l.splitn(3, ':').nth(2).is_some_and(|c| decl.is_match(c))
            });
            emit(
                &mut out,
                repo_name,
                sym,
                tier,
                "rg_n",
                rgo.len(),
                rgo.lines().count(),
                files.len(),
                rg_self,
                false,
            )?;
        }
    }
    eprintln!("wrote {out_path}");
    Ok(())
}
