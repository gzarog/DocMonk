# DocMonk Admin UI

DocMonk ships with a built-in, local-first administration web interface. It
lets you manage DocMonk from a browser instead of the CLI, while reusing the
same underlying application services — the CLI, the MCP server and the UI all
call the same code (the `docmonk-service` crate), so there is one source of truth and
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
| `--host` | `127.0.0.1` | Interface to bind. A non-loopback address is refused unless [remote access](#remote-access-behind-an-authenticating-proxy) is configured. |
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

By default the UI is **local-only and single-user**:

- **Loopback only.** The server binds to `127.0.0.1` and needs no identity
  provider; every local request acts as the administrator.
- **Fails closed off-host.** `docmonk ui --host 0.0.0.0` (or any non-loopback
  address) refuses to start unless authenticated remote access is fully
  configured (below). It never serves an unauthenticated network listener.
- **CSRF protection.** Every state-changing request needs the double-submit
  CSRF token in the `X-CSRF-Token` header (sent automatically on HTMX
  requests); tokens in URLs are not accepted. The cookie is `HttpOnly`,
  `SameSite=Strict` and expires after 12 hours.
- **Same-origin check.** A state-changing request whose `Origin` (or
  `Referer`) names another site is refused with `403`.
- **Host-header validation.** Requests whose `Host` is not an allowed value are
  rejected with `421` (DNS-rebinding defense).
- **Server-side confirmation.** Removing a source, rebuilding indexes and
  restoring a backup require `confirm=yes`, which the confirmation dialog sends;
  a bare or replayed POST does nothing.
- **Hardening headers.** `X-Frame-Options: DENY`, `X-Content-Type-Options:
  nosniff`, `Referrer-Policy: same-origin`; request bodies over 1 MiB are
  refused.
- **No credentials displayed.** API keys, tokens and passwords are never
  rendered — only whether a provider is configured.

## Remote access behind an authenticating proxy

DocMonk does not log users in itself. To use the UI from other machines, put
an authenticating reverse proxy (an SSO gateway, `oauth2-proxy`, a corporate
access proxy...) in front of it. The proxy terminates TLS, authenticates the
user, and forwards to DocMonk with:

| Header | Value |
| --- | --- |
| `X-DocMonk-Proxy-Secret` | the shared secret (from the environment, at least 32 characters) |
| `X-Forwarded-Proto` | `https` |
| `X-Forwarded-User` (`ui.user_header`) | the authenticated user name |
| `X-Forwarded-Groups` (`ui.groups_header`) | comma-separated groups, optional |

The proxy **must strip** these headers from incoming client requests.

```yaml
ui:
  remote_enabled: true
  allowed_hosts: [docmonk.example.com]   # the public host name(s)
  trusted_proxies: [10.0.0.5]            # the proxy's IP address(es)
  proxy_secret_env: DOCMONK_UI_PROXY_SECRET
  admin_groups: [docmonk-admins]
  viewer_groups: [engineering]
```

```bash
export DOCMONK_UI_PROXY_SECRET="$(openssl rand -hex 32)"   # also given to the proxy
docmonk ui --host 0.0.0.0 --no-browser
```

Startup is refused unless `remote_enabled`, `allowed_hosts`,
`trusted_proxies`, an admin user or group, and the secret are all present.
Once running, a request is accepted only if it comes from a trusted proxy
address **and** carries the secret **and** `X-Forwarded-Proto: https`;
identity headers from anywhere else are ignored and the request is refused
(`403`). A request without a user gets `401`; a user with no role, or a
viewer on a privileged route, gets `403`. Browsers must send an `Origin` or
`Referer` naming an allowed host for any state change. The CSRF cookie becomes
`__Host-docmonk_csrf` with `Secure`, internal error details are logged rather
than returned, and every authorized change and every denial is logged with the
user.

### Authorization matrix

| Route | Local | Viewer | Admin |
| --- | --- | --- | --- |
| `GET /health`, `GET /ready` | yes | yes (the proxy may probe without a user) | yes |
| Read-only pages and APIs (dashboard, sources, indexing, documents, search, knowledge, AI, daemon status, health, system, status API, event stream) | yes | yes | yes |
| `GET /config`, `/logs`, `/daemon/logs`, `/backups`, `/backups/download/*` | yes | no | yes |
| Any `POST` (sources, indexing/rebuild, config, AI test, daemon controls, backup create/restore) | yes | no | yes |

## Localhost binding

The default bind address is `127.0.0.1`. To reach a local-only UI from another
device, prefer an SSH tunnel to remote mode:

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

Inside a container a published port is a network listener, so
`--host 0.0.0.0` is refused unless remote access is configured. For local use,
share the host's network and keep the loopback bind:

```bash
docker run --network host your-docmonk-image docmonk ui --no-browser
```

For shared use, run DocMonk in remote mode behind an authenticating proxy
container, with `ui.trusted_proxies` set to the proxy container's address.

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
