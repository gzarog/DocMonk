# Relationship-building performance: report

Before/after measurements for `PLAN.json`, phases P0 to P6. Everything here is **measured (SQLite, synthetic)** on a generated C# + Markdown corpus, unless a row says it was measured against the in-process server engine (request and record counts, not cluster timings). Nothing was measured on a live cluster or on the real 8,205-file source; those rows are marked *not measured*.

## Environment

- Host: Linux container, 4 vCPU (Intel Xeon @ 2.80 GHz), 15 GB RAM, local SSD-backed disk.
- Toolchain: rustc 1.90.0 (pinned by `rust-toolchain.toml`), release profile.
- Final build: `0d6f91d` (P6). P0 reference: `cd6ffdd` (P0 plus the benchmark's stable work directory). The 1,013-file P0 baseline was measured with `a558707` (P0 and the scenario selector; identical graph code).
- Workers: 4 parser threads, graph batch 256, default bulk limits.
- Corpus at full scale: `--files 8100 --docs 105` gives 8,203 files, 53,986 entities, 148,455 edges and 14,603 links (the reported workload: 8,205 files, 57,442 entities).

## Headline (8,203 files)

| What | P0 | Now | Change |
|---|---|---|---|
| Server publication, cold | 2,287.9 s | 4.03 s | 568.4x faster |
| Server publication, one-file body edit | 2,150.6 s | 1.14 s | 1,881.1x faster |
| Local graph stage, cold | 20.39 s | 14.33 s | 1.4x faster |
| Local graph stage, one-file body edit | 1.48 s | 0.51 s | 2.9x faster |
| Local graph stage, rename referenced by 2,700 files | 726.78 s | 1.19 s | 612.3x faster |
| Local graph stage, delete a defining file | 604.91 s | 1.22 s | 495.4x faster |
| Local graph stage, document edit | 1.47 s | 1.52 s | unchanged (within run-to-run noise) |
| Index + graph + server publication, cold (end to end) | 2,319.0 s | 22.6 s | 102.4x faster |
| Index + graph + server publication, one-file body edit (end to end) | 2,153.3 s | 2.28 s | 945.0x faster |

### Against the plan's targets

| Target (plan `success_criteria`) | Measured | Verdict |
|---|---|---|
| Cold graph stage at least 2x faster | 1.4x (local); server publication 568.4x faster | local: **not met**; end to end with a server: met |
| One-file edit at least 5x faster | local graph stage 2.9x; with a server 945.0x faster end to end | local graph stage: **not met**; with a server: met |
| Skip/rebind publish nothing | `no_change` 0 requests; `equivalent_republish` 3 requests, 0 records | met |
| One staging query per file replaced | 8,206 statements (one per file plus three) → 4 | met |
| Zero graph-output differences | identical fingerprints in every scenario and run (below) | met |
| No uncontrolled memory growth | peak RSS 338 MB at 8,203 files, flat across batch sizes | met (measured, one machine) |

The local cold and one-file targets are missed because what remains is fixed cost P1 to P6 did not attack: writing 148k relationship rows into six B-tree indexes, parsing, and per-build manifest reads and commits. The large gains are where the work was quadratic (server staging, selective re-resolution) or redundant (route matching, record copies).

P0 local figures are one run of the P0 reference build (its quadratic scenarios take minutes each); P0 server figures are one run (a cold server publication took 38 minutes) and survive only as the benchmark's log lines, because that run reached the session's two-hour limit just before writing its JSON. "Now" is the median of 5 runs after 1 warmup.

## Graph equivalence

Each optimization was accepted only with byte-identical graphs: the local fingerprint (SHA-256 over every edge and link field, ordered by id) and the server fingerprint (the promoted generation's per-file record digests).

| Comparison | Scenarios | Local graph identical | Server records identical |
|---|---|---|---|
| P0 reference vs P1, 1,013 files | all 8 | yes | yes |
| P0 reference vs final, 8,203 files | all 8 | yes | (P0 not run with the server at this size) |
| Final, 5 runs, 8,203 files | all 8 | identical every run | identical every run |
| Fact cache on vs off, 8,203 files | all 8 | yes | (local only) |

CI keeps these properties: `crates/docmonk-knowledge/tests/scaling.rs` (incremental equals a from-scratch build after each edit kind, and a one-file edit costs the same SQL statement count at four times the files), the seeded random change sequences in `crates/docmonk-code/tests/indexing.rs`, staging-vs-per-file digests in `docmonk-backends`, and the existing golden and live OpenSearch/Elasticsearch suites.

Server request counts of the final runs exclude one state read the benchmark itself made after publishing (fixed in the benchmark afterwards); P0 counts never included it.

## Repeated runs, 1,013 files (P0 vs now, both 5 runs + 1 warmup)

| Scenario | Plan | Graph stage P0 | Graph stage now (median / p95) | Server P0 | Server now (median) | Requests P0 → now |
|---|---|---|---|---|---|---|
| cold | full | 1.343 s | 1.155 / 1.198 s | 16.826 s | 0.344 s | 47 → 47 |
| no_change | skip | 0.030 s | 0.026 / 0.047 s | 0.000 s | 0.000 s | 0 → 0 |
| equivalent_republish | rebind | 0.050 s | 0.045 / 0.052 s | 0.025 s | 0.021 s | 3 → 3 |
| body_edit | incremental | 0.164 s | 0.064 / 0.100 s | 16.188 s | 0.134 s | 10 → 4 |
| rename_definition | incremental | 6.608 s | 0.122 / 0.180 s | 16.548 s | 0.138 s | 11 → 11 |
| add_file | incremental | 0.184 s | 0.086 / 0.088 s | 16.937 s | 0.147 s | 11 → 11 |
| remove_file | incremental | 5.899 s | 0.109 / 0.125 s | 17.699 s | 0.136 s | 10 → 10 |
| doc_edit | incremental | 0.178 s | 0.133 / 0.141 s | 16.686 s | 0.133 s | 11 → 11 |

## Full scale, 8,203 files, now (5 runs + 1 warmup)

| Scenario | Plan | Index | Graph median / p95 | Server median | Pipeline median | CPU | Peak RSS | SQL statements | Server requests | Records copied |
|---|---|---|---|---|---|---|---|---|---|---|
| cold | full | 4.232 s | 14.333 / 15.024 s | 4.025 s | 22.642 s | 28.90 s | 338 MB | 225,189 | 334 | 0 |
| no_change | skip | 0.000 s | 0.241 / 0.280 s | 0.000 s | 0.390 s | 1.97 s | 338 MB | 9 | 0 | n/a |
| equivalent_republish | rebind | 11.237 s | 0.419 / 0.439 s | 0.137 s | 11.829 s | 15.81 s | 338 MB | 15 | 3 | n/a |
| body_edit | incremental | 0.364 s | 0.515 / 0.544 s | 1.143 s | 2.279 s | 3.49 s | 338 MB | 84 | 4 | 0 |
| rename_definition | incremental | 0.418 s | 1.187 / 1.410 s | 1.229 s | 3.131 s | 4.32 s | 338 MB | 92 | 12 | 163,034 |
| add_file | incremental | 0.409 s | 0.796 / 0.832 s | 1.327 s | 2.972 s | 4.52 s | 338 MB | 58 | 12 | 163,058 |
| remove_file | incremental | 0.454 s | 1.221 / 1.290 s | 1.170 s | 3.283 s | 4.48 s | 338 MB | 42 | 11 | 163,057 |
| doc_edit | incremental | 0.457 s | 1.524 / 1.568 s | 1.237 s | 3.684 s | 4.99 s | 338 MB | 181 | 12 | 162,925 |

Where the cold graph stage spends its time now (run 1):

| Phase | Seconds |
|---|---|
| state_load | 0.019 |
| derivation | 1.286 |
| parse (summed over parser threads) | 3.774 |
| sqlite_write | 6.338 |
| symbol_manifest | 0.973 |
| resolution | 2.240 |
| linking | 1.859 |
| commit | 1.173 |
| linking: load / index / match / write | 0.599 / 0.074 / 0.745 / 0.248 |

## P0 reference at 8,203 files (local, one run)

| Scenario | Graph stage | Resolution | Linking | SQL statements |
|---|---|---|---|---|
| cold | 20.391 s | 2.364 s | 8.809 s | 225,189 |
| no_change | 0.198 s | 0.000 s | 0.000 s | 9 |
| equivalent_republish | 0.346 s | 0.000 s | 0.000 s | 15 |
| body_edit | 1.479 s | 0.427 s | 0.466 s | 90 |
| rename_definition | 726.781 s | 725.455 s | 0.536 s | 5,507 |
| add_file | 1.669 s | 0.437 s | 0.494 s | 60 |
| remove_file | 604.910 s | 604.345 s | 0.033 s | 2,741 |
| doc_edit | 1.470 s | 0.185 s | 0.585 s | 181 |

P0 server publication at 8,203 files (one run, from the benchmark log): cold 2,287.9 s, no_change 0.0 s, body_edit 2,150.6 s. Staging issued one relationship query per file, each scanning the build's 148k edges.

## P2: graph fact cache (`indexing.graph_fact_cache`, 3 runs + 1 warmup each, local)

| Scenario | Index off → on | Graph off → on | Parse (summed) off → on | Hits / misses |
|---|---|---|---|---|
| cold | 4.147 → 6.136 s | 14.583 → 13.014 s | 3.640 → 0.000 s | 8098 / 0 |
| no_change | 0.000 → 0.000 s | 0.198 → 0.217 s | 0.000 → 0.000 s | 0 / 0 |
| equivalent_republish | 10.807 → 14.324 s | 0.344 → 0.350 s | 0.000 → 0.000 s | 0 / 0 |
| body_edit | 0.334 → 0.352 s | 0.464 → 0.529 s | 0.001 → 0.000 s | 1 / 0 |
| rename_definition | 0.345 → 0.351 s | 1.012 → 1.027 s | 0.001 → 0.000 s | 1 / 0 |
| add_file | 0.342 → 0.347 s | 0.643 → 0.743 s | 0.000 → 0.000 s | 1 / 0 |
| remove_file | 0.331 → 0.398 s | 0.872 → 0.954 s | 0.000 → 0.000 s | 0 / 0 |
| doc_edit | 0.406 → 0.416 s | 1.270 → 1.405 s | 0.000 → 0.000 s | 0 / 0 |

Cold, end to end: indexing costs 1.989 s more with the cache, the graph stage 1.568 s less: a net loss on this machine, and a forced full rebuild (`equivalent_republish`) indexes slower too because every file's facts are rewritten. Graph outputs are identical either way. The setting therefore stays off by default (plan risk: "only enable by default after a verified net win"): once P1, P3 and P4 removed the larger costs, parsing is a minority of the graph stage. It may pay where parsing costs more than storing and reading facts (larger files, slower CPUs; not measured here); measure with `--fact-cache` before turning it on.

## P6: graph batch size and parser threads (cold, local, one run each)

| Batch \ workers | 1 | 2 | 4 |
|---|---|---|---|
| 64 | 17.18 s, 287 MB | 16.48 s, 291 MB | 15.31 s, 273 MB |
| 128 | 17.73 s, 287 MB | 15.95 s, 290 MB | 14.55 s, 291 MB |
| 256 | 16.44 s, 287 MB | 14.68 s, 288 MB | 13.56 s, 276 MB |
| 512 | 16.68 s, 293 MB | 15.72 s, 292 MB | 13.72 s, 297 MB |

Graph-stage seconds and the scenario's peak RSS. Defaults stay at 256 files per batch and the CPU count for workers; the table is one machine, so it supports keeping the defaults rather than choosing new ones.

## Documentation-heavy corpus (2,000 code files, 3,000 notes, 75k units; one run)

| Scenario | Graph stage | Linking | Units loaded | Unit index build | Narrow load |
|---|---|---|---|---|---|
| cold | 19.241 s | 14.800 s | 75,000 | 0.328 s | False |
| body_edit | 0.766 s | 0.568 s | 75,000 | 0.176 s | True |
| doc_edit | 0.885 s | 0.694 s | 75,001 | 0.171 s | False |

Persisting the unit index (plan P3-02/P3-05) would save at most the load and index time of a pass (~0.4 s here), so it was not built. Cold linking here is dominated by writing its 390k links (9.1 s of 14.8 s), which an index does not change.

## What changed, phase by phase

- **P0** Per-phase timings and counters (`timings`, `counters`, `linking`, server `timings`/`counters`), SQL statement counts, graph phases in progress and `docmonk status`, canonical fingerprints, this benchmark.
- **P1** Server staging read one relationship query per file, and SQLite answered each from the primary key (to skip a sort), scanning the whole build: quadratic. Now one ordered pass (4 statements), memory bounded by a 50k-record budget; per-file reads name `idx_rel_file`.
- **P2** Optional reuse of the index parse for graph facts (`indexing.graph_fact_cache`, default off), in a disposable sidecar cache keyed by content hash and derivation.
- **P3** Route matching by one Aho-Corasick pass per unit; identifier matches remembered per needle; code-only edits load only their files' entities and routes.
- **P4** Candidate lookups for selective resolution had the same planner trap (54k entities scanned per name); indexes are named and lookups batched 500 per statement.
- **P5** A new base whose graph records are unchanged re-points the visible generation (one fenced compare-and-swap) instead of copying every record forward.
- **P6** Graph parsing takes the process-wide `cpu` permits; batch size measurable.
- **P7** CI guards (equivalence and per-change statement counts), operations documentation, this report.

## PH09: dependency-manifest writes (2,039 files, 3 runs + 1 warmup, local)

A full promotion rewrote the dependency manifest one file at a time and
first deleted the file's reference keys with `DELETE FROM graph_symbol_refs
WHERE file_id = ?`. That table's key is `(key, file_id)`, so each delete
scanned it: quadratic in the number of files, and the largest part of the
cold "commit" phase (1.5 s of 1.8 s; the SQLite `COMMIT` itself took
0.33 s). Deletes now go by primary key, from the file's stored symbols, and
a full promotion (tables just cleared) skips them.

| Scenario | Graph median before | after | p95 before | after | Fingerprint |
|---|---|---|---|---|---|
| cold | 3.896 s | 2.392 s | 4.028 s | 2.445 s | identical |
| body_edit | 0.070 s | 0.070 s | 0.077 s | 0.113 s | identical |
| rename_definition | 0.249 s | 0.202 s | 0.268 s | 0.206 s | identical |
| add_file | 0.074 s | 0.062 s | 0.081 s | 0.064 s | identical |
| remove_file | 0.173 s | 0.171 s | 0.209 s | 0.187 s | identical |
| doc_edit | 0.162 s | 0.112 s | 0.208 s | 0.119 s | identical |

Peak RSS 129 MB before and 130 MB after. Same container as above (4 vCPU),
`--files 2000 --docs 40 --server none`; raw reports in
`ph09-manifest-2039-before.json` / `ph09-manifest-2039-after.json`. The
8,203-file and 150-repository figures were not re-measured for this change.

## Not measured, not done, and what remains

- **Live clusters** (*not measured*): OpenSearch/Elasticsearch timings, bulk and copy-forward tuning (P5-03), and OpenSearch vs Elasticsearch parity were not measured; the CI live-server suite checks correctness on both. The in-process engine counts requests and records only.
- **The real 8,205-file source** (*not measured*): run `docmonk relationships build --json` there; the per-phase fields show where its time goes.
- **Largest remaining server cost**: an incremental publication that changes any record still copies every unchanged record into the new generation (`_update_by_query`, ~163k record updates at 8,203 files). Making it proportional to the change needs immutable per-file graph fragments with a generation manifest (plan P5-04): read-path changes, fragment garbage collection and pinning. That is a design for a separate change, worth doing only if live-cluster measurements show copy-forward dominating.
- **Largest remaining local cost**: cold builds spend most of their time writing 148k relationship rows (6 B-tree inserts each) and parsing; incremental builds are dominated by fixed per-build bookkeeping (reading the dependency manifest and committing it).
- **Not done by measurement**: persistent unit index (P3), asynchronous update-by-query (P5), new batch or worker defaults (P6).

## Rollback

No schema of a published index changed and nothing needs reindexing. `indexing.graph_fact_cache` is off by default; turning it off (or deleting `<home>/cache/graph-facts.db`) restores parsing. Every other change is an exact equivalence of the previous algorithm, so reverting a commit restores the previous behaviour with no data change. The one visible behaviour change, re-pointing an unchanged generation (P5), leaves generation ids stable; a rollback simply publishes new generations again.
