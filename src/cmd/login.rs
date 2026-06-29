//! `sirbone login` — seed a global `~/.sirbone/.env` so credentials are configured
//! once and read from any directory. Prints the path and the template to fill in;
//! never overwrites an existing file. No interactive secret prompt, no encryption —
//! a plain 0600 file, like `~/.pi/agent/auth.json`.

use anyhow::{Context, Result};

pub fn run_login() -> Result<()> {
    let info = sirbone::config::ensure_global_env().context("seeding ~/.sirbone/.env")?;
    let p = info.path.display();
    if info.created {
        println!("Created {p} (chmod 600).");
    } else {
        println!("Found existing {p} — leaving it untouched.");
    }
    println!("\nEdit it and fill in ONE provider, then run `sirbone`:\n");
    print!("{}", sirbone::config::ENV_TEMPLATE);
    println!("\nQuick check (offline, no model call): sirbone doctor");
    Ok(())
}
