# ADR 0042: Credentials never enter the index

Status: accepted and implemented (improvement roadmap H4).

## Context

Secret filtering was by file name only (eight patterns: `.env`, `*.pem`,
`id_rsa`, ...). A credential written into a file DocMonk does index
(`appsettings.json`, a YAML deployment file, a Markdown runbook, a code
sample, an email) was stored as written, embedded, returned by search
and evidence, and served to AI agents through MCP. Log redaction
(`docmonk-telemetry`) never applied to indexed text.

## Decision

Two layers, both in the indexing pipeline, so every read surface
(search, evidence, explore, documents, the Admin UI, MCP) only ever sees
what was stored:

1. **File names.** The built-in list grows to cover `.netrc`,
   `.git-credentials`, `.npmrc`, `.pypirc`, kubeconfig files, DSA/ECDSA
   keys, PKCS#12/JKS keystores, KeePass databases and Terraform state.
   `indexing.secret_patterns` extends the list; it never replaces it, and
   include patterns never override it.
2. **Content.** `docmonk_core::secrets::redact` runs over everything a
   processor extracted from a file (`docmonk_indexing::redact`), right
   after the processor and before storage and embedding: chunk and passage
   text, search and embedding text, table cells, headings, entity
   signatures, relationship evidence and document metadata. The graph
   stage redacts the relationship evidence it derives the same way.

Redaction replaces only the secret **value** with `[REDACTED:<kind>]`,
on the same line. Line numbers, spans and citation verification (which
reads the stored snapshot) are unaffected, and the key name stays
searchable (`DB_PASSWORD` still finds the configuration line). The rules
are conservative about code: an assignment is redacted only when its
value is a literal (quoted, or a bare value on a config-style line),
never an expression or a reference such as `password = read_password()`.
Private key bodies are recognised inside `BEGIN/END ... PRIVATE KEY`
blocks, before an `END` line whose `BEGIN` lies in an earlier chunk, and
as runs of three or more 64-column base64 lines, so a key split across
chunks is still caught. Certificates and public keys are left alone.

`indexing.redact_secrets` (default `true`) turns content redaction off.
`REDACTION_VERSION` is folded into the parser and chunker identities and
into the graph derivation, so toggling the setting or changing a rule
rebuilds the affected sources (ADR 0032: no conversion of older
indexes).

## Consequences

* A per-source override is not provided yet: the source record has no
  per-source settings and adding them is a schema change. The global
  setting is the rollback (`indexing.redact_secrets: false`, then
  rebuild).
* Pattern-based detection has false negatives (an unrecognised token
  format in free text) and rare false positives (a 64-column base64 blob
  that is not a key). Both are covered by the fixture corpus in
  `fixtures/secrets/`; new formats are added as rules with a version
  bump.
* Source files are never modified; only what DocMonk stores is redacted.
