# Relationship-building performance

`PLAN.json` is the optimization plan this directory tracks (its version suffix is dropped: the clean-slate audit forbids version naming). Work lands
phase by phase in one PR; each phase records its measurements here.

Measurements in this directory are labelled by where they came from:

- **measured (SQLite, synthetic)**: produced in this repository by the
  relationship benchmark against a generated corpus. No proprietary
  source is used.
- **measured (mock server)**: request/record counts against the
  in-process mock OpenSearch/Elasticsearch backend. These are counts, not
  cluster timings.
- **not measured**: live-cluster timings and the real 8,205-file
  workload. They need a live OpenSearch/Elasticsearch (nightly) or a
  private run.

## Phase checklist

| Phase | Scope | Status |
|---|---|---|
| P0 | Phase timers, counters, stage progress, synthetic benchmark, baseline | pending |
| P1 | Remove per-file relationship queries from server graph staging | pending |
| P2 | Reuse parsed reference facts from indexing (flag, default off) | pending |
| P3 | Incremental code-document linker (flag, default off) | pending |
| P4 | Batched symbol candidate resolution | pending |
| P5 | Server publication: avoidable writes, bounded copy-forward | pending |
| P6 | Worker scheduling and bounded memory | pending |
| P7 | Equivalence validator, CI smoke guards, docs, final report | pending |
