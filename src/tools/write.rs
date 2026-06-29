use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{freshness::ReadStamps, undo::UndoStore, TypedTool};

#[derive(Deserialize, JsonSchema)]
pub struct WriteInput {
    /// Path to write (created or overwritten)
    pub path: String,
    /// File content to write
    pub content: String,
}

pub struct WriteTool {
    pub undo: UndoStore,
    pub stamps: ReadStamps,
}

#[async_trait]
impl TypedTool for WriteTool {
    type Input = WriteInput;

    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating parent directories if needed. \
         For changes to an existing file prefer `edit`; read a file before \
         overwriting it. Never create files (especially docs/README) that \
         were not requested."
    }

    fn mutation_target(&self, args: &serde_json::Value) -> Option<std::path::PathBuf> {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
    }

    async fn run(&self, input: WriteInput) -> Result<String> {
        let path = std::path::Path::new(&input.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("cannot create dirs for {}", input.path))?;
        }
        self.undo.snapshot(&input.path).await;
        tokio::fs::write(path, &input.content)
            .await
            .with_context(|| format!("cannot write {}", input.path))?;
        // Refresh the freshness stamp so a later edit sees the content we just
        // wrote, not a stale earlier read, as the baseline.
        self.stamps.record(&input.path, &input.content).await;
        Ok(format!(
            "wrote {} bytes to {}",
            input.content.len(),
            input.path
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let tool = WriteTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        };
        tool.run(WriteInput {
            path: path.to_str().unwrap().into(),
            content: "hello world".into(),
        })
        .await
        .unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/dir/file.txt");
        let tool = WriteTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        };
        tool.run(WriteInput {
            path: path.to_str().unwrap().into(),
            content: "nested".into(),
        })
        .await
        .unwrap();
        assert!(path.exists());
    }
}
