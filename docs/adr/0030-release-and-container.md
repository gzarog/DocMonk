# ADR 0030: Native release workflow and container

Status: accepted

## Decisions

- **Workflow:** `.github/workflows/rust-release.yml` publishes releases.
- **Container:** published to GHCR as a multi-arch image
  (`ghcr.io/gzarog/docmonk`, linux/amd64 and linux/arm64).
- **Signing:** a hook. When the `MINISIGN_SECRET_KEY` secret exists the
  workflow signs `SHA256SUMS`; otherwise it publishes checksums only.
- **Installer CI:** the installer jobs from ADR 0029 run in `rust.yml` on
  every PR.

## Release workflow

The workflow starts in three ways:

- automatically, after the CI workflow passes on a push to `main`: the
  next `vX.Y.Z` is computed (a patch bump, or a minor bump when the merged
  commit message contains `[release minor]`), tagged and published as a
  full release;
- a pushed `v<MAJOR.MINOR.PATCH>` tag: a pre-release with the image,
  for dry runs;
- manual dispatch (default branch only), with inputs `version` (strict
  `MAJOR.MINOR.PATCH`), `prerelease` (default true) and `image` (default
  true).

The GitHub release itself is always tagged `v<version>`, because that is
the tag `docmonk update` expects.

1. **validate:** checks the version format and refuses a tag `v<version>`
   that already exists.
2. **build:** builds one archive per target on its own runner:

   | Target | Runner |
   |--------|--------|
   | `x86_64-unknown-linux-gnu` | ubuntu-22.04 (an older glibc, for wider compatibility) |
   | `aarch64-apple-darwin` | macos-latest |
   | `x86_64-apple-darwin` | macos-latest (cross target) |
   | `x86_64-pc-windows-msvc` | windows-latest |

   Each build does the following:
   - fetches the pinned models with `scripts/fetch_models.sh`, using
     the same digests as the crates' manifests;
   - runs `cargo build --release --locked` with `DOCMONK_BUILD_VERSION`,
     `DOCMONK_BUILD_COMMIT` and `DOCMONK_MINISIGN_PUBKEY` (from
     `vars.MINISIGN_PUBLIC_KEY`);
   - smoke-tests `version --json`;
   - packages the archive with `cargo xtask package --models`.
3. **release:** assembles and publishes the GitHub release:
   - writes `SHA256SUMS` over every archive (`cargo xtask release-manifest`);
   - writes `install.sh` and `install.ps1` with the public key filled in;
   - signs `SHA256SUMS` with minisign when the secret is set, then verifies
     the signature against the public variable;
   - runs `gh release create v<version> --target <sha>`.
4. **image / image-manifest:** builds the image natively on amd64 and arm64
   runners and pushes each by digest. Each per-platform image is
   smoke-tested, then the two are joined into a manifest list tagged
   `<version>`, plus `latest` for a full release.

Manual and tag-triggered releases are pre-releases by default. GitHub's
"latest release" endpoint, which `docmonk update` follows, skips
pre-releases, so such a release reaches existing users only once it is
published as a full release.

## Container (`Dockerfile`)

- **Build stage:** `rust:1.90.0-slim-bookworm`, matching
  `rust-toolchain.toml`. It builds `docmonk-cli` with `--locked` and fetches
  the models.
- **Runtime stage:** `debian:bookworm-slim` with `ca-certificates` and
  `tini`, and nothing else.
- **User and data:** runs as the non-root user `docmonk` (uid 10001).
  `DOCMONK_HOME=/data` is a volume.
- **Models:** baked in at `/opt/docmonk/models`, set through
  `DOCMONK_MODELS_DIR` and `DOCMONK_OCR_MODELS_DIR`.
- **Updates:** `DOCMONK_NO_UPDATE_CHECK=1`, because an image is updated by
  pulling a new tag.
- **Entrypoint:** `docmonk`, with default arguments `--help`.

Example invocations:
- MCP over stdio: `docker run -i -v docmonk:/data ghcr.io/gzarog/docmonk serve --mcp`
- Admin UI: `docker run -p 8765:8765 -v docmonk:/data ghcr.io/gzarog/docmonk ui --host 0.0.0.0 --no-browser`

There is no cross-compilation. Multi-arch comes from native runners, so the
Dockerfile stays a plain two-stage build.

## Setting up signing

```sh
minisign -G -p docmonk.pub -s docmonk.key
```

Then configure the repository:
- **Repository variable `MINISIGN_PUBLIC_KEY`:** the second line of
  `docmonk.pub`, which is the base64 key.
- **Secret `MINISIGN_SECRET_KEY`:** the whole of `docmonk.key`.
- **Secret `MINISIGN_PASSWORD`:** the key's password.

From then on, every binary refuses an update whose `SHA256SUMS` signature
does not verify. The installers check the signature too when `minisign` is
installed.

## CI (`rust.yml`)

- `install-script-tests` (Linux and macOS) and
  `install-script-tests-windows` exercise the installers (ADR 0029).
- `container-image` builds the amd64 image and checks that:
  - `version --json` reports the build-arg version;
  - no script interpreter is installed in the image;
  - `init` followed by `doctor` succeeds;
  - the models are present.

## Not done here

- macOS notarization and Windows Authenticode signing. These need
  certificates the project does not have. The minisign hook covers
  integrity.
