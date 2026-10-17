//! The `docmonk` command line: argument parsing, rendering and wiring of
//! the service, operations, MCP and Admin UI crates.
//!
//! `--json` output is the envelope `{"schema_version": "1", "data": {...}}`.
//! Errors print `Error: <redacted message>` to stderr and exit with a
//! stable per-kind exit code.

mod ai_cmd;
mod daemon_cmd;
mod doctor_cmd;
mod ops_cmd;
mod query_cmd;
mod status_cmd;
mod update_cmd;
mod workflow;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use docmonk_config::loader::{dump_yaml, get_path, set_path};
use docmonk_config::write_user_config;
use docmonk_core::errors::{DocMonkError, EXIT_GENERIC_FAILURE};
use docmonk_core::version;
use docmonk_telemetry::redact::redact_urls_in_text;

/// The `--json` envelope schema version.
const SCHEMA_VERSION: &str = "1";

#[derive(Parser)]
#[command(
    name = "docmonk",
    about = "DocMonk: local code and document knowledge for people and agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the DocMonk version.
    Version {
        #[arg(long = "json")]
        json: bool,
    },
    /// Inspect and edit configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// OpenSearch/Elasticsearch index schema.
    #[command(subcommand)]
    Server(ServerCommand),
    /// Manage the background indexing daemon.
    #[command(subcommand)]
    Daemon(daemon_cmd::DaemonCommand),
    /// Bootstrap the DocMonk runtime directory.
    Init(workflow::InitArgs),
    /// Manage registered sources.
    #[command(subcommand)]
    Source(workflow::SourceCommand),
    /// Scan sources and process pending files, then build relationship
    /// graphs once every selected source is indexed.
    Index {
        /// Only index this source id.
        #[arg(long = "source")]
        source: Option<String>,
    },
    /// Manage relationship graphs (built after indexing).
    #[command(subcommand)]
    Relationships(RelationshipsCommand),
    /// Show indexing status.
    Status(status_cmd::StatusArgs),
    /// List indexed documents.
    Docs {
        /// Only list this source id.
        #[arg(long = "source")]
        source: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
    /// Run the indexing daemon in the foreground.
    Watch,
    /// Answer a question with the configured AI provider, grounded in
    /// the evidence `explore` retrieves.
    Ask {
        /// Natural-language question to ask.
        question: String,
        #[arg(long = "json")]
        json: bool,
    },
    /// Start the local administration web interface.
    Ui {
        /// Interface to bind. Defaults to localhost. A non-loopback address
        /// is refused unless remote access through an authenticating TLS
        /// reverse proxy is configured (`ui.remote_enabled`).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Do not open a browser automatically.
        #[arg(long = "no-browser")]
        no_browser: bool,
    },
    /// Manage subscription AI providers.
    #[command(subcommand)]
    Ai(ai_cmd::AiCommand),
    /// Serve the indexed knowledge to agents (MCP over stdio).
    Serve {
        /// Run the MCP server over stdio.
        #[arg(long = "mcp")]
        mcp: bool,
    },
    /// Lexical search across code and documents.
    Search(query_cmd::SearchArgs),
    /// Routed, filtered and citation-verified evidence for a question.
    Evidence(query_cmd::EvidenceArgs),
    /// Look up a code symbol by name.
    Symbol {
        /// Symbol name or fully qualified name.
        name: String,
        #[arg(long = "json")]
        json: bool,
    },
    /// Show entities that call the given symbol.
    Callers(query_cmd::GraphArgs),
    /// Show entities the given symbol calls.
    Callees(query_cmd::GraphArgs),
    /// Show all edges touching the given symbol.
    References(query_cmd::GraphArgs),
    /// Show blast-radius impact analysis for a symbol.
    Impact(query_cmd::GraphArgs),
    /// Explore code and documents for a question or identifier.
    Explore {
        /// Natural-language or identifier query.
        query: String,
        #[arg(long = "json")]
        json: bool,
    },
    /// Inspect and manually correct the link graph.
    #[command(subcommand)]
    Link(query_cmd::LinkCommand),
    /// Run health checks.
    Doctor {
        #[arg(long = "json")]
        json: bool,
    },
    /// Show a condensed health summary.
    Health {
        #[arg(long = "json")]
        json: bool,
    },
    /// Create a restorable backup archive of DocMonk's databases.
    Backup {
        /// Output archive path (default: <home>/backups/docmonk-backup-<timestamp>.tar.gz).
        dest: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
    /// Restore DocMonk's databases from a backup archive.
    Restore {
        /// Path to a backup archive created by 'docmonk backup'.
        archive: String,
        #[arg(long = "json")]
        json: bool,
    },
    /// Re-index one or every source's derived knowledge from scratch.
    Rebuild {
        /// Only rebuild this source id.
        #[arg(long = "source")]
        source: Option<String>,
        /// Check every source root first and ask for confirmation.
        #[arg(long)]
        fresh: bool,
        /// Skip the confirmation prompt for --fresh.
        #[arg(long)]
        yes: bool,
        #[arg(long = "json")]
        json: bool,
    },
    /// Check for and install DocMonk updates.
    Update {
        #[command(subcommand)]
        command: Option<update_cmd::UpdateCommand>,
    },
    /// Remove DocMonk's data (and explain how to remove the binary).
    Uninstall {
        /// Remove only the application; keep DOCMONK_HOME.
        #[arg(long = "keep-data")]
        keep_data: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long = "json")]
        json: bool,
    },
    /// Manage the semantic-search ANN index.
    #[command(subcommand)]
    Vectors(ops_cmd::VectorsCommand),
}

#[derive(Subcommand)]
enum RelationshipsCommand {
    /// Build (or retry) relationship graphs from the published index,
    /// without reindexing any content.
    Build {
        /// Only this source id.
        #[arg(long = "source")]
        source: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Print the index schema manifest for the configured engine.
    Schema {
        #[arg(long = "json")]
        json: bool,
    },
    /// Create any missing indexes and verify existing ones (never modifies them).
    Init {
        #[arg(long = "json")]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the effective configuration as YAML.
    Show,
    /// Print one value by dotted key, e.g. runtime.log_level.
    Get { key: String },
    /// Set one value by dotted key and write the user config.
    Set { key: String, value: String },
}

fn version_payload() -> serde_json::Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "data": { "version": version::version(), "runtime": "rust" },
    })
}

pub(crate) use docmonk_service::{load, prepared_home};

fn print_json(data: &impl serde::Serialize) -> Result<(), DocMonkError> {
    let envelope = serde_json::json!({ "schema_version": SCHEMA_VERSION, "data": data });
    let text = serde_json::to_string_pretty(&envelope)
        .map_err(|e| DocMonkError::new(docmonk_core::ErrorKind::Generic, e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn server_config() -> Result<docmonk_config::model::ServerStorageConfig, DocMonkError> {
    let home = prepared_home()?;
    let cfg = load(&home)?;
    if cfg.storage.mode != "server" {
        return Err(DocMonkError::config(
            "storage.mode is not 'server'; configure storage.server first",
        ));
    }
    Ok(cfg.storage.server)
}

fn run_server(cmd: ServerCommand) -> Result<(), DocMonkError> {
    use docmonk_backends::engine::{default_vector_spec, Engine};
    use docmonk_backends::{schema, ServerBackend};
    match cmd {
        ServerCommand::Schema { json } => {
            let home = prepared_home()?;
            let server = load(&home)?.storage.server;
            let engine = if server.engine.as_str() == "elasticsearch" {
                Engine::Elasticsearch
            } else {
                Engine::OpenSearch
            };
            let manifest =
                schema::manifest(&server.index_prefix, engine, Some(&default_vector_spec()));
            if json {
                print_json(&manifest)?;
            } else {
                for idx in manifest["indexes"].as_array().into_iter().flatten() {
                    println!("{}", idx["name"].as_str().unwrap_or_default());
                }
            }
        }
        ServerCommand::Init { json } => {
            let backend = ServerBackend::connect(&server_config()?, Some(default_vector_spec()))?;
            let report = backend.init()?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "{} index(es) created, {} already present and verified (prefix {}).",
                    report.created.len(),
                    report.existing.len(),
                    backend.prefix()
                );
            }
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<(), DocMonkError> {
    match cli.command {
        Command::Version { json } => {
            if json {
                let text = serde_json::to_string_pretty(&version_payload()).map_err(|e| {
                    DocMonkError::new(docmonk_core::ErrorKind::Generic, e.to_string())
                })?;
                println!("{text}");
            } else {
                println!("docmonk {}", version::version());
            }
        }
        Command::Config(ConfigCommand::Show) => {
            let home = prepared_home()?;
            println!("{}", dump_yaml(&load(&home)?));
        }
        Command::Config(ConfigCommand::Get { key }) => {
            let home = prepared_home()?;
            println!("{}", get_path(&load(&home)?, &key)?);
        }
        Command::Config(ConfigCommand::Set { key, value }) => {
            let home = prepared_home()?;
            let (updated, stored) = set_path(&load(&home)?, &key, &value)?;
            let path = write_user_config(&updated, &home)
                .map_err(|e| DocMonkError::new(docmonk_core::ErrorKind::Generic, e.to_string()))?;
            println!("Set {key} = {stored} in {}", path.display());
        }
        Command::Server(cmd) => run_server(cmd)?,
        Command::Daemon(cmd) => daemon_cmd::run(cmd)?,
        Command::Init(a) => workflow::init(&a)?,
        Command::Source(cmd) => workflow::source(cmd)?,
        Command::Index { source } => workflow::index(source)?,
        Command::Relationships(RelationshipsCommand::Build { source, json }) => {
            workflow::relationships_build(source, json)?
        }
        Command::Status(a) => status_cmd::status(&a)?,
        Command::Docs { source, json } => workflow::docs(source, json)?,
        Command::Watch => daemon_cmd::run(daemon_cmd::DaemonCommand::Run)?,
        Command::Serve { mcp } => docmonk_mcp::serve(mcp)?,
        Command::Ask { question, json } => ai_cmd::ask(&question, json)?,
        Command::Ai(cmd) => ai_cmd::run(cmd)?,
        Command::Ui {
            host,
            port,
            no_browser,
        } => docmonk_ui::serve(&host, port, no_browser)?,
        Command::Search(a) => query_cmd::search(&a)?,
        Command::Symbol { name, json } => query_cmd::symbol(&name, json)?,
        Command::Callers(a) => query_cmd::calls(&a, docmonk_retrieval::graph::Direction::Incoming)?,
        Command::Callees(a) => query_cmd::calls(&a, docmonk_retrieval::graph::Direction::Outgoing)?,
        Command::References(a) => query_cmd::references(&a)?,
        Command::Impact(a) => query_cmd::impact(&a)?,
        Command::Explore { query, json } => query_cmd::explore(&query, json)?,
        Command::Evidence(a) => query_cmd::evidence(&a)?,
        Command::Link(cmd) => query_cmd::link(cmd)?,
        Command::Doctor { json } => doctor_cmd::doctor(json)?,
        Command::Health { json } => doctor_cmd::health(json)?,
        Command::Backup { dest, json } => ops_cmd::backup(dest, json)?,
        Command::Restore { archive, json } => ops_cmd::restore(&archive, json)?,
        Command::Rebuild {
            source,
            fresh,
            yes,
            json,
        } => ops_cmd::rebuild(source, fresh, yes, json)?,
        Command::Update { command } => update_cmd::run(command)?,
        Command::Uninstall {
            keep_data,
            yes,
            json,
        } => ops_cmd::uninstall(keep_data, yes, json)?,
        Command::Vectors(cmd) => ops_cmd::vectors(cmd)?,
    }
    Ok(())
}

fn main() -> ExitCode {
    // clap exits with code 2 on usage errors, matching EXIT_INVALID_ARGUMENTS.
    let cli = Cli::parse();
    // Commands that must stay quiet and network-free: the update commands
    // themselves, the MCP/daemon processes, and the ones `update install`
    // runs on a freshly installed binary.
    if std::env::var_os("DOCMONK_NO_UPDATE_CHECK").is_none()
        && !matches!(
            cli.command,
            Command::Update { .. }
                | Command::Version { .. }
                | Command::Serve { .. }
                | Command::Daemon(_)
                | Command::Watch
                | Command::Doctor { .. }
        )
    {
        update_cmd::on_startup();
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // An empty message is a deliberate silent exit (e.g. doctor's
            // UNHEALTHY verdict, already printed).
            if !err.message().is_empty() {
                eprintln!("Error: {}", redact_urls_in_text(err.message()));
            }
            ExitCode::from(if err.exit_code() == 0 {
                EXIT_GENERIC_FAILURE
            } else {
                err.exit_code()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_json_envelope() {
        let value = version_payload();
        assert_eq!(value["schema_version"], "1");
        assert_eq!(value["data"]["version"], version::version());
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
