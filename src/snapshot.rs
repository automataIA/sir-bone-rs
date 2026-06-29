//! Workspace snapshots via a shadow git repo.
//!
//! A separate git dir under `~/.sirbone/projects/<slug>/snapshots.git` with the
//! project root as work-tree: the user's own `.git` (if any) is never touched,
//! and non-git projects get rollback for free. One snapshot is taken per agent
//! run, lazily — right before the first mutating tool call — so read-only turns
//! cost nothing. Unlike the per-file `undo` tool, a snapshot captures *every*
//! file, including bash side effects (scripts that create/move/delete files).
//!
//! Everything shells out to the `git` CLI (best-effort: no git on PATH means
//! snapshots silently disable). Restore uses plumbing (`read-tree` +
//! `checkout-index` + `clean`) so files created after the snapshot are removed
//! too; a safety snapshot is taken first, making every rollback itself
//! reversible. `SIRBONE_NO_SNAPSHOT=1` disables the feature.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::process::Command;

/// Build dirs excluded even when the project has no `.gitignore`.
const EXCLUDES: &str = "target/\nnode_modules/\ndist/\nbuild/\n.venv/\n__pycache__/\n";

pub struct Snapshots {
    git_dir: PathBuf,
    work_tree: PathBuf,
    taken: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotDetails {
    pub short_id: String,
    pub full_id: String,
    pub age: String,
    pub label: String,
    pub changed_files: Vec<String>,
}

/// Snapshots for the current working directory, as the agent context wants
/// them: `None` when disabled via `SIRBONE_NO_SNAPSHOT`. Cheap — call per turn.
pub fn workspace_snapshots() -> Option<std::sync::Arc<Snapshots>> {
    if std::env::var_os("SIRBONE_NO_SNAPSHOT").is_some() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    Some(std::sync::Arc::new(Snapshots::open(&cwd)))
}

impl Snapshots {
    /// Cheap constructor — no I/O until the first snapshot.
    pub fn open(project: &Path) -> Self {
        Self::at(
            crate::project_store::project_dir(project).join("snapshots.git"),
            project.to_path_buf(),
        )
    }

    /// Explicit-paths constructor (tests use a tempdir git_dir).
    pub(crate) fn at(git_dir: PathBuf, work_tree: PathBuf) -> Self {
        Self {
            git_dir,
            work_tree,
            taken: AtomicBool::new(false),
        }
    }

    async fn git(&self, args: &[&str]) -> Result<std::process::Output> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(args)
            .current_dir(&self.work_tree)
            .output()
            .await
            .context("git not found on PATH")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    async fn ensure_repo(&self) -> Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.git_dir).await?;
        self.git(&["init", "-q"]).await?;
        self.git(&["config", "user.name", "sirbone"]).await?;
        self.git(&["config", "user.email", "sirbone@localhost"])
            .await?;
        tokio::fs::write(self.git_dir.join("info/exclude"), EXCLUDES).await?;
        Ok(())
    }

    /// Take at most one snapshot per agent run (first mutating tool call).
    /// Best-effort: failures are logged, never block the agent.
    pub async fn take_once(&self, label: &str) {
        if self.taken.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.snapshot(label).await {
            tracing::warn!("workspace snapshot failed: {e}");
        }
    }

    /// Like [`Snapshots::take_once`], but returns the full commit id when this
    /// call is the one that actually creates/claims the run snapshot.
    pub async fn take_once_id(&self, label: &str) -> Option<String> {
        if self.taken.swap(true, Ordering::SeqCst) {
            return None;
        }
        match self.snapshot_id(label).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!("workspace snapshot failed: {e}");
                None
            }
        }
    }

    /// Commit the current work-tree state. `Ok(false)` = nothing changed.
    /// A clean tree with no history still commits an empty baseline, so a
    /// run starting in an empty/ignored-only dir is rollback-able too.
    pub async fn snapshot(&self, label: &str) -> Result<bool> {
        self.ensure_repo().await?;
        self.git(&["add", "-A"]).await?;
        let status = self.git(&["status", "--porcelain"]).await?;
        let has_head = self
            .git(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .await
            .is_ok();
        if status.stdout.is_empty() && has_head {
            return Ok(false);
        }
        let label: String = label.chars().take(60).collect();
        let msg = if label.is_empty() {
            "(no prompt)".into()
        } else {
            label
        };
        self.git(&["commit", "-q", "--allow-empty", "-m", &msg])
            .await?;
        Ok(true)
    }

    /// Commit the current state (like [`Snapshots::snapshot`]) and return the
    /// full commit id, usable as a [`Snapshots::rollback`] target. The 40-char
    /// SHA never parses as a 1-based index, so it is unambiguous. Used by the
    /// verification oracle to mark a per-attempt rollback point.
    pub async fn snapshot_id(&self, label: &str) -> Result<String> {
        self.snapshot(label).await?;
        let out = self.git(&["rev-parse", "HEAD"]).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Most recent snapshots as `(short_id, age, label)` rows, newest first.
    /// Empty when no snapshot was ever taken (or git is unavailable).
    pub async fn list(&self, limit: usize) -> Vec<(String, String, String)> {
        let n = limit.to_string();
        let Ok(out) = self.git(&["log", "--format=%h\t%cr\t%s", "-n", &n]).await else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some((f.next()?.into(), f.next()?.into(), f.next()?.into()))
            })
            .collect()
    }

    /// Most recent snapshots with full id and changed files, newest first.
    pub async fn list_detailed(&self, limit: usize) -> Vec<SnapshotDetails> {
        let n = limit.to_string();
        let Ok(out) = self
            .git(&["log", "--format=%H\t%h\t%cr\t%s", "-n", &n])
            .await
        else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut f = line.splitn(4, '\t');
            let (Some(full_id), Some(short_id), Some(age), Some(label)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let changed_files = self.changed_files_for(full_id).await;
            rows.push(SnapshotDetails {
                short_id: short_id.into(),
                full_id: full_id.into(),
                age: age.into(),
                label: label.into(),
                changed_files,
            });
        }
        rows
    }

    async fn changed_files_for(&self, commit: &str) -> Vec<String> {
        let Ok(out) = self
            .git(&[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-status",
                "-r",
                commit,
            ])
            .await
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Restore the work-tree to a snapshot. `target` = short id from [`list`],
    /// or a 1-based index (`"1"` = newest). A safety snapshot of the current
    /// state is committed first, so a rollback can itself be rolled back.
    pub async fn rollback(&self, target: &str) -> Result<String> {
        let entries = self.list(50).await;
        if entries.is_empty() {
            bail!("no snapshots yet");
        }
        let id = match target.parse::<usize>() {
            Ok(n) if n >= 1 && n <= entries.len() => entries[n - 1].0.clone(),
            Ok(n) => bail!("index {n} out of range (1..={})", entries.len()),
            Err(_) => {
                self.git(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{target}^{{commit}}"),
                ])
                .await
                .with_context(|| format!("unknown snapshot '{target}'"))?;
                target.to_string()
            }
        };
        self.snapshot("pre-rollback (auto)").await?;
        self.git(&["read-tree", &id]).await?;
        self.git(&["checkout-index", "-a", "-f"]).await?;
        // The pre-rollback snapshot above committed (git add -A) any untracked
        // files, so `clean` below removes them from the work-tree but they remain
        // recoverable. Preview after read-tree (when they read as untracked again)
        // and surface what vanished instead of deleting it silently.
        let preview = self.git(&["clean", "-fdn"]).await?;
        let removed: Vec<String> = String::from_utf8_lossy(&preview.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("Would remove ").map(str::to_string))
            .collect();
        self.git(&["clean", "-fdq"]).await?;
        let mut msg = format!("workspace restored to snapshot {id}");
        if !removed.is_empty() {
            msg.push_str(&format!(
                "\nremoved {} untracked file(s) (recoverable: roll back the \
                 'pre-rollback (auto)' snapshot): {}",
                removed.len(),
                removed.join(", ")
            ));
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_snaps(proj: &Path, store: &Path) -> Snapshots {
        Snapshots::at(store.join("snapshots.git"), proj.to_path_buf())
    }

    /// Snapshot → mutate (edit + create + delete) → rollback restores all
    /// three classes of change.
    #[tokio::test]
    async fn snapshot_rollback_roundtrip() -> Result<()> {
        let store = tempfile::tempdir()?;
        let proj = tempfile::tempdir()?;

        tokio::fs::write(proj.path().join("keep.txt"), "v1").await?;
        tokio::fs::write(proj.path().join("doomed.txt"), "bye").await?;
        let snaps = temp_snaps(proj.path(), store.path());
        assert!(snaps.snapshot("initial").await?);
        assert!(!snaps.snapshot("no changes").await?, "clean tree must skip");

        tokio::fs::write(proj.path().join("keep.txt"), "v2").await?;
        tokio::fs::write(proj.path().join("junk.txt"), "new").await?;
        tokio::fs::remove_file(proj.path().join("doomed.txt")).await?;

        let msg = snaps.rollback("1").await?;
        assert!(msg.contains("restored"));
        let keep = tokio::fs::read_to_string(proj.path().join("keep.txt")).await?;
        assert_eq!(keep, "v1");
        assert!(
            !proj.path().join("junk.txt").exists(),
            "post-snapshot file removed"
        );
        assert!(
            proj.path().join("doomed.txt").exists(),
            "deleted file restored"
        );
        // Removing the untracked file is surfaced (not silent) with a recovery hint.
        assert!(
            msg.contains("junk.txt"),
            "removed untracked file named: {msg}"
        );
        assert!(msg.contains("recoverable"), "recovery hint present: {msg}");

        // Safety snapshot makes the rollback itself reversible.
        let entries = snaps.list(10).await;
        assert!(entries.iter().any(|(_, _, s)| s.contains("pre-rollback")));
        Ok(())
    }

    #[tokio::test]
    async fn take_once_fires_single_snapshot() -> Result<()> {
        let store = tempfile::tempdir()?;
        let proj = tempfile::tempdir()?;
        tokio::fs::write(proj.path().join("a.txt"), "x").await?;

        let snaps = temp_snaps(proj.path(), store.path());
        snaps.take_once("first").await;
        tokio::fs::write(proj.path().join("a.txt"), "y").await?;
        snaps.take_once("second (must not commit)").await;

        assert_eq!(snaps.list(10).await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn list_detailed_includes_ids_and_changed_files() -> Result<()> {
        let store = tempfile::tempdir()?;
        let proj = tempfile::tempdir()?;
        tokio::fs::write(proj.path().join("a.txt"), "one").await?;

        let snaps = temp_snaps(proj.path(), store.path());
        assert!(snaps.snapshot("initial").await?);
        tokio::fs::write(proj.path().join("a.txt"), "two").await?;
        tokio::fs::write(proj.path().join("b.txt"), "new").await?;
        assert!(snaps.snapshot("change files").await?);

        let entries = snaps.list_detailed(2).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "change files");
        assert_eq!(entries[0].full_id.len(), 40);
        assert!(!entries[0].short_id.is_empty());
        assert!(entries[0].changed_files.iter().any(|f| f == "M\ta.txt"));
        assert!(entries[0].changed_files.iter().any(|f| f == "A\tb.txt"));
        Ok(())
    }
}
