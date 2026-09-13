//! code_map Fase A.2 bench — deterministic, no LLM.
//! Compares the REAL code_map logic (sirbone::structure) vs `rg` on "locate the
//! files relevant to symbol X". Decides whether code_map earns its place vs grep.
//!
//! Two repos: sir-bone-rs (own) + graphrag-rs (read-only external, polyglot —
//! discover skips its .venv). For each repo: build the production Index, enumerate
//! declared symbols, sample a common/rare/mid spread, then per symbol measure:
//!
//!   task=find_ref  : does the returned file set contain the definer (gold =
//!                    declarations)?  arms: code_map find_references vs rg -w -l.
//!   task=definer_id: does the tool IDENTIFY the definer file (not just mention it)?
//!                    code_map callers().defined_in (structured) vs rg (flat list →
//!                    cannot separate definer from callers → miss). This quantifies
//!                    code_map's unique structured capability.
//!
//! Metrics per record: hit, n_returned, bytes (output size the model reads).
//! MRR omitted (file-set tools, not rankers). Emits JSONL for paired stats.
//!
//! Usage:
//!   cargo run --example code_map_bench -- examples/code_map_results.jsonl

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde::Serialize;

use sirbone::structure::{self, callers, declarations, discover, find_references, Index};

#[derive(Serialize)]
struct Rec {
    repo: &'static str,
    symbol: String,
    tier: &'static str,
    task: &'static str,
    arm: &'static str,
    hit: bool,
    n_returned: usize,
    bytes: usize,
    gold_n: usize,
}

/// Read every discovered source file once; return (paths, contents).
fn read_corpus(root: &Path) -> Vec<(PathBuf, String)> {
    discover(root)
        .into_iter()
        .filter_map(|(p, _)| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .collect()
}

/// Whole-word mention count per declared symbol, via one alternation regex per
/// file (faithful to find_references semantics, one disk walk).
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
    let re = match Regex::new(&format!(r"\b(?:{alt})\b")) {
        Ok(r) => r,
        Err(_) => return counts,
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

/// Resolve a real ripgrep *executable*.
///
/// `Command::new("rg")` is not safe here: on a shell where `rg` is a function or
/// alias (Claude Code installs one), the spawn fails, the old code swallowed the
/// error as an empty file set, and the rg arm silently scored zero — which reads
/// as a free win for code_map on every metric. So resolve an executable up front
/// and refuse to run the bench without one.
///
/// Override with `RG_BIN=/path/to/rg`.
fn resolve_rg() -> PathBuf {
    if let Some(explicit) = std::env::var_os("RG_BIN") {
        let p = PathBuf::from(explicit);
        assert!(is_exec(&p), "RG_BIN={} is not executable", p.display());
        return p;
    }
    let candidates = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/usr/local/bin")])
        .map(|dir| dir.join("rg"));
    for c in candidates {
        if is_exec(&c) {
            return c;
        }
    }
    panic!(
        "no ripgrep executable found — the rg arm is this bench's control and must not be \
         faked. Install it (`cargo install ripgrep` or `apt install ripgrep`) or point \
         RG_BIN at the binary. NOTE: a shell function named `rg` does not count."
    );
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `rg -w -l --type rust SYMBOL` from root → file set.
///
/// Exit 1 means "no match" (a legitimate empty result); anything else is a tool
/// failure and aborts the bench rather than degrading the arm.
fn rg_files(rg: &Path, root: &Path, symbol: &str) -> Vec<PathBuf> {
    let out = Command::new(rg)
        .args(["-w", "-l", "--type", "rust", "--", symbol, "."])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("spawning {} failed: {e}", rg.display()));
    let code = out.status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "{} exited with {code:?} on symbol `{symbol}`: {}",
        rg.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l))
        .collect()
}

fn paths_bytes(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .map(|p| p.to_string_lossy().len() + 1)
        .sum::<usize>()
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
        .unwrap_or_else(|| "examples/code_map_results.jsonl".into());
    let mut out = std::fs::File::create(&out_path)?;
    let rg = resolve_rg();
    eprintln!("rg arm binary: {}", rg.display());
    let n_per_tier = 8usize; // 8 rare + 8 common + 8 mid = 24 symbols/repo

    println!(
        "code_map bench — find_ref + definer_id  ({} repo × ≤{} symbols)\n",
        repos.len(),
        n_per_tier * 3
    );

    for (repo_name, root) in repos {
        if !root.exists() {
            eprintln!("SKIP {repo_name}: {} not found", root.display());
            continue;
        }
        // Build the production index WITHOUT saving (graphrag-rs is read-only).
        let index = structure::update(root, Index::load(root));
        let symbols = {
            let mut s: HashSet<String> = HashSet::new();
            for c in index.files.values() {
                for d in &c.data.defs {
                    s.insert(d.clone());
                }
            }
            let mut v: Vec<String> = s.into_iter().collect();
            v.sort();
            v
        };
        let corpus = read_corpus(root);
        let counts = mention_counts(&corpus, &symbols);

        // Bucket by tier, sort each for determinism, take n_per_tier.
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
                for s in v.iter().take(n_per_tier) {
                    sample.push((s.clone(), tier));
                }
            }
        }
        eprintln!(
            "{repo_name}: {} declared symbols, sampled {}",
            symbols.len(),
            sample.len()
        );

        // Per-symbol results collected once, used for both JSONL + summary.
        let mut fr_cm = (0usize, 0usize, 0usize); // (hits, n, tot_bytes)
        let mut fr_rg = (0, 0, 0);
        let mut di_cm = (0, 0, 0);
        let mut di_rg_n = 0usize;

        for (sym, tier) in &sample {
            let gold = declarations(&index, sym); // definer file(s) — unambiguous gold
            if gold.is_empty() {
                continue;
            }
            let gold_set: HashSet<PathBuf> = gold.iter().cloned().collect();

            // --- task=find_ref ---
            let cm_refs = find_references(root, sym);
            let cm_hit = gold_set.iter().all(|g| cm_refs.iter().any(|p| p == g));
            let cm_bytes = paths_bytes(&cm_refs);
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Rec {
                    repo: repo_name,
                    symbol: sym.clone(),
                    tier,
                    task: "find_ref",
                    arm: "code_map",
                    hit: cm_hit,
                    n_returned: cm_refs.len(),
                    bytes: cm_bytes,
                    gold_n: gold.len(),
                })?
            )?;
            fr_cm = (fr_cm.0 + cm_hit as usize, fr_cm.1 + 1, fr_cm.2 + cm_bytes);

            let rgf = rg_files(&rg, root, sym);
            let rg_hit = gold_set.iter().all(|g| rgf.iter().any(|p| p == g));
            let rg_bytes = paths_bytes(&rgf);
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Rec {
                    repo: repo_name,
                    symbol: sym.clone(),
                    tier,
                    task: "find_ref",
                    arm: "rg",
                    hit: rg_hit,
                    n_returned: rgf.len(),
                    bytes: rg_bytes,
                    gold_n: gold.len(),
                })?
            )?;
            fr_rg = (fr_rg.0 + rg_hit as usize, fr_rg.1 + 1, fr_rg.2 + rg_bytes);

            // --- task=definer_id (unique structured capability) ---
            // code_map callers() separates the definer; rg returns a flat list and
            // cannot identify which file DEFINES (vs merely mentions) the symbol.
            let cm_def: HashSet<PathBuf> = callers(&index, sym).defined_in.into_iter().collect();
            let cm_def_ok = cm_def == gold_set;
            let cm_def_v: Vec<PathBuf> = cm_def.iter().cloned().collect();
            let cm_def_bytes = paths_bytes(&cm_def_v);
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Rec {
                    repo: repo_name,
                    symbol: sym.clone(),
                    tier,
                    task: "definer_id",
                    arm: "code_map",
                    hit: cm_def_ok,
                    n_returned: cm_def.len(),
                    bytes: cm_def_bytes,
                    gold_n: gold.len(),
                })?
            )?;
            di_cm = (
                di_cm.0 + cm_def_ok as usize,
                di_cm.1 + 1,
                di_cm.2 + cm_def_bytes,
            );
            // rg arm: by construction rg cannot mark a definer → honest miss.
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Rec {
                    repo: repo_name,
                    symbol: sym.clone(),
                    tier,
                    task: "definer_id",
                    arm: "rg",
                    hit: false,
                    n_returned: rgf.len(),
                    bytes: rg_bytes,
                    gold_n: gold.len(),
                })?
            )?;
            di_rg_n += 1;
        }

        let pct = |h: usize, n: usize| {
            if n == 0 {
                0.0
            } else {
                100.0 * h as f64 / n as f64
            }
        };
        println!("── {repo_name} (n={}) ──", fr_cm.1);
        println!(
            "  find_ref   code_map hit {}/{} ({:>5.1}%)  mean_bytes {:>6.1}",
            fr_cm.0,
            fr_cm.1,
            pct(fr_cm.0, fr_cm.1),
            fr_cm.2 as f64 / fr_cm.1.max(1) as f64
        );
        println!(
            "  find_ref   rg       hit {}/{} ({:>5.1}%)  mean_bytes {:>6.1}",
            fr_rg.0,
            fr_rg.1,
            pct(fr_rg.0, fr_rg.1),
            fr_rg.2 as f64 / fr_rg.1.max(1) as f64
        );
        println!(
            "  definer_id code_map hit {}/{} ({:>5.1}%)  (unique: structured definer)",
            di_cm.0,
            di_cm.1,
            pct(di_cm.0, di_cm.1)
        );
        println!(
            "  definer_id rg       hit 0/{} ({:>5.1}%)  (flat list, cannot mark definer)",
            di_rg_n, 0.0
        );
    }
    eprintln!("wrote {out_path}");
    Ok(())
}
