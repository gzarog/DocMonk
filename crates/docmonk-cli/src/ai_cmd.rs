//! `ask` and `ai providers|status|login|logout|models`.
//!
//! `ask` runs the same retrieval as `explore` and hands the question and
//! that evidence to the configured provider; the answer is returned
//! alongside the evidence it was based on. The `ai` lifecycle commands
//! never take, print or store a credential: sign-in is the runtime's.

use clap::Subcommand;
use docmonk_ai::registry::{self, ProviderCapability};
use docmonk_ai::runtime::resolve_runtime;
use docmonk_core::errors::DocMonkError;
use serde_json::json;

use crate::{load, prepared_home, print_json};
use docmonk_service::ask::ask_value;

pub fn ask(question: &str, json_output: bool) -> Result<(), DocMonkError> {
    let r = ask_value(question)?;
    if json_output {
        return print_json(&r);
    }
    println!("{}", r["answer"]["text"].as_str().unwrap_or_default());
    println!(
        "\nprovider={} model={} evidence={} item(s){}",
        r["answer"]["provider"].as_str().unwrap_or_default(),
        r["answer"]["model"].as_str().unwrap_or_default(),
        r["evidence"].as_array().map_or(0, Vec::len),
        if r["evidence_truncated"] == true {
            " (truncated)"
        } else {
            ""
        }
    );
    Ok(())
}

#[derive(Subcommand)]
pub enum AiCommand {
    /// List every AI provider DocMonk knows about and its capabilities.
    Providers {
        #[arg(long = "json")]
        json: bool,
    },
    /// Show a subscription provider's connection state (no secrets).
    Status {
        /// Subscription provider id, e.g. codex.
        provider: String,
        #[arg(long = "json")]
        json: bool,
    },
    /// Sign in to a subscription provider through its official runtime.
    Login {
        /// Subscription provider id, e.g. codex.
        provider: String,
    },
    /// Sign out of a subscription provider via its official runtime.
    Logout {
        /// Subscription provider id, e.g. codex.
        provider: String,
    },
    /// Analyse one file with the configured documents.analysis provider and
    /// print the result, without indexing it (a preview before switching a
    /// corpus to AI analysis; it sends the file to the provider).
    Analyze {
        /// The file to analyse.
        file: std::path::PathBuf,
        #[arg(long = "json")]
        json: bool,
    },
    /// List the models a signed-in subscription provider offers.
    Models {
        /// Subscription provider id, e.g. codex.
        provider: String,
        #[arg(long = "json")]
        json: bool,
    },
}

fn require_subscription(provider: &str) -> Result<&'static ProviderCapability, DocMonkError> {
    let cap = registry::get(provider).ok_or_else(|| {
        DocMonkError::usage(format!(
            "unknown ai provider '{provider}'; run 'docmonk ai providers' to list them"
        ))
    })?;
    if !cap.subscription {
        return Err(DocMonkError::usage(format!(
            "provider '{provider}' does not use account sign-in (it is an API-key or local \
             provider); this command applies only to subscription providers: {}",
            registry::SUBSCRIPTION_PROVIDERS.join(", ")
        )));
    }
    Ok(cap)
}

/// Refuses to contact a cloud runtime while the privacy flag is off,
/// before any runtime is started.
fn gate_privacy(external_ai_allowed: bool, cap: &ProviderCapability) -> Result<(), DocMonkError> {
    if cap.cloud_egress && !external_ai_allowed {
        return Err(docmonk_ai::errors::policy_blocked(format!(
            "ai provider '{}' contacts a cloud runtime, which requires \
             privacy.external_ai_allowed=true (it defaults to false). Set it with: docmonk config \
             set privacy.external_ai_allowed true",
            cap.provider_id
        )));
    }
    Ok(())
}

pub fn run(cmd: AiCommand) -> Result<(), DocMonkError> {
    let gated = |provider: &str, gate: bool| -> Result<_, DocMonkError> {
        let cap = require_subscription(provider)?;
        let cfg = load(&prepared_home()?)?;
        if gate {
            gate_privacy(cfg.privacy.external_ai_allowed, cap)?;
        }
        Ok((cap, resolve_runtime(cap.provider_id, &cfg.ai)?))
    };
    match cmd {
        AiCommand::Providers { json } => {
            if json {
                return print_json(&json!({"providers": registry::REGISTRY}));
            }
            for cap in registry::REGISTRY {
                let kind = if cap.subscription {
                    "subscription"
                } else if !cap.cloud_egress {
                    "local"
                } else {
                    "api"
                };
                // The " [beta]" suffix is deliberately reduced to its
                // leading space.
                let beta = if cap.beta { " " } else { "" };
                println!("{}{beta} — {}", cap.provider_id, cap.display_name);
                println!(
                    "  kind={kind} auth={} cloud_egress={} models={} usage={} analysis={}",
                    cap.auth_modes.join("/"),
                    cap.cloud_egress,
                    cap.model_discovery,
                    cap.usage_reporting,
                    cap.analysis
                );
                println!("  {}", cap.notes);
            }
        }
        AiCommand::Analyze { file, json } => {
            let cfg = load(&prepared_home()?)?;
            let (extracted_by, r) = docmonk_convert::ai::preview(&cfg, &file)?;
            if json {
                return print_json(&json!({"extracted_by": extracted_by, "analysis": r}));
            }
            println!(
                "{} — {} chunk(s), {} code reference(s) [{extracted_by}]",
                r.title.as_deref().unwrap_or("(untitled)"),
                r.chunks.len(),
                r.code_references.len()
            );
            if let Some(s) = &r.summary {
                println!("  {s}");
            }
            for (i, c) in r.chunks.iter().enumerate() {
                let pages = match (c.page_start, c.page_end) {
                    (Some(a), Some(b)) if a != b => format!(" p.{a}-{b}"),
                    (Some(a), _) => format!(" p.{a}"),
                    _ => String::new(),
                };
                let head: String = c.text.chars().take(80).collect();
                let path = if c.heading_path.is_empty() {
                    String::new()
                } else {
                    format!(" {}", c.heading_path.join(" > "))
                };
                println!(
                    "  {i:>3} {:<8}{pages}{path}: {}",
                    c.kind,
                    head.replace('\n', " ")
                );
            }
        }
        AiCommand::Status { provider, json } => {
            let (_, mut runtime) = gated(&provider, true)?;
            let state = runtime.status()?;
            if json {
                return print_json(&state.to_json());
            }
            println!(
                "provider={} authenticated={} account={} runtime={}",
                state.provider_id,
                state.authenticated,
                state.account.as_deref().unwrap_or("-"),
                state.runtime_version.as_deref().unwrap_or("-")
            );
            if !state.detail.is_empty() {
                println!("{}", state.detail);
            }
        }
        AiCommand::Login { provider } => {
            let (_, mut runtime) = gated(&provider, true)?;
            let state = runtime.login()?;
            println!(
                "Signed in to {} as {}.",
                state.provider_id,
                state.account.as_deref().unwrap_or("the current account")
            );
        }
        AiCommand::Logout { provider } => {
            let (cap, mut runtime) = gated(&provider, false)?;
            runtime.logout()?;
            println!("Signed out of {}.", cap.provider_id);
        }
        AiCommand::Models { provider, json } => {
            let (cap, mut runtime) = gated(&provider, true)?;
            let models = runtime.models()?;
            if json {
                return print_json(&json!({"provider": cap.provider_id, "models": models}));
            }
            if models.is_empty() {
                println!("No models reported for {}.", cap.provider_id);
            }
            for m in models {
                println!("{m}");
            }
        }
    }
    Ok(())
}
