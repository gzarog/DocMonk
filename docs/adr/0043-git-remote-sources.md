# ADR 0043: Git remote sources

Status: accepted and implemented (improvement roadmap P6).

## Context

A source is a directory. Teams with many repositories had to clone each
one, keep the clones current themselves and register the folders. A
repository that moved on was indexed only when someone pulled it.

## Decision

`docmonk source add <git url> [--branch B] [--subpath DIR]` registers a
repository by URL (`https://`, `ssh://`, `git://`, `file://` and the
scp-like `user@host:path`). The Admin UI form takes the same three
fields.

* **Checkout.** DocMonk makes a shallow, single-branch clone under
  `<home>/sources/git/<key>/checkout`, where `<key>` is derived from the
  URL, branch and subpath, so adding the same remote again returns the
  same source. The registered source is that directory (or its
  `subpath`), so scanning, indexing, search and citations work exactly as
  for a folder. `remote.json` next to the checkout holds the URL, branch,
  subpath, the fetched commit, the last indexed commit, the fetch time
  and the last fetch error.
* **Refresh.** Before every full pass (`docmonk index`, the daemon's
  startup and reconciliation passes) the checkout is fetched with depth 1
  and reset to the branch head; DocMonk owns the checkout, so local
  changes there are discarded. The daemon gives git sources no filesystem
  watcher; it asks the remote for the branch head every
  `indexing.git_poll_seconds` (default 300; `git ls-remote`, no objects)
  and queues a full pass (`git_poll`) when it moved.
* **Failure.** A failed fetch (unreachable remote, refused credentials, a
  deleted branch) leaves the checkout and its published index as they
  were. The source reads offline with the reason (`git: branch 'x' no
  longer exists on <url>`) in `source info`, `docmonk status` and the
  status contract, and comes back online on the next successful fetch.
* **Status.** `source info` and each source of `docmonk status --json`
  (and MCP `docmonk_status`) carry a `git` object for git sources (absent
  for folders): `url`, `branch`, `subpath`, `commit` (fetched),
  `indexed_commit` and `last_error`. The text view adds
  `git <url> <branch>: indexed <commit>` under the source.
* **Removal.** `source remove` deletes the checkout with the indexed
  data: it is DocMonk's copy, not the user's files.

### System git, not a Rust git library

ADR 0003 asks for no runtime dependency outside the binary. Git sources
are an exception, scoped to themselves: they run the system `git`, and
folder sources need nothing new.

* Credentials come only from git's own configuration (credential
  managers, `.netrc`, SSH agents and keys, `insteadOf` rewrites),
  exactly as the user's own `git clone` would use them. DocMonk never
  stores or prompts for credentials: git runs with
  `GIT_TERMINAL_PROMPT=0` (and `GCM_INTERACTIVE=never`), so a missing
  credential fails the fetch instead of blocking a daemon, and a URL
  with a password or token in it is refused.
* A Rust implementation (`gix`) adds a large dependency tree and its own
  transport and credential handling, which would still differ from the
  user's git in exactly the cases (enterprise helpers, proxies, SSH
  configuration) that matter for private repositories.
* Without git installed, adding a git source fails with "install git to
  use git sources".

## Consequences

* Git sources are local-mode only. In server mode a checkout would live
  on one indexing host while the catalog is shared, so `source add`
  refuses a URL there; check the repository out on each host and add the
  directory.
* Only the branch head is indexed (no history, tags or other branches);
  a shallow clone keeps disk use near the size of one working tree.
* Polling costs one `ls-remote` per git source per interval. A remote
  that rate-limits can be polled less often with
  `indexing.git_poll_seconds`.
* Rollback: revert. Folder sources are unaffected; registered git sources
  keep working as plain folders without refresh.
