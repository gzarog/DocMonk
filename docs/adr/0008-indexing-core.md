# ADR 0008: Indexing core

Status: accepted

- Scanning, ignore rules (`.gitignore`-style), file classification and fingerprinting live in `docmonk-indexing`; their behavior is pinned by `fixtures/expected/indexing.json`.
- Diff states: new, changed, unchanged, unchanged-restat (stat drift, same hash), moved (same hash, new path; counted as moved + unchanged but re-extracted), unseen (incomplete scan: kept, never deleted), deleted (complete scan only).
- Incremental passes update the active build in place inside one transaction (ADR 0009). Incomplete builds are never published.
- Retry: exponential backoff 2s·2^(n-1), capped at 300 s, 5 attempts; stored per file (`attempt_count`, `next_attempt_at`).
- `RunLock` uses an OS file lock (`File::try_lock`) with owner metadata.
- Symlinks/junctions are not followed by default.
- Files with no registered processor go through the raw processor (`raw-1`). Cross-source links are not carried forward; the knowledge linker recomputes them for every build.
- Concurrency is bounded: `sync_channel` workers plus a single SQLite writer.
