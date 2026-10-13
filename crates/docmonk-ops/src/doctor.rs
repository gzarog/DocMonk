//! Health checks (`doctor` / `health`): database, sources, locks, queue,
//! disk, AI provider, tokenizer and (in server mode) the cluster.

use std::path::Path;

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::lock::{inspect_lock, LockState};
use docmonk_indexing::scan::check_root_accessible;
use docmonk_service::load;
use docmonk_service::sources::control_plane;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::maintenance;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

const LOW_DISK_WARN_GB: f64 = 1.0;

const QUEUE_DEPTH_WARN: i64 = 1000;

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    /// `ok`, `warn` or `fail`.
    pub status: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: &'static str,
    pub checks: Vec<Check>,
}

pub fn check(name: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

/// `UNHEALTHY` on any fail, `HEALTHY WITH WARNINGS` on any warn.
pub fn overall(sections: &[Section]) -> &'static str {
    let any = |s: &str| {
        sections
            .iter()
            .flat_map(|x| &x.checks)
            .any(|c| c.status == s)
    };
    if any("fail") {
        "UNHEALTHY"
    } else if any("warn") {
        "HEALTHY WITH WARNINGS"
    } else {
        "HEALTHY"
    }
}

fn free_gb(path: &Path) -> Option<f64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let path = docmonk_core::paths::resolve(path).ok()?;
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space() as f64 / 1024f64.powi(3))
}

pub fn run_checks(home: &Home) -> Result<Vec<Section>, DocMonkError> {
    let cfg = load(home)?;
    if cfg.storage.mode == "server" {
        return server_checks(home);
    }
    let cp = control_plane(home)?;
    let layout = StorageLayout::new(home);
    let db =
        |e: docmonk_storage::StorageError| DocMonkError::new(ErrorKind::Database, e.to_string());
    let mut sections = vec![Section {
        name: "Core",
        checks: vec![check(
            "version",
            "ok",
            format!("version {}", docmonk_core::version::version()),
        )],
    }];

    let journal = maintenance::journal_mode(cp.connection());
    let schema = maintenance::control_schema_state(&layout.control_db());
    sections.push(Section {
        name: "Database",
        checks: vec![
            check("sqlite", "ok", "SQLite"),
            check(
                "wal",
                if journal == "wal" { "ok" } else { "warn" },
                format!("WAL ({journal})"),
            ),
            check(
                "schema",
                if schema == maintenance::SchemaState::Current {
                    "ok"
                } else {
                    "fail"
                },
                match &schema {
                    maintenance::SchemaState::Incompatible(d) => {
                        format!("schema incompatible: {d}")
                    }
                    _ => format!("schema {}", docmonk_storage::schema::control_fingerprint()),
                },
            ),
        ],
    });

    let sources = cp.list_sources(true).map_err(db)?;
    let mut unreachable = 0;
    for s in &sources {
        let offline = cp.state(&s.id).map_err(db)?.online_status == "offline";
        if offline || check_root_accessible(Path::new(&s.path)).is_some() {
            unreachable += 1;
        }
    }
    sections.push(Section {
        name: "Sources",
        checks: vec![check(
            "reachability",
            if unreachable > 0 { "warn" } else { "ok" },
            format!(
                "{}/{} reachable",
                sources.len() - unreachable,
                sources.len()
            ),
        )],
    });

    let lock = match inspect_lock(&home.locks_dir().join("index.lock")) {
        LockState::Free => check("index_lock", "ok", "free"),
        LockState::Unknown => check("index_lock", "warn", "unknown"),
        LockState::Held(owner) => {
            let mut parts = vec!["held".to_owned()];
            if let Some(o) = owner {
                if let Some(p) = o.pid {
                    parts.push(format!("PID {p}"));
                }
                if let Some(op) = o.operation {
                    parts.push(format!("operation {op}"));
                }
                if let Some(sid) = o.source_id {
                    parts.push(format!("source {sid}"));
                }
            }
            check("index_lock", "warn", parts.join(", "))
        }
    };
    sections.push(Section {
        name: "Index Lock",
        checks: vec![lock],
    });

    // Files of the published builds waiting for a retry (the durable
    // per-file retry queue), from the canonical status report.
    let status = docmonk_service::status::report(home)?;
    let retrying = status.summary.files.retrying;
    sections.push(Section {
        name: "Index",
        checks: vec![match retrying {
            None => check("queue", "warn", "retry queue unknown (status is partial)"),
            Some(0) => check("queue", "ok", "queue empty"),
            Some(n) => check(
                "queue",
                if (n as i64) < QUEUE_DEPTH_WARN {
                    "ok"
                } else {
                    "warn"
                },
                format!("{n} file(s) waiting for a retry"),
            ),
        }],
    });

    let disk = match free_gb(home.root()) {
        Some(gb) => check(
            "free_space",
            if gb >= LOW_DISK_WARN_GB { "ok" } else { "warn" },
            format!("{gb:.0} GB free"),
        ),
        None => check("free_space", "warn", "free space unknown"),
    };
    sections.push(Section {
        name: "Disk",
        checks: vec![disk],
    });

    // Every enabled source's active build.
    let mut stores = Vec::new();
    for s in &sources {
        if let Some(build) = cp.state(&s.id).map_err(db)?.active_build_id {
            let pid = project_id_for_canonical(&s.path);
            let store = ProjectStore::open(&layout, &pid, &s.id, cfg.runtime.sqlite_cache_size_mb)
                .map_err(db)?;
            stores.push((pid, store, build));
        }
    }

    if cfg.search.semantic {
        let fp = docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL.fingerprint();
        let mut vectors = 0usize;
        let mut bytes = 0u64;
        for (pid, store, build) in &stores {
            vectors += store.embedding_keys(build, &fp).map_err(db)?.len();
            let index = docmonk_ml::ann::index_path(&layout.project_dir(pid));
            bytes += std::fs::metadata(index).map(|m| m.len()).unwrap_or(0);
        }
        sections.push(Section {
            name: "Semantic",
            checks: vec![check(
                "ann_backend",
                "ok",
                format!(
                    "backend hnsw, {vectors} vector(s), {:.1} MB index",
                    bytes as f64 / (1024.0 * 1024.0)
                ),
            )],
        });
    }

    sections.push(tokenizer_section(&cfg, &stores)?);
    sections.push(ai_section(&cfg));

    if cfg.storage.mode == "server" {
        sections.push(server_section(&cfg));
    }
    Ok(sections)
}

/// The configured AI provider and privacy posture; offline and
/// secret-free, never more than a warning (`_ai_section`).
fn ai_section(cfg: &docmonk_config::DocMonkConfig) -> Section {
    let provider = &cfg.ai.provider;
    let allowed = cfg.privacy.external_ai_allowed;
    let mut checks = vec![check(
        "provider",
        "ok",
        format!("provider={provider}, external_ai_allowed={allowed}"),
    )];
    if let Some(cap) = docmonk_ai::registry::get(provider).filter(|c| c.subscription) {
        if cap.cloud_egress && !allowed {
            checks.push(check(
                "privacy",
                "warn",
                format!(
                    "{provider} is a cloud provider but privacy.external_ai_allowed=false; set it \
                     true to use it"
                ),
            ));
        }
        let found = |exe| docmonk_ai::runtime::which(exe).is_some();
        checks.push(match cap.provider_id {
            "codex" if found("codex") => check("runtime", "ok", "codex runtime found on PATH"),
            "codex" => check(
                "runtime",
                "warn",
                "codex runtime not found on PATH; run 'docmonk ai login codex' setup",
            ),
            // The Rust adapter drives the Copilot CLI (ADR 0026).
            _ if found("copilot") => check("runtime", "ok", "Copilot CLI found on PATH"),
            _ => check(
                "runtime",
                "warn",
                "Copilot CLI not found on PATH; install it and sign in with `copilot`",
            ),
        });
    }
    Section { name: "AI", checks }
}

fn tokenizer_section(
    cfg: &docmonk_config::DocMonkConfig,
    stores: &[(String, ProjectStore, String)],
) -> Result<Section, DocMonkError> {
    use docmonk_documents::tokenizer as t;
    let fp = t::tokenizer_fingerprint();
    let mut checks = vec![
        check("model", "ok", t::EMBEDDING_MODEL_ID),
        check(
            "revision",
            "ok",
            format!("revision {}", &t::TOKENIZER_REVISION[..12]),
        ),
        check("fingerprint", "ok", fp.chars().take(19).collect::<String>()),
        check(
            "limits",
            "ok",
            format!(
                "model max {} tokens, chunk ceiling {}",
                t::MAX_SEQUENCE_TOKENS,
                cfg.documents.chunking.resolved_max_tokens()
            ),
        ),
    ];
    let tok = match t::model_tokenizer() {
        Ok(tok) => tok,
        Err(e) => {
            checks.push(check(
                "payloads",
                "fail",
                format!("tokenizer unavailable: {e}"),
            ));
            return Ok(Section {
                name: "Tokenizer",
                checks,
            });
        }
    };
    let (mut scanned, mut max, mut truncated) = (0usize, 0i64, 0usize);
    let mut seen = std::collections::HashSet::new();
    for (pid, store, build) in stores {
        if !seen.insert(pid.clone()) {
            continue;
        }
        let mut stmt = store
            .connection()
            .prepare(
                "SELECT embedding_text FROM chunks
                 WHERE build_id = ?1 AND embedding_text IS NOT NULL AND embedding_text <> ''",
            )
            .map_err(|e| DocMonkError::new(ErrorKind::Database, e.to_string()))?;
        let texts = stmt
            .query_map([build], |r| r.get::<_, String>(0))
            .map_err(|e| DocMonkError::new(ErrorKind::Database, e.to_string()))?;
        for text in texts.flatten() {
            let n = tok.count(&text, true);
            scanned += 1;
            max = max.max(n);
            if n > t::MAX_SEQUENCE_TOKENS {
                truncated += 1;
            }
        }
    }
    checks.push(if scanned == 0 {
        check("payloads", "ok", "no embedding payloads indexed yet")
    } else {
        check(
            "payloads",
            if truncated > 0 { "fail" } else { "ok" },
            format!(
                "{scanned} scanned, max {max}/{} tokens, truncated {truncated}",
                t::MAX_SEQUENCE_TOKENS
            ),
        )
    });
    Ok(Section {
        name: "Tokenizer",
        checks,
    })
}

fn server_section(cfg: &docmonk_config::DocMonkConfig) -> Section {
    let s = &cfg.storage.server;
    let mut checks = vec![check(
        "engine",
        "ok",
        format!(
            "engine={}, endpoint={}, index_prefix={}",
            s.engine.as_str(),
            docmonk_telemetry::redact::redact_urls_in_text(&s.url),
            s.index_prefix
        ),
    )];
    let redact =
        |e: &dyn std::fmt::Display| docmonk_telemetry::redact::redact_urls_in_text(&e.to_string());
    match docmonk_backends::ServerBackend::connect(s, None) {
        Err(e) => {
            checks.push(check(
                "connectivity",
                "fail",
                format!("server unreachable: {}", redact(&e)),
            ));
            checks.push(check("indices", "fail", "skipped -- server unreachable"));
        }
        Ok(backend) => {
            checks.push(check(
                "connectivity",
                "ok",
                format!("reachable ({})", backend.engine().as_str()),
            ));
            match backend.index_status() {
                Ok(statuses) => {
                    let missing: Vec<&str> = statuses
                        .iter()
                        .filter(|(_, exists)| !exists)
                        .map(|(n, _)| n.as_str())
                        .collect();
                    let n = statuses.len();
                    checks.push(if missing.is_empty() {
                        check("indices", "ok", format!("{n}/{n} indices present"))
                    } else {
                        check(
                            "indices",
                            "warn",
                            format!(
                                "{}/{n} indices present; missing: {} (run 'docmonk server init' to create)",
                                n - missing.len(),
                                missing.join(", ")
                            ),
                        )
                    });
                }
                Err(e) => checks.push(check(
                    "indices",
                    "fail",
                    format!("could not check indices: {}", redact(&e)),
                )),
            }
        }
    }
    Section {
        name: "Server",
        checks,
    }
}

pub fn sections_json(sections: &[Section], result: &str) -> Value {
    json!({
        "result": result,
        "sections": sections.iter().map(|s| json!({
            "name": s.name,
            "checks": s.checks.iter().map(|c| json!({
                "name": c.name, "status": c.status, "detail": c.detail,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// Unhealthy: exit with the health-check code and no extra error line.
pub fn verdict(result: &str) -> Result<(), DocMonkError> {
    if result == "UNHEALTHY" {
        return Err(DocMonkError::new(ErrorKind::HealthCheck, ""));
    }
    Ok(())
}

/// Server-mode checks: the cluster is the authoritative store, so its
/// reachability, schema and per-source state are what is checked; no
/// local database is opened.
fn server_checks(home: &Home) -> Result<Vec<Section>, DocMonkError> {
    let mut sections = vec![Section {
        name: "Core",
        checks: vec![check(
            "version",
            "ok",
            format!("version {}", docmonk_core::version::version()),
        )],
    }];
    let cfg = load(home)?;
    let backend = match docmonk_service::backend::connect(&cfg) {
        Ok(b) => b,
        Err(e) => {
            sections.push(Section {
                name: "Server backend",
                checks: vec![check("reachable", "fail", e.message().to_owned())],
            });
            return Ok(sections);
        }
    };
    let mut checks = vec![check(
        "reachable",
        "ok",
        format!("{} at the configured URL", backend.engine().as_str()),
    )];
    let health = backend
        .cluster_health()
        .unwrap_or_else(|_| "unknown".into());
    checks.push(check(
        "cluster_health",
        match health.as_str() {
            "green" | "yellow" => "ok",
            _ => "fail",
        },
        health,
    ));
    let missing: Vec<String> = backend
        .index_status()
        .map(|v| v.into_iter().filter(|(_, e)| !e).map(|(n, _)| n).collect())
        .unwrap_or_default();
    checks.push(match (missing.is_empty(), backend.verify_schema()) {
        (false, _) => check(
            "schema",
            "fail",
            format!(
                "missing indexes: {}; run `docmonk server init`",
                missing.join(", ")
            ),
        ),
        (true, Ok(())) => check(
            "schema",
            "ok",
            format!(
                "schema version {}",
                docmonk_backends::schema::SCHEMA_VERSION
            ),
        ),
        (true, Err(e)) => check("schema", "fail", e.to_string()),
    });
    sections.push(Section {
        name: "Server backend",
        checks,
    });
    if missing.is_empty() {
        let sources = backend.list_catalog(true).unwrap_or_default();
        let mut unreachable = 0;
        for s in &sources {
            let offline = backend
                .source_state(&s.source_id)
                .ok()
                .flatten()
                .and_then(|st| st.field("online_status").map(|v| v == "offline"))
                .unwrap_or(false);
            if offline || check_root_accessible(Path::new(&s.path)).is_some() {
                unreachable += 1;
            }
        }
        sections.push(Section {
            name: "Sources",
            checks: vec![check(
                "reachability",
                if unreachable > 0 { "warn" } else { "ok" },
                format!(
                    "{}/{} reachable",
                    sources.len() - unreachable,
                    sources.len()
                ),
            )],
        });
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_rules() {
        let s = |st: &'static str| Section {
            name: "x",
            checks: vec![check("c", st, "")],
        };
        assert_eq!(overall(&[s("ok")]), "HEALTHY");
        assert_eq!(overall(&[s("ok"), s("warn")]), "HEALTHY WITH WARNINGS");
        assert_eq!(overall(&[s("warn"), s("fail")]), "UNHEALTHY");
    }
}
