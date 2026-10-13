# ADR 0024: CLI operations commands

Status: accepted

## Decisions

1. Backups contain only the current storage format. An archive whose
   format or schema fingerprints differ from the running build is refused,
   never converted.
2. `doctor` and `health` include every applicable section.

## Commands

### `doctor` and `health`

- **Verdict.** The overall result is `UNHEALTHY` on any fail and `HEALTHY WITH WARNINGS`
  on any warn.
- **Checks:**
  - Core: version.
  - Database (the control plane): SQLite, WAL mode, schema fingerprint
    equal to the running build's.
  - Sources: reachability of enabled sources.
  - Index Lock: free, held (with owner) or unknown.
  - Index: queue depth from the status model.
  - Disk: free space on the home's filesystem, through `sysinfo`.
  - Semantic (when enabled): vectors under the current model fingerprint
    and the HNSW index size.
  - Tokenizer: identity, limits, and every stored chunk embedding payload
    measured with special tokens. Any payload over the model limit is a
    fail.
  - AI: the configured provider and privacy posture.
  - Server (in server mode):
    - the engine and the endpoint, with credentials redacted;
    - connectivity, through `ServerBackend::connect`;
    - index existence, through `ServerBackend::index_status`
      (`HEAD` only, never creates anything).
- **Exit code.** An unhealthy result exits with the health-check code and
  no extra error line (an empty error message is a silent exit).

### `backup [DEST] [--json]`

- **Locking.** Runs under `index.lock`.
- **Contents.** A `.tar.gz` holding:
  - `manifest.json`;
  - `state/control.db`;
  - `projects/<id>/knowledge.db` for each indexed project;
  - `config.yaml`.
- **Snapshots.** Databases are copied with SQLite's `VACUUM INTO` over a
  read-only connection. The copy is consistent while a writer is active,
  and the source database is never modified.
- **Manifest.** `format_version`, `docmonk_version`, `created_at`,
  `control_schema` (the control-plane schema fingerprint), `projects`
  (each with its knowledge schema fingerprint) and `sources`.
- **Not archived.** ANN index files are caches. Search falls back to
  exact matching until the next sync rebuilds them.

### `restore ARCHIVE [--json]`

- **Order of steps.** Nothing in the live home is touched until
  everything has been verified:
  1. Extract into a scratch staging directory. The `tar` crate's `unpack`
     refuses entries that would escape it.
  2. Read the manifest.
  3. Refuse an archive whose format version or schema fingerprints differ
     from this build; the user reindexes instead.
  4. Run `PRAGMA integrity_check` on every database.
  5. Stop a running daemon.
  6. Move the live databases and config aside, then rename the staged
     ones into place.
  7. Restart the daemon if it was running.
- **Rollback.** A failed swap puts the previous state back.

### `rebuild [--source ID] [--fresh] [--yes] [--json]`

- **How it works.** Each selected source is marked for a full rebuild,
  then runs through the normal coordinator under `index.lock` and
  `progress.track("rebuild")`.
- **Always recoverable.** A full build is written next to the visible
  build and published only on success, so a failed rebuild never leaves a
  source without a usable index. `--fresh` adds a reachability check of
  every source root and a confirmation prompt (`--yes` skips it).
- **Exit code.** A file failure exits with the partial-failure code.

### `uninstall [--keep-data] [-y] [--json]`

- **Data.** Stops a running daemon, then deletes `DOCMONK_HOME`, unless
  `--keep-data` is given.
- **The binary.** A running executable cannot delete itself on Windows,
  so `install_method` is `standalone_binary`, `app_removed` is false, and
  the output includes manual removal instructions.

### `vectors rebuild` and `vectors backfill`

- **`rebuild`** drops each project's HNSW file and rebuilds it from the
  vectors stored under the current model's fingerprint. A source with no
  vectors reports `backend: null`.
- **`backfill`** embeds every subject missing a current vector
  (`embed_build`) and syncs the index. It needs the embedding model, and
  reports a usage error when the model is not installed.

## Storage additions

- **`docmonk_storage::maintenance`:**
  - read-only schema state (`control_schema_state`,
    `knowledge_schema_state`);
  - `integrity_check`;
  - `snapshot` (`VACUUM INTO`);
  - the registered source list, read-only;
  - the journal mode.

## Tests

- **`docmonk-cli/tests/ops.rs`:**
  - doctor and health sections, verdict and text;
  - a backup → change → restore round trip, after which status and
    queries work;
  - incompatible, garbage and missing archives are refused and leave the
    home untouched;
  - rebuild (plain and `--fresh`), `vectors rebuild` without embeddings,
    and uninstall with and without `--keep-data`.
- **Unit tests:** maintenance helpers, the doctor verdict rules.
