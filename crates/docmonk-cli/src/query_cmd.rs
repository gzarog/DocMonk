//! Query commands: `search`, `symbol`, `callers`, `callees`,
//! `references`, `impact`, `explore` and `link …`.

use std::path::Path;

use clap::Subcommand;
use docmonk_core::errors::DocMonkError;
use docmonk_retrieval::graph::{self, Direction, SourceMatch, TraversalEdge};
use docmonk_retrieval::lexical::{self, SearchResult};
use docmonk_retrieval::{classify, explore};
use docmonk_service::query::{
    config_budget, corpora, edge_json, explore_value_with, impact_value, link_add, link_list,
    link_remove, match_json, open_sources, run_search, search_err, semantic_json, symbol_value,
    tokenizer_json, ExploreRequest, LinkAdded, Opened, SearchRequest,
};
use serde_json::{json, Value};

use crate::workflow::print_table;
use crate::{load, prepared_home, print_json};

fn no_matches(name: &str) {
    println!("No entities found matching '{name}'.");
}

fn print_matches(matches: &[SourceMatch]) {
    let rows: Vec<Vec<String>> = matches
        .iter()
        .map(|m| {
            vec![
                m.entity.kind.clone(),
                m.entity.qualified_name.clone(),
                m.entity.language.clone(),
                format!("{}:{}", m.entity.file_id, m.entity.start_line),
                m.source_id.clone(),
            ]
        })
        .collect();
    print_table(
        &["Kind", "Qualified name", "Language", "Location", "Source"],
        &rows,
    );
}

fn print_edges(edges: &[TraversalEdge]) {
    let rows: Vec<Vec<String>> = edges
        .iter()
        .map(|e| {
            let r = &e.relationship;
            vec![
                e.depth.to_string(),
                r.relationship_type.clone(),
                r.confidence.clone(),
                r.target_entity_id
                    .clone()
                    .unwrap_or_else(|| format!("~{}", r.target_symbol.clone().unwrap_or_default())),
                r.source_location.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print_table(
        &["Depth", "Type", "Confidence", "Target", "Location"],
        &rows,
    );
}

#[derive(clap::Args)]
pub struct GraphArgs {
    /// Symbol name or fully qualified name.
    name: String,
    #[arg(long = "max-depth", default_value_t = graph::DEFAULT_MAX_DEPTH,
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=10))]
    max_depth: usize,
    #[arg(long, default_value_t = graph::DEFAULT_LIMIT,
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=1000))]
    limit: usize,
    #[arg(long = "json")]
    json: bool,
}

pub fn symbol(name: &str, json_output: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let opened = open_sources(&home, None)?;
    if json_output {
        return print_json(&symbol_value(&opened, name)?);
    }
    let matches = graph::find_symbol_matches(&corpora(&opened), name).map_err(search_err)?;
    if matches.is_empty() {
        no_matches(name);
        return Ok(());
    }
    print_matches(&matches);
    Ok(())
}

/// `callers` (incoming CALLS) and `callees` (outgoing CALLS).
pub fn calls(a: &GraphArgs, direction: Direction) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let opened = open_sources(&home, None)?;
    let (matches, edges) = graph::traverse_symbol(
        &corpora(&opened),
        &a.name,
        direction,
        &graph::CALL_TYPES,
        a.max_depth,
        a.limit,
    )
    .map_err(search_err)?;
    graph_output(
        a,
        &opened,
        &matches,
        &edges,
        matches.is_empty() && edges.is_empty(),
    )
}

pub fn references(a: &GraphArgs) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let opened = open_sources(&home, None)?;
    let (matches, edges) =
        graph::references(&corpora(&opened), &a.name, a.max_depth, a.limit).map_err(search_err)?;
    graph_output(a, &opened, &matches, &edges, matches.is_empty())
}

fn graph_output(
    a: &GraphArgs,
    opened: &[Opened],
    matches: &[SourceMatch],
    edges: &[TraversalEdge],
    nothing: bool,
) -> Result<(), DocMonkError> {
    if a.json {
        return print_json(&json!({
            "query": a.name,
            "matches": matches.iter().map(|m| match_json(m, opened)).collect::<Vec<_>>(),
            "edges": edges.iter().map(|e| edge_json(e, opened)).collect::<Vec<_>>(),
        }));
    }
    if nothing {
        no_matches(&a.name);
        return Ok(());
    }
    print_edges(edges);
    Ok(())
}

fn format_location(d: &Value) -> String {
    let path = d["path"].as_str().unwrap_or_default();
    let loc = &d["location"];
    if let Some(s) = loc["section"].as_str().filter(|s| !s.is_empty()) {
        return format!("{path} §{s}");
    }
    if !loc["page"].is_null() {
        return format!("{path} p{}", loc["page"]);
    }
    path.to_owned()
}

pub fn impact(a: &GraphArgs) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let opened = open_sources(&home, None)?;
    let payload = impact_value(&opened, &a.name, a.max_depth, a.limit)?;
    if a.json {
        return print_json(&payload);
    }
    if payload["found"] != true {
        no_matches(&a.name);
        return Ok(());
    }
    let list = |k: &str| -> String {
        let v: Vec<&str> = payload[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if v.is_empty() {
            "-".into()
        } else {
            v.join(", ")
        }
    };
    println!("{}", a.name);
    for d in payload["defined"].as_array().into_iter().flatten() {
        println!(
            "Defined: {}:{}",
            d["path"].as_str().unwrap_or_default(),
            d["start_line"]
        );
    }
    println!("Callers: {}", list("callers"));
    println!("Callees: {}", list("callees"));
    if payload.get("messaging").is_some() {
        println!("Publishers: {}", list("publishers"));
        println!("Consumers: {}", list("consumers"));
    }
    println!("Tests: {}", list("tests"));
    let docs: Vec<String> = payload["documentation"]
        .as_array()
        .into_iter()
        .flatten()
        .map(format_location)
        .collect();
    println!(
        "Documentation: {}",
        if docs.is_empty() {
            "-".into()
        } else {
            docs.join(", ")
        }
    );
    let c = &payload["confidence"];
    println!(
        "Confidence: Code references: {} / Document links: {}",
        c["code_references"].as_str().unwrap_or("none"),
        c["document_links"].as_str().unwrap_or("none")
    );
    println!(
        "Blast radius: {}",
        payload["blast_radius"].as_str().unwrap_or_default()
    );
    Ok(())
}

// --------------------------------------------------------- semantic ---

// ---------------------------------------------------------- explore ---

pub fn explore(query: &str, json_output: bool, explain: bool) -> Result<(), DocMonkError> {
    let started = std::time::Instant::now();
    let home = prepared_home()?;
    let cfg = load(&home)?;
    let config_ms = started.elapsed().as_secs_f64() * 1000.0;
    let t = std::time::Instant::now();
    let opened = open_sources(&home, None)?;
    let open_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut r = explore_value_with(
        &home,
        &cfg,
        &opened,
        &ExploreRequest {
            query,
            budget: config_budget(&cfg),
            explain,
        },
    )?;
    if explain {
        // One-shot CLI costs, reported apart from the in-process stages
        // (a warm MCP server pays neither).
        r["diagnostics"]["cli"] = json!({
            "config_load_ms": config_ms,
            "open_sources_ms": open_ms,
            "process_total_ms": started.elapsed().as_secs_f64() * 1000.0,
        });
    }
    if json_output {
        let t = std::time::Instant::now();
        let text = serde_json::to_string_pretty(&r).unwrap_or_default();
        if explain {
            eprintln!(
                "explain: serialization {:.3} ms",
                t.elapsed().as_secs_f64() * 1000.0
            );
        }
        println!("{text}");
        return Ok(());
    }
    print_explore(&r);
    if explain {
        print_explore_diagnostics(&r["diagnostics"]);
    }
    Ok(())
}

fn print_explore_diagnostics(d: &Value) {
    println!("Diagnostics");
    if let Some(route) = d.get("route") {
        println!(
            "  Route: {} (confidence {}), signals: {}",
            route["intent"].as_str().unwrap_or_default(),
            route["confidence"],
            route["signals"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
    }
    for key in ["stages", "service_stages"] {
        for st in d[key].as_array().into_iter().flatten() {
            println!(
                "  {:<22} {:>10.3} ms  {}",
                st["stage"].as_str().unwrap_or_default(),
                st["duration_ms"].as_f64().unwrap_or(0.0),
                st.get("hits")
                    .map(|h| format!("{h} hit(s)"))
                    .unwrap_or_default()
            );
        }
    }
    println!(
        "  Sources: considered {}, searched {}, expanded {}",
        d["source_count_considered"], d["source_count_searched"], d["source_count_expanded"]
    );
    println!(
        "  Backend: {} requests {}",
        d["backend"]["kind"].as_str().unwrap_or("?"),
        d["backend"]["requests"]
    );
}

fn print_explore(r: &Value) {
    let strs = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
            .collect()
    };
    let line = |label: &str, items: Vec<String>| {
        println!(
            "{label}: {}",
            if items.is_empty() {
                "-".into()
            } else {
                items.join(", ")
            }
        );
    };
    let arr = |k: &str| r[k].as_array().cloned().unwrap_or_default();
    println!("{}", r["summary"].as_str().unwrap_or_default());
    println!(
        "Intent: {}  Strategies: {}",
        r["intent"].as_str().unwrap_or_default(),
        strs(&r["strategies"]).join(", ")
    );
    line(
        "Relevant symbols",
        arr("symbols")
            .iter()
            .map(|s| s["qualified_name"].as_str().unwrap_or_default().to_owned())
            .collect(),
    );
    line("Relevant paths", strs(&r["paths"]));
    line(
        "Call flows",
        arr("call_flows")
            .iter()
            .map(|p| {
                format!(
                    "{} -[{}]-> {}",
                    p["source"].as_str().unwrap_or_default(),
                    p["relationship"].as_str().unwrap_or_default(),
                    p["target"].as_str().unwrap_or_default()
                )
            })
            .collect(),
    );
    line(
        "Dependencies",
        arr("dependencies")
            .iter()
            .map(|d| {
                format!(
                    "{} ({})",
                    d["qualified_name"].as_str().unwrap_or_default(),
                    d["relationship"].as_str().unwrap_or_default()
                )
            })
            .collect(),
    );
    line(
        "Documents",
        arr("documents")
            .iter()
            .map(|d| {
                d["title"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .or(d["path"].as_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect(),
    );
    line("Tests", strs(&r["tests"]));
    line("Requirements", strs(&r["requirements"]));
    line("Incidents", strs(&r["incidents"]));
    let sem = arr("semantic_results");
    if !sem.is_empty() {
        line(
            "Semantic matches",
            sem.iter()
                .map(|h| {
                    format!(
                        "{} ({:.3})",
                        h["title"].as_str().unwrap_or_default(),
                        h["score"].as_f64().unwrap_or(0.0)
                    )
                })
                .collect(),
        );
    } else if r["semantic_available"] == false {
        println!(
            "Semantic search unavailable: {}",
            r["semantic_reason"].as_str().unwrap_or_default()
        );
    }
    println!(
        "Evidence: {} item(s){}",
        arr("evidence").len(),
        if r["evidence_truncated"] == true {
            " (truncated)"
        } else {
            ""
        }
    );
}

// ----------------------------------------------------------- search ---

#[derive(clap::Args)]
pub struct SearchArgs {
    /// Identifier, phrase, or path fragment to search for.
    query: String,
    #[arg(long, default_value_t = lexical::DEFAULT_LIMIT,
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=200))]
    limit: usize,
    #[arg(long = "json")]
    json: bool,
    /// Show document hits as match-centered snippet blocks (the default;
    /// see search.output.fallback in config.yaml).
    #[arg(long)]
    snippets: bool,
    /// Show every hit as a plain title/path/tier table.
    #[arg(long)]
    table: bool,
    /// Show per-stage timing diagnostics.
    #[arg(long)]
    explain: bool,
    /// Also show one merged, reranked view of lexical and semantic results.
    #[arg(long)]
    hybrid: bool,
    /// Only these source ids (repeatable; a hard filter).
    #[arg(long = "source")]
    sources: Vec<String>,
    /// Only paths under this source-relative prefix (repeatable).
    #[arg(long = "path")]
    paths: Vec<String>,
    /// Only these result kinds: entity, document, path (repeatable).
    #[arg(long = "kind")]
    kinds: Vec<String>,
    /// Only these document ids (repeatable).
    #[arg(long = "document")]
    documents: Vec<String>,
    /// Exclude hits inside email attachments.
    #[arg(long)]
    no_attachments: bool,
}

fn hits_table(results: &[&SearchResult]) {
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|r| {
            vec![
                r.kind.to_owned(),
                r.tier.label().to_owned(),
                r.title.clone(),
                r.path.clone(),
                r.source_id.clone(),
            ]
        })
        .collect();
    print_table(&["Kind", "Tier", "Title", "Path", "Source"], &rows);
}

fn format_label(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    if ext.is_empty() {
        "FILE".into()
    } else {
        ext
    }
}

fn print_expanded(c: &Value) {
    let prev = c["previous"].as_array().cloned().unwrap_or_default();
    let next = c["next"].as_array().cloned().unwrap_or_default();
    if c["parent_heading"].is_null() && prev.is_empty() && next.is_empty() {
        return;
    }
    println!("Expanded context:");
    if let Some(t) = c["parent_heading"]["text"].as_str() {
        println!("[heading] {t}");
    }
    for p in &prev {
        println!("[previous] {}", p["text"].as_str().unwrap_or_default());
    }
    for p in &next {
        println!("[next] {}", p["text"].as_str().unwrap_or_default());
    }
    println!();
}

fn print_document_snippet(r: &SearchResult, fallback: &[String], expanded: Option<&Value>) {
    let snippet = r.snippet.as_deref().filter(|s| !s.is_empty());
    let Some(snippet) = snippet else {
        let next = fallback
            .iter()
            .find(|m| matches!(m.as_str(), "json" | "files"))
            .map_or("files", String::as_str);
        if next == "json" {
            println!(
                "{}",
                serde_json::to_string_pretty(&explore::search_result_json(r)).unwrap_or_default()
            );
        } else {
            println!("{}", r.path);
        }
        return;
    };
    println!("{}: {}", format_label(&r.path), r.path);
    let loc = r.location.as_ref();
    if let Some(a) = loc.and_then(|l| l.attachment.as_ref()) {
        let a = json!(a);
        println!(
            "Attachment: {} -> {}",
            r.path,
            a["name"].as_str().unwrap_or_default()
        );
        if let Some(t) = a["parent_title"].as_str().filter(|t| !t.is_empty()) {
            println!("Email: {t}");
        }
    }
    if let Some(l) = loc {
        if let Some(start) = l.page_start {
            match l.page_end {
                Some(end) if end != start => println!("Page: {start}-{end}"),
                _ => println!("Page: {start}"),
            }
        } else if !l.heading_path.is_empty() {
            println!("Section: {}", l.heading_path.join(" > "));
        } else if let Some(s) = l.section.as_deref().filter(|s| !s.is_empty()) {
            println!("Section: {s}");
        }
    }
    println!("Match:");
    println!("{snippet}");
    if let Some(c) = expanded {
        print_expanded(c);
    }
    println!();
}

// ------------------------------------------------------------- link ---

#[derive(Subcommand)]
pub enum LinkCommand {
    /// Link a code entity to a document (resolver "user", confidence exact).
    Add {
        /// Entity id, name, or qualified name.
        entity: String,
        /// Document id, file path, or filename.
        document: String,
        /// Specific section (chunk) id within the document.
        #[arg(long)]
        section: Option<String>,
        /// Restrict lookup to this source id.
        #[arg(long = "source")]
        source: Option<String>,
    },
    /// Remove a manual link by id, as shown by 'docmonk link list'.
    Remove {
        link_id: String,
        #[arg(long = "source")]
        source: Option<String>,
    },
    /// List links.
    List {
        /// Only links for this entity id/name.
        #[arg(long)]
        entity: Option<String>,
        /// Only links for this document id.
        #[arg(long = "document")]
        document: Option<String>,
        #[arg(long = "source")]
        source: Option<String>,
        #[arg(long = "json")]
        json: bool,
    },
}

pub fn search(a: &SearchArgs) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let cfg = load(&home)?;
    let sc = &cfg.search;
    let opened = open_sources(&home, None)?;
    let mode = if a.json {
        "json".to_owned()
    } else if a.snippets {
        "snippets".into()
    } else if a.table {
        "table".into()
    } else {
        sc.output.fallback[0].clone()
    };
    let run = run_search(
        &home,
        &cfg,
        &opened,
        &SearchRequest {
            query: &a.query,
            limit: a.limit,
            hybrid: a.hybrid,
            with_context: matches!(mode.as_str(), "json" | "snippets"),
            filters: docmonk_retrieval::route::SearchFilters {
                source_ids: a.sources.clone(),
                path_prefixes: a.paths.clone(),
                kinds: a.kinds.clone(),
                document_ids: a.documents.clone(),
                exclude_attachments: a.no_attachments,
            },
        },
    )?;
    let results = &run.results;
    let total_ms = run.total_ms();
    let confidence = run.confidence;

    if mode == "json" {
        let results_json: Vec<Value> = results
            .iter()
            .map(|r| {
                let mut v = explore::search_result_json(r);
                if let Some(c) = run.expanded.get(&r.id) {
                    v["context"] = c.clone();
                }
                v
            })
            .collect();
        let mut payload = json!({"query": a.query, "results": results_json});
        if let Some(s) = &run.semantic {
            payload["semantic"] = semantic_json(s);
        }
        if let Some(h) = &run.ranked {
            payload["hybrid"] = json!(h
                .iter()
                .map(|h| {
                    let mut v = h.to_json();
                    if let Some(c) = run.expanded.get(&h.candidate.id) {
                        v["context"] = c.clone();
                    }
                    v
                })
                .collect::<Vec<_>>());
        }
        payload["graph"] = docmonk_service::query::graph_json(&opened)?;
        if a.explain {
            let round3 = |v: f64| (v * 1000.0).round() / 1000.0;
            payload["explain"] = json!({
                "query_kind": classify::classify_query(&a.query),
                "lexical_confidence": confidence,
                "semantic_skipped": run.semantic_skipped,
                "stages": run.timings.iter().map(|t| {
                    let mut t = t.clone();
                    t["duration_ms"] = json!(round3(t["duration_ms"].as_f64().unwrap_or(0.0)));
                    if let Some(subs) = t.get_mut("substages").and_then(Value::as_array_mut) {
                        for s in subs {
                            s["duration_ms"] = json!(round3(s["duration_ms"].as_f64().unwrap_or(0.0)));
                        }
                    }
                    t
                }).collect::<Vec<_>>(),
                "total_ms": round3(total_ms),
                "counters": run.counters,
                "tokenizer": tokenizer_json(),
            });
            // Routing plan and filters (additive keys, ADR 0033).
            let plan = docmonk_retrieval::route::route(
                &a.query,
                docmonk_service::query::route_options(&cfg),
            );
            payload["explain"]["plan"] = json!(plan);
            // The neural pass's state (ADR 0038): applied, disabled,
            // unavailable, failed or nothing_to_rescore.
            if let Some(r) = &run.reranker {
                payload["explain"]["reranker"] = json!(r);
            }
            payload["explain"]["filters"] = json!(docmonk_retrieval::route::SearchFilters {
                source_ids: a.sources.clone(),
                path_prefixes: a.paths.clone(),
                kinds: a.kinds.clone(),
                document_ids: a.documents.clone(),
                exclude_attachments: a.no_attachments,
            });
        }
        return print_json(&payload);
    }

    if results.is_empty() {
        println!("No results for '{}'.", a.query);
    } else if mode == "table" {
        hits_table(&results.iter().collect::<Vec<_>>());
    } else {
        let others: Vec<&SearchResult> = results.iter().filter(|r| r.kind != "document").collect();
        let docs: Vec<&SearchResult> = results.iter().filter(|r| r.kind == "document").collect();
        if !others.is_empty() {
            hits_table(&others);
        }
        if mode == "files" {
            let mut seen = std::collections::HashSet::new();
            for r in docs {
                if seen.insert(r.path.clone()) {
                    println!("{}", r.path);
                }
            }
        } else {
            let fallback: Vec<String> = sc
                .output
                .fallback
                .iter()
                .filter(|m| m.as_str() != "snippets")
                .cloned()
                .collect();
            for r in docs {
                print_document_snippet(r, &fallback, run.expanded.get(&r.id));
            }
        }
    }

    if let Some(s) = &run.semantic {
        if !s.hits.is_empty() {
            println!("Semantic matches");
            let rows: Vec<Vec<String>> = s
                .hits
                .iter()
                .map(|h| {
                    vec![
                        h.hit.kind.clone(),
                        format!("{:.3}", h.hit.score),
                        h.hit.title.clone(),
                        h.hit.path.clone(),
                        h.source_id.clone(),
                    ]
                })
                .collect();
            print_table(&["Kind", "Score", "Title", "Path", "Source"], &rows);
        } else if !s.available {
            println!("Semantic search unavailable: {}", s.reason);
        }
    } else if sc.semantic {
        println!("Semantic search skipped: lexical confidence is {confidence}.");
    }
    if let Some(hits) = &run.ranked {
        println!("Hybrid ranked results");
        let rows: Vec<Vec<String>> = hits
            .iter()
            .map(|h| {
                let c = &h.candidate;
                vec![
                    c.kind.clone(),
                    h.tier_label.to_owned(),
                    c.title.clone(),
                    c.path.clone(),
                    c.semantic_score
                        .map_or_else(|| "-".into(), |s| format!("{s:.3}")),
                ]
            })
            .collect();
        print_table(&["Kind", "Tier", "Title", "Path", "Semantic"], &rows);
    }
    if a.explain {
        println!(
            "Query kind: {}  Lexical confidence: {confidence}",
            classify::classify_query(&a.query)
        );
        let rows: Vec<Vec<String>> = run
            .timings
            .iter()
            .map(|t| {
                vec![
                    t["stage"].as_str().unwrap_or_default().to_owned(),
                    t["hits"].to_string(),
                    format!("{:.3}", t["duration_ms"].as_f64().unwrap_or(0.0)),
                ]
            })
            .collect();
        print_table(&["Stage", "Hits", "Duration (ms)"], &rows);
        println!("Total: {total_ms:.3} ms");
        let t = tokenizer_json();
        println!(
            "Tokenizer: {} (revision {}, max {} tokens)",
            t["model_id"].as_str().unwrap_or_default(),
            &t["revision"].as_str().unwrap_or_default()[..12],
            t["max_sequence_tokens"]
        );
    }
    Ok(())
}

pub fn link(cmd: LinkCommand) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    match cmd {
        LinkCommand::Add {
            entity,
            document,
            section,
            source,
        } => match link_add(
            &home,
            &entity,
            &document,
            section.as_deref(),
            source.as_deref(),
        )? {
            LinkAdded::Exists => println!("That link already exists."),
            LinkAdded::Added {
                entity,
                document_id,
                link_id,
            } => println!("Linked {entity} -> {document_id} ({link_id})"),
        },
        LinkCommand::Remove { link_id, source } => {
            link_remove(&home, &link_id, source.as_deref())?;
            println!("Removed {link_id}");
        }
        LinkCommand::List {
            entity,
            document,
            source,
            json: json_output,
        } => {
            let rows = link_list(
                &home,
                entity.as_deref(),
                document.as_deref(),
                source.as_deref(),
            )?;
            if json_output {
                return print_json(&json!({ "links": rows }));
            }
            if rows.is_empty() {
                println!("No links found.");
                return Ok(());
            }
            let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
            let table: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    vec![
                        s(&r["link_id"]),
                        s(&r["link_type"]),
                        r["entity"]
                            .as_str()
                            .map_or_else(|| s(&r["entity_id"]), str::to_owned),
                        r["document_path"]
                            .as_str()
                            .map_or_else(|| s(&r["document_id"]), str::to_owned),
                        s(&r["resolver"]),
                        s(&r["confidence"]),
                    ]
                })
                .collect();
            print_table(
                &["ID", "Type", "Entity", "Document", "Resolver", "Confidence"],
                &table,
            );
        }
    }
    Ok(())
}

/// `docmonk evidence`: hard-filtered, routed, decomposed and grounded
/// evidence with typed citations (ADR 0033).
#[derive(clap::Args)]
pub struct EvidenceArgs {
    /// The question or lookup.
    query: String,
    /// Only these source ids (repeatable). A hard filter.
    #[arg(long = "source")]
    sources: Vec<String>,
    /// Only paths under this source-relative prefix (repeatable).
    #[arg(long = "path")]
    paths: Vec<String>,
    /// Only these result kinds: entity, document, path (repeatable).
    #[arg(long = "kind")]
    kinds: Vec<String>,
    /// Only these document ids (repeatable).
    #[arg(long = "document")]
    documents: Vec<String>,
    /// Exclude hits inside email attachments.
    #[arg(long)]
    no_attachments: bool,
    #[arg(long, default_value_t = 10,
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=100))]
    limit: usize,
    #[arg(long = "json")]
    json: bool,
}

pub fn evidence(a: &EvidenceArgs) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let cfg = load(&home)?;
    let v = docmonk_service::evidence::evidence_value(
        &home,
        &cfg,
        &docmonk_service::evidence::EvidenceRequest {
            query: &a.query,
            filters: docmonk_retrieval::route::SearchFilters {
                source_ids: a.sources.clone(),
                path_prefixes: a.paths.clone(),
                kinds: a.kinds.clone(),
                document_ids: a.documents.clone(),
                exclude_attachments: a.no_attachments,
            },
            limit: a.limit,
        },
    )?;
    if a.json {
        return print_json(&v);
    }
    println!(
        "Intent: {} (confidence {}) · verdict: {}",
        v["plan"]["intent"].as_str().unwrap_or("-"),
        v["plan"]["confidence"],
        v["verdict"].as_str().unwrap_or("-")
    );
    for s in v["plan"]["subqueries"].as_array().into_iter().flatten() {
        println!("  subquery: {}", s["text"].as_str().unwrap_or_default());
    }
    for e in v["evidence"].as_array().into_iter().flatten() {
        let mut loc = String::new();
        if let Some(l) = e["line_start"].as_i64() {
            loc = format!(":{l}");
        } else if let Some(p) = e["page_start"].as_i64() {
            loc = format!(" (page {p})");
        }
        println!(
            "[{}]{} {}{}  {}",
            e["id"].as_str().unwrap_or_default(),
            if e["valid"] == true {
                ""
            } else {
                " (unverified)"
            },
            e["path"].as_str().unwrap_or_default(),
            loc,
            e["title"].as_str().unwrap_or_default()
        );
    }
    for s in v["support"].as_array().into_iter().flatten() {
        println!(
            "  {}: {} (coverage {})",
            s["query"].as_str().unwrap_or_default(),
            s["status"].as_str().unwrap_or_default(),
            s["coverage"]
        );
    }
    Ok(())
}
