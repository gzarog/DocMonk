# Relationship-building performance

`PLAN.json` is the optimization plan this directory tracks (its version suffix is dropped: the clean-slate audit forbids version naming). `REPORT.md` is the before/after report; the `*.json` files are the measurements it is built from.

Measurements in this directory are labelled by where they came from:

- **measured (SQLite, synthetic)**: produced in this repository by the relationship benchmark against a generated corpus. No proprietary source is used.
- **measured (in-process server engine)**: request and record counts against the benchmark's in-memory engine, which implements only the requests graph publication sends. These are counts, not cluster timings.
- **not measured**: live-cluster timings and the real 8,205-file workload. They need a live OpenSearch/Elasticsearch or a private run.

## Running the benchmark

```bash
cargo build --release -p docmonk-service --bin docmonk-graph-bench
target/release/docmonk-graph-bench --files 8100 --docs 105 --runs 5 --warmup 1 --out result.json
```

| Flag | Meaning |
|---|---|
| `--files N` / `--docs M` | Generated C# files (controllers, services, models, in groups of three) and Markdown notes. `8100`/`105` gives 8,203 files and about 54k entities, comparable to an 8,205-file / 57,442-entity source. |
| `--runs R` / `--warmup W` | Measured and discarded repetitions; the summary reports median and p95. |
| `--scenarios a,b` | Measure only these scenarios (the others still apply their change, unmeasured). |
| `--server none` | Skip the server publication. |
| `--fact-cache` | Run with `indexing.graph_fact_cache` on. |
| `--graph-batch N` | Files parsed per graph batch (default 256). |
| `--workers K` | Parser threads (default: CPUs). |
| `--work-dir DIR` | Where the corpus and home live (default `<temp>/docmonk-graph-bench`). Ids derive from this path, so keep it fixed when comparing fingerprints of two builds, and never run two instances on one directory. |

Scenarios: `cold`, `no_change`, `equivalent_republish`, `body_edit`, `rename_definition`, `add_file`, `remove_file`, `doc_edit`. Each records the plan action, index/graph/server/full-pipeline seconds, CPU seconds and peak RSS (Linux), per-phase timings, SQL statement and request counts, and two fingerprints: the local graph (every edge and link field) and the server's per-file record digests. Equal fingerprints across runs or builds mean byte-identical graphs.

Correctness and cost guards that run in CI live in the tests, not here: `crates/docmonk-knowledge/tests/scaling.rs` (incremental equals full, and a one-file edit costs the same number of SQL statements at four times the source size), plus the per-phase equivalence tests of each change.
