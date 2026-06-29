use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;
use crate::rag::{DocIndex, DEFAULT_CONTEXT, DEFAULT_K, ROUTE_WHOLE_BELOW};

#[derive(Deserialize, JsonSchema)]
pub struct DocSearchInput {
    /// What to look up — an identifier/signature ("getCookie", "TcpStream connect")
    /// or a how-to phrase ("stream server-sent events").
    pub query: String,
    /// Which downloaded doc to search: a filename under `.sirbone/docs/` such as
    /// "hono@4.x" or "hono@4.x.md" (download it first with `fetch_docs`), or a path.
    pub source: String,
    /// Max chunks to return (default 4).
    #[serde(default = "default_k")]
    pub k: usize,
    /// Lines of surrounding context kept around each hit (default 8).
    #[serde(default = "default_context")]
    pub context: usize,
}

fn default_k() -> usize {
    DEFAULT_K
}
fn default_context() -> usize {
    DEFAULT_CONTEXT
}

pub struct DocSearchTool;

#[async_trait]
impl TypedTool for DocSearchTool {
    type Input = DocSearchInput;

    fn name(&self) -> &'static str {
        "doc_search"
    }

    fn description(&self) -> &'static str {
        "Search a downloaded library doc under .sirbone/docs/ and return only the few \
         relevant lines (with citations), instead of reading the whole file. Use to \
         verify a signature/method exists or how an API is used — ground the answer in \
         pinned docs, not memory. Pair with fetch_docs (download the versioned doc first). \
         Small docs (< ~800 lines) are returned whole automatically since retrieval saves \
         little there. BM25 keyword retrieval: query with the identifier or key terms."
    }

    async fn run(&self, input: DocSearchInput) -> Result<String> {
        let raw = input.source.trim();
        let path = if raw.contains('/') {
            PathBuf::from(raw)
        } else {
            let dir = PathBuf::from(".sirbone/docs");
            let direct = dir.join(raw);
            if direct.exists() {
                direct
            } else {
                dir.join(format!("{raw}.md"))
            }
        };

        let text = tokio::fs::read_to_string(&path).await.with_context(|| {
            format!(
                "doc '{raw}' not found at {} — download it first with fetch_docs",
                path.display()
            )
        })?;

        let label = path.file_name().and_then(|s| s.to_str()).unwrap_or(raw);
        let n = text.lines().count();

        // Routing: below the threshold, retrieval saves little — return the whole doc.
        if n < ROUTE_WHOLE_BELOW {
            return Ok(format!(
                "doc '{label}' is {n} lines (< {ROUTE_WHOLE_BELOW}) — returning whole:\n\n{text}"
            ));
        }

        let di = DocIndex::build(&text)?;
        let spans = di.search(&input.query, input.k.max(1), input.context)?;
        if spans.is_empty() {
            return Ok(format!(
                "no match for '{}' in {label} ({n} lines)",
                input.query
            ));
        }
        let injected: usize = spans.iter().map(|(lo, hi)| hi - lo + 1).sum();
        let body = di.extract(label, &spans);
        Ok(format!(
            "{body}{injected}/{n} lines from {label} ({:.1}%) — cite as {label}:line",
            100.0 * injected as f64 / n as f64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_doc_errors_with_hint() {
        let err = DocSearchTool
            .run(DocSearchInput {
                query: "x".into(),
                source: "definitely-not-here".into(),
                k: 4,
                context: 8,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fetch_docs"));
    }
}
