#!/usr/bin/env bash
# Classifies a PR's changed paths (one per line on stdin) for the
# conditional CI jobs in .github/workflows/rust.yml. Prints
# `installer=true|false` and `docker=true|false` (GITHUB_OUTPUT format).
# Patterns are deliberately broad: shared build inputs select both.
# Self-test: scripts/ci_changes.sh --self-test
set -euo pipefail

classify() {
  local installer=false docker=false path
  while IFS= read -r path || [ -n "$path" ]; do
    [ -z "$path" ] && continue
    case "$path" in
      Cargo.toml | Cargo.lock | rust-toolchain.toml | crates/*/Cargo.toml | xtask/Cargo.toml \
        | .github/workflows/* | .github/actions/* | scripts/ci_changes.sh)
        installer=true docker=true ;;
    esac
    case "$path" in
      install.sh | install.ps1 | crates/docmonk-update/* | xtask/*) installer=true ;;
    esac
    case "$path" in
      Dockerfile | .dockerignore | docker/* | scripts/fetch_models.sh) docker=true ;;
    esac
  done
  echo "installer=$installer"
  echo "docker=$docker"
}

if [ "${1:-}" != --self-test ]; then
  classify
  exit 0
fi

expect() { # paths(\n-separated) expected-output
  local got
  got=$(printf '%b\n' "$1" | classify | tr '\n' ' ')
  if [ "$got" != "$2 " ]; then
    echo "FAIL: [$1] -> [$got], want [$2]" >&2
    exit 1
  fi
}
expect "crates/docmonk-code/src/lib.rs" "installer=false docker=false"
expect "README.md\ndocs/CI.md" "installer=false docker=false"
expect "" "installer=false docker=false"
expect "install.ps1" "installer=true docker=false"
expect "install.sh" "installer=true docker=false"
expect "crates/docmonk-update/src/lib.rs" "installer=true docker=false"
expect "xtask/src/main.rs" "installer=true docker=false"
expect "Dockerfile" "installer=false docker=true"
expect "scripts/fetch_models.sh" "installer=false docker=true"
expect "Cargo.lock" "installer=true docker=true"
expect "crates/docmonk-cli/Cargo.toml" "installer=true docker=true"
expect ".github/workflows/rust.yml" "installer=true docker=true"
expect "src/a.rs\nDockerfile\ninstall.sh" "installer=true docker=true"
echo "ci_changes self-test passed"
