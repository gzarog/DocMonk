//! Operations: `backup`, `restore`, `rebuild`, `uninstall` and
//! `vectors rebuild|backfill`.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use docmonk_core::errors::{ErrorKind, RagMonkError};
use docmonk_core::paths::Home;
use docmonk_ops::backup::{create_backup, restore_archive};
use docmonk_ops::rebuild::rebuild_sources;
use docmonk_service::sources::control_plane;
use serde_json::json;

use crate::{prepared_home, print_json};

/// Whole-home exclusive locks (index.lock, then every source lock).
fn index_lock(
    home: &Home,
    operation: &str,
) -> Result<docmonk_service::indexing::HomeLocks, RagMonkError> {
    docmonk_service::indexing::home_exclusive(home, operation)
}

pub fn backup(dest: Option<String>, json_output: bool) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    control_plane(&home)?;
    let lock = index_lock(&home, "backup")?;
    let result = create_backup(&home, dest.as_deref().map(Path::new));
    lock.release();
    let (archive, m) = result?;
    let data = json!({
        "archive": archive.to_string_lossy(),
        "ragmonk_version": m["ragmonk_version"],
        "created_at": m["created_at"],
        "control_schema": m["control_schema"],
        "projects": m["projects"],
    });
    if json_output {
        return print_json(&data);
    }
    println!("Backup created {}", archive.display());
    println!(
        "Projects included: {}",
        m["projects"].as_object().map_or(0, serde_json::Map::len)
    );
    Ok(())
}

pub fn restore(archive: &str, json_output: bool) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    let archive = PathBuf::from(archive);
    let report = restore_archive(&home, &archive)?;
    if json_output {
        return print_json(&report);
    }
    let restored = report["projects_restored"].as_array().map_or(0, Vec::len);
    let daemon_was_running = report["daemon_was_running"] == true;
    println!("Restored from {}", archive.display());
    println!("Projects restored: {restored}");
    if daemon_was_running {
        println!(
            "Daemon was running before restore; {}",
            if report["daemon_restarted"] == true {
                "restarted"
            } else {
                "left stopped"
            }
        );
    }
    Ok(())
}

/// Full rebuild of one or every enabled source. A full build is
/// written next to the visible one and published only on success, so a
/// failed rebuild leaves the previous index usable. `--fresh` is
/// therefore always the behavior; the flag adds a root
/// reachability check and confirmation.
pub fn rebuild(
    source_id: Option<String>,
    fresh: bool,
    yes: bool,
    json_output: bool,
) -> Result<(), RagMonkError> {
    if fresh && !yes && !json_output {
        print!(
            "rebuild --fresh will re-index every selected source from its source files \
             (the old index is kept as a backup until the rebuild succeeds). Continue? [y/N]: "
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let ok = std::io::stdin().read_line(&mut line).is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        if !ok {
            return Err(RagMonkError::usage(
                "aborted: rebuild --fresh not confirmed (pass --yes to skip)",
            ));
        }
    }
    let home = prepared_home()?;
    let outcomes = rebuild_sources(&home, source_id.as_deref(), fresh, |_| {})?;
    let failed: i64 = outcomes
        .iter()
        .map(|o| o["failed"].as_i64().unwrap_or(0))
        .sum();
    let failed_sources = outcomes.iter().filter(|o| o.get("error").is_some()).count();
    if json_output {
        print_json(&json!({ "sources": outcomes }))?;
    } else {
        for o in &outcomes {
            let (id, path) = (
                o["id"].as_str().unwrap_or_default(),
                o["path"].as_str().unwrap_or_default(),
            );
            match o["error"].as_str() {
                Some(e) => println!("{id} {path}: rebuild failed: {e}"),
                None => println!(
                    "{id} {path}: rebuilt scanned={} indexed={} failed={} linked={}",
                    o["scanned"], o["indexed"], o["failed"], o["linked"]
                ),
            }
        }
    }
    if failed > 0 || failed_sources > 0 {
        let mut parts = Vec::new();
        if failed_sources > 0 {
            parts.push(format!("{failed_sources} source(s) failed to rebuild"));
        }
        if failed > 0 {
            parts.push(format!("{failed} file(s) failed to (re)index"));
        }
        return Err(RagMonkError::new(
            ErrorKind::IndexingPartialFailure,
            format!("{}; see 'ragmonk doctor' for details", parts.join("; ")),
        ));
    }
    Ok(())
}

/// Purges `RAGMONK_HOME` (stopping a running daemon first) unless
/// `--keep-data`. A standalone Rust binary is never deleted by itself
/// (on Windows a running executable cannot be), so its removal is a
/// manual instruction.
pub fn uninstall(keep_data: bool, yes: bool, json_output: bool) -> Result<(), RagMonkError> {
    let home = Home::discover();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ragmonk".into());
    let manual = format!(
        "this is a standalone RagMonk binary; remove it by deleting {exe} (and any launcher or PATH entry you added)."
    );
    if !yes {
        println!("This will permanently remove:");
        println!("  - the application (see manual instructions below)");
        if !keep_data {
            println!(
                "  - all data under {} (databases, config, backups, logs)",
                home.root().display()
            );
        }
        print!("Proceed? [y/N]: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let ok = std::io::stdin().read_line(&mut line).is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        if !ok {
            println!("Aborted; nothing was removed.");
            return Ok(());
        }
    }
    let data_purged = !keep_data && docmonk_ops::uninstall::purge_data(&home)?;
    if json_output {
        return print_json(&json!({
            "install_method": "standalone_binary",
            "data_purged": data_purged,
            "app_removed": false,
            "manual_instructions": manual,
        }));
    }
    if data_purged {
        println!("✓ Removed data under {}", home.root().display());
    }
    println!("\n! {manual}");
    Ok(())
}

#[derive(Subcommand)]
pub enum VectorsCommand {
    /// Rebuild the ANN index from stored vectors.
    Rebuild {
        /// Only rebuild this source id.
        #[arg(long = "source")]
        source: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
    /// Compute vectors for indexed files that have none yet.
    Backfill {
        /// Only backfill this source id.
        #[arg(long = "source")]
        source: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
}

pub fn vectors(cmd: VectorsCommand) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    match cmd {
        VectorsCommand::Rebuild { source, json: j } => {
            let rebuilt = docmonk_ops::vectors::rebuild(&home, source.as_deref())?;
            if j {
                return print_json(&json!({ "rebuilt": rebuilt }));
            }
            if rebuilt.is_empty() {
                println!("No sources to rebuild.");
            }
            for e in &rebuilt {
                let id = e["source_id"].as_str().unwrap_or_default();
                if e["backend"].is_null() {
                    println!("{id}: no embeddings computed yet.");
                } else {
                    println!(
                        "{id}: rebuilt {} vector(s) via {}",
                        e["vectors"],
                        e["backend"].as_str().unwrap_or_default()
                    );
                }
            }
        }
        VectorsCommand::Backfill { source, json: j } => {
            let done = docmonk_ops::vectors::backfill(&home, source.as_deref())?;
            if j {
                return print_json(&json!({ "backfilled": done }));
            }
            if done.is_empty() {
                println!("No sources to backfill.");
            }
            for e in &done {
                let id = e["source_id"].as_str().unwrap_or_default();
                if e["embedded"].as_u64().unwrap_or(0) > 0 {
                    println!("{id}: embedded {} vector(s)", e["embedded"]);
                } else {
                    println!("{id}: nothing to backfill.");
                }
            }
        }
    }
    Ok(())
}
