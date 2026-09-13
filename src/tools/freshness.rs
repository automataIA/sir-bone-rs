use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

/// Content hash of each file at the moment the agent last read or wrote it.
/// Lets an edit detect that the file changed underneath the model since the
/// `read` it based the edit on — the "lost update" class of bug. Shares the
/// `Arc<Mutex<…>>` shape of [`super::undo::UndoStore`] so it threads through the
/// same tools.
#[derive(Clone, Default)]
pub struct ReadStamps(Arc<Mutex<HashMap<PathBuf, u64>>>);

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Short content tag shown by `read` and echoed back in a hashline patch
/// header. Same hash as the freshness stamp, narrowed to four hex digits: it
/// only has to catch "this file is not the one you read", which the stamp
/// already guards; the tag makes the mismatch visible to the model.
pub fn tag_of(content: &str) -> String {
    format!("{:04X}", hash_str(content) & 0xFFFF)
}

/// Per-line tag shown by `read` next to the line number and optionally quoted
/// back in a patch address (`32#a7`). [`tag_of`] answers "is this the file you
/// read?"; this answers the different question "is line 32 the line you meant?",
/// which a whole-file tag cannot catch — the file can be untouched while the
/// address is simply wrong. Two hex digits, lowercase so it never reads as a
/// file tag: one line in 256 collides, which is a check the model can trip over
/// by accident far less often than it miscounts a line.
pub fn line_tag(line: &str) -> String {
    format!("{:02x}", hash_str(line) & 0xFF)
}

impl ReadStamps {
    /// Record the content the agent has now seen for `path` (after a read, or
    /// after a successful write/edit so consecutive edits don't trip the guard).
    pub async fn record(&self, path: &str, content: &str) {
        self.0
            .lock()
            .await
            .insert(PathBuf::from(path), hash_str(content));
    }

    /// Guard a mutation: error if `path` was read before and its on-disk content
    /// no longer matches what the agent saw. A missing stamp (never read through
    /// the agent) is allowed — the edit's own string-match stays the fallback.
    pub async fn guard(&self, path: &str, current: &str) -> Result<()> {
        if let Some(&stamp) = self.0.lock().await.get(&PathBuf::from(path)) {
            if stamp != hash_str(current) {
                bail!("file {path} changed since you last read it — re-read it before editing");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guard_passes_when_unchanged() {
        let s = ReadStamps::default();
        s.record("/f", "hello").await;
        assert!(s.guard("/f", "hello").await.is_ok());
    }

    #[tokio::test]
    async fn guard_rejects_when_changed() {
        let s = ReadStamps::default();
        s.record("/f", "hello").await;
        let err = s.guard("/f", "hello world").await.unwrap_err().to_string();
        assert!(err.contains("changed since you last read it"), "{err}");
    }

    #[tokio::test]
    async fn guard_allows_unread_file() {
        let s = ReadStamps::default();
        assert!(s.guard("/never-read", "anything").await.is_ok());
    }
}
