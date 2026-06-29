//! CLI sub-command entry points split out of `main.rs`: setup check (`doctor`),
//! session inspection (`audit`, snapshots), and project initialization.

mod doctor;
mod ground;
mod login;
mod session_cmds;

pub use doctor::run_doctor;
pub use ground::run_ground;
pub use login::run_login;
pub use session_cmds::{run_audit, run_snapshots};

use std::path::Path;

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
        Ok(()) => println!("created {}", dest.display()),
        Err(e) => eprintln!("could not write AGENTS.md: {e}"),
    }
}
