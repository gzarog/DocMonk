# ADR 0029: Native `update` and installers

Status: accepted

## Decisions

- **Layout:** versioned directories plus a `current` pointer. `update rollback`
  switches back to the previous version.
- **Assets:** `docmonk-<ver>-<target>.tar.gz`, or `.zip` on Windows. Each
  release also carries `SHA256SUMS`, and `SHA256SUMS.minisig` when it is
  signed.
- **Signatures:** minisign. The public key is compiled in through
  `DOCMONK_MINISIGN_PUBKEY` at build time, and the installers carry the same
  key.
- **Background check:** a periodic background check and a once-per-version
  startup notice.

## Layout

```text
<install_dir>/versions/<ver>/{docmonk[.exe], models/, README.md, LICENSE}
<install_dir>/current            POSIX: symlink -> versions/<ver>
                                 Windows: a text file holding <ver>
<install_dir>/bin/docmonk.exe    Windows only: a copy of the current exe
<install_dir>/install_state.json {"current": <ver>, "previous": <ver>|null}
<home>/install_info.json         {"install_method": "native", install_dir, bin_dir}
```

- **POSIX:** `<bin_dir>/docmonk` links to `current/docmonk`. Switching is an
  atomic rename of a temporary symlink over `current`.
- **Windows:** a running exe can be renamed but not overwritten. The swap
  renames it to `docmonk.exe.old`, or to `.old-<pid>` while an older one is
  still running, then copies the new exe in.
- **Retention:** two versions are kept, the current one and the previous one.
  Older versions and stale `.partial` directories are pruned after an
  install.
- **Models:** bundled models are copied into `<home>/models`, where
  `docmonk-ml` looks for them. This happens on both install and rollback.
  A release that bundles no models leaves the existing ones untouched.

## `docmonk update`

| Command | Behaviour |
|---------|-----------|
| `check` (also bare `update`) | Queries GitHub, writes `update.json`, prints the result as text or JSON. |
| `status` | Reads the cache only. |
| `install` | Fetches the latest release (the tag must be strict `MAJOR.MINOR.PATCH`) and stops if it is not newer. Downloads the archive and `SHA256SUMS` from that tag's assets. When a key is compiled in, it verifies the minisign signature over `SHA256SUMS` first. Then it checks the archive digest, unpacks to `versions/<ver>.partial` (refusing absolute, `..`, link and device entries) and renames it into place. It runs the self-check, copies the models, switches, and runs the new binary's `doctor`. A failure exits with code 7. |
| `rollback` | Switches `current` and the models back to `previous`. |

**Self-check:** the new binary's `version --json` must report the version it
was published as. If it does not, the install is refused and its directory
is removed. After the switch the check runs again. If the new version fails
then, the install rolls back automatically.

**Download URLs** are built from the validated tag and this repository's
hardcoded owner and name, never from release text.

**Test host:** `DOCMONK_UPDATE_TEST_BASE` replaces both the API host and the
download host. Only debug builds honour it.

## Background check and notice

Every command except `update`, `version`, `serve`, `daemon`, `watch` and
`doctor` runs the startup step. Setting
`DOCMONK_NO_UPDATE_CHECK` turns it off entirely. The step does two things:

- **Notice:** it reads `update.json` and prints the "newer version" notice
  to stderr, once per version.
- **Background check:** when the cache is older than
  `updates.check_interval_hours`, it spawns a detached
  `docmonk update background-check`. The marker carries over only while the
  latest version is unchanged.

Debug builds only run the background check against the test host, so
development and test runs never contact GitHub.

## Installers

`install.sh` and `install.ps1` install the native binary. Each one:

1. detects the target and refuses any target outside the four release
   targets;
2. resolves the version from `DOCMONK_VERSION`, or else from the latest
   release;
3. downloads the archive from GitHub, or from `DOCMONK_DOWNLOAD_BASE` (a URL
   or a local directory);
4. verifies the sha256 digest, and the minisign signature when a key is set
   and `minisign` is installed;
5. writes the same layout, state and `install_info.json` as above.

The `minisign` step is optional because the installer is itself fetched over
HTTPS. The binary's own self-update always verifies the signature once a key
is compiled in.

## Release assets

`cargo xtask package` builds the archives. Each archive is reproducible:
entries are sorted and use fixed mtimes. It also maintains `SHA256SUMS`
(`cargo xtask release-manifest` rewrites it over a whole directory). Tags
are `v<MAJOR.MINOR.PATCH>`. The release workflow (ADR 0030) calls it for
each target.

## Tests

- **Unit tests** cover versioning, the cache, digests, minisign, layout
  switch/prune/rollback, and archive unpacking, including path-escape
  rejection. The minisign vectors use a fixed test-only seed.
- **`docmonk-cli/tests/update.rs`** (POSIX) runs against a local HTTP stand-in
  release host. It covers `check` and `status`, rejection of non-semver tags,
  the once-only notice, a stale cache triggering the background check,
  install refusal on a bad checksum, install refusal when the binary reports
  the wrong version, a successful install with models copied, rollback, and
  refusal for a non-native install.
- **CI (`rust.yml`)** packages two versions from this commit's build. It
  installs them with `install.sh` on Linux and macOS and `install.ps1` on
  Windows, then rolls back. It also checks that a tampered archive is
  refused. On Windows the second install runs while the daemon holds the
  current exe. These jobs are blocking.

## Install method

- `install_method` is `native`. An `install_info.json` with any other
  method does not belong to a native install, so `update install` refuses
  it and says to reinstall.

## `updates.channel`

- **`stable`** (the default): follows GitHub's `releases/latest`, which
  skips pre-releases.
- **`prerelease`**: reads the release list (`releases?per_page=30`) and
  picks the newest strict-`MAJOR.MINOR.PATCH` release that ships the
  archive for this platform. Drafts are skipped, and pre-releases are
  included. Releases without an archive for this platform are skipped.
- **Any other value**: stays stable; values are not validated.

This channel lets a published pre-release be tested end to end with
`update install` and `rollback`.

## TLS trust roots

reqwest is built with `rustls-tls` plus `rustls-tls-native-roots`, so every
HTTPS client trusts the operating system's trust store, including
`SSL_CERT_FILE`, in addition to the bundled Mozilla roots. Those clients
are update, AI providers, and the OpenSearch/Elasticsearch server backend.

With only the bundled roots, `docmonk update` and the cloud providers
would fail behind TLS-inspecting corporate proxies and with private CAs.
