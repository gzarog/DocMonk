<div align="center">

# 🧘 DocMonk

### Ask your company documents, and your code, anything.
### Answers with exact sources. Entirely on your machine.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Native binary](https://img.shields.io/badge/native-Rust-orange)](docs/architecture.md)
[![MCP Compatible](https://img.shields.io/badge/MCP-compatible-green)](https://modelcontextprotocol.io)
[![Zero Telemetry](https://img.shields.io/badge/telemetry-none-brightgreen)](https://github.com/gzarog/DocMonk)

**[Overview slides](docs/docmonk-overview.html)** · **[Install](#-install)** · **[Quick start](#-quick-start-5-minutes)** · **[Features](#-what-you-get)** · **[How indexing works](#-how-indexing-works)** · **[Commands](#-command-cheat-sheet)** · **[Status](#-is-it-working-docmonk-status)** · **[Configuration](#%EF%B8%8F-configuration)** · **[FAQ](#-faq--troubleshooting)**

</div>

---

<table>
<tr>
<td width="33%" valign="top">

### 📄 Documents
PDF, Word, PowerPoint, Excel, CSV, HTML, Markdown, OpenDocument, EPUB, plus OCR for scanned files. Headings, tables and page numbers are preserved.

</td>
<td width="33%" valign="top">

### 💻 Code
<!-- clean-slate-audit: allow-start -->

Python, JavaScript, TypeScript, Go, Java, Rust, C#. Symbols, callers, call graphs, HTTP routes and impact analysis.

<!-- clean-slate-audit: allow-end -->

</td>
<td width="33%" valign="top">

### 🔒 Private
No telemetry, no cloud, no external AI unless you opt in. Your data stays in `~/.docmonk`.

</td>
</tr>
</table>

---

## 🤔 Why DocMonk?

> *"What does the onboarding policy say about remote equipment?"*
> *"Which spec describes settlement retries, and what code implements them?"*

Knowledge is scattered across **PDFs, Word files, slides, spreadsheets, wikis** and **repositories**. Most RAG tools send that data to the cloud and answer **without sources**.

DocMonk is a **local-first knowledge engine**. Point it at folders and you get an evidence-backed knowledge base:

| | |
|---|---|
| ✅ **Evidence-first** | Every answer cites a file plus its page, heading or line |
| ⚙️ **Deterministic** | Full-text search, symbols, call graphs and document↔code links work with **no LLM at all** |
| 🔗 **Linked** | Specs and docs are automatically linked to the code they mention |
| 🧩 **Everywhere** | CLI, web Admin UI, and AI agents via MCP (Claude Code, Codex, Cursor, VS Code) |

---

## 📦 Install

| Platform | Command |
|---|---|
| **macOS / Linux** | `curl -fsSL https://raw.githubusercontent.com/gzarog/DocMonk/main/install.sh \| sh` |
| **Windows (PowerShell)** | `irm https://raw.githubusercontent.com/gzarog/DocMonk/main/install.ps1 \| iex` |
| **Docker** | `docker pull ghcr.io/gzarog/docmonk` (see [Container](docs/adr/0030-release-and-container.md)) |

**Requirements:** none. A single native binary for Linux x86_64, macOS (Apple silicon and Intel) and Windows x86_64, with the embedding, reranker and OCR models bundled, so everything runs offline from the first start. Update in place with `docmonk update install`.

Check the install with `docmonk version` and `docmonk doctor`.

---

## 🚀 Quick start (5 minutes)

```bash
docmonk init                              # 1. create the runtime folder (~/.docmonk)
docmonk source add ~/company-docs         # 2. register folders to index
docmonk source add ~/code/payments
docmonk index                             # 3. build the knowledge base
docmonk status                            # 4. check progress and errors
docmonk explore "what does the onboarding policy say about equipment?"   # 5. ask
```

Example of what you get back:

```
docs/onboarding-policy.pdf  →  page 4 · Remote Work Policy › Equipment Allowance
contracts/msa-2024.docx     →  page 12 · Termination › Notice Period
src/payments/settlement.py  →  line 88 · SettlementService.retry()
```

> 💡 **Keep it fresh automatically:** run `docmonk daemon start`, and changed files are re-indexed in the background.

> 🎞️ **New here?** Open [`docs/docmonk-overview.html`](docs/docmonk-overview.html) in a browser: a short slide overview for users and engineers.

---

## 🔄 How indexing works

Every `docmonk index` (and every background daemon pass) runs in **two stages**:

```
 Stage 1 · INDEX (all selected sources)          Stage 2 · RELATIONSHIPS (after ALL of stage 1)
 scan → diff → convert → entities → chunks     ┃  code edges → cross-file resolution
 → embeddings → PUBLISH  ── searchable now ──  ┃  → doc↔code links → manual links → PUBLISH graph
```

| | |
|---|---|
| ⚡ **Search first** | A source is searchable as soon as stage 1 publishes it. Relationship work never delays or rolls back the index. |
| 🧱 **A barrier** | Stage 2 starts only after every selected source finished stage 1. |
| 🎯 **Exact snapshots** | Each relationship graph belongs to one published index. While a newer graph is being built, the last committed graph is still served and flagged `stale` (set `graph.require_current: true` to hide it instead). A file edited mid-run marks the graph `stale` instead of mixing versions. |
| ♻️ **Incremental** | Only changed files are re-parsed, and only files that reference a changed symbol are re-resolved. In server mode unchanged edges and links are copied forward on the cluster, not rewritten. |
| 🩹 **Retry cheaply** | `docmonk relationships build [--source ID]` rebuilds graphs without re-reading or re-converting any content. |
| 🔌 **Optional** | `indexing.relationships_enabled: false` skips stage 2 and hides graphs (callers, callees, impact, doc↔code links); search still works, nothing is deleted, and toggling it never forces a reindex. |

A run where every index published but a graph failed is reported as a **partial success**: the index is usable, and `status` shows the graph separately.

---

## ✨ What you get

<details open>
<summary><b>📄 Documents</b></summary>

| Format | What is extracted |
|---|---|
| **PDF** | Layout model: headings, tables, page numbers · OCR for scanned pages (`documents.ocr: auto`) |
| **DOCX / ODT** | Heading structure with page provenance |
| **PPTX / ODP** | Slide titles and body text, slide numbers |
| **XLSX / ODS / CSV** | Tables, chunked row by row |
| **HTML / Markdown** | Wikis, docs-as-code, ADRs, runbooks |
| **TXT / EPUB** | Plain text and e-books |
| **PNG / JPG / TIFF** | Opt-in OCR (`documents.image_ocr: true`) |

- A corrupt file is isolated and marked as failed. The rest of the run continues.
- Older binary `.doc`, `.ppt`, `.xls` files and `.rtf` files are detected but not converted.
- Incremental: only changed files are reprocessed, and PDF conversions are cached by content.

</details>

<details open>
<summary><b>💻 Code</b></summary>

<!-- clean-slate-audit: allow-start -->

Full extraction for **Python, JavaScript, TypeScript/TSX, Go, Java, Rust and C#**, including HTTP routes from Python decorators and C# attributes. Other languages are still indexed for full-text search.

<!-- clean-slate-audit: allow-end -->

```bash
docmonk symbol SettlementService      # where is it defined?
docmonk callers SettlementService     # who calls it?
docmonk impact SettlementService      # callers, tests, and the specs that mention it
```

</details>

<details open>
<summary><b>🔎 One question, one command</b></summary>

```bash
docmonk explore "what breaks if SettlementService changes?"
```

A deterministic planner combines symbol lookup, the call graph, full-text search, document links and (optionally) semantic search into **one cited evidence package**.

</details>

<details>
<summary><b>🖥️ Admin UI</b></summary>

```bash
docmonk ui          # → http://127.0.0.1:8765
```

Manage sources, run indexing with live progress, browse document chunks, test search, explore the code graph, edit settings, and control the daemon, backups and logs. It listens only on `127.0.0.1`, with CSRF protection. See [docs/ui.md](docs/ui.md).

</details>

<details>
<summary><b>🤖 AI agents (MCP)</b></summary>

```bash
docmonk serve --mcp   # register this command as a stdio MCP server in your client
```

Tools exposed: `docmonk_explore`, `docmonk_search`, `docmonk_documents`, `docmonk_symbol`, `docmonk_callers`, `docmonk_callees`, `docmonk_impact`, `docmonk_status`, `docmonk_ask`.

</details>

<details>
<summary><b>🧠 Optional AI answers and semantic search</b></summary>

```bash
docmonk config set search.semantic true      # local semantic search (no cloud)
docmonk config set ai.provider ollama
docmonk config set ai.model llama3.2
docmonk ask "how are settlement retries handled?"
```

| Provider | Notes |
|---|---|
| **Ollama**, OpenAI-compatible | Local / self-hosted |
| **OpenAI**, **Anthropic** | Require `privacy.external_ai_allowed: true` |
| **Codex**, **GitHub Copilot** (beta) | `docmonk ai login <provider>`, see [docs/providers](docs/providers/) |

</details>

<details>
<summary><b>🏢 Large or shared corpora: OpenSearch / Elasticsearch</b></summary>

```bash
docmonk init --storage-mode server --storage-engine opensearch \
    --storage-url https://search.internal:9200
```

- `init` checks the real cluster first: reachable, right engine, OpenSearch 2+ / Elasticsearch 8+, permissions.
- Credentials come **only** from environment variables (`DOCMONK_OPENSEARCH_USERNAME`, `…_PASSWORD`, `…_API_KEY`, and the same names with `ELASTICSEARCH`). They are never stored in `config.yaml`.
- Rebuilds publish atomically. A failed rebuild keeps serving the previous index.
- Local dev cluster: `docker compose -f docker/docker-compose.opensearch.yml up -d`

</details>

---

## 📋 Command cheat sheet

| Task | Command |
|---|---|
| Set up | `docmonk init` |
| Add / list / remove a folder | `docmonk source add PATH [--include GLOB] [--exclude GLOB]` · `source list` · `source remove ID` |
| Pause / resume a folder | `docmonk source disable ID` · `source enable ID` |
| Index now | `docmonk index [--source ID]` |
| Retry relationship graphs only | `docmonk relationships build [--source ID]` |
| Index automatically | `docmonk daemon start` · `daemon status` · `daemon stop` |
| Is it working? | `docmonk status [--watch] [--errors] [--verbose] [--json]` |
| Search | `docmonk search "text" [--hybrid] [--table] [--json]` |
| Ask across everything | `docmonk explore "question"` · `docmonk ask "question"` (AI) |
| Code navigation | `docmonk symbol` · `callers` · `callees` · `references` · `impact` `NAME` |
| List documents | `docmonk docs [--source ID]` |
| Pin a doc↔code link | `docmonk link add ENTITY DOCUMENT` · `link list` · `link remove ID` |
| Web UI | `docmonk ui` |
| Health check | `docmonk doctor` · `docmonk health` |
| Settings | `docmonk config show` · `config get KEY` · `config set KEY VALUE` |
| Backup / restore | `docmonk backup [PATH]` · `docmonk restore ARCHIVE` |
| Rebuild from scratch | `docmonk rebuild [--source ID] [--fresh]` |
| Update DocMonk | `docmonk update check` · `update install` |
| Uninstall | `docmonk uninstall [--keep-data]` |

Every command supports `--help`.

---

## 📊 Is it working? `docmonk status`

`docmonk status` tells you whether indexing is **running, progressing, stalled or failing**, on which sources and hosts, and what failed. Example while two sources index on two hosts (server mode):

```
DocMonk status  [server mode, opensearch http://search.example:9200 prefix=docmonk, authoritative]
Snapshot 2026-10-08T15:00:00.000000Z (0s ago)
Health: DEGRADED  (2 problem(s))  Cluster: yellow (unassigned shards 7)
Sources: 3 registered, 3 enabled, 2 active, 0 failed, 0 offline
Published: 2400 indexed, 4 failed, 6 retrying of 2410 files; 18000 entities, 320 documents, 9100 chunks, 41000 relationships
In progress: 2 pass(es) over 2 source(s) on 2 host(s); files pending in current runs: N/A
== Active passes ==
Source     Host    Run   Stage       Progress        Indexed  Failed  Retry  Heartbeat
src_alpha  host-a  live  processing  120/300 (40%)   118      1       1      1s ago
src_beta   host-b  live  scanning    0/N/A           0        0       0      3s ago
```

| Flag | What it does |
|---|---|
| `--watch` | Live view (cheap refresh; Ctrl+C to exit) |
| `--errors` | Only the sources, problems and errors that need attention |
| `--verbose` | Full ids and paths, builds, run ids, locks and diagnostics |
| `--json` | The canonical machine-readable report (same data as the MCP tool and the Admin UI's `/api/status`) |

**Reading it**

| Term | Meaning |
|---|---|
| **Published** | Counts of each source's published build only; work in progress never mixes in |
| **In progress / Active passes** | Passes running now, on this host (local mode) or any host (server mode) |
| **Relationships** | The source's graph, independent of its index: `ready` · `pending` · `building` · `stale` · `failed` · `disabled`. Shown per source in `--verbose`, and whenever it needs attention |
| **State** | `queued` · `scanning` · `indexing` · `finalizing` · `publishing` · `completed` · `retrying` · `failed` · `stalled` · `offline` · `disabled` · `not_indexed` · `unknown` |
| **Access** | Can the folder be reached? `online` · `offline` · `disabled` |
| **N/A** | Not observable (for example the total before a scan finished); never shown as `0` |

**Health:** `healthy` · `degraded` (failed or retrying files, an offline source, a yellow cluster) · `failed` (backend unreachable, red cluster, a pass stalled or crashed, all sources offline) · `unknown` (part of the status could not be read). The problems list always says why, with a code and a hint.

> ⏱️ **"stalled"** means a pass stopped sending heartbeats for **120 seconds** (`indexing.status_stall_threshold_seconds`). Passes send heartbeats every 5 seconds even during a long OCR or embedding step, so a stall means the process is stuck or gone.

---

## ⚙️ Configuration

Settings live in `~/.docmonk/config.yaml` (Windows: `%LOCALAPPDATA%\DocMonk`); unknown keys are rejected. Change them with `docmonk config set KEY VALUE`, or override any of them with an environment variable, e.g. `DOCMONK_DOCUMENTS__OCR=off`. Set `DOCMONK_HOME` to move the whole runtime folder.

| Setting | Default | What it does |
|---|---|---|
| `documents.ocr` | `auto` | OCR scanned PDFs: `off` · `auto` · `always` |
| `documents.image_ocr` | `false` | OCR plain images (PNG/JPG/TIFF) |
| `documents.max_pages` | `1000` | Skip larger PDFs |
| `documents.pdf_mode` | `accurate` | `fast` = text layer only, much faster, no layout/tables |
| `documents.email_attachments` | `true` | Index supported `.eml` attachments as child documents |
| `search.semantic` | `false` | Local embedding-based semantic search |
| `indexing.watch` | `true` | Daemon watches folders for changes |
| `indexing.max_file_size_mb` | `100` | Skip larger files |
| `indexing.relationships_enabled` | `true` | Build relationship graphs (code edges, doc↔code links) after indexing |
| `indexing.relationship_dependencies` | `[]` | Explicit cross-source dependencies, `"<consumer> -> <producer>"` (C# project references are discovered) |
| `indexing.lock_timeout_seconds` | `30` | Max wait when another process is indexing |
| `graph.require_current` | `false` | `true`: graph queries return nothing until the graph matches the published index (instead of serving the last graph, flagged `stale`) |
| `search.ann.ef_search` | `400` | Local vector-search beam width (higher = better recall, slightly slower) |
| `search.ann.num_candidates` | `0` | Server kNN candidates per query (`0` = automatic) |
| `indexing.status_stall_threshold_seconds` | `120` | When `status` reports "stalled" |
| `ai.provider` / `ai.model` | *(none)* | Optional AI answers for `docmonk ask` |
| `privacy.external_ai_allowed` | `false` | Required for cloud AI providers |

<details>
<summary><b>⚡ Faster indexing tips</b></summary>

| Situation | Try |
|---|---|
| Many digital (non-scanned) PDFs, speed matters more than layout | `docmonk config set documents.pdf_mode fast`: 81s → 1.6s on a 24-PDF test set |
| Big first index | Let it finish once. After that only changed files are reprocessed |

Changing `pdf_mode` reprocesses PDFs on the next `docmonk index`.

</details>

---

## 🆕 Upgrading to this release

This release changes the index layout (better identifier and phrase
search, faster graph writes), so **existing indexes must be rebuilt once**:

- **Local mode:** DocMonk refuses a project store with the old schema and
  says which file it is. Remove it (or reset the DocMonk home) and run
  `docmonk index`.
- **Server mode:** `docmonk init` refuses indexes with the old mappings.
  Initialize a new `storage.server.index_prefix` (or delete the old
  indexes and run `init` again), then run `docmonk index`.

New in this release: warm queries reuse the embedder, the reranker and the
vector index, instead of reloading them for every query. Identifier,
phrase and path search ranks better. The last relationship graph stays
available while a new one builds. A full index that was interrupted
resumes where it stopped. Moved or renamed folders are picked up by the
watcher. Long server operations are polled instead of waiting on one
request. See [docs/reference.md](docs/reference.md) for details.

---

## ❓ FAQ & troubleshooting

<details>
<summary><b>Does any of my data leave my machine?</b></summary>

No. There is no telemetry. Indexes are stored locally (or in *your* OpenSearch/Elasticsearch). Cloud AI providers only work if you explicitly set `privacy.external_ai_allowed: true`.
</details>

<details>
<summary><b>A folder shows <code>offline</code></b></summary>

The folder (for example a network share) could not be reached. Its existing knowledge is **kept, never deleted**. Reconnect it and run `docmonk index`.
</details>

<details>
<summary><b>A source shows <code>errors</code> or <code>retrying</code></b></summary>

Run `docmonk status --errors` to see the failing files and why. Files that fail temporarily are retried automatically with backoff. Files that keep failing (corrupt or password-protected) are recorded as failed, and the rest of the run continues.
</details>

<details>
<summary><b>Relationships show <code>stale</code> or <code>failed</code></b></summary>

The index itself is published and searchable; only the relationship graph (callers, impact, doc↔code links) is not current. `stale` usually means a file changed while the run was going: the next `docmonk index` picks it up. To retry just the graphs, run `docmonk relationships build`. `docmonk status --verbose` shows the reason.
</details>

<details>
<summary><b>"Another DocMonk process holds index.lock"</b></summary>

Another index run, the daemon or the Admin UI is indexing right now. The error names the PID and the operation. Wait for it, or stop it with `docmonk daemon stop`. **Do not delete the lock file.** It is released automatically when its owner exits or crashes.
</details>

<details>
<summary><b>The first index is slow</b></summary>

The first run downloads the document models once, and every file is new. Later runs only process changes. See the *Faster indexing tips* above, and watch progress with `docmonk status --watch`.
</details>

<details>
<summary><b>Something looks broken</b></summary>

Run `docmonk doctor`. It checks the runtime folder, sources, databases, the index lock, the storage backend and AI providers. If the index itself is damaged: `docmonk rebuild --fresh`. Your previous index is restored if the rebuild fails.
</details>

---

<div align="center">

**📄 Documents** · **💻 Code** · **🔗 Linked** · **🔒 Local**

🎞️ [Overview slides](docs/docmonk-overview.html) · 📘 [Full reference](docs/reference.md) · 🖥️ [Admin UI guide](docs/ui.md) · 📝 [Releases](https://github.com/gzarog/DocMonk/releases) · 🤝 [Contributing](CONTRIBUTING.md) · 🔐 [Security](SECURITY.md)

MIT licensed

</div>
