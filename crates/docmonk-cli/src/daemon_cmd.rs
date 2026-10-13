//! `ragmonk daemon start|stop|restart|status|run`.

use clap::Subcommand;
use docmonk_core::errors::RagMonkError;
use docmonk_core::paths::Home;
use docmonk_indexing::daemon::format_uptime;
use docmonk_service::daemon::{self, Started, Stopped};

use crate::{prepared_home, print_json};

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon in the background.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Stop, then start.
    Restart,
    /// Report on the daemon from its PID and health files.
    Status {
        #[arg(long = "json")]
        json: bool,
    },
    /// Run the daemon in the foreground (what `start` spawns).
    Run,
}

pub fn run(cmd: DaemonCommand) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    match cmd {
        DaemonCommand::Start => start(&home),
        DaemonCommand::Stop => stop(&home),
        DaemonCommand::Restart => {
            stop(&home)?;
            start(&home)
        }
        DaemonCommand::Status { json } => status(&home, json),
        DaemonCommand::Run => daemon::run_foreground(&home, |me| {
            println!("Daemon running (pid {me}); stop with: ragmonk daemon stop");
        }),
    }
}

fn start(home: &Home) -> Result<(), RagMonkError> {
    match daemon::start(home)? {
        Started::AlreadyRunning(pid) => println!("Daemon already running (pid {pid})"),
        Started::Started(pid) => {
            println!("Daemon started (pid {pid})");
            println!("  Monitor via: ragmonk daemon status");
        }
    }
    Ok(())
}

fn stop(home: &Home) -> Result<(), RagMonkError> {
    match daemon::stop(home)? {
        Stopped::NotRunning => println!("Daemon is not running"),
        Stopped::Stopped(pid) => println!("Daemon stopped (pid {pid})"),
    }
    Ok(())
}

fn status(home: &Home, json: bool) -> Result<(), RagMonkError> {
    let data = docmonk_indexing::daemon::status_payload(home);
    if json {
        return print_json(&data);
    }
    if data["running"].as_bool() == Some(true) {
        let uptime = data["uptime_seconds"].as_f64().unwrap_or(0.0);
        let uptime = if uptime == 0.0 {
            "0s".to_owned()
        } else {
            format_uptime(uptime)
        };
        println!("RUNNING pid={} uptime={uptime}", data["pid"]);
    } else {
        println!("STOPPED");
    }
    println!(
        "Last reconciliation: {}",
        data["last_reconciliation_at"].as_str().unwrap_or("-")
    );
    for s in data["sources"].as_array().into_iter().flatten() {
        let state = if s["online"].as_bool() == Some(true) {
            "online"
        } else {
            "OFFLINE"
        };
        println!(
            "  {} ({}) {state} last_pass={}",
            s["source_id"].as_str().unwrap_or_default(),
            s["source_type"].as_str().unwrap_or_default(),
            s["last_pass_at"].as_str().unwrap_or("-")
        );
    }
    Ok(())
}
