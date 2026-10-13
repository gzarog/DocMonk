//! P0 retrieval evaluation (ADR 0033, P0-R04).
//!
//! A deterministic corpus of three sources (two C# services and an HR
//! document library, English and Greek, with EML messages carrying text
//! attachments) is generated together with **250 labeled queries**. The
//! labels come from the generator (each query is written from one fact the
//! generator planted, and its expected files are where that fact lives).
//! They are *not* independently human-labeled; they test whether retrieval
//! finds planted facts, routes, filters, decomposes and abstains correctly.
//! The repository's existing 72 golden queries keep their own floors in
//! `docmonk-retrieval/tests/quality.rs` and are untouched.
//!
//! Queries are split into `dev` and `heldout` by a fixed hash of their id.
//! Floors (`fixtures/p0/eval-gates.json`) apply to the held-out split per
//! category and per arm. `RAGMONK_EVAL_OUT=path` writes the full report
//! (metrics, per-query failures with route and stage diagnostics,
//! latency); `RAGMONK_BLESS=1` rewrites the floors from this run minus a
//! fixed margin.

#![allow(clippy::needless_range_loop)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use docmonk_config::RagMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use serde_json::{json, Value};

const DOMAINS: &[&str] = &[
    "Zarvex", "Quillon", "Brintek", "Morvala", "Tessaro", "Feldric", "Ombrix", "Calvane", "Drevon",
    "Hyskal", "Jorrin", "Kelvaro", "Lumdra", "Nexpra", "Pyrell", "Rostiva", "Selvane", "Turvix",
    "Ulmara", "Vintor",
];
const THINGS: &[(&str, &str)] = &[
    ("retry", "attempts"),
    ("timeout", "seconds"),
    ("batch", "records"),
    ("approval", "euros"),
    ("retention", "days"),
];
const GREEK: &[(&str, &str, &str)] = &[
    ("Άδεια", "αδείας", "ημέρες"),
    ("Τηλεργασία", "τηλεργασίας", "ημέρες"),
    ("Εκπαίδευση", "εκπαίδευσης", "ώρες"),
    ("Αποζημίωση", "αποζημίωσης", "ευρώ"),
    ("Υπερωρίες", "υπερωριών", "ώρες"),
    ("Εξοπλισμός", "εξοπλισμού", "ευρώ"),
];

#[derive(Debug, Clone)]
struct Query {
    id: String,
    category: &'static str,
    text: String,
    /// Expected relative paths (any order); empty for a negative query.
    expected: Vec<String>,
    /// For comparisons: every expected source must appear in the top 10.
    sources: Vec<&'static str>,
    filters: SearchFilters,
}

struct Corpus {
    _dir: tempfile::TempDir,
    home: Home,
    queries: Vec<Query>,
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn number(i: usize, j: usize) -> usize {
    (i * 37 + j * 11) % 90 + 7
}

/// Generates the corpus and its labeled queries.
fn generate() -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = RagMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.indexing.max_parallel_sources = 3;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let roots = ["billing", "ledger", "hrdocs"].map(|n| dir.path().join(n));
    let mut q: Vec<Query> = Vec::new();
    let mut push =
        |category, text: String, expected: Vec<String>, sources: Vec<&'static str>, filters| {
            let id = format!("q{:03}", q.len() + 1);
            q.push(Query {
                id,
                category,
                text,
                expected,
                sources,
                filters,
            });
        };
    // --- policy documents: one fact per (domain, thing) -----------------
    // Billing/ledger docs: the first 10 domains belong to billing, the
    // next 10 to ledger.
    for (i, d) in DOMAINS.iter().enumerate() {
        let (src, root) = if i < 10 {
            ("billing", &roots[0])
        } else {
            ("ledger", &roots[1])
        };
        let rel = format!("docs/{}-operations.md", d.to_lowercase());
        let mut body = format!("# {d} operations guide\n\n");
        for (j, (thing, unit)) in THINGS.iter().enumerate() {
            body += &format!(
                "## {} policy\n\nThe {thing} limit for {d} transfers is {} {unit}. Changes need sign-off by the {d} owner.\n\n",
                capital(thing),
                number(i, j)
            );
        }
        write(root, &rel, &body);
        // 3 document-fact queries per domain (60).
        for (j, (thing, _)) in THINGS.iter().enumerate().take(3) {
            push(
                "document_fact",
                format!("what is the {thing} limit for {d} transfers"),
                vec![rel.clone()],
                vec![src],
                SearchFilters::default(),
            );
            let _ = j;
        }
        // Ambiguous single keyword (20).
        push(
            "ambiguous",
            d.to_lowercase(),
            vec![rel.clone()],
            vec![src],
            SearchFilters::default(),
        );
        // --- C# code: a service class per domain calling the next domain.
        let next = DOMAINS[(i + 1) % 10 + if i < 10 { 0 } else { 10 }];
        let cls = format!("{d}TransferService");
        let callee = format!("{next}TransferService");
        let code_rel = format!("src/{d}/{cls}.cs");
        write(
            root,
            &code_rel,
            &format!(
                "namespace Corp.{d}\n{{\n    public class {cls}\n    {{\n        private readonly {callee} _next = new {callee}();\n        public int Settle{d}(int amount)\n        {{\n            return _next.Settle{next}(amount) + {};\n        }}\n    }}\n}}\n",
                number(i, 9)
            ),
        );
        // Exact symbol (20) and navigation (20): who calls Settle<next>.
        push(
            "exact_symbol",
            cls.clone(),
            vec![code_rel.clone()],
            vec![src],
            SearchFilters::default(),
        );
        push(
            "code_navigation",
            format!("who calls Settle{next}"),
            vec![code_rel.clone()],
            vec![src],
            SearchFilters::default(),
        );
        push(
            "impact",
            format!("what breaks if Settle{next} changes"),
            vec![code_rel.clone()],
            vec![src],
            SearchFilters::default(),
        );
    }
    // --- comparisons across sources (30) ---------------------------------
    for k in 0..30 {
        let a = DOMAINS[k % 10];
        let b = DOMAINS[10 + (k * 3) % 10];
        let (thing, _) = THINGS[k % THINGS.len()];
        push(
            "comparison",
            format!("compare {a} and {b} {thing} limits"),
            vec![
                format!("docs/{}-operations.md", a.to_lowercase()),
                format!("docs/{}-operations.md", b.to_lowercase()),
            ],
            vec!["billing", "ledger"],
            SearchFilters::default(),
        );
    }
    // --- HR documents: Greek and English (30) ----------------------------
    for (g, (title, genitive, unit)) in GREEK.iter().enumerate() {
        let rel = format!("policies/gr-{g}.md");
        let n = 5 + g * 3;
        write(
            &roots[2],
            &rel,
            &format!("# Πολιτική {title}\n\nΤο όριο {genitive} είναι {n} {unit} ανά έτος για κάθε υπάλληλο.\n"),
        );
        for phrase in [
            format!("ποιο είναι το όριο {genitive}"),
            format!("πολιτική {}", title.to_lowercase()),
            format!("όριο {genitive} ανά έτος"),
        ] {
            push(
                "greek",
                phrase,
                vec![rel.clone()],
                vec!["hrdocs"],
                SearchFilters::default(),
            );
        }
    }
    for k in 0..12 {
        let topic = [
            "parental",
            "sabbatical",
            "relocation",
            "mentoring",
            "wellness",
            "commuting",
        ][k % 6];
        let rel = format!("policies/en-{topic}.md");
        if k < 6 {
            write(
                &roots[2],
                &rel,
                &format!("# {} programme\n\nEmployees may request the {topic} programme after {} months of service.\n", capital(topic), 3 + k),
            );
        }
        push(
            "english_hr",
            format!("when can employees request the {topic} programme"),
            vec![rel],
            vec!["hrdocs"],
            SearchFilters::default(),
        );
    }
    // --- email attachments (10) -------------------------------------------
    for e in 0..10 {
        let d = DOMAINS[e];
        let rel = format!("mail/{}-audit.eml", d.to_lowercase());
        write(
            &roots[2],
            &rel,
            &format!(
                "From: audit@corp.example\r\nTo: ops@corp.example\r\nSubject: {d} audit\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nPlease see the attached findings.\r\n--b\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"findings.txt\"\r\n\r\nAudit finding for {d}: the vault rotation interval was {} hours.\r\n--b--\r\n",
                12 + e
            ),
        );
        push(
            "email_attachment",
            format!("vault rotation interval audit finding for {d}"),
            vec![rel],
            vec!["hrdocs"],
            SearchFilters::default(),
        );
    }
    // --- hard-filter queries (10): the answer exists in another source -----
    for k in 0..10 {
        let d = DOMAINS[k];
        push(
            "filtered",
            format!("what is the retry limit for {d} transfers"),
            Vec::new(),
            Vec::new(),
            SearchFilters {
                source_ids: vec!["@ledger".into()],
                ..Default::default()
            },
        );
    }
    // --- negative / no-answer (28) -------------------------------------
    for t in [
        "quantum teleportation budget",
        "submarine maintenance schedule",
        "volcano insurance premium",
        "llama grooming allowance",
        "asteroid mining royalties",
        "glacier tourism permits",
        "orchestra tuning frequency",
        "falconry license renewal",
        "cathedral bell inspection",
        "zeppelin hangar lease",
    ]
    .iter()
    {
        push(
            "negative",
            format!("what is the {t}"),
            Vec::new(),
            Vec::new(),
            SearchFilters::default(),
        );
        push(
            "negative",
            format!("{t} policy details"),
            Vec::new(),
            Vec::new(),
            SearchFilters::default(),
        );
        {
            push(
                "negative",
                format!("who approves the {t}"),
                Vec::new(),
                Vec::new(),
                SearchFilters::default(),
            );
        }
    }
    assert_eq!(q.len(), 250, "the dataset has exactly 250 queries");

    // Register and index (local mode, all three sources in parallel).
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    let mut ids = BTreeMap::new();
    for (name, root) in ["billing", "ledger", "hrdocs"].iter().zip(&roots) {
        let s = cat.add(root.to_str().unwrap(), vec![], vec![]).unwrap();
        ids.insert(format!("@{name}"), s.id);
    }
    let sources = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {})
        .unwrap()
        .into_result()
        .unwrap();
    for query in &mut q {
        for s in &mut query.filters.source_ids {
            if let Some(id) = ids.get(s.as_str()) {
                *s = id.clone();
            }
        }
    }
    Corpus {
        _dir: dir,
        home,
        queries: q,
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Deterministic dev/held-out split (about one third held out).
fn split(id: &str) -> &'static str {
    let h = id.bytes().fold(2166136261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16777619)
    });
    if h % 3 == 0 {
        "heldout"
    } else {
        "dev"
    }
}

#[derive(Default, Clone, Copy)]
struct Acc {
    n: f64,
    /// Positive (answerable) queries: the denominator of retrieval metrics.
    pos: f64,
    r1: f64,
    r3: f64,
    r5: f64,
    r10: f64,
    mrr: f64,
    ndcg: f64,
    cite_prec: f64,
    coverage: f64,
    abstain_ok: f64,
}

impl Acc {
    fn json(&self) -> Value {
        let over = |v: f64, n: f64| {
            if n == 0.0 {
                Value::Null
            } else {
                json!(((v / n) * 1000.0).round() / 1000.0)
            }
        };
        let p = |v: f64| over(v, self.pos);
        json!({
            "n": self.n as u64, "answerable": self.pos as u64,
            "recall@1": p(self.r1), "recall@3": p(self.r3), "recall@5": p(self.r5),
            "recall@10": p(self.r10), "mrr": p(self.mrr), "ndcg@10": p(self.ndcg),
            "citation_precision@5": p(self.cite_prec), "source_coverage": p(self.coverage),
            "abstention_accuracy": over(self.abstain_ok, self.n),
        })
    }
}

struct Arm {
    name: &'static str,
    tweak: fn(&mut RagMonkConfig),
}

const ARMS: &[Arm] = &[
    Arm {
        name: "lexical_baseline",
        tweak: |c| {
            c.search.routing.enabled = false;
            c.search.decomposition.enabled = false;
            c.search.diversity.enabled = false;
        },
    },
    Arm {
        name: "routed",
        tweak: |c| {
            c.search.routing.enabled = true;
            c.search.decomposition.enabled = false;
            c.search.diversity.enabled = false;
        },
    },
    Arm {
        name: "routed_decomposed",
        tweak: |c| {
            c.search.routing.enabled = true;
            c.search.decomposition.enabled = true;
            c.search.diversity.enabled = false;
        },
    },
    Arm {
        name: "routed_decomposed_diverse",
        tweak: |c| {
            c.search.routing.enabled = true;
            c.search.decomposition.enabled = true;
            c.search.diversity.enabled = true;
        },
    },
];

fn rel_of(path: &str) -> String {
    path.replace('\\', "/")
}

#[test]
fn p0_retrieval_eval() {
    let corpus = generate();
    let base = docmonk_service::load(&corpus.home).unwrap();
    let mut report = json!({
        "dataset": {"queries": corpus.queries.len(), "labels": "generator-derived (not independently human-labeled)", "seed": "fixed"},
        "arms": {},
    });
    let mut failures = Vec::new();
    for arm in ARMS {
        let mut cfg = base.clone();
        (arm.tweak)(&mut cfg);
        let mut acc: BTreeMap<(String, String), Acc> = BTreeMap::new();
        let mut lat = Vec::new();
        for q in &corpus.queries {
            let t = Instant::now();
            let v = evidence_value(
                &corpus.home,
                &cfg,
                &EvidenceRequest {
                    query: &q.text,
                    filters: q.filters.clone(),
                    limit: 10,
                },
            )
            .unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            let ev: Vec<&Value> = v["evidence"].as_array().unwrap().iter().collect();
            let paths: Vec<String> = ev
                .iter()
                .map(|e| rel_of(e["path"].as_str().unwrap_or_default()))
                .collect();
            let hit_at = |k: usize| {
                paths
                    .iter()
                    .take(k)
                    .any(|p| q.expected.iter().any(|x| p.ends_with(x.as_str())))
            };
            let first = paths
                .iter()
                .position(|p| q.expected.iter().any(|x| p.ends_with(x.as_str())));
            let mut a = Acc {
                n: 1.0,
                ..Acc::default()
            };
            if q.expected.is_empty() {
                // Negative / filtered: correct means no in-scope support.
                a.abstain_ok = f64::from(u8::from(
                    v["verdict"] == "insufficient_evidence" || ev.is_empty(),
                ));
                if q.category == "filtered" {
                    // The hard filter must hold: nothing from other sources.
                    let leaked = ev.iter().any(|e| {
                        !q.filters
                            .source_ids
                            .iter()
                            .any(|s| e["source_id"] == s.as_str())
                    });
                    assert!(!leaked, "filter bypass on {}: {v}", q.id);
                    a.abstain_ok = f64::from(u8::from(!leaked));
                }
            } else {
                a.pos = 1.0;
                a.r1 = f64::from(u8::from(hit_at(1)));
                a.r3 = f64::from(u8::from(hit_at(3)));
                a.r5 = f64::from(u8::from(hit_at(5)));
                a.r10 = f64::from(u8::from(hit_at(10)));
                a.mrr = first.map_or(0.0, |r| 1.0 / (r as f64 + 1.0));
                let dcg: f64 = paths
                    .iter()
                    .take(10)
                    .enumerate()
                    .filter(|(_, p)| q.expected.iter().any(|x| p.ends_with(x.as_str())))
                    .map(|(i, _)| 1.0 / ((i as f64) + 2.0).log2())
                    .sum();
                let idcg: f64 = (0..q.expected.len().min(10))
                    .map(|i| 1.0 / ((i as f64) + 2.0).log2())
                    .sum();
                a.ndcg = if idcg > 0.0 {
                    (dcg / idcg).min(1.0)
                } else {
                    0.0
                };
                let top5: Vec<&&Value> = ev.iter().take(5).collect();
                a.cite_prec = if top5.is_empty() {
                    0.0
                } else {
                    top5.iter().filter(|e| e["valid"] == true).count() as f64 / top5.len() as f64
                };
                a.coverage = if q.sources.len() > 1 {
                    let all = q
                        .expected
                        .iter()
                        .all(|x| paths.iter().any(|p| p.ends_with(x.as_str())));
                    f64::from(u8::from(all))
                } else {
                    1.0
                };
                // A positive query must not abstain.
                a.abstain_ok = f64::from(u8::from(v["verdict"] != "insufficient_evidence"));
            }
            let ok = if q.expected.is_empty() {
                a.abstain_ok == 1.0
            } else {
                a.r10 == 1.0 && a.coverage == 1.0
            };
            if !ok && arm.name == "routed_decomposed" {
                failures.push(json!({
                    "id": q.id, "category": q.category, "split": split(&q.id), "query": q.text,
                    "expected": q.expected, "verdict": v["verdict"], "intent": v["plan"]["intent"],
                    "strategies": v["plan"]["strategies"], "subqueries": v["plan"]["subqueries"],
                    "top": paths.iter().take(5).collect::<Vec<_>>(), "degraded": v["diagnostics"]["degraded"],
                }));
            }
            for key in [
                (split(&q.id).to_owned(), q.category.to_owned()),
                (split(&q.id).to_owned(), "all".to_owned()),
            ] {
                let e = acc.entry(key).or_default();
                e.n += a.n;
                e.pos += a.pos;
                e.r1 += a.r1;
                e.r3 += a.r3;
                e.r5 += a.r5;
                e.r10 += a.r10;
                e.mrr += a.mrr;
                e.ndcg += a.ndcg;
                e.cite_prec += a.cite_prec;
                e.coverage += a.coverage;
                e.abstain_ok += a.abstain_ok;
            }
        }
        lat.sort_by(f64::total_cmp);
        let pct = |p: f64| (lat[((lat.len() - 1) as f64 * p) as usize] * 10.0).round() / 10.0;
        let mut m = serde_json::Map::new();
        for ((s, c), a) in &acc {
            m.entry(s.clone()).or_insert_with(|| json!({}))[c] = a.json();
        }
        report["arms"][arm.name] =
            json!({"metrics": m, "latency_ms": {"p50": pct(0.5), "p95": pct(0.95)}});
    }
    report["failures_routed_decomposed"] = json!(failures);
    if let Ok(out) = std::env::var("RAGMONK_EVAL_OUT") {
        std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    // --- gates ---------------------------------------------------------------
    let gates_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p0/eval-gates.json");
    if std::env::var_os("RAGMONK_BLESS").is_some() {
        // Floors: held-out metrics of every arm minus a 0.05 margin.
        let mut floors = serde_json::Map::new();
        for arm in ARMS {
            let held = &report["arms"][arm.name]["metrics"]["heldout"];
            let mut f = serde_json::Map::new();
            for (cat, m) in held.as_object().unwrap() {
                let mut g = serde_json::Map::new();
                for k in ["recall@10", "mrr", "source_coverage", "abstention_accuracy"] {
                    let Some(v) = m[k].as_f64() else { continue };
                    g.insert(
                        k.into(),
                        json!(((v - 0.05).max(0.0) * 1000.0).round() / 1000.0),
                    );
                }
                f.insert(cat.clone(), Value::Object(g));
            }
            floors.insert(arm.name.into(), Value::Object(f));
        }
        std::fs::write(
            &gates_path,
            serde_json::to_string_pretty(
                &json!({"split": "heldout", "margin": 0.05, "floors": floors}),
            )
            .unwrap()
                + "\n",
        )
        .unwrap();
    }
    let gates: Value = serde_json::from_str(
        &std::fs::read_to_string(&gates_path).expect("eval gates; run with RAGMONK_BLESS=1 once"),
    )
    .unwrap();
    let mut regressions = Vec::new();
    for (arm, cats) in gates["floors"].as_object().unwrap() {
        for (cat, floors) in cats.as_object().unwrap() {
            for (k, floor) in floors.as_object().unwrap() {
                let got = report["arms"][arm]["metrics"]["heldout"][cat][k]
                    .as_f64()
                    .unwrap_or(0.0);
                if got + 1e-9 < floor.as_f64().unwrap() {
                    regressions.push(format!("{arm}/{cat}/{k}: {got} < floor {floor}"));
                }
            }
        }
    }
    assert!(
        regressions.is_empty(),
        "held-out quality regressions:\n{}\nfailing queries: {}",
        regressions.join("\n"),
        serde_json::to_string_pretty(&report["failures_routed_decomposed"]).unwrap()
    );
    // Hard functional gates, independent of floors: filters never leak
    // (asserted per query above) and negatives abstain in the default arm.
    let neg = report["arms"]["routed_decomposed"]["metrics"]["heldout"]["negative"]
        ["abstention_accuracy"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(neg >= 0.9, "negative abstention {neg}");
}

#[test]
#[ignore = "diagnostic: prints one query's evidence"]
fn p0_eval_debug_one() {
    let c = generate();
    let cfg = docmonk_service::load(&c.home).unwrap();
    let q = std::env::var("Q")
        .unwrap_or_else(|_| "what is the retry limit for Zarvex transfers".into());
    let v = evidence_value(
        &c.home,
        &cfg,
        &EvidenceRequest {
            query: &q,
            filters: SearchFilters::default(),
            limit: 5,
        },
    )
    .unwrap();
    for e in v["evidence"].as_array().unwrap() {
        println!("{} {} {} {}", e["tier"], e["kind"], e["path"], e["title"]);
    }
    println!("{}", v["plan"]);
}
