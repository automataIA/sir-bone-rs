use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{freshness::ReadStamps, truncate::truncate_default, TypedTool};

#[derive(Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Path to the file to read
    pub path: String,
    /// Starting line number, 1-indexed (default: 1)
    #[serde(default = "default_offset")]
    pub offset: u64,
    /// Maximum number of lines to read (default: all)
    pub limit: Option<u64>,
}

fn default_offset() -> u64 {
    1
}

#[derive(Default)]
pub struct ReadTool {
    pub stamps: ReadStamps,
}

#[async_trait]
impl TypedTool for ReadTool {
    type Input = ReadInput;

    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read file contents, optionally from a starting line with a line limit. \
         Prefer this over cat/head/tail via bash. For large files read the \
         relevant slice (offset + limit) instead of the whole file."
    }

    async fn run(&self, input: ReadInput) -> Result<String> {
        let content = tokio::fs::read_to_string(&input.path)
            .await
            .with_context(|| format!("cannot open {}", input.path))?;

        // Stamp the whole-file hash so a later edit can tell the file changed
        // underneath it, independent of which slice we return below.
        self.stamps.record(&input.path, &content).await;

        let start = input.offset.max(1) as usize;
        let output: String = content
            .lines()
            .skip(start - 1)
            .take(input.limit.map_or(usize::MAX, |l| l as usize))
            .flat_map(|l| [l, "\n"])
            .collect();

        Ok(truncate_default(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn read_whole_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line1\nline2\nline3").unwrap();
        let tool = ReadTool::default();
        let result = tool
            .run(ReadInput {
                path: tmp.path().to_str().unwrap().into(),
                offset: 1,
                limit: None,
            })
            .await
            .unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=5 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let tool = ReadTool::default();
        let result = tool
            .run(ReadInput {
                path: tmp.path().to_str().unwrap().into(),
                offset: 2,
                limit: Some(2),
            })
            .await
            .unwrap();
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line4"));
    }
}
