//! `docmonk update [check|status|install|rollback]`,
//! plus the throttled background check and the startup notice every
//! other command runs. `update` alone is `update check`.

use clap::Subcommand;
use docmonk_core::{DocMonkError, ErrorKind};
use docmonk_update::release::Channel;
use docmonk_update::{cache, check, install, versioning};

use crate::{load, prepared_home, print_json};

#[derive(Subcommand, Debug)]
pub enum UpdateCommand {
    /// Query GitHub for the latest release and refresh the local cache.
    Check {
        #[arg(long = "json")]
        json: bool,
    },
    /// Show the cached update status (run `update check` to refresh it).
    Status {
        #[arg(long = "json")]
        json: bool,
    },
    /// Check for, and install, the latest release.
    Install {
        #[arg(long = "json")]
        json: bool,
    },
    /// Switch back to the previously installed version.
    Rollback {
        #[arg(long = "json")]
        json: bool,
    },
    /// The detached background check (internal).
    #[command(name = "background-check", hide = true)]
    BackgroundCheck,
}

fn installed() -> &'static str {
    docmonk_core::version::version()
}

/// `updates.channel` from the home's config (stable when unreadable).
fn channel(home: &docmonk_core::paths::Home) -> Channel {
    load(home)
        .map(|cfg| Channel::from_config(&cfg.updates.channel))
        .unwrap_or(Channel::Stable)
}

fn err(message: String) -> DocMonkError {
    DocMonkError::new(ErrorKind::Generic, message)
}

pub fn run(cmd: Option<UpdateCommand>) -> Result<(), DocMonkError> {
    match cmd.unwrap_or(UpdateCommand::Check { json: false }) {
        UpdateCommand::Check { json } => check_cmd(json),
        UpdateCommand::Status { json } => status(json),
        UpdateCommand::Install { json } => install_cmd(json),
        UpdateCommand::Rollback { json } => rollback(json),
        UpdateCommand::BackgroundCheck => {
            // Never recreates a home that went away while it ran.
            let home = docmonk_core::paths::Home::discover();
            if home.root().is_dir() {
                let _ = check::check_now(home.root(), installed(), channel(&home));
            }
            Ok(())
        }
    }
}

fn check_cmd(json: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let installed = installed();
    let latest = check::check_now(home.root(), installed, channel(&home))
        .map_err(|e| err(format!("could not check for updates: {e}")))?;
    let available = versioning::is_newer(&latest.latest_version, installed);
    if json {
        return print_json(&serde_json::json!({
            "installed_version": installed,
            "latest_version": latest.latest_version,
            "update_available": available,
            "release_url": latest.release_url,
        }));
    }
    println!("Installed: {installed}");
    println!("Latest:    {}", latest.latest_version);
    println!();
    if available {
        println!("Update available. Run:");
        println!();
        println!("    docmonk update install");
    } else {
        println!("DocMonk {installed} is up to date.");
    }
    Ok(())
}

fn status(json: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let installed = installed();
    let Some(cached) = cache::read(home.root()) else {
        if json {
            return print_json(&serde_json::json!({
                "installed_version": installed,
                "latest_version": null,
                "status": "unknown",
                "last_checked": null,
            }));
        }
        println!("Installed: {installed}");
        println!("Latest:    unknown -- run `docmonk update check`");
        return Ok(());
    };
    let label = if versioning::is_newer(&cached.latest_version, installed) {
        "update available"
    } else {
        "up to date"
    };
    if json {
        return print_json(&serde_json::json!({
            "installed_version": installed,
            "latest_version": cached.latest_version,
            "status": label,
            "last_checked": cached.last_checked,
            "release_url": cached.release_url,
        }));
    }
    println!("Installed: {installed}");
    println!("Latest:    {}", cached.latest_version);
    println!("Status:    {label}");
    Ok(())
}

fn install_cmd(json: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let out = install::install_latest(home.root(), installed(), channel(&home)).map_err(err)?;
    if json {
        print_json(&serde_json::json!({
            "installed_version": out.installed_version,
            "upgraded": out.upgraded,
            "healthy": out.healthy,
        }))?;
    } else if !out.upgraded {
        println!("DocMonk {} is already up to date.", out.installed_version);
    } else {
        println!("✓ Installed DocMonk {}", out.installed_version);
        if out.healthy {
            println!("✓ Health check passed");
        } else {
            println!("! Health check reported issues -- see `docmonk doctor`");
        }
        println!("\nDocMonk {} is ready.", out.installed_version);
    }
    if out.upgraded && !out.healthy {
        return Err(DocMonkError::new(ErrorKind::HealthCheck, String::new()));
    }
    Ok(())
}

fn rollback(json: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let (from, to) = install::rollback(home.root()).map_err(err)?;
    if json {
        return print_json(&serde_json::json!({ "from_version": from, "installed_version": to }));
    }
    println!("✓ Rolled back DocMonk {from} → {to}");
    Ok(())
}

/// Before a command: spawn the detached background check when the cache
/// is stale, and print a pending notice to stderr. Never fails.
pub fn on_startup() {
    let home = docmonk_core::paths::Home::discover();
    if !home.root().is_dir() {
        return;
    }
    let Ok(cfg) = load(&home) else { return };
    let u = &cfg.updates;
    if let Some(text) = check::take_notice(home.root(), installed(), u.enabled, u.notify) {
        eprintln!("{text}");
    }
    // Debug builds (tests, development) only check against the test
    // stand-in, never the real GitHub.
    let allowed = !cfg!(debug_assertions) || docmonk_update::release::test_base().is_some();
    if allowed && check::background_due(home.root(), u.enabled, u.check_interval_hours) {
        spawn_background();
    }
}

fn spawn_background() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["update", "background-check"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    let _ = cmd.spawn();
}
