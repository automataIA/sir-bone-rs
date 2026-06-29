//! RAG #3 offline bench — deterministic, no LLM. The gate that decided the design.
//! Runs the SAME retrieval engine as production (sirbone::rag), so the numbers
//! validate the real code path. Needs `--features rag`.
//!
//! Compares THREE modes over a downloaded library doc (the real corpus):
//!   - bm25   : sirbone::rag (tantivy) — production retrieval
//!   - grep   : line-level token match (what a naive grep of the doc does)
//!   - hybrid : grep if it matches anything, else bm25 (grep-first)
//!
//! Metrics: recall@K (gold range ∩ a returned span) + compression (injected/total).
//! Cases tagged kind = "exact" (signature/method existence) vs "concept" (how-to).
//!
//! Usage:
//!   cargo run --features rag --example rag_bench -- examples/rag_bench_code.json
//!   cargo run --features rag --example rag_bench                 # default prose dataset

use std::collections::HashMap;

use serde::Deserialize;
use sirbone::rag::{merge, DocIndex, DEFAULT_CONTEXT};

const TOP_K: usize = 3;

#[derive(Deserialize)]
struct Case {
    doc: String,
    query: String,
    expected_lines: [usize; 2],
    #[serde(default)]
    kind: String,
}

/// Naive grep baseline: rank lines by how many query tokens they contain
/// (case-insensitive), take top-K, expand ±CONTEXT. No IDF — the bench showed
/// this loses to BM25, which is why production uses BM25.
fn grep_spans(di: &DocIndex, query: &str) -> Vec<(usize, usize)> {
    let toks: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect();
    let mut scored: Vec<(usize, usize)> = di
        .lines()
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let low = line.to_lowercase();
            let c = toks.iter().filter(|t| low.contains(t.as_str())).count();
            (c > 0).then_some((c, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let n = di.n_lines();
    merge(
        scored
            .into_iter()
            .take(TOP_K)
            .map(|(_, i)| {
                let l = i + 1;
                (
                    l.saturating_sub(DEFAULT_CONTEXT).max(1),
                    (l + DEFAULT_CONTEXT).min(n),
                )
            })
            .collect(),
    )
}

fn score(spans: &[(usize, usize)], gold: [usize; 2], n_lines: usize) -> (bool, f64) {
    let hit = spans.iter().any(|&(lo, hi)| lo <= gold[1] && gold[0] <= hi);
    let injected: usize = spans.iter().map(|(lo, hi)| hi - lo + 1).sum();
    (hit, 100.0 * injected as f64 / n_lines.max(1) as f64)
}

#[derive(Default)]
struct Agg {
    n: usize,
    hits: usize,
    comp: f64,
}
impl Agg {
    fn add(&mut self, hit: bool, comp: f64) {
        self.n += 1;
        self.hits += hit as usize;
        self.comp += comp;
    }
    fn line(&self, label: &str) {
        if self.n > 0 {
            println!(
                "  {label:<22} recall {}/{} ({:>5.1}%)   mean comp {:>5.1}%",
                self.hits,
                self.n,
                100.0 * self.hits as f64 / self.n as f64,
                self.comp / self.n as f64,
            );
        }
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/rag_bench_data.json".into());
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(&path)?)?;

    let mut indexes: HashMap<String, DocIndex> = HashMap::new();
    for c in &cases {
        if !indexes.contains_key(&c.doc) {
            indexes.insert(
                c.doc.clone(),
                DocIndex::build(&std::fs::read_to_string(&c.doc)?)?,
            );
        }
    }

    let modes = ["bm25", "grep", "hybrid"];
    let mut agg: HashMap<&str, Agg> = modes.iter().map(|&m| (m, Agg::default())).collect();
    let mut agg_kind: HashMap<(String, &str), Agg> = HashMap::new();

    println!(
        "RAG bench — recall@{TOP_K}, ctx ±{DEFAULT_CONTEXT}  ({} cases)\n",
        cases.len()
    );
    for c in &cases {
        let di = &indexes[&c.doc];
        let n = di.n_lines();
        let bm = di.search(&c.query, TOP_K, DEFAULT_CONTEXT)?;
        let gr = grep_spans(di, &c.query);
        let hy = if gr.is_empty() {
            bm.clone()
        } else {
            gr.clone()
        };

        let results = [
            ("bm25", score(&bm, c.expected_lines, n)),
            ("grep", score(&gr, c.expected_lines, n)),
            ("hybrid", score(&hy, c.expected_lines, n)),
        ];
        for (m, (h, cm)) in results {
            agg.get_mut(m).unwrap().add(h, cm);
            if !c.kind.is_empty() {
                agg_kind.entry((c.kind.clone(), m)).or_default().add(h, cm);
            }
        }
        println!(
            "  [{}] gold {:?}  bm25 {} grep {} hybrid {}  «{}»",
            c.kind,
            c.expected_lines,
            if results[0].1 .0 { "✓" } else { "✗" },
            if results[1].1 .0 { "✓" } else { "✗" },
            if results[2].1 .0 { "✓" } else { "✗" },
            c.query,
        );
    }

    println!("\n── by mode (all) ──");
    for m in modes {
        agg[m].line(m);
    }
    let mut kinds: Vec<String> = cases
        .iter()
        .map(|c| c.kind.clone())
        .filter(|k| !k.is_empty())
        .collect();
    kinds.sort();
    kinds.dedup();
    for k in &kinds {
        println!("── kind = {k} ──");
        for m in modes {
            if let Some(a) = agg_kind.get(&(k.clone(), m)) {
                a.line(m);
            }
        }
    }
    Ok(())
}
