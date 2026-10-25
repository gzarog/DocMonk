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
PDF, Word, PowerPoint, Excel, CSV, HTML, Markdown, OpenDocument, EPUB, emails with their attachments, plus OCR for scanned files. Headings, tables and page numbers are preserved.

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

**Verification:** the installers and `docmonk update` check every archive against the release's `SHA256SUMS`. Once a release-signing key is configured, they also require a valid minisign signature on `SHA256SUMS` and refuse to install without one: install [`minisign`](https://jedisct1.github.io/minisign/) first. `DOCMONK_INSECURE_SKIP_SIGNATURE=1` is the only opt-out, and it prints a warning.

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

You don't need to manage any of this, but it helps to know what you see in `docmonk status`:

1. **Search is ready first.** Each folder becomes searchable as soon as its files are indexed.
2. **Links follow.** Once every folder is indexed, DocMonk builds the relationships between them: who calls what, and which documents mention which code.
3. **Only changes are redone.** On the next run, only new or edited files are processed again.

| You see | What it means |
|---|---|
| Relationships `ready` | Callers, impact and doc↔code links match the latest index |
| Relationships `stale` | A file changed during the run. You still get the previous links, and the next `docmonk index` refreshes them |
| Relationships `failed` | Search still works. Retry just the links with `docmonk relationships build` |

Details: [Indexing behaviour](docs/reference.md#indexing-behaviour).

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
| **EML** | Subject, headers and body · attachments in any format above are indexed with the email |

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

Need answers you can check? `docmonk evidence` returns the same kind of result with every citation re-verified against the index, and a verdict: `supported`, `partial` or `insufficient_evidence`.

```bash
docmonk evidence "compare billing and ledger retry limits"
docmonk search "refund policy" --path docs/ --kind document   # narrow by folder, path or kind
```

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

Tools exposed: `docmonk_explore`, `docmonk_evidence`, `docmonk_search`, `docmonk_documents`, `docmonk_symbol`, `docmonk_callers`, `docmonk_callees`, `docmonk_impact`, `docmonk_status`, `docmonk_ask`.

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

For teams that share one large index. `init` checks the cluster first (OpenSearch 2+ or Elasticsearch 8+), and credentials are read only from environment variables such as `DOCMONK_OPENSEARCH_USERNAME` / `DOCMONK_OPENSEARCH_PASSWORD`, never from `config.yaml`. A failed rebuild keeps serving the previous index.

Setup and credentials: [Server storage](docs/reference.md#server-storage-opensearch--elasticsearch).

</details>

---

## 📋 Command cheat sheet

| Task | Command |
|---|---|
| Set up | `docmonk init` |
| Add / list / remove a folder | `docmonk source add PATH [--include GLOB] [--exclude GLOB]` · `source list` · `source info ID` · `source remove ID` |
| Pause / resume a folder | `docmonk source disable ID` · `source enable ID` |
| Index now | `docmonk index [--source ID]` |
| Retry relationship graphs only | `docmonk relationships build [--source ID]` |
| Index automatically | `docmonk daemon start` · `daemon status` · `daemon stop` (or `docmonk watch` in the foreground) |
| Is it working? | `docmonk status [--watch] [--errors] [--verbose] [--json]` |
| Search | `docmonk search "text" [--source ID] [--path PREFIX] [--kind document] [--hybrid] [--table] [--json]` |
| Ask across everything | `docmonk explore "question"` · `docmonk evidence "question"` · `docmonk ask "question"` (AI) |
| Code navigation | `docmonk symbol` · `callers` · `callees` · `references` · `impact` `NAME` |
| List documents | `docmonk docs [--source ID]` |
| Pin a doc↔code link | `docmonk link add ENTITY DOCUMENT` · `link list` · `link remove ID` |
| Web UI | `docmonk ui` |
| Health check | `docmonk doctor` · `docmonk health` |
| Settings | `docmonk config show` · `config get KEY` · `config set KEY VALUE` |
| Backup / restore | `docmonk backup [PATH]` · `docmonk restore ARCHIVE` |
| Rebuild from scratch | `docmonk rebuild [--source ID] [--fresh]` |
| Add semantic vectors to an existing index | `docmonk vectors backfill [--source ID]` |
| Update DocMonk | `docmonk update check` · `update install` |
| Uninstall | `docmonk uninstall [--keep-data]` |

Every command supports `--help`.

---

## 📊 Is it working? `docmonk status`

`docmonk status` tells you whether indexing is **running, finished, stuck or failing**, and what failed:

```
DocMonk status  [local mode, sqlite, authoritative]
Health: HEALTHY  (0 problem(s))
Sources: 2 registered, 2 enabled, 0 active, 0 failed, 0 offline
Published: 2400 indexed, 0 failed, 0 retrying of 2400 files; 18000 entities, 320 documents, 9100 chunks, 41000 relationships
== Sources ==
Source          State      Access  Indexed  Failed  Retry  In progress  Last scan  Last error
src_2b2faec82b  completed  online  320      0       0      -            2m ago     -
src_7c41d09a3e  completed  online  2080     0       0      -            2m ago     -
```

| Flag | What it does |
|---|---|
| `--watch` | Live view (Ctrl+C to exit) |
| `--errors` | Only the sources and files that need attention |
| `--verbose` | Full ids and paths, plus each source's relationship state |
| `--json` | Machine-readable report (the same data the MCP tool and the Admin UI show) |

**Health:** `healthy` · `degraded` (some files failed or are retrying, or a folder is offline) · `failed` (indexing is stuck or crashed, or storage can't be reached) · `unknown`. When it isn't healthy, the problems list says why and what to do. A number shown as `N/A` isn't known yet (for example before a scan finishes).

> ⏱️ **"stalled"** means an indexing run gave no sign of life for **120 seconds**, even through long OCR or embedding steps, so the process is stuck or gone. More: [Status and observability](docs/reference.md#status-and-observability).

---

## ⚙️ Configuration

Every setting is in [the reference](docs/reference.md). Settings live in `~/.docmonk/config.yaml` (Windows: `%LOCALAPPDATA%\DocMonk`); unknown keys are rejected. Change them with `docmonk config set KEY VALUE`, or override any of them with an environment variable, e.g. `DOCMONK_DOCUMENTS__OCR=off`. Set `DOCMONK_HOME` to move the whole runtime folder.

| Setting | Default | What it does |
|---|---|---|
| `documents.ocr` | `auto` | OCR scanned PDFs: `off` · `auto` · `always` |
| `documents.image_ocr` | `false` | OCR plain images (PNG/JPG/TIFF) |
| `documents.max_pages` | `1000` | Skip larger PDFs |
| `documents.pdf_mode` | `accurate` | `fast` = text layer only, much faster, no layout/tables |
| `documents.email_attachments` | `true` | Index supported `.eml` attachments as child documents |
| `search.semantic` | `false` | Local semantic (meaning-based) search |
| `search.reranker.enabled` | `false` | Local reranking of hybrid results (better order, slower) |
| `indexing.watch` | `true` | Daemon watches folders for changes |
| `indexing.max_file_size_mb` | `100` | Skip larger files |
| `indexing.relationships_enabled` | `true` | Build relationships (callers, impact, doc↔code links). `false` keeps search only |
| `indexing.pipeline_relationships` | `false` | `true`: build each source's relationships right after its own index, instead of after every source is indexed |
| `graph.require_current` | `false` | `true`: hide links while they are being refreshed, instead of showing the previous ones marked `stale` |
| `ai.provider` / `ai.model` | `none` | Optional AI answers for `docmonk ask` |
| `privacy.external_ai_allowed` | `false` | Required for cloud AI providers |
| `updates.enabled` | `true` | Check for new versions in the background and tell you when one is out |

<details>
<summary><b>⚡ Faster indexing tips</b></summary>

| Situation | Try |
|---|---|
| Many digital (non-scanned) PDFs, speed matters more than layout | `docmonk config set documents.pdf_mode fast`: 81s → 1.6s on a 24-PDF test set |
| Big first index | Let it finish once. After that only changed files are reprocessed |

Changing `pdf_mode` reprocesses PDFs on the next `docmonk index`.

</details>

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

On the first run every file is new, and PDF layout analysis and OCR take time. Later runs only process changes. See the *Faster indexing tips* above, and watch progress with `docmonk status --watch`.
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
