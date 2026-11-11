# ADR 0045: MCP resources and prompts

Status: accepted and implemented (upgrade plan A3).

## Context

The MCP server advertised the `resources` and `prompts` capabilities but
answered every list with nothing. Two things were missing for a client:

- A way to attach a known document or symbol to the model's context
  without a tool call. Clients offer resources in a picker and re-read
  them by URI.
- The evidence-first way of working (explore, then verified evidence,
  cite or abstain) as something a user picks, instead of text every user
  writes again.

Anything new that reads the index must keep the guarantees the tools
have: only the published build is read, a source in `mcp.hidden_sources`
does not exist for an agent, stored text is redacted (ADR 0042), calls
are bounded and logged.

## Decision

### Resource URIs

| URI | Content | Type |
|---|---|---|
| `docmonk://source/{source_id}` | The source's id, path, published build and backend | `application/json` |
| `docmonk://document/{source_id}/{document_id}{?from}` | The document's chunks in reading order, from chunk ordinal `from` (default 0) | `text/plain` |
| `docmonk://chunk/{source_id}/{chunk_id}` | One chunk with its path, heading path, pages and its document's URI | `text/plain` |
| `docmonk://symbol/{source_id}/{entity_id}` | A symbol's qualified name, kind, file, lines, signature and body passages | `text/plain` |

The ids are the ones tool results already carry: `source_id` everywhere,
an evidence entry's `record_id` (a chunk for `kind: document`, an entity
for `kind: entity`), an explore entity's `entity_id`, a link's
`document_id`. A document evidence entry that cites a title hit carries
the document id; read as a chunk it gives the document's first chunk,
which is what citation verification reads for it. Ids are
percent-encoded as one path segment. A URI carries no build id: it names
a record, and a read answers from the build published at that moment and
says which build that is in the text.

`resources/list` returns one `source` resource per visible source with a
published build, without pagination (a home has few sources).
`resources/templates/list` returns the `document`, `chunk` and `symbol`
templates. Documents, chunks and symbols are not enumerated: an index
holds too many, and tools are how an agent finds them.

### Pagination of a document

A document is returned chunk by chunk until three quarters of
`mcp.max_result_bytes` is used; the rest of the budget is left for JSON
escaping. A cut read ends with the line `[continues at <uri>]`, where
the URI is the same document with `from` set to the next chunk's
ordinal. A read always returns at least one chunk, so following the
line always advances. A single chunk or symbol longer than the budget is
cut on a character boundary and says so.

### Guards

`resources/list` and `resources/read` run through the same path as a
tool call (`run_tool`):

- **Published build.** Sources are opened with `open_sources`, which
  pins each to its active build; records are read through the
  `KnowledgeRead` port, so local and server storage behave alike.
- **Visibility.** The call runs in the `with_hidden_sources` scope. The
  source id in a URI is checked like a tool's `source_id` argument
  before anything is opened, a hidden source is not opened, and the
  result is scrubbed like a tool result. A hidden source is not listed.
- **One answer for everything unreadable.** A malformed URI, another
  scheme, an unknown source, a hidden source and an unknown record all
  get JSON-RPC error `-32002` "Resource not found" with the URI in
  `data`. A hidden source cannot be told from one that does not exist.
- **Redaction.** Text is stored redacted (ADR 0042) and resources return
  stored text only; nothing is read from the original files.
- **Limits and log.** The executor's concurrency limit and
  `mcp.request_timeout_seconds` apply. With `mcp.query_log`, each list
  and read is one line with `tool` `resources/list` or `resources/read`,
  the URI as its argument, whether it succeeded and the source it
  touched.

Subscriptions and list-change notifications are not offered
(`subscribe: false`, `listChanged: false`): the stdio and HTTP
transports send nothing unasked.

### Prompts

Two prompts, each with one required argument:

| Prompt | Argument | Workflow |
|---|---|---|
| `answer_with_evidence` | `question` | `docmonk_explore`, then `docmonk_evidence`; answer only from the evidence with `[E#]` citations, or say the evidence is insufficient |
| `impact_review` | `symbol` | `docmonk_symbol`, `docmonk_impact`, then `docmonk_links` and `docmonk_read` for the linked specifications |

A prompt is fixed text around the argument and reads nothing from the
index, so it needs no visibility check and is not logged. An unknown
prompt, a missing argument or one longer than 4,000 characters is
`-32602` (invalid params).

## Consequences

- A client can attach what a tool result pointed to, and re-read it
  later; after a re-index the same URI answers from the new build, or is
  "not found" when the record's id changed.
- `resources/list` is no longer empty once a source is indexed, and
  names source paths. It hides what `mcp.hidden_sources` hides.
- Resources add no new read path into storage: they reuse the port, the
  scope and the log the tools use. A future surface (connectors, UI
  pages) should do the same.
- Not done: resource subscriptions, completion of template arguments,
  and a `passage` resource (a symbol resource returns all of a symbol's
  body passages).
