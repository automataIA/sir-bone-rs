//! Spill-to-file for truncated tool output.
//!
//! [`super::truncate`] caps a tool result at ~16k estimated tokens by keeping the
//! head and tail and eliding the middle. Without this module the elided part
//! stops existing anywhere: the only way back to it is to re-run the command,
//! paying latency, tokens, and — for `bash` — whatever side effects it has.
//!
//! So the full output is written to a file first and the truncation marker
//! carries its path. Nothing is promised about quality here; this is simply not
//! destroying data that was already produced.
//!
//! Cheap by construction: the file is written *only* on the path where
//! truncation actually happens, so an ordinary result never touches the disk.
//! Best-effort throughout — a spill that cannot be written degrades to the old
//! marker rather than failing the tool call.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Keep at most this many spill files per project.
const MAX_FILES: usize = 32;
/// …and at most this many bytes in total, whichever binds first.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// `~/.sirbone/projects/<slug>/spill/`.
///
/// Deliberately not `/tmp`: a session resumed with `--session` days later must
/// still find the outputs its transcript points at, and this machine's `/tmp` is
/// cleared on boot. Retention is bounded by [`prune`] instead.
fn spill_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(crate::project_store::project_dir(&cwd).join("spill"))
}

/// Write `content` and return its path, or `None` if spilling is off or failed.
///
/// The name is the content hash, so the same output spilled twice is one file.
pub fn write(content: &str) -> Option<PathBuf> {
    // A `tusk` filter exists to keep something out of the transcript, and this
    // runs inside the tool — before any filter can see the content. Spilling it
    // would leave the unfiltered original on disk, which is the one outcome a
    // redaction filter is configured to prevent.
    if crate::ablate::spill_disabled() || crate::checks::tusk_armed() {
        return None;
    }
    let dir = spill_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    let path = dir.join(format!("{:016x}.txt", h.finish()));
    if !path.exists() {
        std::fs::write(&path, content).ok()?;
        prune(&dir);
    }
    // Counted per call, not per file created: the same output spilled twice
    // reuses one file, but the agent was handed a recovery path both times, and
    // that is what the counter is asked about.
    crate::telemetry::add(&crate::telemetry::SPILL_WRITES, 1);
    Some(path)
}

/// Drop the oldest spills until the directory is inside both caps. Called after
/// a write, so the bound holds without a background task or a session registry.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then_some(())
                .and(Some((meta.modified().ok()?, meta.len(), e.path())))
        })
        .collect();
    // Newest first, so the survivors are taken from the front.
    files.sort_unstable_by_key(|f| std::cmp::Reverse(f.0));
    let mut bytes = 0u64;
    for (i, (_, len, path)) in files.iter().enumerate() {
        bytes += len;
        if i >= MAX_FILES || bytes > MAX_BYTES {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `projects_root()` already redirects to a temp dir under cfg(test), so
    // these write to the unit-test scratch, never to the real `~/.sirbone`.

    #[test]
    fn same_content_spills_to_one_file() {
        let a = write("spill: same content twice").expect("spill written");
        assert_eq!(
            std::fs::read_to_string(&a).expect("readable"),
            "spill: same content twice"
        );
        assert_eq!(
            write("spill: same content twice").as_deref(),
            Some(a.as_path())
        );
    }

    #[test]
    fn prune_keeps_the_newest_within_the_cap() {
        let dir = std::env::temp_dir().join("sirbone-spill-prune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        for i in 0..MAX_FILES + 8 {
            std::fs::write(dir.join(format!("{i:03}.txt")), format!("payload {i}")).expect("write");
            // Distinct mtimes: the prune order is defined by them.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        prune(&dir);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), MAX_FILES, "pruned to the file cap");
        assert!(
            left.iter().any(|n| n == "039.txt"),
            "the newest survives: {left:?}"
        );
        assert!(
            !left.iter().any(|n| n == "000.txt"),
            "the oldest is gone: {left:?}"
        );
    }
}
