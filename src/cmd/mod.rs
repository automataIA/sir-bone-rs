//! CLI sub-command entry points split out of `main.rs`: setup check (`doctor`),
//! session inspection (`audit`, snapshots), and project initialization.

mod demo;
mod doctor;
mod env_list;
mod ground;
mod hook;
mod login;
mod session_cmds;
mod setup_verification;
mod stats;
mod update;

pub use demo::run_demo;
pub use doctor::run_doctor;
pub use env_list::run_env_list;
pub use ground::run_ground;
pub use hook::run_hook;
pub use login::{login_codex, run_login, set_credentials};
pub use session_cmds::{run_audit, run_snapshots};
pub use setup_verification::run as run_setup_verification;
pub use stats::run_stats;
pub use update::run_update;

use std::path::Path;

/// Mask a token for display: never print more than a short prefix.
pub(crate) fn mask(token: &str) -> String {
    let n = token.chars().count();
    if n <= 8 {
        "•".repeat(n)
    } else {
        let head: String = token.chars().take(6).collect();
        format!("{head}…••••")
    }
}

pub fn init_project(cwd: &Path) {
    let idx = sirbone::structure::update(cwd, sirbone::structure::Index::load(cwd));
    let _ = idx.save(cwd);
    let edges = sirbone::structure::graph_cached(cwd, &idx);
    println!(
        "built code map: {} files, {} edges",
        idx.files.len(),
        edges.len()
    );
    match sirbone::project_store::link_config_into_repo(cwd) {
        Ok(sirbone::project_store::LinkOutcome::Created(link)) => {
            println!(
                "linked {} → per-project config/state (add it to .gitignore)",
                link.display()
            );
        }
        Ok(sirbone::project_store::LinkOutcome::Existed) => {}
        Ok(sirbone::project_store::LinkOutcome::Unsupported) => {}
        Err(e) => eprintln!("could not link project config: {e}"),
    }
    let dest = cwd.join("AGENTS.md");
    if dest.exists() {
        println!("AGENTS.md already exists — not overwriting");
        return;
    }
    match std::fs::write(&dest, sirbone::structure::init_doc(cwd, &idx, &edges)) {
        Ok(()) => {
            println!("created {}", dest.display());
            println!(
                "tip: maintain AGENTS.md like a pilot's checklist — earn each line \
                 (add a rule only after a real failure or hard constraint), keep it short, \
                 and prune rules the model has outgrown. It lands in the prompt every turn."
            );
        }
        Err(e) => eprintln!("could not write AGENTS.md: {e}"),
    }
}
