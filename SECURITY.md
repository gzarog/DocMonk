# Security Policy

## Local-first by design

DocMonk is a local-first tool. By default it does not transmit indexed
source material, file contents, or derived metadata to any external
service, and network/API/MCP surfaces (`api.enabled`, `mcp.enabled`,
`privacy.external_ai_allowed`) are either disabled or bound to localhost by
default. Nothing leaves your machine unless you explicitly configure an
integration that does so.

DocMonk only reads files under source roots you explicitly register with
`docmonk source add`. `PathGuard` resolves and validates every path the
scanner touches, rejecting path traversal and symlinks that escape a
registered root, and known secret-like filenames (`.env`, `*.pem`, `*.key`,
`id_rsa`, `id_ed25519`, `credentials*`, `secrets*`) are excluded from
indexing by default.

## Admin UI

The Admin UI (`docmonk ui`) binds to loopback by default and then needs no
identity provider. It refuses to start on any other address unless remote
access is configured, and then accepts requests only from configured
reverse-proxy addresses carrying a shared secret (read from the environment)
and `X-Forwarded-Proto: https`: the proxy authenticates users and terminates
TLS, and DocMonk maps the user and groups it asserts to an `admin` or
`viewer` role. Identity headers from any other peer are refused, so they cannot
be spoofed by direct connections. State changes require a header CSRF token
and a same-origin `Origin`/`Referer`; destructive actions require explicit
confirmation. See [docs/ui.md](docs/ui.md) for the deployment contract and the
authorization matrix. The proxy is part of the trust boundary: it must strip
client-supplied identity and secret headers.

## Reporting a vulnerability

If you discover a security vulnerability in DocMonk, please report it
privately rather than opening a public issue:

- Open a GitHub security advisory ("Report a vulnerability" under the
  repository's Security tab), or
- Email the maintainers with a description of the issue, steps to
  reproduce, and its potential impact.

Please do not disclose the issue publicly until it has been triaged and a
fix or mitigation is available. We aim to acknowledge reports within a
reasonable timeframe and will credit reporters who wish to be credited once
a fix ships.
