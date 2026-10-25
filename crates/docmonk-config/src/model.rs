//! The typed configuration schema.
//!
//! Field order drives validation-error order and the `config show` /
//! `config.yaml` output order.

use crate::coerce::{self, Coerced};
use crate::value::Value;

/// The pinned embedding model's maximum sequence length.
pub const MAX_SEQUENCE_TOKENS: i64 = 256;

/// Collected `(location, message)` validation errors, in field order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Errors(pub Vec<(String, String)>);

impl Errors {
    pub fn push(&mut self, loc: &str, msg: impl Into<String>) {
        self.0.push((loc.to_owned(), msg.into()));
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// `loc: msg; loc: msg`.
    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(|(loc, msg)| {
                if loc.is_empty() {
                    msg.clone()
                } else {
                    format!("{loc}: {msg}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

pub fn join_loc(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}.{child}")
    }
}

/// A field type: conversion from a loaded [`Value`] and back.
pub trait ConfigField: Sized + Clone {
    fn coerce(errors: &mut Errors, value: &Value, loc: &str) -> Option<Self>;
    fn dump(&self) -> Value;
}

fn scalar<T>(errors: &mut Errors, loc: &str, r: Coerced<T>) -> Option<T> {
    r.map_err(|msg| errors.push(loc, msg)).ok()
}

impl ConfigField for i64 {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_int(v))
    }
    fn dump(&self) -> Value {
        Value::Int(*self)
    }
}

impl ConfigField for f64 {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_float(v))
    }
    fn dump(&self) -> Value {
        Value::Float(*self)
    }
}

impl ConfigField for bool {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_bool(v))
    }
    fn dump(&self) -> Value {
        Value::Bool(*self)
    }
}

impl ConfigField for String {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_str(v))
    }
    fn dump(&self) -> Value {
        Value::Str(self.clone())
    }
}

impl ConfigField for Option<String> {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_opt_str(v))
    }
    fn dump(&self) -> Value {
        self.as_ref().map_or(Value::Null, |s| Value::Str(s.clone()))
    }
}

impl ConfigField for Vec<String> {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        scalar(e, loc, coerce::to_str_list(v))
    }
    fn dump(&self) -> Value {
        Value::List(self.iter().map(|s| Value::Str(s.clone())).collect())
    }
}

/// `documents.chunking.max_tokens`: an integer or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokens {
    Auto,
    Value(i64),
}

impl ConfigField for MaxTokens {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        if matches!(v, Value::Str(s) if s.trim() == "auto") {
            return Some(MaxTokens::Auto);
        }
        match coerce::to_int(v) {
            Ok(i) => Some(MaxTokens::Value(i)),
            Err(_) => {
                e.push(loc, "expected an integer or 'auto'");
                None
            }
        }
    }
    fn dump(&self) -> Value {
        match self {
            MaxTokens::Auto => Value::str("auto"),
            MaxTokens::Value(i) => Value::Int(*i),
        }
    }
}

/// `storage.server.engine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageEngine {
    OpenSearch,
    Elasticsearch,
}

impl StorageEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            StorageEngine::OpenSearch => "opensearch",
            StorageEngine::Elasticsearch => "elasticsearch",
        }
    }
}

impl ConfigField for StorageEngine {
    fn coerce(e: &mut Errors, v: &Value, loc: &str) -> Option<Self> {
        match v {
            Value::Str(s) if s == "opensearch" => Some(StorageEngine::OpenSearch),
            Value::Str(s) if s == "elasticsearch" => Some(StorageEngine::Elasticsearch),
            _ => {
                e.push(loc, "expected 'opensearch' or 'elasticsearch'");
                None
            }
        }
    }
    fn dump(&self) -> Value {
        Value::str(self.as_str())
    }
}

type Check<T> = fn(&T) -> Result<(), String>;

fn coerce_field<T: ConfigField>(
    e: &mut Errors,
    obj: &Value,
    loc: &str,
    key: &str,
    default: &T,
    check: Option<Check<T>>,
) -> Option<T> {
    let floc = join_loc(loc, key);
    let value = match obj.get(key) {
        None => return Some(default.clone()),
        Some(v) => T::coerce(e, v, &floc)?,
    };
    if let Some(check) = check {
        if let Err(msg) = check(&value) {
            e.push(&floc, msg);
            return None;
        }
    }
    Some(value)
}

macro_rules! section {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $field:ident : $ty:ty = $default:expr $(, check = $check:expr)? ; )*
        }
        $(model_check = $mcheck:expr ;)?
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            $( pub $field: $ty, )*
        }

        impl Default for $name {
            fn default() -> Self {
                Self { $( $field: $default, )* }
            }
        }

        impl ConfigField for $name {
            fn coerce(e: &mut Errors, value: &Value, loc: &str) -> Option<Self> {
                let Some(entries) = value.as_map() else {
                    e.push(loc, coerce::MSG_MAP);
                    return None;
                };
                let defaults = Self::default();
                let start = e.len();
                const FIELDS: &[&str] = &[$( stringify!($field), )*];
                for (key, _) in entries {
                    if !FIELDS.contains(&key.as_str()) {
                        e.push(&join_loc(loc, key), "unknown setting");
                    }
                }
                $(
                    #[allow(unused_mut, unused_assignments)]
                    let mut check: Option<Check<$ty>> = None;
                    $( check = Some($check); )?
                    let $field = coerce_field::<$ty>(
                        e, value, loc, stringify!($field), &defaults.$field, check,
                    );
                )*
                if e.len() != start {
                    return None;
                }
                let out = Self { $( $field: $field?, )* };
                $(
                    let mcheck: Check<Self> = $mcheck;
                    if let Err(msg) = mcheck(&out) {
                        e.push(loc, msg);
                        return None;
                    }
                )?
                Some(out)
            }

            fn dump(&self) -> Value {
                Value::Map(vec![
                    $( (stringify!($field).to_owned(), self.$field.dump()), )*
                ])
            }
        }
    };
}

/// `"<consumer> -> <producer>"` of `indexing.relationship_dependencies`.
pub fn parse_dependency(d: &str) -> Option<(&str, &str)> {
    let (c, p) = d.split_once("->")?;
    let (c, p) = (c.trim(), p.trim());
    (!c.is_empty() && !p.is_empty() && c != p).then_some((c, p))
}

/// `search.embedding_model` values: the pinned models of docmonk-ml's
/// manifest (`EMBEDDING_MODELS`, checked there).
pub const EMBEDDING_MODEL_SLUGS: &[&str] = &["all-minilm-l6-v2", "multilingual-e5-small"];

fn one_of(value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(format!(
        "unknown value {value:?}; expected one of: {}",
        allowed.join(", ")
    ))
}

section! {
    RuntimeConfig {
        log_level: String = "info".into();
        sqlite_cache_size_mb: i64 = 64;
    }
}

section! {
    IndexingConfig {
        watch: bool = true;
        debounce_ms: i64 = 2000;
        max_file_size_mb: i64 = 100;
        follow_symlinks: bool = false;
        // File-name patterns never indexed, added to the built-in secret
        // list (.env, *.pem, id_rsa, .netrc, *.tfstate, ...); never replace it.
        secret_patterns: Vec<String> = Vec::new();
        // Replace credentials found in indexed text (private keys, cloud
        // and VCS tokens, JWTs, password/secret/api-key literals,
        // connection-string passwords, URL credentials) with
        // [REDACTED:<kind>] before anything is stored or embedded.
        redact_secrets: bool = true;
        network_poll_seconds: i64 = 30;
        reconciliation_interval_seconds: i64 = 900;
        code_extraction_workers: i64 = 1;
        document_extraction_workers: i64 = 1;
        embedding_batch_size: i64 = 16;
        // Call graphs and code<->document links. Off skips their extraction,
        // cross-file resolution and knowledge linking (faster indexing).
        relationships_enabled: bool = true;
        // false (default): a run indexes every selected source first, then
        // builds relationship graphs as a second step. true: each source's
        // graph starts as soon as its own index is published, while other
        // sources still index (helps when one large source would hold
        // small sources' graphs back).
        pipeline_relationships: bool = false;
        // Experimental: indexing stores each code file's graph facts from
        // its parse (a disposable cache under <home>/cache), so the
        // relationship stage reuses them instead of parsing again. Off:
        // the stage parses every file it derives.
        graph_fact_cache: bool = false;
        // Explicit cross-source dependencies, "<consumer> -> <producer>"
        // (source ids or paths). Unresolved references of the consumer are
        // looked up by exact qualified name in its producers only; C#
        // project references between registered sources are discovered.
        relationship_dependencies: Vec<String> = Vec::new(), check = |v| {
            match v.iter().find(|d| parse_dependency(d).is_none()) {
                None => Ok(()),
                Some(d) => Err(format!("{d:?} is not \"<consumer> -> <producer>\"")),
            }
        };
        lock_timeout_seconds: f64 = 30.0, check = |v| {
            if *v > 0.0 && *v <= 3600.0 { Ok(()) } else {
                Err("must be > 0 and <= 3600".into())
            }
        };
        status_stall_threshold_seconds: f64 = 120.0, check = |v| {
            if *v <= 0.0 { Err("must be > 0".into()) } else { Ok(()) }
        };
        // P0 parallel indexing (ADR 0033). 0 = auto, evaluated at runtime.
        max_parallel_sources: i64 = 0, check = |v| if (0..=32).contains(v) { Ok(()) } else {
            Err("must be between 0 (auto) and 32".into())
        };
        cpu_workers: i64 = 0, check = |v| if (0..=256).contains(v) { Ok(()) } else {
            Err("must be between 0 (auto) and 256".into())
        };
        ocr_workers: i64 = 1, check = |v| if (1..=64).contains(v) { Ok(()) } else {
            Err("must be between 1 and 64".into())
        };
        embedding_workers: i64 = 1, check = |v| if (1..=64).contains(v) { Ok(()) } else {
            Err("must be between 1 and 64".into())
        };
        io_concurrency: i64 = 8, check = |v| if (1..=1024).contains(v) { Ok(()) } else {
            Err("must be between 1 and 1024".into())
        };
        max_in_flight_mb: i64 = 512, check = |v| if (16..=1_048_576).contains(v) { Ok(()) } else {
            Err("must be between 16 and 1048576".into())
        };
        fairness_max_wait_seconds: i64 = 120, check = |v| if (1..=86_400).contains(v) { Ok(()) } else {
            Err("must be between 1 and 86400".into())
        };
        max_targeted_paths: i64 = 512, check = |v| if (1..=1_000_000).contains(v) { Ok(()) } else {
            Err("must be between 1 and 1000000".into())
        };
    }
}

impl IndexingConfig {
    /// `max_parallel_sources`, resolving `0` to `min(4, max(1, cpus / 2))`.
    pub fn resolved_max_parallel_sources(&self) -> usize {
        if self.max_parallel_sources > 0 {
            return self.max_parallel_sources as usize;
        }
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        (cpus / 2).clamp(1, 4)
    }

    /// `cpu_workers`, resolving `0` to the number of available CPUs.
    pub fn resolved_cpu_workers(&self) -> usize {
        if self.cpu_workers > 0 {
            return self.cpu_workers as usize;
        }
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }
}

impl ChunkingConfig {
    /// `"auto"` resolves to the pinned model's `MAX_SEQUENCE_TOKENS`.
    pub fn resolved_max_tokens(&self) -> i64 {
        match self.max_tokens {
            MaxTokens::Auto => MAX_SEQUENCE_TOKENS,
            MaxTokens::Value(v) => v,
        }
    }
}

section! {
    ChunkingConfig {
        max_tokens: MaxTokens = MaxTokens::Auto, check = |v| match v {
            MaxTokens::Auto => Ok(()),
            MaxTokens::Value(n) if *n < 16 => Err("must be at least 16".into()),
            MaxTokens::Value(n) if *n > MAX_SEQUENCE_TOKENS => Err(format!(
                "{n} exceeds the embedding model's \
                 maximum sequence length ({MAX_SEQUENCE_TOKENS}); a larger value would \
                 let chunks be silently truncated at embedding time. Use 'auto' or a \
                 value <= {MAX_SEQUENCE_TOKENS}."
            )),
            MaxTokens::Value(_) => Ok(()),
        };
        min_tokens: i64 = 60, check = |v| if *v < 1 { Err("must be at least 1".into()) } else { Ok(()) };
        overlap_tokens: i64 = 40, check = |v| if *v < 0 { Err("must not be negative".into()) } else { Ok(()) };
        safety_tokens: i64 = 4, check = |v| if *v < 0 { Err("must not be negative".into()) } else { Ok(()) };
        merge_peers: bool = true;
    }
    model_check = |c| {
        let resolved = c.resolved_max_tokens();
        if c.min_tokens > resolved {
            return Err("documents.chunking.min_tokens must not exceed documents.chunking.max_tokens".into());
        }
        if c.overlap_tokens >= resolved {
            return Err("documents.chunking.overlap_tokens must be less than documents.chunking.max_tokens".into());
        }
        if c.safety_tokens >= resolved {
            return Err("documents.chunking.safety_tokens must be less than documents.chunking.max_tokens".into());
        }
        Ok(())
    };
}

fn positive_attachment(field: &'static str) -> Check<i64> {
    match field {
        "email_attachment_max_bytes" => |v| {
            if *v <= 0 {
                Err("must be > 0".into())
            } else {
                Ok(())
            }
        },
        "email_attachment_max_count" => |v| {
            if *v <= 0 {
                Err("must be > 0".into())
            } else {
                Ok(())
            }
        },
        _ => |v| {
            if *v <= 0 {
                Err("must be > 0".into())
            } else {
                Ok(())
            }
        },
    }
}

section! {
    DocumentsConfig {
        ocr: String = "auto".into(), check = |v| one_of(v, &["off", "auto", "always"]);
        max_pages: i64 = 1000;
        chunking: ChunkingConfig = ChunkingConfig::default();
        image_ocr: bool = false;
        pdf_mode: String = "accurate".into(), check = |v| one_of(v, &["accurate", "fast"]);
        email_attachments: bool = true;
        email_attachment_max_bytes: i64 = 25 * 1024 * 1024, check = positive_attachment("email_attachment_max_bytes");
        email_attachment_max_count: i64 = 50, check = positive_attachment("email_attachment_max_count");
        email_attachment_total_max_bytes: i64 = 100 * 1024 * 1024, check = positive_attachment("email_attachment_total_max_bytes");
    }
}

section! {
    SearchOutputConfig {
        fallback: Vec<String> = vec!["snippets".into(), "json".into(), "files".into()], check = |v| {
            if v.is_empty() {
                return Err("must not be empty".into());
            }
            for mode in v {
                if !["snippets", "json", "files", "table"].contains(&mode.as_str()) {
                    return Err(format!(
                        "unknown search.output.fallback mode {mode:?}; expected one of: snippets, json, files, table"
                    ));
                }
            }
            Ok(())
        };
        snippet_max_tokens: i64 = 32, check = |v| if (1..=64).contains(v) { Ok(()) } else {
            Err("must be between 1 and 64 (SQLite FTS5's own snippet() limit)".into())
        };
    }
}

fn non_negative_chunks(v: &i64) -> Result<(), String> {
    if *v < 0 {
        Err("must be >= 0".into())
    } else {
        Ok(())
    }
}

section! {
    SearchContextConfig {
        parent_heading: bool = true;
        previous_chunks: i64 = 1, check = non_negative_chunks;
        next_chunks: i64 = 1, check = non_negative_chunks;
        max_tokens: i64 = 1200, check = |v| if *v <= 0 { Err("must be > 0".into()) } else { Ok(()) };
    }
}

section! {
    SearchRerankerConfig {
        enabled: bool = false;
        top_n: i64 = 20, check = |v| if *v < 1 { Err("must be >= 1".into()) } else { Ok(()) };
    }
}

section! {
    /// Approximate nearest-neighbour search.
    SearchAnnConfig {
        // HNSW beam width per query (local index). Higher raises recall
        // at a small latency cost.
        ef_search: i64 = 400, check = |v| if (10..=10_000).contains(v) { Ok(()) } else {
            Err("must be between 10 and 10000".into())
        };
        // Elasticsearch kNN `num_candidates`; 0 = `max(10 * k, 100)`.
        num_candidates: i64 = 0, check = |v| if (0..=10_000).contains(v) { Ok(()) } else {
            Err("must be between 0 and 10000 (0 = automatic)".into())
        };
    }
}

section! {
    SearchRoutingConfig {
        enabled: bool = true;
    }
}

section! {
    SearchDecompositionConfig {
        enabled: bool = true;
        max_subqueries: i64 = 4, check = |v| if (1..=8).contains(v) { Ok(()) } else {
            Err("must be between 1 and 8".into())
        };
        llm_enabled: bool = false;
    }
}

section! {
    SearchDiversityConfig {
        enabled: bool = false;
        lambda: f64 = 0.7, check = |v| if (0.0..=1.0).contains(v) { Ok(()) } else {
            Err("must be between 0 and 1".into())
        };
        per_document_cap: i64 = 3, check = |v| if *v < 1 { Err("must be >= 1".into()) } else { Ok(()) };
        per_source_cap: i64 = 0, check = |v| if *v < 0 { Err("must be >= 0 (0 = no cap)".into()) } else { Ok(()) };
    }
}

section! {
    SearchGroundingConfig {
        enabled: bool = true;
        min_coverage: f64 = 0.5, check = |v| if (0.0..=1.0).contains(v) { Ok(()) } else {
            Err("must be between 0 and 1".into())
        };
    }
}

section! {
    /// Typed technical anchors (URL paths, file paths, symbols) steer
    /// routing; false restores the pre-anchor route (rollback).
    SearchAnchorsConfig {
        enabled: bool = true;
    }
}

section! {
    /// Exact URL-literal and composed-route occurrences (graph derivation
    /// `graph-3` and later), with a verified document fallback.
    SearchExactOccurrencesConfig {
        enabled: bool = true;
        max_results: i64 = 50, check = |v| if (1..=1000).contains(v) { Ok(()) } else {
            Err("must be between 1 and 1000".into())
        };
    }
}

section! {
    /// Progressive lexical tiers: server reads prefetch only the precise
    /// tiers; looser tiers run only when the precise ones fall short.
    SearchAdaptiveLexicalConfig {
        enabled: bool = true;
    }
}

section! {
    /// Graph expansion only from sources with exact seeds, within a node
    /// budget per request.
    SearchSelectiveGraphConfig {
        enabled: bool = true;
        max_seed_entities: i64 = 25, check = |v| if (1..=500).contains(v) { Ok(()) } else {
            Err("must be between 1 and 500".into())
        };
    }
}

section! {
    /// Process-local explore result cache (long-lived MCP / UI processes
    /// only), keyed by every source's build and graph generation.
    SearchCacheConfig {
        enabled: bool = true;
        max_entries: i64 = 64, check = |v| if (1..=10_000).contains(v) { Ok(()) } else {
            Err("must be between 1 and 10000".into())
        };
        ttl_seconds: i64 = 300, check = |v| if (1..=86_400).contains(v) { Ok(()) } else {
            Err("must be between 1 and 86400".into())
        };
    }
}

section! {
    SearchConfig {
        lexical: bool = true;
        graph: bool = true;
        semantic: bool = false;
        lazy_semantic: bool = false;
        semantic_top_k: i64 = 30;
        // The pinned embedding model (docmonk-ml's manifest). Vectors of
        // another model are never mixed in: changing it re-embeds.
        embedding_model: String = "all-minilm-l6-v2".into(),
            check = |v| one_of(v, EMBEDDING_MODEL_SLUGS);
        // Candidates each stage (lexical, semantic) fetches before fusion,
        // independent of the response size; 0 = max(4 * limit, 40).
        candidate_pool: i64 = 0, check = |v| if (0..=1000).contains(v) { Ok(()) } else {
            Err("must be between 0 and 1000 (0 = automatic)".into())
        };
        output: SearchOutputConfig = SearchOutputConfig::default();
        context: SearchContextConfig = SearchContextConfig::default();
        reranker: SearchRerankerConfig = SearchRerankerConfig::default();
        ann: SearchAnnConfig = SearchAnnConfig::default();
        routing: SearchRoutingConfig = SearchRoutingConfig::default();
        decomposition: SearchDecompositionConfig = SearchDecompositionConfig::default();
        diversity: SearchDiversityConfig = SearchDiversityConfig::default();
        grounding: SearchGroundingConfig = SearchGroundingConfig::default();
        max_query_budget_ms: i64 = 5000, check = |v| if (100..=600_000).contains(v) { Ok(()) } else {
            Err("must be between 100 and 600000".into())
        };
        anchors: SearchAnchorsConfig = SearchAnchorsConfig::default();
        exact_occurrences: SearchExactOccurrencesConfig = SearchExactOccurrencesConfig::default();
        adaptive_lexical: SearchAdaptiveLexicalConfig = SearchAdaptiveLexicalConfig::default();
        selective_graph: SearchSelectiveGraphConfig = SearchSelectiveGraphConfig::default();
        cache: SearchCacheConfig = SearchCacheConfig::default();
        // Server mode: corpora searched concurrently per request (a bounded
        // worker pool, never one thread per source).
        max_concurrency: i64 = 8, check = |v| if (1..=64).contains(v) { Ok(()) } else {
            Err("must be between 1 and 64".into())
        };
    }
}

section! {
    /// Relationship-graph reads.
    GraphConfig {
        // false: while a new graph is being built after a base update, the
        // last committed graph of the same base build is served, flagged
        // stale. true: graph queries return nothing until the graph
        // matches the published base exactly.
        require_current: bool = false;
    }
}

section! {
    ContextConfig {
        max_chars: i64 = 30000;
        max_files: i64 = 20;
        max_graph_nodes: i64 = 100;
    }
}

section! {
    McpConfig {
        enabled: bool = true;
        request_timeout_seconds: f64 = 30.0, check = |v| {
            if *v > 0.0 && *v <= 3600.0 { Ok(()) } else { Err("must be > 0 and at most 3600".into()) }
        };
        // Tool calls running at once (a timed-out call holds its slot
        // until its cancelled work has stopped).
        max_concurrent_calls: i64 = 4, check = |v| {
            if (1..=64).contains(v) { Ok(()) } else { Err("must be between 1 and 64".into()) }
        };
        // Calls waiting for a free slot; past it, calls fail with `overloaded`.
        max_queued_calls: i64 = 16, check = |v| {
            if (0..=1024).contains(v) { Ok(()) } else { Err("must be between 0 and 1024".into()) }
        };
        // Largest JSON-RPC message accepted (bytes).
        max_request_bytes: i64 = 1_048_576, check = |v| {
            if (1024..=67_108_864).contains(v) { Ok(()) } else { Err("must be between 1024 and 67108864".into()) }
        };
    }
}

section! {
    /// Remote access to the Admin UI. Identity always comes from an
    /// authenticating reverse proxy that terminates TLS; DocMonk stores no
    /// credentials (the proxy's shared secret is read from the environment
    /// variable named by `proxy_secret_env`).
    UiConfig {
        // Serve authenticated remote access through the proxy. Required for
        // any non-loopback bind; without it such a bind is refused.
        remote_enabled: bool = false;
        // Public host names (optionally `host:port`) the proxy forwards;
        // every other Host, Origin or Referer is refused.
        allowed_hosts: Vec<String> = Vec::new();
        // IP addresses of the proxies; direct connections from anywhere
        // else are refused, so identity headers cannot be spoofed.
        trusted_proxies: Vec<String> = Vec::new(), check = |v| {
            match v.iter().find(|a| a.trim().parse::<std::net::IpAddr>().is_err()) {
                Some(bad) => Err(format!("not an IP address: {bad}")),
                None => Ok(()),
            }
        };
        proxy_secret_env: String = "DOCMONK_UI_PROXY_SECRET".into();
        user_header: String = "x-forwarded-user".into();
        groups_header: String = "x-forwarded-groups".into();
        // Admins may change anything; viewers get read-only pages.
        admin_users: Vec<String> = Vec::new();
        admin_groups: Vec<String> = Vec::new();
        viewer_users: Vec<String> = Vec::new();
        viewer_groups: Vec<String> = Vec::new();
    }
}

section! {
    PrivacyConfig {
        external_ai_allowed: bool = false;
    }
}

section! {
    /// API keys are never stored here; providers read them from env vars.
    AiConfig {
        provider: String = "none".into();
        model: String = String::new();
        base_url: Option<String> = None;
        timeout_seconds: f64 = 60.0;
    }
}

section! {
    UpdatesConfig {
        enabled: bool = true;
        check_interval_hours: i64 = 24;
        notify: bool = true;
        channel: String = "stable".into();
    }
}

fn bulk_positive(v: &i64) -> Result<(), String> {
    if *v < 1 {
        Err("must be >= 1".into())
    } else {
        Ok(())
    }
}

section! {
    BulkConfig {
        max_actions: i64 = 500, check = bulk_positive;
        max_bytes: i64 = 5_000_000, check = bulk_positive;
        max_retries: i64 = 3, check = |v| if *v < 0 { Err("must be >= 0".into()) } else { Ok(()) };
    }
}

section! {
    /// Never holds credentials: they come from
    /// `DOCMONK_{OPENSEARCH,ELASTICSEARCH}_{USERNAME,PASSWORD,API_KEY}`.
    ServerStorageConfig {
        engine: StorageEngine = StorageEngine::OpenSearch;
        url: String = String::new(), check = |v| {
            if docmonk_telemetry::redact::url_has_userinfo(v) {
                Err("must not contain credentials (user-info such as \
                     'user:password@'); set DOCMONK_OPENSEARCH_USERNAME/_PASSWORD/_API_KEY \
                     or DOCMONK_ELASTICSEARCH_USERNAME/_PASSWORD/_API_KEY instead".into())
            } else {
                Ok(())
            }
        };
        index_prefix: String = "docmonk".into();
        verify_tls: bool = true;
        request_timeout_seconds: f64 = 30.0, check = |v| {
            if *v <= 0.0 { Err("must be > 0".into()) } else { Ok(()) }
        };
        // Budget for one server-side by-query task (closing replaced
        // records, sweeping an aborted attempt, GC). The task runs detached and is polled, so no single
        // request waits longer than the request timeout.
        task_timeout_seconds: f64 = 3600.0, check = |v| {
            if *v <= 0.0 { Err("must be > 0".into()) } else { Ok(()) }
        };
        bulk: BulkConfig = BulkConfig::default();
        gc_grace_seconds: f64 = 60.0, check = |v| {
            if (0.0..=86_400.0).contains(v) { Ok(()) } else { Err("must be between 0 and 86400".into()) }
        };
        lease_seconds: f64 = 300.0, check = |v| {
            if (5.0..=86_400.0).contains(v) { Ok(()) } else { Err("must be between 5 and 86400".into()) }
        };
    }
}

section! {
    /// `mode="local"` stays the default even when the section is absent.
    StorageConfig {
        mode: String = "local".into(), check = |v| one_of(v, &["local", "server"]);
        server: ServerStorageConfig = ServerStorageConfig::default();
    }
}

section! {
    DocMonkConfig {
        runtime: RuntimeConfig = RuntimeConfig::default();
        indexing: IndexingConfig = IndexingConfig::default();
        documents: DocumentsConfig = DocumentsConfig::default();
        search: SearchConfig = SearchConfig::default();
        context: ContextConfig = ContextConfig::default();
        graph: GraphConfig = GraphConfig::default();
        mcp: McpConfig = McpConfig::default();
        ui: UiConfig = UiConfig::default();
        privacy: PrivacyConfig = PrivacyConfig::default();
        ai: AiConfig = AiConfig::default();
        updates: UpdatesConfig = UpdatesConfig::default();
        storage: StorageConfig = StorageConfig::default();
    }
}

/// Top-level sections recognized by the `DOCMONK_<SECTION>__...` env layer.
pub const KNOWN_SECTIONS: &[&str] = &[
    "runtime",
    "indexing",
    "documents",
    "search",
    "context",
    "mcp",
    "ui",
    "privacy",
    "ai",
    "updates",
    "storage",
];

impl DocMonkConfig {
    /// Validates a merged value tree into the typed configuration.
    pub fn validate(value: &Value) -> Result<Self, Errors> {
        let mut errors = Errors::default();
        match <Self as ConfigField>::coerce(&mut errors, value, "") {
            Some(cfg) if errors.is_empty() => Ok(cfg),
            _ => Err(errors),
        }
    }

    /// The configuration as a value tree (field order preserved).
    pub fn to_value(&self) -> Value {
        self.dump()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_and_bad_values_are_reported_in_order() {
        let v = crate::yaml::parse(
            "runtime:\n  log_level: debug\n  max_workers: 4\nsearch:\n  semantic: maybe\nstorage:\n  mode: cloud\n",
        )
        .unwrap();
        let merged = crate::value::deep_merge(&DocMonkConfig::default().to_value(), &v);
        let err = DocMonkConfig::validate(&merged).unwrap_err().render();
        assert_eq!(
            err,
            "runtime.max_workers: unknown setting; search.semantic: expected true or false; \
             storage.mode: unknown value \"cloud\"; expected one of: local, server"
        );
    }

    #[test]
    fn defaults_round_trip() {
        let cfg = DocMonkConfig::default();
        assert_eq!(DocMonkConfig::validate(&cfg.to_value()).unwrap(), cfg);
        assert_eq!(
            cfg.documents.chunking.resolved_max_tokens(),
            MAX_SEQUENCE_TOKENS
        );
    }
}
