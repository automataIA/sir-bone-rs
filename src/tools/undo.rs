use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::TypedTool;

/// Per-file stack of previous contents. `None` = file did not exist.
#[derive(Clone, Default)]
pub struct UndoStore(Arc<Mutex<HashMap<PathBuf, Vec<Option<String>>>>>);

impl UndoStore {
    /// Save current file state before a destructive operation.
    pub async fn snapshot(&self, path: &str) {
        let p = PathBuf::from(path);
        let prev = tokio::fs::read_to_string(&p).await.ok();
        self.0.lock().await.entry(p).or_default().push(prev);
    }

    /// Pop the most recent snapshot and restore it.
    pub async fn restore(&self, path: &str) -> Result<String> {
        let p = PathBuf::from(path);
        let mut map = self.0.lock().await;
        let Some(prev) = map.get_mut(&p).and_then(|s| s.pop()) else {
            bail!("no undo history for {path}");
        };
        if map.get(&p).is_some_and(|s| s.is_empty()) {
            map.remove(&p);
        }
        drop(map);

        match prev {
            Some(content) => {
                tokio::fs::write(&p, &content).await?;
                Ok(format!("restored {path} ({} bytes)", content.len()))
            }
            None => {
                tokio::fs::remove_file(&p).await?;
                Ok(format!("removed {path} (file did not exist before)"))
            }
        }
    }

    pub async fn list_entries(&self) -> Vec<(String, usize)> {
        let map = self.0.lock().await;
        let mut v: Vec<_> = map
            .iter()
            .map(|(p, s)| (p.display().to_string(), s.len()))
            .collect();
        v.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        v
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct UndoInput {
    /// File path to undo, or omit to list files with undo history
    pub path: Option<String>,
}

pub struct UndoTool {
    pub store: UndoStore,
}

#[async_trait]
impl TypedTool for UndoTool {
    type Input = UndoInput;

    fn name(&self) -> &'static str {
        "undo"
    }

    fn description(&self) -> &'static str {
        "Undo the last edit/write/sed change to a file. Call with no path to list available undo targets."
    }

    fn mutation_target(&self, args: &serde_json::Value) -> Option<std::path::PathBuf> {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
    }

    async fn run(&self, input: UndoInput) -> Result<String> {
        match input.path {
            Some(path) => self.store.restore(&path).await,
            None => {
                let entries = self.store.list_entries().await;
                if entries.is_empty() {
                    return Ok("no undo history".into());
                }
                Ok(entries
                    .iter()
                    .map(|(p, n)| format!("{p}  ({n} snapshots)"))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn undo_restores_edit() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "original").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let store = UndoStore::default();
        store.snapshot(&path).await;
        tokio::fs::write(&path, "modified").await.unwrap();

        let result = store.restore(&path).await.unwrap();
        assert!(result.contains("restored"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[tokio::test]
    async fn undo_removes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let path_str = path.to_str().unwrap().to_string();

        let store = UndoStore::default();
        store.snapshot(&path_str).await; // file doesn't exist yet
        tokio::fs::write(&path, "created").await.unwrap();
        assert!(path.exists());

        store.restore(&path_str).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn undo_no_history_errors() {
        let store = UndoStore::default();
        assert!(store.restore("/tmp/nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn undo_stacks() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "v1").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let store = UndoStore::default();
        store.snapshot(&path).await; // v1
        tokio::fs::write(&path, "v2").await.unwrap();
        store.snapshot(&path).await; // v2
        tokio::fs::write(&path, "v3").await.unwrap();

        store.restore(&path).await.unwrap(); // back to v2
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");

        store.restore(&path).await.unwrap(); // back to v1
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    }
}
