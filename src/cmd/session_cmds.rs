//! `sirbone audit` and the snapshot listing command.

use std::path::PathBuf;

use anyhow::Result;
use sirbone::session;

pub async fn run_audit(path: Option<PathBuf>, json: bool) -> Result<()> {
    let path = match path {
        Some(path) => path,
        None => session::latest_session_path()
            .await
            .ok_or_else(|| anyhow::anyhow!("no previous session found"))?,
    };
    let summary = session::audit(&path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{}", summary.to_markdown());
    }
    Ok(())
}

pub async fn run_snapshots(json: bool) -> Result<()> {
    let Some(snaps) = sirbone::snapshot::workspace_snapshots() else {
        anyhow::bail!("snapshots disabled (SIRBONE_NO_SNAPSHOT)");
    };
    let entries = snaps.list_detailed(20).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("no snapshots yet (one is taken before each run that edits files)");
        return Ok(());
    }
    for (i, entry) in entries.iter().enumerate() {
        println!(
            "{:>2}. {}  {}  — {}",
            i + 1,
            entry.short_id,
            entry.age,
            entry.label
        );
        for file in &entry.changed_files {
            println!("    {file}");
        }
    }
    println!("restore with /rollback <n|id>");
    Ok(())
}
