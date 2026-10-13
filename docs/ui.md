# DocMonk Admin UI

DocMonk ships with a built-in, local-first administration web interface. It
lets you manage DocMonk from a browser instead of the CLI, while reusing the
same underlying application services — the CLI, the MCP server and the UI all
call the same code (`docmonk.service.*`), so there is one source of truth and
no business logic duplicated in the web layer.

## Starting the UI

```bash
docmonk init      # once, if you haven't already
docmonk ui
```

```text
DocMonk Admin UI
URL: http://127.0.0.1:8765
Opening browser...
Press Ctrl+C to stop.
```

Your browser opens automatically to the dashboard. Press `Ctrl+C` to stop the
server; it shuts down cleanly and closes all database connections.

## CLI options

| Option | Default | Description |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Interface to bind. Leave as localhost for the default local-first mode. |
| `--port` | `8765` | Port to listen on. |
| `--no-browser` | off | Do not open a browser automatically (useful on headless hosts). |

```bash
docmonk ui --port 9000
docmonk ui --no-browser
```

## Admin screens

- **Dashboard** — DocMonk version, source/enabled counts, indexed/queued/failed
  files, document/symbol/relationship counts, database + index size, daemon
  state, tokenizer identity, and the latest indexing errors.
- **Sources** — list, add (with include/exclude patterns), enable, disable and
  remove sources, plus a per-source detail screen. Removing a source deletes
  DocMonk's indexed data for it but never your original files.
- **Indexing** — start an indexing pass, rebuild a source or everything, inspect
  failed files, and watch live progress over Server-Sent Events.
- **Documents** — browse indexed documents with source/format/filename filters
  and pagination, and drill into a document to inspect its extracted chunks
  (heading path, page, embedding state). Email attachments are listed as
  their own documents, tagged "attachment" and named
  `email.eml -> attachment-name`, with a link back to the parent email.
- **Search** — run lexical, semantic or hybrid queries and see ranked results,
  scores and snippets. Semantic search degrades gracefully with a reason when
  it is disabled or unavailable.
- **Knowledge** — search code symbols and inspect callers, callees, references
  and blast-radius impact.
- **AI** — see the selected provider, model, authentication status and the full
  provider catalog, and test a provider's connectivity. Credentials are never
  shown — only "configured / not configured".
- **Configuration** — a typed form generated from `DocMonkConfig`, validated
  with the same validation as the CLI before writing. Environment-overridden
  values are shown read-only with their variable name; sensitive fields show
  only whether they are configured.
- **Daemon** — start, stop and restart the background daemon and see watched
  sources.
- **Health** — the same checks as `docmonk doctor` / `docmonk health`.
- **Backups** — create, download and restore backups, and see update status.
- **Logs** — read `docmonk.log` newest-first with level, component, text and
  errors-only filters.
- **System** — installed vs. latest version, update channel, home directory and
  tokenizer identity.

## Security

The first release is **local-first and single-user**. Its security model is:

- **Localhost by default.** The server binds to `127.0.0.1`; it is not reachable
  from other machines unless you explicitly change `--host`.
- **CSRF protection.** Every state-changing request must carry a double-submit
  CSRF token (sent automatically by the UI via an `X-CSRF-Token` header on HTMX
  requests). Requests without a valid token are rejected with `403`.
- **Host-header validation.** Requests whose `Host` header is not an allowed
  localhost value are rejected with `421`, defending the local admin service
  against DNS-rebinding attacks from a malicious web page.
- **Confirmation for destructive actions.** Removing a source, rebuilding
  indexes and restoring a backup all require an explicit confirmation click.
- **No credentials displayed.** API keys, tokens and passwords are never
  rendered — only whether a provider is configured.

There is intentionally **no authentication** in this release, because the UI is
localhost-only and single-user. Exposing the UI to a network (`--host 0.0.0.0`)
is **not** the supported default and has no auth model yet: the command warns
when you bind to a non-localhost address. A future shared/remote deployment
must add authentication, roles and an audit log before it is used off-host.

## Localhost binding

The default bind address is `127.0.0.1`. To reach the UI from another device you
would need to bind a routable address (`--host 0.0.0.0`) — do this only on a
trusted network and behind your own authentication/proxy, never on an untrusted
one. Prefer an SSH tunnel instead:

```bash
ssh -L 8765:127.0.0.1:8765 you@server   # then open http://127.0.0.1:8765 locally
```

## Running headless

On a machine with no browser or no `$DISPLAY`, pass `--no-browser`; the URL is
printed for you to open (or tunnel to) manually. Failure to launch a browser is
never fatal.

## Running under WSL

Start `docmonk ui` inside WSL and open `http://127.0.0.1:8765` from your Windows
browser — recent WSL forwards localhost automatically. If it does not resolve,
use the WSL VM's IP (`ip addr` inside WSL) or an SSH tunnel.

## Running in Docker

Bind to all interfaces inside the container and publish the port, keeping access
restricted at the host/network layer (the in-container UI still has no auth):

```bash
docker run -p 127.0.0.1:8765:8765 your-docmonk-image docmonk ui --host 0.0.0.0 --no-browser
```

Publishing to `127.0.0.1:8765` on the host (as above) keeps it reachable only
from the host machine.

## Troubleshooting

- **Port already in use** — start with `--port` on a free port.
- **`CSRF validation failed` (403)** — reload the page so a fresh CSRF cookie is
  issued; ensure cookies are enabled for `127.0.0.1`.
- **`Host not allowed` (421)** — you reached the server via a hostname it does
  not recognize. Use `http://127.0.0.1:<port>` (or `localhost`).
- **Blank/unstyled page** — confirm the wheel shipped the UI assets; the
  templates and vendored HTMX/CSS live under `docmonk/ui/` and are declared in
  the build configuration.
- **Indexing looks stuck** — check the Logs screen and the Indexing page's
  failed-files list; heavy first-time indexing loads the document/embedding
  models, which can take a while on first run.
