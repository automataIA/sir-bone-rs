//! RAG #3 interactive viewer — eyeball what `doc_search` retrieves for a query.
//! Throwaway eval tool; runs the SAME engine as the production tool (sirbone::rag),
//! so what you see here is what the agent gets. Needs `--features rag`.
//!
//! Usage:
//!   cargo run --features rag --example rag_spike -- <file.md> <query words...>
//!   cargo run --features rag --example rag_spike -- README.md prompt caching anthropic

use std::time::Instant;

use sirbone::rag::{DocIndex, DEFAULT_CONTEXT};

const TOP_K: usize = 3;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: cargo run --features rag --example rag_spike -- <file.md> <query...>");
        std::process::exit(2);
    });
    let query: String = args.collect::<Vec<_>>().join(" ");
    if query.trim().is_empty() {
        eprintln!("no query given");
        std::process::exit(2);
    }

    let text = std::fs::read_to_string(&path)?;
    let t = Instant::now();
    let di = DocIndex::build(&text)?;
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let spans = di.search(&query, TOP_K, DEFAULT_CONTEXT)?;
    let search_ms = t.elapsed().as_secs_f64() * 1000.0;

    let n = di.n_lines();
    println!("file: {path}  ({n} lines) | index {index_ms:.1}ms  search {search_ms:.2}ms");
    println!("query: {query:?}\n");

    if spans.is_empty() {
        println!("(no hits)");
        return Ok(());
    }
    print!("{}", di.extract(&path, &spans));

    let injected: usize = spans.iter().map(|(lo, hi)| hi - lo + 1).sum();
    println!(
        "{} span(s) | injected {injected}/{n} lines ({:.1}% of file)",
        spans.len(),
        100.0 * injected as f64 / n.max(1) as f64,
    );
    Ok(())
}
