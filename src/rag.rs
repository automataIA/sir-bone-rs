//! Local document retrieval — BM25 over heading/fence-aware chunks. Powers the
//! `doc_search` tool. Gated by the `rag` feature (tantivy is the only extra dep).
//!
//! Design validated offline by `examples/rag_bench.rs` on a real 14k-line library
//! doc: BM25 + heading/fence chunking + range-merge gave 100% recall on signature
//! lookups while injecting ~1% of the file, and beat a naive grep baseline (no IDF).
//! See ROADMAP-RETRIEVAL.md §"Bench su corpus REALE".

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, TEXT};
use tantivy::{doc, Index, TantivyDocument};

/// Docs at/above this line count go through retrieval; smaller ones are cheaper to
/// read whole (RAG saves little below it — see the size/compression curve in the bench).
pub const ROUTE_WHOLE_BELOW: usize = 800;
pub const DEFAULT_K: usize = 4;
pub const DEFAULT_CONTEXT: usize = 8;

const MAX_LINES: usize = 60;
const MIN_LINES: usize = 12;

fn is_heading(l: &str) -> bool {
    l.trim_start().starts_with('#')
}
fn is_fence(l: &str) -> bool {
    l.trim_start().starts_with("```")
}

/// Heading- and paragraph-aware chunk boundaries `[start, end)` (0-based) over
/// `lines`, never splitting inside a ``` fence: a heading opens a new chunk once
/// the current one has ≥ MIN_LINES, and an over-long chunk is cut at MAX_LINES
/// only outside a fence (so code blocks stay intact).
pub fn chunk_bounds(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut in_fence, mut start, mut i) = (false, 0usize, 0usize);
    while i < lines.len() {
        if is_fence(lines[i]) {
            in_fence = !in_fence;
        }
        if !in_fence && is_heading(lines[i]) && i - start >= MIN_LINES {
            out.push((start, i));
            start = i;
        }
        i += 1;
        if !in_fence && i - start >= MAX_LINES {
            out.push((start, i));
            start = i;
        }
    }
    if start < lines.len() {
        out.push((start, lines.len()));
    }
    out
}

/// Merge overlapping/adjacent 1-based inclusive ranges into disjoint spans.
pub fn merge(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// In-RAM BM25 index over one document's chunks.
pub struct DocIndex {
    lines: Vec<String>,
    index: Index,
    f_body: Field,
    f_start: Field,
    f_end: Field,
}

impl DocIndex {
    pub fn build(text: &str) -> Result<Self> {
        let lines: Vec<String> = text.lines().map(String::from).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

        let mut sb = Schema::builder();
        let f_body = sb.add_text_field("body", TEXT);
        let f_start = sb.add_u64_field("start", STORED);
        let f_end = sb.add_u64_field("end", STORED);
        let index = Index::create_in_ram(sb.build());
        let mut w = index.writer(15_000_000)?;
        for (a, b) in chunk_bounds(&refs) {
            w.add_document(doc!(
                f_body  => refs[a..b].join("\n"),
                f_start => (a + 1) as u64,
                f_end   => b as u64,
            ))?;
        }
        w.commit()?;
        Ok(Self {
            lines,
            index,
            f_body,
            f_start,
            f_end,
        })
    }

    pub fn n_lines(&self) -> usize {
        self.lines.len()
    }
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Top-K BM25 chunks, each expanded by `context` lines and then range-merged.
    /// Returns disjoint 1-based inclusive spans, best-first by lowest line.
    pub fn search(&self, query: &str, k: usize, context: usize) -> Result<Vec<(usize, usize)>> {
        let searcher = self.index.reader()?.searcher();
        let qp = QueryParser::for_index(&self.index, vec![self.f_body]);
        let q = qp.parse_query(query)?;
        let hits = searcher.search(&q, &TopDocs::with_limit(k))?;
        let mut ranges = Vec::new();
        for (_score, addr) in &hits {
            let d: TantivyDocument = searcher.doc(*addr)?;
            let s = d
                .get_first(self.f_start)
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize;
            let e = d
                .get_first(self.f_end)
                .and_then(|v| v.as_u64())
                .unwrap_or(s as u64) as usize;
            ranges.push((
                s.saturating_sub(context).max(1),
                (e + context).min(self.lines.len()),
            ));
        }
        Ok(merge(ranges))
    }

    /// Render spans as numbered, citation-headed text for injection into context.
    pub fn extract(&self, source: &str, spans: &[(usize, usize)]) -> String {
        let mut out = String::new();
        for &(lo, hi) in spans {
            out.push_str(&format!("── {source}:{lo}-{hi}\n"));
            for (off, line) in self.lines[lo - 1..hi].iter().enumerate() {
                out.push_str(&format!("{:>6} | {}\n", lo + off, line));
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Intro
line two
line three
```rust
// fence: blank line below must NOT split the chunk

fn keep_together() {}
```
more prose here
## Cookies
setCookie(c, name, value)
getCookie(c, name)
deleteCookie(c, name)
## Streaming
streamSSE(c, cb)
";

    #[test]
    fn chunking_does_not_split_fences() {
        let lines: Vec<&str> = DOC.lines().collect();
        for (a, b) in chunk_bounds(&lines) {
            let block = &lines[a..b];
            let fences = block.iter().filter(|l| is_fence(l)).count();
            assert!(fences % 2 == 0, "chunk {a}..{b} splits a code fence");
        }
    }

    #[test]
    fn merge_collapses_overlaps() {
        assert_eq!(
            merge(vec![(1, 5), (4, 8), (20, 22)]),
            vec![(1, 8), (20, 22)]
        );
        assert_eq!(merge(vec![(1, 3), (4, 6)]), vec![(1, 6)]); // adjacent
    }

    #[test]
    fn search_finds_signature_chunk() {
        let di = DocIndex::build(DOC).unwrap();
        let spans = di.search("getCookie", DEFAULT_K, 1).unwrap();
        assert!(!spans.is_empty());
        let text = di.extract("doc.md", &spans);
        assert!(
            text.contains("getCookie"),
            "retrieved span must contain the queried identifier"
        );
    }
}
