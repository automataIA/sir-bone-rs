use std::path::PathBuf;

use anyhow::{bail, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;
use crate::project_store;

#[derive(Deserialize, JsonSchema)]
pub struct HistoriaInput {
    /// Short title for the entry — no date (the tool stamps the local time).
    pub title: String,
    /// One bullet per change: what changed and which files were touched.
    pub bullets: Vec<String>,
}

/// Append entries to this project's persistent memory log (`HISTORIA.md`). A
/// structured stand-in for hand-editing the file: the model supplies title +
/// bullets, the tool stamps the exact local time and prepends newest-first. Being
/// a tool makes the "log every change" workflow a forcing function rather than a
/// prose instruction the model can skip.
pub struct HistoriaTool {
    pub project: PathBuf,
}

#[async_trait]
impl TypedTool for HistoriaTool {
    type Input = HistoriaInput;

    fn name(&self) -> &'static str {
        "historia"
    }

    fn description(&self) -> &'static str {
        "Append an entry to this project's persistent memory log (HISTORIA.md). Call \
         this after you change project files — it is the required final step of a \
         file-changing task, even for small changes. Give a short `title` and \
         `bullets` (what changed, files touched); the tool stamps the exact local \
         date/time and prepends the entry newest-first. Do NOT hand-edit HISTORIA.md \
         to add entries — use this so nothing is missed."
    }

    async fn run(&self, input: HistoriaInput) -> Result<String> {
        if input.title.trim().is_empty() {
            bail!("historia entry needs a non-empty title");
        }
        project_store::prepend_historia(&self.project, &input.title, &input.bullets)?;
        Ok("historia entry added".into())
    }
}
