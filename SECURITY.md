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
