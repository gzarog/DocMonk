//! `docmonk status [--json] [--watch] [--interval S] [--errors] [--verbose]`
//!
//! Renders the canonical [`StatusReport`] of `docmonk-status`; `--json`
//! prints it unchanged inside the CLI's JSON envelope. The text view is
//! plain ASCII (safe in any terminal and when piped) and shows unknown
//! values as `N/A`, never as `0`.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use docmonk_core::errors::DocMonkError;
use docmonk_service::status::{options, StatusSession};
use docmonk_status::model::*;
use docmonk_status::snapshot::age_seconds;

use crate::{prepared_home, print_json};

#[derive(clap::Args)]
pub struct StatusArgs {
    /// Machine-readable output (the canonical status report).
    #[arg(long = "json")]
    json: bool,
    /// Refresh continuously (cheap refresh; published counts are re-read
    /// on publication); Ctrl+C to exit. Cannot be combined with --json.
    #[arg(long)]
    watch: bool,
    /// Seconds between --watch refreshes.
    #[arg(long, default_value_t = 2.0)]
    interval: f64,
    /// Only problematic sources, problems and recent errors.
    #[arg(long)]
    errors: bool,
    /// Every source with full ids and paths, locks, runs and diagnostics.
    #[arg(long, short = 'v')]
    verbose: bool,
}

/// Sources listed individually in the default view before completed ones
/// are folded into one line.
const DEFAULT_SOURCE_ROWS: usize = 25;

fn na<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "N/A".into(), |x| x.to_string())
}

fn age(seconds: Option<f64>) -> String {
    match seconds {
        None => "N/A".into(),
        Some(s) if s < 60.0 => format!("{s:.0}s ago"),
        Some(s) if s < 3600.0 => format!("{:.0}m ago", s / 60.0),
        Some(s) if s < 86400.0 => format!("{:.1}h ago", s / 3600.0),
        Some(s) => format!("{:.1}d ago", s / 86400.0),
    }
}

fn since(ts: Option<&str>) -> String {
    match ts {
        None => "-".into(),
        Some(t) => age(age_seconds(Some(t), Utc::now())),
    }
}

/// Keeps the end of long values (`...tail`), ASCII only.
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n - (max - 3)).collect();
    format!("...{tail}")
}

fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(c.chars().count());
            }
        }
    }
    let line = |cells: &[String]| {
        let s: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths.get(i).copied().unwrap_or(0)))
            .collect();
        format!("{}\n", s.join("  ").trim_end())
    };
    let mut out = line(&headers.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>());
    for r in rows {
        out += &line(r);
    }
    out
}

/// What `--watch` compares between two samples.
#[derive(Clone)]
struct Sample {
    at: Instant,
    runs: BTreeSet<(Option<String>, String)>,
    processed: Option<u64>,
    published_indexed: Option<u64>,
}

fn sample(r: &StatusReport) -> Sample {
    let live: Vec<&LiveRun> = r
        .indexer
        .workers
        .iter()
        .filter(|w| w.liveness == Liveness::Live)
        .collect();
    Sample {
        at: Instant::now(),
        runs: live
            .iter()
            .map(|w| (w.run_id.clone(), w.source_id.clone()))
            .collect(),
        processed: live.iter().map(|w| w.processed).sum(),
        published_indexed: r.summary.files.published_indexed,
    }
}

/// Throughput over live passes, only between comparable samples (the same
/// passes, both with counters).
fn throughput(prev: &Sample, cur: &Sample) -> Option<f64> {
    if prev.runs != cur.runs || cur.runs.is_empty() {
        return None;
    }
    let (a, b) = (prev.processed?, cur.processed?);
    let secs = cur.at.duration_since(prev.at).as_secs_f64();
    (b >= a && secs > 0.0).then(|| (b - a) as f64 / secs)
}

fn header(r: &StatusReport) -> String {
    let b = &r.backend;
    let mut where_ = b.kind.as_str().to_owned();
    if let Some(e) = &b.endpoint {
        where_ += &format!(" {e}");
    }
    if let Some(p) = &b.index_prefix {
        where_ += &format!(" prefix={p}");
    }
    let mut s = format!(
        "DocMonk status  [{} mode, {where_}, {}]\n",
        match r.mode {
            Mode::Local => "local",
            Mode::Server => "server",
        },
        if b.authoritative {
            "authoritative"
        } else {
            "not authoritative"
        }
    );
    s += &format!(
        "Snapshot {} ({})\n",
        r.observed_at,
        age(age_seconds(Some(&r.observed_at), Utc::now()))
    );
    s += &format!(
        "Health: {}  ({} problem(s))",
        r.health.state.as_str().to_uppercase(),
        r.health.problem_count
    );
    if let Some(c) = &b.cluster {
        let health = c.health.map_or("unknown", |h| match h {
            ClusterHealth::Green => "green",
            ClusterHealth::Yellow => "yellow",
            ClusterHealth::Red => "red",
        });
        s += &format!(
            "  Cluster: {health} (unassigned shards {})",
            na(c.unassigned_shards)
        );
    }
    s + "\n"
}

fn summary(r: &StatusReport, thr: Option<Option<f64>>) -> String {
    let m = &r.summary;
    let src = &m.sources;
    let f = &m.files;
    let mut s = format!(
        "Sources: {} registered, {} enabled, {} active, {} failed, {} offline\n",
        src.registered, src.enabled, src.active, src.failed, src.offline
    );
    s += &format!(
        "Published: {} indexed, {} failed, {} retrying of {} files; {} entities, {} documents, {} chunks, {} relationships\n",
        na(f.published_indexed),
        na(f.failed),
        na(f.retrying),
        na(f.discovered),
        na(m.entities),
        na(m.documents),
        na(m.chunks),
        na(m.relationships),
    );
    let hosts: BTreeSet<&str> = r
        .indexer
        .workers
        .iter()
        .filter_map(|w| w.host.as_deref())
        .collect();
    s += &format!(
        "In progress: {} pass(es) over {} source(s) on {} host(s); files pending in current runs: {}",
        r.indexer.active_run_count,
        r.indexer.active_source_count,
        hosts.len(),
        na(f.pending_in_current_runs)
    );
    if let Some(t) = thr {
        s += &format!(
            "; throughput: {}",
            t.map_or_else(|| "N/A".into(), |v| format!("{v:.1} files/s"))
        );
    }
    s + "\n"
}

/// `1h02m`, `3m05s`, `42s`.
fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

fn progress(w: &LiveRun) -> String {
    if let Some(e) = &w.embedding {
        let rate = e
            .rate_per_second
            .map(|r| format!(", {r:.1}/s"))
            .unwrap_or_default();
        let eta = e
            .eta_seconds
            .map(|s| format!(", ETA {}", duration(s)))
            .unwrap_or_default();
        return format!("embedding {}/{}{rate}{eta}", e.processed, e.planned);
    }
    if let Some(g) = &w.relationships {
        return match (g.processed, g.planned) {
            (Some(p), Some(t)) => format!("{p}/{t} files"),
            _ => "N/A".into(),
        };
    }
    match (w.processed, w.planned, w.percentage) {
        (Some(p), Some(t), Some(pct)) => format!("{p}/{t} ({pct:.0}%)"),
        (Some(p), None, _) => format!("{p}/N/A"),
        _ => "N/A".into(),
    }
}

fn workers(r: &StatusReport, verbose: bool) -> String {
    if r.indexer.workers.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = r
        .indexer
        .workers
        .iter()
        .map(|w| {
            let mut row = vec![
                if verbose {
                    w.source_id.clone()
                } else {
                    clip(&w.source_id, 24)
                },
                na(w.host.as_deref()),
                format!("{:?}", w.liveness).to_lowercase(),
                match (
                    w.stage.as_deref(),
                    w.relationships.as_ref().and_then(|g| g.phase.as_deref()),
                ) {
                    (Some(stage), Some(phase)) => format!("{stage}: {phase}"),
                    (stage, _) => na(stage),
                },
                progress(w),
                na(w.indexed),
                na(w.failed),
                na(w.retry),
                age(w.heartbeat_age_seconds),
            ];
            if verbose {
                row.push(na(w.run_id.as_deref()));
                row.push(na(w.pid));
                row.push(na(w.lease_token));
            }
            row
        })
        .collect();
    let mut headers = vec![
        "Source",
        "Host",
        "Run",
        "Stage",
        "Progress",
        "Indexed",
        "Failed",
        "Retry",
        "Heartbeat",
    ];
    if verbose {
        headers.extend(["Run id", "PID", "Lease"]);
    }
    let mut s = "== Active passes ==\n".to_owned();
    s += &table(&headers, &rows);
    if let Some(res) = &r.indexer.resources {
        if verbose {
            s += &format!("Permits: {res}\n");
        } else if let Some(obj) = res.as_object() {
            let parts: Vec<String> = obj
                .iter()
                .filter_map(|(k, v)| {
                    Some(format!(
                        "{k} {}/{}",
                        v.get("in_use")?.as_u64()?,
                        v.get("capacity")?.as_u64()?
                    ))
                })
                .collect();
            if !parts.is_empty() {
                s += &format!("Permits in use: {}\n", parts.join(", "));
            }
        }
    }
    s
}

fn wants(s: &SourceStatus, errors_only: bool) -> bool {
    let troubled = s.index_state.is_problem()
        || s.published
            .as_ref()
            .is_some_and(|p| p.failed > 0 || p.retrying > 0)
        || s.last_error.is_some()
        || !s.warnings.is_empty()
        || s.relationship_state
            .as_ref()
            .is_some_and(|r| matches!(r.state.as_str(), "failed" | "stale"))
        || s.dependency_state
            .as_ref()
            .is_some_and(|d| d.state != "current");
    !errors_only || troubled
}

/// The relationship graph note of a source row: always in verbose mode,
/// otherwise only while it is not ready (indexing is reported separately).
fn relationship_note(s: &SourceStatus, verbose: bool) -> Option<String> {
    let r = s.relationship_state.as_ref()?;
    if !verbose && matches!(r.state.as_str(), "ready" | "disabled" | "pending") {
        return None;
    }
    let mut note = format!("relationships: {}", r.state);
    if verbose {
        if let Some(g) = &r.generation {
            note += &format!(" (generation {}", clip(g, 40));
            if let Some(b) = &r.base_build_id {
                note += &format!(", base {}", clip(b, 40));
            }
            note += ")";
        }
        if let Some(t) = &r.last_success_at {
            note += &format!(", last success {t}");
        }
    }
    if let Some(why) = r.last_error.as_deref().or(r.stale_reason.as_deref()) {
        note += &format!(": {}", clip(&ascii(why), if verbose { 400 } else { 100 }));
    }
    Some(note)
}

/// The cross-source dependency note of a source row: always in verbose
/// mode, otherwise only while they are not current (with the retry).
fn dependency_note(s: &SourceStatus, verbose: bool) -> Option<String> {
    let d = s.dependency_state.as_ref()?;
    if !verbose && d.state == "current" {
        return None;
    }
    let mut note = format!("dependencies: {}", d.state);
    if verbose && !d.producers.is_empty() {
        note += &format!(" (producers {})", clip(&d.producers.join(", "), 200));
    }
    if let Some(why) = d.reason.as_deref().filter(|_| d.state != "current") {
        note += &format!(": {}", clip(&ascii(why), if verbose { 400 } else { 100 }));
    }
    if let Some(r) = &d.retry {
        note += &format!("; retry {} attempt(s) [{}]", r.attempts, r.code);
        if let Some(t) = &r.next_attempt_at {
            note += &format!(", next {t}");
        }
    }
    Some(note)
}

fn sources(r: &StatusReport, errors_only: bool, verbose: bool) -> String {
    let mut rows = Vec::new();
    let mut folded = 0;
    let many = r.sources.len() > DEFAULT_SOURCE_ROWS;
    for s in r.sources.iter().filter(|s| wants(s, errors_only)) {
        if many
            && !verbose
            && !errors_only
            && s.index_state == SourceIndexState::Completed
            && !wants(s, true)
        {
            folded += 1;
            continue;
        }
        let p = s.published.as_ref();
        let live = s.live.as_ref().filter(|l| l.liveness == Liveness::Live);
        let mut row = vec![
            if verbose {
                s.source_id.clone()
            } else {
                clip(&s.source_id, 24)
            },
            s.index_state.as_str().into(),
            format!("{:?}", s.access).to_lowercase(),
            p.map_or_else(|| "N/A".into(), |p| p.indexed.to_string()),
            p.map_or_else(|| "N/A".into(), |p| p.failed.to_string()),
            p.map_or_else(|| "N/A".into(), |p| p.retrying.to_string()),
            live.map_or_else(|| "-".into(), progress),
            since(s.last_scan_at.as_deref()),
            since(s.last_error_at.as_deref()),
        ];
        if verbose {
            row.push(ascii(&s.path));
            row.push(na(s.build.active_id.as_deref()));
            row.push(na(s.build.pending_id.as_deref()));
            row.push(na(s.build.published_at.as_deref()));
        }
        rows.push(row);
        let notes: Vec<String> = s
            .warnings
            .iter()
            .cloned()
            .chain(
                s.last_error
                    .as_deref()
                    .filter(|_| verbose || s.index_state != SourceIndexState::Completed)
                    .map(|e| {
                        format!(
                            "last error: {}",
                            clip(&ascii(e), if verbose { 400 } else { 100 })
                        )
                    }),
            )
            .chain(relationship_note(s, verbose))
            .chain(dependency_note(s, verbose))
            .collect();
        for n in notes {
            rows.push(vec![String::new(), format!("  {n}")]);
        }
    }
    if rows.is_empty() && folded == 0 {
        return if errors_only {
            String::new()
        } else {
            "== Sources ==\nNo sources registered. Add one with 'docmonk add <path>'.\n".into()
        };
    }
    let mut headers = vec![
        "Source",
        "State",
        "Access",
        "Indexed",
        "Failed",
        "Retry",
        "In progress",
        "Last scan",
        "Last error",
    ];
    if verbose {
        headers.extend(["Path", "Active build", "Pending build", "Published"]);
    }
    let mut out = "== Sources ==\n".to_owned();
    out += &table(&headers, &rows);
    if folded > 0 {
        out +=
            &format!("({folded} completed source(s) without problems not shown; use --verbose)\n");
    }
    out
}

fn problems(r: &StatusReport, detailed: bool) -> String {
    let shown: Vec<&Problem> = r
        .problems
        .iter()
        .filter(|p| detailed || p.severity != Severity::Info)
        .collect();
    let limit = if detailed { r.recent_errors.len() } else { 5 };
    let errors = &r.recent_errors[..limit.min(r.recent_errors.len())];
    let mut s = String::new();
    if !shown.is_empty() {
        s += "== Problems ==\n";
        for p in shown {
            let mut scope = format!("{:?}", p.scope).to_lowercase();
            if let Some(id) = &p.source_id {
                scope += &format!(" {id}");
            }
            if let Some(h) = &p.host {
                scope += &format!(" @{h}");
            }
            s += &format!(
                "[{}] {} ({scope}): {}\n",
                format!("{:?}", p.severity).to_lowercase(),
                p.code.as_str(),
                ascii(&p.message)
            );
            if let (true, Some(h)) = (detailed, &p.hint) {
                s += &format!("    hint: {}\n", ascii(h));
            }
        }
    }
    if !errors.is_empty() {
        s += "== Recent errors (newest first) ==\n";
        for e in errors {
            s += &format!(
                "{}  {}  {}: {}: {}\n",
                e.occurred_at.as_deref().unwrap_or("time unknown"),
                e.source_id,
                ascii(e.path.as_deref().unwrap_or("-")),
                e.code,
                clip(&ascii(&e.message), if detailed { 400 } else { 120 })
            );
        }
    }
    s
}

fn diagnostics(r: &StatusReport) -> String {
    let d = &r.diagnostics;
    let mut s = format!(
        "== Diagnostics ==\nsnapshot {}  collected in {} ms with {} backend request(s); consistency {:?}\n",
        r.snapshot_id,
        na(d.collection_ms),
        na(d.request_count),
        d.consistency
    );
    for c in &d.cached_sections {
        s += &format!(
            "cached: {} ({:.0}s old, refreshed on publication or after {:.0}s)\n",
            c.section, c.age_seconds, c.ttl_seconds
        );
    }
    if let Some(l) = &r.indexer.lock {
        s += &format!(
            "home lock: {} pid={} host={} operation={} since={}\n",
            l.state,
            na(l.pid),
            na(l.hostname.as_deref()),
            na(l.operation.as_deref()),
            na(l.acquired_at.as_deref())
        );
    }
    if let Some(l) = &r.indexer.last_run {
        s += &format!(
            "last run: {} {} at {}{}\n",
            na(l.operation.as_deref()),
            l.outcome,
            na(l.completed_at.as_deref()),
            l.error
                .as_deref()
                .map(|e| format!(": {}", ascii(e)))
                .unwrap_or_default()
        );
    }
    s += &format!("stall threshold: {}s\n", r.indexer.stall_threshold_seconds);
    s
}

/// The text view.
fn render(r: &StatusReport, errors_only: bool, verbose: bool, thr: Option<Option<f64>>) -> String {
    let mut out = header(r);
    if r.diagnostics.partial {
        out += &format!(
            "WARNING: partial status; unavailable: {}\n",
            r.diagnostics.missing_sections.join(", ")
        );
    }
    if !errors_only {
        out += &summary(r, thr);
        out += &workers(r, verbose);
    }
    out += &sources(r, errors_only, verbose);
    let p = problems(r, verbose || errors_only);
    if p.is_empty() && errors_only {
        if r.diagnostics.partial {
            out += "Status is partial: problems in the unavailable sections cannot be ruled out.\n";
        } else {
            out += "No problems found.\n";
        }
    }
    out += &p;
    if verbose {
        out += &diagnostics(r);
    }
    out
}

pub fn status(a: &StatusArgs) -> Result<(), DocMonkError> {
    if a.watch && a.json {
        return Err(DocMonkError::usage(
            "--watch cannot be combined with --json (use --json for one snapshot)",
        ));
    }
    if a.interval <= 0.0 || !a.interval.is_finite() {
        return Err(DocMonkError::usage("--interval must be > 0"));
    }
    let home = prepared_home()?;
    let mut session = StatusSession::open(&home)?;
    let opts = options(session.config());
    if !a.watch {
        let report = session.collect(&opts)?;
        if a.json {
            return print_json(&report);
        }
        print!("{}", render(&report, a.errors, a.verbose, None));
        return Ok(());
    }
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }
    let mut previous: Option<Sample> = None;
    while !stop.load(Ordering::SeqCst) {
        let report = session.collect(&opts)?;
        let current = sample(&report);
        let thr = previous.as_ref().map(|p| throughput(p, &current));
        // Clear the screen and redraw.
        print!(
            "\x1b[2J\x1b[H{}",
            render(&report, a.errors, a.verbose, Some(thr.flatten()))
        );
        if let (Some(before), Some(now)) = (
            previous.as_ref().and_then(|p| p.published_indexed),
            current.published_indexed,
        ) {
            println!(
                "Published indexed since last refresh: {:+}",
                now as i64 - before as i64
            );
        }
        println!("(refreshing every {}s; Ctrl+C to exit)", a.interval);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        previous = Some(current);
        let until = Instant::now() + Duration::from_secs_f64(a.interval);
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> StatusReport {
        serde_json::from_str(include_str!(
            "../../docmonk-status/tests/fixtures/server_multi_host.json"
        ))
        .unwrap()
    }

    #[test]
    fn embedding_progress_shows_rate_and_eta() {
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(185.0), "3m05s");
        assert_eq!(duration(3725.0), "1h02m");
        let mut w = fixture().indexer.workers[0].clone();
        w.embedding = Some(docmonk_status::model::EmbeddingProgress::at(
            300,
            1200,
            Some(10.0),
        ));
        assert_eq!(progress(&w), "embedding 300/1200, 30.0/s, ETA 30s");
        w.embedding = Some(docmonk_status::model::EmbeddingProgress::at(
            0,
            1200,
            Some(1.0),
        ));
        assert_eq!(progress(&w), "embedding 0/1200");
    }

    #[test]
    fn formats_unknown_as_na_and_ages() {
        assert_eq!(na::<u64>(None), "N/A");
        assert_eq!(age(Some(5.4)), "5s ago");
        assert_eq!(age(Some(150.0)), "2m ago");
        assert_eq!(age(Some(5400.0)), "1.5h ago");
        assert_eq!(age(Some(172800.0)), "2.0d ago");
        assert_eq!(clip("abcdefghij", 6), "...hij");
        assert_eq!(ascii("ok \u{2713}"), "ok ?");
    }

    #[test]
    fn text_view_shows_live_passes_published_counts_and_problems() {
        let r = fixture();
        let out = render(&r, false, false, None);
        assert!(out.is_ascii());
        assert!(out.contains("Health: DEGRADED  (2 problem(s))"), "{out}");
        assert!(
            out.contains("Cluster: yellow (unassigned shards 7)"),
            "{out}"
        );
        assert!(out.contains("Published: 2400 indexed, 4 failed, 6 retrying of 2410 files"));
        assert!(out.contains("files pending in current runs: N/A"));
        assert!(out.contains("== Active passes =="));
        assert!(out.contains("host-a") && out.contains("host-b"));
        assert!(out.contains("120/300 (40%)"));
        assert!(
            out.contains("0/N/A"),
            "a scanning pass has no denominator: {out}"
        );
        assert!(out.contains("[warning] cluster_yellow (cluster)"));
        assert!(out.contains("docs/broken.pdf"));
        for line in out.lines() {
            assert!(line.len() <= 160, "fits a terminal: {line}");
        }
    }

    #[test]
    fn errors_view_never_claims_no_problems_on_a_partial_report() {
        let mut r = fixture();
        r.problems.clear();
        r.recent_errors.clear();
        r.sources.clear();
        assert!(render(&r, true, false, None).contains("No problems found."));
        r.diagnostics.partial = true;
        r.diagnostics.missing_sections = vec!["published (timeout)".into()];
        let out = render(&r, true, false, None);
        assert!(!out.contains("No problems found."));
        assert!(out.contains("partial"));
    }

    #[test]
    fn many_sources_fold_completed_rows_unless_verbose() {
        let mut r = fixture();
        let base = r.sources[0].clone();
        r.sources = (0..150)
            .map(|i| SourceStatus {
                source_id: format!("src_{i:03}"),
                index_state: if i == 7 {
                    SourceIndexState::Offline
                } else {
                    SourceIndexState::Completed
                },
                published: Some(PublishedCounts::default()),
                relationship_state: None,
                build: SourceBuild {
                    state: BuildState::Ready,
                    ..base.build.clone()
                },
                ..base.clone()
            })
            .collect();
        let out = render(&r, false, false, None);
        assert!(out.contains("src_007"));
        assert!(out.contains("(149 completed source(s) without problems not shown"));
        let all = render(&r, false, true, None);
        assert!(all.contains("src_149"));
        assert!(all.contains("== Diagnostics =="));
    }

    #[test]
    fn throughput_needs_comparable_samples() {
        let at = Instant::now();
        let s = |runs: &[&str], processed| Sample {
            at,
            runs: runs
                .iter()
                .map(|r| (Some("r".to_owned()), (*r).to_owned()))
                .collect(),
            processed,
            published_indexed: None,
        };
        let mut later = s(&["a"], Some(30));
        later.at = at + Duration::from_secs(10);
        assert_eq!(throughput(&s(&["a"], Some(10)), &later), Some(2.0));
        assert_eq!(
            throughput(&s(&["b"], Some(10)), &later),
            None,
            "other passes"
        );
        assert_eq!(throughput(&s(&["a"], None), &later), None);
    }
}
