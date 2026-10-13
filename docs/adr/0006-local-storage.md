# ADR 0006 — Local storage and the control plane

Status: accepted

## Decision
* **Layout.** The control plane lives in `<home>/state/control.db`; each
  project's index-derived knowledge lives in
  `<home>/projects/<project_id>/knowledge.db`. `config.yaml` sits at the
  home root.
* **Schema identity.** A new database is created directly at the current
  schema and records its schema fingerprint. An existing database is used
  only when that fingerprint matches the running code; anything else is
  refused, untouched, with a reset/reindex instruction. Databases are never
  upgraded in place: every source can be rebuilt from its original files.
* **Sources.** A source definition holds its path, type, enabled flag,
  include/exclude patterns and creation time. Source IDs are path-derived
  (`src_` + sha256[:10]).
* **Mandatory first build.** Every new source starts as
  `needs_full_rebuild`. `control::plan_for` returns `Full` unless the source
  has a published build whose recorded `IndexVersions` (schema, parser,
  chunker, converter, embedding model, embedding text version) equal the
  running code's; `ProjectStore::reusable_files` returns nothing for a full
  plan, so stale state cannot suppress processing.
* **Atomic local publication.** Every index-derived row carries its
  `build_id`; readers query the source's `active_build_id`. A build becomes
  visible only via `publish_build`; an aborted first build leaves the source
  in `needs_full_rebuild`, an aborted later build keeps the previous build
  visible. Unchanged files are carried forward into incremental builds;
  superseded and aborted builds are garbage-collected.
* **Record IDs** (`docmonk_core::ids::record`): 32-hex sha256 over a domain
  tag and natural-key parts, deterministic and idempotent.
* **Manual links** are stored as user data (`manual_links`, keyed by
  qualified name/relative path) so they survive rebuilds.

## Consequences
A schema change ships with a new fingerprint; existing homes are rebuilt
from source rather than converted. Storage behavior is verified by the
`docmonk-storage` tests.
