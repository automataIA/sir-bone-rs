//! `sirbone update` — forward to the `sirbone-update` binary that the shell and
//! PowerShell installers drop next to `sirbone` (axoupdater, enabled by
//! `install-updater` in dist-workspace.toml).
//!
//! Forwarding rather than linking axoupdater as a library keeps the updater out
//! of this binary: a build installed by `cargo install` or a distro package has
//! no `sirbone-update` next to it and must not pretend it can self-upgrade.

use std::path::PathBuf;

use anyhow::{bail, Result};

/// Name of the sibling updater binary dist installs.
const UPDATER: &str = if cfg!(windows) {
    "sirbone-update.exe"
} else {
    "sirbone-update"
};

/// Locate `sirbone-update` next to the running executable, then on `PATH`.
fn locate() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok()?.with_file_name(UPDATER);
    if sibling.is_file() {
        return Some(sibling);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(UPDATER))
        .find(|p| p.is_file())
}

pub fn run_update() -> Result<()> {
    let Some(updater) = locate() else {
        bail!(
            "`{UPDATER}` not found next to this binary.\n\n\
             It ships with the shell/PowerShell installers only. Upgrade with whatever \
             installed sirbone:\n  \
             curl --proto '=https' --tlsv1.2 -LsSf \
             https://github.com/automataIA/sir-bone-rs/releases/latest/download/sir-bone-rs-installer.sh | sh\n  \
             cargo install sir-bone-rs --locked         (built from source)\n  \
             cargo binstall sir-bone-rs                 (prebuilt, no toolchain)"
        );
    };
    // axoupdater takes no arguments: it polls for a release and installs it.
    let status = std::process::Command::new(&updater).status()?;
    if !status.success() {
        bail!("{} exited with {status}", updater.display());
    }
    Ok(())
}
