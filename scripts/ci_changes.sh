#!/usr/bin/env bash
# Classifies a PR's changed paths (one per line on stdin) for the
# conditional CI jobs in .github/workflows/rust.yml. Prints
# `installer=true|false`, `docker=true|false` and
# `cross_platform=true|false` (GITHUB_OUTPUT format).
# Patterns are deliberately broad: shared build inputs select all three.
# Self-test: scripts/ci_changes.sh --self-test
set -euo pipefail

# Sources with Windows/Unix-specific code (`cfg(windows)`, `cfg(unix)`,
# `target_os`, ...). The self-test fails when a tracked Rust file with such
# code is missing here, so the list cannot silently go stale.
platform_source() {
  case "$1" in
    crates/docmonk-ai/src/runtime.rs | crates/docmonk-cli/src/update_cmd.rs \
      | crates/docmonk-cli/tests/status.rs | crates/docmonk-cli/tests/ui.rs \
      | crates/docmonk-cli/tests/update.rs | crates/docmonk-core/src/paths.rs \
      | crates/docmonk-core/src/security.rs | crates/docmonk-indexing/src/ignore.rs \
      | crates/docmonk-indexing/src/lock.rs | crates/docmonk-indexing/src/scan.rs \
      | crates/docmonk-indexing/tests/scenarios.rs | crates/docmonk-service/src/daemon.rs \
      | crates/docmonk-service/src/sources.rs | crates/docmonk-ui/src/lib.rs)
      return 0 ;;
  esac
  return 1
}

classify() {
  local installer=false docker=false cross_platform=false path
  while IFS= read -r path || [ -n "$path" ]; do
    [ -z "$path" ] && continue
    case "$path" in
      Cargo.toml | Cargo.lock | rust-toolchain.toml | crates/*/Cargo.toml | xtask/Cargo.toml \
        | .github/workflows/* | .github/actions/* | scripts/ci_changes.sh)
        installer=true docker=true cross_platform=true ;;
    esac
    case "$path" in
      install.sh | install.ps1 | crates/docmonk-update/* | xtask/*)
        installer=true cross_platform=true ;;
    esac
    case "$path" in
      Dockerfile | .dockerignore | docker/* | scripts/fetch_models.sh) docker=true ;;
    esac
    if platform_source "$path"; then
      cross_platform=true
    fi
  done
  echo "installer=$installer"
  echo "docker=$docker"
  echo "cross_platform=$cross_platform"
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
expect "crates/docmonk-code/src/lib.rs" "installer=false docker=false cross_platform=false"
expect "README.md\ndocs/CI.md" "installer=false docker=false cross_platform=false"
expect "" "installer=false docker=false cross_platform=false"
expect "install.ps1" "installer=true docker=false cross_platform=true"
expect "install.sh" "installer=true docker=false cross_platform=true"
expect "crates/docmonk-update/src/lib.rs" "installer=true docker=false cross_platform=true"
expect "xtask/src/main.rs" "installer=true docker=false cross_platform=true"
expect "Dockerfile" "installer=false docker=true cross_platform=false"
expect "scripts/fetch_models.sh" "installer=false docker=true cross_platform=false"
expect "Cargo.lock" "installer=true docker=true cross_platform=true"
expect "crates/docmonk-cli/Cargo.toml" "installer=true docker=true cross_platform=true"
expect ".github/workflows/rust.yml" "installer=true docker=true cross_platform=true"
expect "crates/docmonk-core/src/paths.rs" "installer=false docker=false cross_platform=true"
expect "crates/docmonk-service/src/daemon.rs" "installer=false docker=false cross_platform=true"
expect "src/a.rs\nDockerfile\ninstall.sh" "installer=true docker=true cross_platform=true"

# Every tracked Rust file with platform-conditional code selects
# cross_platform (crate manifests are already shared build inputs).
cd "$(dirname "$0")/.."
missing=0
while IFS= read -r path; do
  if ! platform_source "$path" && [ "$(printf '%s\n' "$path" | classify | tail -n 1)" != cross_platform=true ]; then
    echo "FAIL: $path has platform-specific code; add it to platform_source" >&2
    missing=1
  fi
done < <(git ls-files 'crates/*.rs' 'xtask/*.rs' \
  | xargs grep -lE 'cfg!?\((all\(|any\(|not\()*(windows|unix|target_os|target_family)' || true)
[ "$missing" = 0 ] || exit 1
echo "ci_changes self-test passed"
