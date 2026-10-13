# ADR 0034: One canonical status (`docmonk-status`)

Status: accepted (replaces the status parts of ADR 0019)

## Context

`docmonk status` used to be assembled twice: by a local collector in
`docmonk-indexing` and by a server collector in `docmonk-service`, each
building loosely typed JSON. The server collector issued per-source
requests (more than 1,000 HTTP calls for 150 sources), read only the
local host's progress file, ordered errors by path, and reported numbers
it could not observe (server database size, idle queue depths) as `0`.

## Decision

`crates/docmonk-status` owns the only status model, both collectors and
the only health evaluator. `docmonk-service::status` is a thin facade;
the CLI, the MCP `docmonk_status` tool and the Admin UI render the same
typed [`StatusReport`]. There is one command, `docmonk status`; there is
no second status command, no alias for an earlier field name, and no
adapter for an earlier shape.

Dependency direction: `docmonk-storage` (SQLite reads) and
`docmonk-backends` (batched server reads, runtime heartbeats) provide
primitives; `docmonk-indexing` provides progress instrumentation and
lock/process probes; `docmonk-status` turns them into the report. The
indexing crate never depends on the status crate.

### Authority

| Mode | Catalog, builds, published counts, errors | Runs |
|------|-------------------------------------------|------|
| local | the home's `state/control.db` and project stores | this home's progress file and locks |
| server | the configured OpenSearch/Elasticsearch prefix only | `{prefix}-runtime` heartbeats from every host |

Server mode never opens a local SQLite store for status. An unreachable
cluster is a typed `BackendUnavailableError` (exit code 4), never an empty
or stale report. A corrupt required local database is a typed database
error, never an empty report; a fresh home with no sources is a valid
empty report.

### Contract (`schema_version` 1)

Root fields: `schema_version`, `snapshot_id`, `observed_at`, `mode`,
`backend`, `health`, `indexer`, `summary`, `sources`, `problems`,
`recent_errors`, `diagnostics`. The reference fixture is
`crates/docmonk-status/tests/fixtures/server_multi_host.json` (sample
data, not operational data).

- **Published vs in progress.** `sources[].published` is the active
  (published) build; `sources[].live` and `indexer.workers` are passes in
  progress. A pending build never contributes to published counts.
- **Scan vs publication.** `last_scan_at` says a scan finished. A source
  is `completed` only when it has a published build and no pass is active.
- **Unknown is null.** Pre-scan totals, percentages without a planned
  total, remote PIDs, server disk usage, in-memory queue lengths and ETAs
  are `null` unless the authoritative input observes them. A section that
  failed to load is `null` and named in `diagnostics.missing_sections`
  (`partial: true`); it is never reported as `0`.
- **Snapshot consistency.** Server mode reads the source → active build
  pairs once and scopes every aggregate to those pairs. If a publication
  changes a pair while reading, the changed sources are re-read once
  (`consistency: retried`); if they keep changing they are listed in
  `inconsistent_sources` (`consistency: inconsistent`). Builds are never
  mixed.
- **Errors.** `recent_errors` is globally ordered by the real event time
  (`occurred_at` descending, then source id, then path), selected across
  all sources before truncation (default 25, at most 200). Server file
  errors carry `last_error_at`, written when the failure happened.

Enums: health `healthy | degraded | failed | unknown`; source index state
`not_indexed | queued | scanning | indexing | finalizing | publishing |
completed | retrying | failed | stalled | offline | disabled | unknown`;
backend `sqlite | opensearch | elasticsearch`.

### Health

`health.state` is derived only from `problems`: any `error` → `failed`,
any `warning` → `degraded`, a partial report without errors → `unknown`,
otherwise `healthy`. Problems carry a stable `code`, `severity`, `scope`,
optional `source_id`/`host`, a message, a hint and the observation time.
Cluster `red` is an error and `yellow` a warning (single-node replica
cases included: the hint says how to fix them). An expired lease or stale
heartbeat is never reported as running.

### Collection errors and privacy

`CollectError` classes: `Unavailable`, `SchemaMismatch`, `Database`,
`Config`. URLs are reported without credentials and error messages pass
through the telemetry redactor; a caller can ask for file paths to be
redacted (`CollectOptions::redact_paths`, used for shared surfaces).

## Consumers

- `docmonk status [--json] [--watch] [--interval S] [--errors] [--verbose]`
  (`crates/docmonk-cli/src/status_cmd.rs`).
- MCP `docmonk_status` (`crates/docmonk-mcp`).
- Admin UI dashboard, sources, indexing pages and `/ready`
  (`crates/docmonk-ui`).
- `docmonk doctor` queue check (`crates/docmonk-ops`).

## Consequences

Adding the runtime index and `last_error_at` changes the server schema
identity. Existing prefixes are refused with reset instructions; nothing
is converted in place or deleted automatically.
