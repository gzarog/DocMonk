//! Personal evaluation (`docmonk eval`, roadmap P10): measure retrieval on
//! your own corpus. Queries and what they should find are recorded in
//! `<home>/eval/queries.json`, which stays on this machine (nothing is
//! uploaded). `eval run` answers each query through the same evidence
//! pipeline agents use and reports recall@1, recall@10, MRR and
//! abstention. With `--model` (or `--compare A,B`) the registered sources
//! are indexed again, for each model, in a throwaway home under
//! `<home>/eval/runs/`, so models are compared on the same corpus without
//! touching the real index.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;
use docmonk_retrieval::route::SearchFilters;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::evidence::{evidence_value, EvidenceRequest};
use crate::load;

pub const FORMAT_VERSION: u32 = 1;
/// Evidence considered per query (recall@10).
pub const DEPTH: usize = 10;

/// One place a query should find: a file, optionally a line in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

impl Expect {
    /// `path` or `path:line`.
    pub fn parse(s: &str) -> Result<Self, DocMonkError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(DocMonkError::usage("--expect must not be empty"));
        }
        if let Some((p, l)) = s.rsplit_once(':') {
            if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
                let line = l
                    .parse::<i64>()
                    .map_err(|_| DocMonkError::usage(format!("bad line in --expect {s}")))?;
                return Ok(Self {
                    path: p.to_owned(),
                    line: Some(line),
                });
            }
        }
        Ok(Self {
            path: s.to_owned(),
            line: None,
        })
    }

    /// Whether an evidence entry (absolute path, optional line span)
    /// finds this place. A relative expectation matches as a path suffix
    /// at a directory boundary.
    pub fn matches(&self, path: &str, lines: Option<(i64, i64)>) -> bool {
        let norm = |p: &str| p.replace('\\', "/");
        let (got, want) = (norm(path), norm(&self.path));
        let want = want.trim_start_matches("./");
        let same = got == want
            || got
                .strip_suffix(want)
                .is_some_and(|rest| rest.ends_with('/'));
        same && match (self.line, lines) {
            (None, _) => true,
            (Some(l), Some((s, e))) => s <= l && l <= e,
            (Some(_), None) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub id: String,
    pub query: String,
    /// Empty for an unanswerable query (it should abstain).
    #[serde(default)]
    pub expect: Vec<Expect>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySet {
    pub format_version: u32,
    pub queries: Vec<Query>,
}

impl Default for QuerySet {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            queries: Vec::new(),
        }
    }
}

pub fn queries_path(home: &Home) -> PathBuf {
    home.root().join("eval").join("queries.json")
}

fn generic(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Generic, e.to_string())
}

pub fn load_queries(home: &Home) -> Result<QuerySet, DocMonkError> {
    let path = queries_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(QuerySet::default());
    };
    let set: QuerySet = serde_json::from_str(&text).map_err(|e| {
        generic(format!(
            "{} is not a DocMonk query set: {e}",
            path.display()
        ))
    })?;
    if set.format_version != FORMAT_VERSION {
        return Err(generic(format!(
            "{} has format_version {} (this DocMonk reads {FORMAT_VERSION})",
            path.display(),
            set.format_version
        )));
    }
    Ok(set)
}

fn save_queries(home: &Home, set: &QuerySet) -> Result<(), DocMonkError> {
    let path = queries_path(home);
    std::fs::create_dir_all(path.parent().unwrap_or(home.root())).map_err(generic)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(set).map_err(generic)? + "\n",
    )
    .map_err(generic)?;
    std::fs::rename(&tmp, &path).map_err(generic)
}

/// `eval record`: adds a query; an empty `expect` records an
/// unanswerable one. Returns the stored query.
pub fn record(home: &Home, query: &str, expect: Vec<Expect>) -> Result<Query, DocMonkError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(DocMonkError::usage("the query must not be empty"));
    }
    let mut set = load_queries(home)?;
    let next = set
        .queries
        .iter()
        .filter_map(|q| q.id.strip_prefix('q').and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let q = Query {
        id: format!("q{next}"),
        query: query.to_owned(),
        expect,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    set.queries.push(q.clone());
    save_queries(home, &set)?;
    Ok(q)
}

/// `eval remove <id>`.
pub fn remove(home: &Home, id: &str) -> Result<(), DocMonkError> {
    let mut set = load_queries(home)?;
    let before = set.queries.len();
    set.queries.retain(|q| q.id != id);
    if set.queries.len() == before {
        return Err(DocMonkError::usage(format!("no such query: {id}")));
    }
    save_queries(home, &set)
}

/// Rank (1-based) of the first evidence entry that finds `expect`.
fn first_hit(evidence: &[Value], expect: &[Expect]) -> Option<usize> {
    evidence
        .iter()
        .position(|e| {
            let path = e["path"].as_str().unwrap_or_default();
            let lines = e["line_start"]
                .as_i64()
                .map(|s| (s, e["line_end"].as_i64().unwrap_or(s)));
            expect.iter().any(|x| x.matches(path, lines))
        })
        .map(|i| i + 1)
}

/// Scores `set` against `home`'s index with `cfg`.
pub fn score(home: &Home, cfg: &DocMonkConfig, set: &QuerySet) -> Result<Value, DocMonkError> {
    let mut rows = Vec::new();
    let (mut answerable, mut unanswerable) = (0usize, 0usize);
    let (mut r1, mut r10, mut rr) = (0f64, 0f64, 0f64);
    let (mut false_abstain, mut false_support) = (0usize, 0usize);
    for q in &set.queries {
        let ev = evidence_value(
            home,
            cfg,
            &EvidenceRequest {
                query: &q.query,
                filters: SearchFilters::default(),
                limit: DEPTH,
            },
        )?;
        let evidence = ev["evidence"].as_array().cloned().unwrap_or_default();
        let verdict = ev["verdict"]
            .as_str()
            .unwrap_or("insufficient_evidence")
            .to_owned();
        let abstained = verdict == "insufficient_evidence";
        let rank = if q.expect.is_empty() {
            None
        } else {
            first_hit(&evidence, &q.expect)
        };
        if q.expect.is_empty() {
            unanswerable += 1;
            if !abstained {
                false_support += 1;
            }
        } else {
            answerable += 1;
            if rank == Some(1) {
                r1 += 1.0;
            }
            if rank.is_some() {
                r10 += 1.0;
            }
            rr += rank.map_or(0.0, |r| 1.0 / r as f64);
            if abstained {
                false_abstain += 1;
            }
        }
        rows.push(json!({
            "id": q.id,
            "query": q.query,
            "answerable": !q.expect.is_empty(),
            "rank": rank,
            "verdict": verdict,
            "top": evidence.iter().take(3).map(|e| json!({
                "path": e["path"], "line_start": e["line_start"], "line_end": e["line_end"],
            })).collect::<Vec<_>>(),
        }));
    }
    let ratio = |n: f64, d: usize| {
        if d == 0 {
            Value::Null
        } else {
            json!((n / d as f64 * 1000.0).round() / 1000.0)
        }
    };
    let model = docmonk_ml::manifest::configured_embedding_model(&cfg.search.embedding_model);
    Ok(json!({
        "embedding_model": model.slug,
        "semantic": cfg.search.semantic,
        "queries": set.queries.len(),
        "answerable": answerable,
        "unanswerable": unanswerable,
        "metrics": {
            "recall@1": ratio(r1, answerable),
            "recall@10": ratio(r10, answerable),
            "mrr": ratio(rr, answerable),
            "false_abstention": ratio(false_abstain as f64, answerable),
            "false_support": ratio(false_support as f64, unanswerable),
        },
        "results": rows,
    }))
}

/// Sets `key` to `value` unless it is already set, restoring on drop.
struct EnvDefault(&'static str, bool);

impl EnvDefault {
    fn set(key: &'static str, value: &Path) -> Self {
        let present = std::env::var_os(key).is_some();
        if !present {
            std::env::set_var(key, value);
        }
        Self(key, !present)
    }
}

impl Drop for EnvDefault {
    fn drop(&mut self) {
        if self.1 {
            std::env::remove_var(self.0);
        }
    }
}

/// Indexes `home`'s registered sources into a throwaway home with
/// `slug` as the embedding model (semantic search on) and scores there.
fn score_with_model(
    home: &Home,
    base: &DocMonkConfig,
    set: &QuerySet,
    slug: &str,
) -> Result<Value, DocMonkError> {
    let Some(spec) = docmonk_ml::manifest::embedding_model(slug) else {
        let known: Vec<&str> = docmonk_ml::manifest::EMBEDDING_MODELS
            .iter()
            .map(|m| m.slug)
            .collect();
        return Err(DocMonkError::usage(format!(
            "unknown embedding model {slug}; one of: {}",
            known.join(", ")
        )));
    };
    // The models stay where they are installed: the throwaway home reads
    // the real home's models (or DOCMONK_MODELS_DIR when set).
    let _models = EnvDefault::set("DOCMONK_MODELS_DIR", &home.models_dir());
    let _ocr = EnvDefault::set("DOCMONK_OCR_MODELS_DIR", &home.models_dir().join("ocrs"));
    let root = docmonk_ml::embedder::models_root(Some(&home.models_dir()));
    docmonk_ml::LazyEmbedder::new(root.clone(), spec, base.indexing.embedding_batch_size)
        .get()
        .map_err(|e| {
            DocMonkError::usage(format!(
                "embedding model {slug} is not installed under {} ({e})",
                root.display()
            ))
        })?;
    let run_dir = home.root().join("eval").join("runs").join(slug);
    let _ = std::fs::remove_dir_all(&run_dir);
    let run = Home::new(&run_dir);
    run.ensure_layout().map_err(generic)?;
    let mut cfg = base.clone();
    cfg.search.semantic = true;
    cfg.search.embedding_model = slug.to_owned();
    cfg.updates.enabled = false;
    cfg.storage = Default::default();
    docmonk_config::write_user_config(&cfg, &run).map_err(generic)?;
    let mut catalog = crate::sources::catalog(&run)?;
    for s in crate::sources::catalog(home)?.list(true)? {
        catalog.add(
            &s.path,
            s.include_patterns.clone(),
            s.exclude_patterns.clone(),
        )?;
    }
    let sources = crate::indexing::selected_sources(&run, None)?;
    crate::indexing::index_sources(&run, &sources, "index", |_| {})?;
    let cfg = load(&run)?;
    let out = score(&run, &cfg, set);
    crate::query::release_pooled_stores();
    out
}

/// `eval run`: scores the recorded queries on the real index, or on a
/// throwaway index per model in `models`.
pub fn run(home: &Home, models: &[String]) -> Result<Value, DocMonkError> {
    let set = load_queries(home)?;
    if set.queries.is_empty() {
        return Err(DocMonkError::usage(
            "no queries recorded yet; add some with `docmonk eval record \"<query>\" --expect <path>[:line]`",
        ));
    }
    let cfg = load(home)?;
    let runs = if models.is_empty() {
        vec![score(home, &cfg, &set)?]
    } else {
        models
            .iter()
            .map(|m| score_with_model(home, &cfg, &set, m.trim()))
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(json!({
        "queries_file": queries_path(home).to_string_lossy(),
        "runs": runs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expectations_parse_and_match() {
        let e = Expect::parse("src/a.rs:12").unwrap();
        assert_eq!(e.line, Some(12));
        assert!(e.matches("/w/proj/src/a.rs", Some((10, 14))));
        assert!(!e.matches("/w/proj/src/a.rs", Some((13, 20))));
        assert!(!e.matches("/w/proj/src/a.rs", None));
        assert!(!e.matches("/w/proj/xsrc/a.rs", Some((10, 14))));
        let f = Expect::parse("docs/refunds.md").unwrap();
        assert!(f.matches("C:\\w\\docs\\refunds.md", None));
        assert!(Expect::parse("  ").is_err());
        // A Windows drive letter is not a line number.
        assert_eq!(Expect::parse("C:/w/a.md").unwrap().line, None);
    }

    #[test]
    fn first_hit_is_one_based() {
        let ev = vec![
            json!({"path": "/p/other.md"}),
            json!({"path": "/p/src/a.rs", "line_start": 3, "line_end": 9}),
        ];
        assert_eq!(
            first_hit(&ev, &[Expect::parse("src/a.rs:5").unwrap()]),
            Some(2)
        );
        assert_eq!(first_hit(&ev, &[Expect::parse("nope.rs").unwrap()]), None);
    }
}
