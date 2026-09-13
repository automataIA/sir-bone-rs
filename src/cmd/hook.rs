//! `sirbone hook install|uninstall` — a local pre-commit review.
//!
//! The keyless alternative to reviewing in CI: same `--review-only` gate, but
//! the key stays in `~/.sirbone/.env` instead of a repository secret, and no
//! runner minutes are spent. (GitHub's own free inference, GitHub Models, was
//! retired on 2026-07-30, so a workflow needs a provider key either way.)

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The hook body. Shipped as a file rather than a string literal so it can be
/// read, linted and run on its own.
const HOOK: &str = include_str!("../../assets/pre-commit-review.sh");

/// How an installed hook is recognized as ours. Never overwrite or delete a
/// hook without it: someone else's pre-commit is not ours to manage.
const MARKER: &str = "installed by `sirbone hook install`";

pub fn run_hook(action: Option<&str>, cwd: &Path) -> Result<()> {
    match action {
        Some("install") => {
            let path = install(&hooks_dir(cwd)?)?;
            println!("installed {}", path.display());
            println!(
                "it reviews the staged diff on every commit and blocks only on an explicit \
                 BLOCK verdict — `git commit --no-verify` skips it once, SIRBONE_HOOK=off always"
            );
        }
        Some("uninstall") => {
            let dir = hooks_dir(cwd)?;
            match uninstall(&dir)? {
                Some(path) => println!("removed {}", path.display()),
                None => println!("no sirbone hook in {}", dir.display()),
            }
        }
        _ => println!("usage: sirbone hook install | sirbone hook uninstall"),
    }
    Ok(())
}

/// Ask git where hooks live rather than assuming `.git/hooks`: that answer is
/// wrong inside a worktree, in a submodule, and whenever `core.hooksPath` is
/// set.
fn hooks_dir(cwd: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(cwd)
        .output()
        .context("could not run git — is it installed?")?;
    if !out.status.success() {
        bail!("{} is not a git repository", cwd.display());
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(cwd.join(rel))
}

fn install(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join("pre-commit");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if !existing.is_empty() && !existing.contains(MARKER) {
        bail!(
            "{} already exists and was not written by sirbone — move it aside, or call \
             `sirbone --review-only` from it yourself",
            path.display()
        );
    }
    std::fs::write(&path, HOOK).with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(path)
}

/// `Ok(None)` when there is nothing of ours to remove; an error when a hook is
/// there but somebody else wrote it.
fn uninstall(dir: &Path) -> Result<Option<PathBuf>> {
    let path = dir.join("pre-commit");
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    if !existing.contains(MARKER) {
        bail!(
            "{} was not written by sirbone — leaving it alone",
            path.display()
        );
    }
    std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker is what makes install idempotent and uninstall safe, so the
    /// shipped script must actually carry it.
    #[test]
    fn shipped_hook_carries_the_marker() {
        assert!(HOOK.contains(MARKER), "hook script lost its marker line");
    }

    #[test]
    fn install_is_idempotent_and_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = install(dir.path()).expect("install");
        let second = install(dir.path()).expect("reinstall over our own hook");
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&second).expect("read"), HOOK);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&second)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "hook is not executable: {mode:o}");
        }
    }

    /// Somebody else's pre-commit hook is not ours to overwrite or delete.
    #[test]
    fn foreign_hook_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pre-commit");
        std::fs::write(&path, "#!/bin/sh\nmake lint\n").expect("write");
        assert!(install(dir.path()).is_err());
        assert!(uninstall(dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "#!/bin/sh\nmake lint\n"
        );
    }

    #[test]
    fn uninstall_removes_our_hook_and_tolerates_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(uninstall(dir.path()).expect("no hook"), None);
        install(dir.path()).expect("install");
        assert!(uninstall(dir.path()).expect("uninstall").is_some());
        assert!(!dir.path().join("pre-commit").exists());
    }
}
