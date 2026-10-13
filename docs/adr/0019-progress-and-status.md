# ADR 0019: Indexing progress

Status: accepted (the status report itself is ADR 0034)

## Scope

How indexing runs record their progress. The daemon lifecycle is
ADR 0020; the filesystem watcher is ADR 0021; how `docmonk status`
turns progress into a report is ADR 0034.

## Progress snapshot (`docmonk_indexing::progress`)

- **One entry per source pass.** `<home>/index_progress.json` (schema
  version 2) holds the run (`run_id`, `pid`, `host`, `operation`,
  `outcome`, start/finish times, `max_parallel_sources`, permit counters)
  and a `sources` list with one entry per source pass: `stage`, `scanned`,
  `planned` (known once the scan finished), `processed`, `indexed`,
  `failed`, `retry`, `started_at`, `last_progress_at`, `heartbeat_at`,
  `finished_at` and `outcome`. Parallel passes never overwrite each other.
- **Stages.** `starting`, `scanning`, `processing`, `finalizing`,
  `publishing`, `done`, the same in local and server mode.
- **Writes.** Atomic (a per-process/thread temp file, then rename).
  Counter updates are coalesced to at most one write per second; stage
  changes, source start/finish and the final snapshot are always written.
- **Reading.** A missing, malformed or other-version file reads as
  `None`.
- **Heartbeats are not progress.** A ticker thread refreshes the active
  passes' `heartbeat_at` every 5 seconds while the run is alive; only real
  events move `last_progress_at` and the counters. The process-wide
  `heartbeat()` (called by embedding and linking loops) does the same.
- **Terminal outcome.** `track()` writes `completed`, `failed: …` (500
  characters at most) or `interrupted`; passes still active end with the
  run. A crash leaves `running: true` with a heartbeat that stops; status
  reads that through the process probe as a crashed run.

## Server mode

Each server-mode pass also writes its source's `{prefix}-runtime`
document (ADR 0007): on stage changes, at most every 5 seconds for
counters, from the lease-renewal thread as an independent heartbeat, and
once with the terminal outcome. Writes are fenced by the lease token and
never fail the pass.

## Process probes (`docmonk_indexing::runtime`)

`is_process_alive` (a zombie is not alive) and `host_name` serve the
daemon, the lease owner name and status. They never prove anything about
another host.
