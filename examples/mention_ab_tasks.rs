//! Task set for the `SIRBONE_MENTIONS` A/B (tags vs filter vs neither).
//!
//! "Which files use `X` in code" for symbols where some Rust files mention `X`
//! only in comments, doc comments or string literals: the trap the two arms
//! exist to disarm. Gold comes from `proc_macro2` lexing (an `Ident` token
//! equal to `X`), not from `structure::mentions`, so the lexer under test does
//! not grade itself. Same JSON shape as `grep_symbols_ab_tasks`, so
//! `bench/code_map_lines_agent.sh` and its scorer run it unchanged; `traps`
//! lists the comment/string-only Rust files for the precision breakdown.
//!
//!   cargo run --release --example mention_ab_tasks -- <out.json>

use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{TokenStream, TokenTree};
use sirbone::structure::{self, Index};

const PER_REPO: usize = 8;
/// Matching lines past `grep:route`'s threshold, so every arm's grep is routed.
const MIN_LINES: usize = 41;

fn has_ident(ts: TokenStream, name: &str) -> bool {
    ts.into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == name,
        TokenTree::Group(g) => has_ident(g.stream(), name),
        _ => false,
    })
}

/// `Some(true)` if `path` uses `name` as an identifier; `None` if it does not lex.
fn uses_in_code(path: &Path, name: &str) -> Option<bool> {
    let src = std::fs::read_to_string(path).ok()?;
    Some(has_ident(TokenStream::from_str(&src).ok()?, name))
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: mention_ab_tasks <out.json>"))?;
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
            .filter(|s| {
                s.len() >= 4 && (s.contains('_') || s.starts_with(|c: char| c.is_ascii_uppercase()))
            })
            .collect();
        names.sort_unstable();
        names.dedup();
        let eligible: Vec<_> = names
            .iter()
            .filter_map(|s| {
                let defs = structure::declarations(&index, s);
                let [definer] = defs.as_slice() else {
                    return None;
                };
                let hits = structure::find_reference_lines(root, s, 1, false);
                let n_lines: usize = hits.iter().map(|h| h.lines.len() + h.extra).sum();
                if n_lines < MIN_LINES {
                    return None;
                }
                let mut code = Vec::new();
                let mut traps = Vec::new();
                for h in hits
                    .iter()
                    .filter(|h| h.path.extension().is_some_and(|e| e == "rs"))
                {
                    match uses_in_code(&h.path, s)? {
                        true => code.push(h.path.clone()),
                        false => traps.push(h.path.clone()),
                    }
                }
                (!traps.is_empty() && (2..=25).contains(&code.len()) && code.contains(definer))
                    .then(|| (s.clone(), definer.clone(), code, traps, n_lines))
            })
            .collect();
        eprintln!("{repo}: {} eligible", eligible.len());
        let step = (eligible.len() / PER_REPO).max(1);
        for (i, (sym, definer, code, traps, n_lines)) in
            eligible.iter().step_by(step).take(PER_REPO).enumerate()
        {
            // How far the lexer under test agrees with the gold (needs
            // SIRBONE_MENTIONS=filter, else code_only keeps every file).
            let lexed: Vec<PathBuf> = structure::find_reference_lines(root, sym, 1, true)
                .into_iter()
                .map(|h| h.path)
                .collect();
            let missed = code.iter().filter(|p| !lexed.contains(p)).count();
            let extra = lexed.iter().filter(|p| !code.contains(p)).count();
            eprintln!("{repo}-code-{i} {sym}: lexer missed {missed}, extra {extra}");
            tasks.push(serde_json::json!({
                "id": format!("{repo}-code-{i}"),
                "symbol": sym,
                "repo_path": root.display().to_string(),
                "tier": "code",
                "match_lines": n_lines,
                "gold_definer": rel(root, definer),
                "gold_refs": code.iter().map(|p| rel(root, p)).collect::<Vec<_>>(),
                "traps": traps.iter().map(|p| rel(root, p)).collect::<Vec<_>>(),
            }));
        }
    }
    std::fs::write(&out, serde_json::to_string_pretty(&tasks)?)?;
    eprintln!("{} tasks → {out}", tasks.len());
    Ok(())
}
