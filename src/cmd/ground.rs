//! `sirbone ground` — deterministic claim grounding as a first-class command.
//!
//! Runs the same no-LLM check as `SIRBONE_GROUND` (paths / symbols / counts vs
//! the project) but reports the verified facts straight to the user / CI instead
//! of feeding them to the model. This is the robust delivery: the A/B showed the
//! *detection* is reliable while trusting the model to self-correct is not — so
//! surface the facts and let a human (or a gate) act on them. No model, no key.
//!
//! `sirbone ground PLAN.md`  — ground a file.
//! `sirbone ground`          — ground the last assistant message of the latest session.
//! Exits non-zero when a divergence is found (a path that is not a current file,
//! or a count that differs) so it can gate CI.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sirbone::Role;

/// `arg = Some(path)` grounds that file; `None` grounds the latest session's
/// final assistant message.
pub async fn run_ground(arg: Option<PathBuf>, cwd: &Path) -> Result<()> {
    let text = match arg {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?,
        None => last_assistant_text().await?,
    };
    if text.trim().is_empty() {
        bail!("ground: nothing to check (empty input)");
    }

    let facts = sirbone::agent::facts(cwd, &text);
    if facts.is_empty() {
        println!("no groundable claims found — nothing to verify");
        return Ok(());
    }
    println!("VERIFIED FACTS (mechanical, no model in the loop):");
    for f in &facts {
        println!("- {f}");
    }
    // Divergences are the actionable subset: a path that does not exist, or a
    // count that differs. (Location facts are confirmations — we can't tell a
    // wrong-location from a right one, so they don't gate.)
    let divergences = facts
        .iter()
        .filter(|f| f.contains("is not a current file") || f.contains("(you wrote"))
        .count();
    if divergences > 0 {
        eprintln!("\n{divergences} divergence(s) from the code — review before trusting the text.");
        std::process::exit(1);
    }
    Ok(())
}

/// Concatenated text of the last assistant message in the most recent session.
async fn last_assistant_text() -> Result<String> {
    let path = sirbone::session::latest_session_path()
        .await
        .context("ground: no session found; pass a file path instead")?;
    let messages = sirbone::session::collapse(sirbone::session::load(&path).await?);
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant))
        .map(|m| sirbone::types::extract_text(&m.content))
        .filter(|t| !t.trim().is_empty())
        .context("ground: latest session has no assistant text")
}
