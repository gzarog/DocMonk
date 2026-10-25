#!/bin/sh
# Installs the native DocMonk CLI: downloads the release archive
# for this platform, verifies it against the release's SHA256SUMS and,
# when a release key is configured below, the minisign signature over it,
# unpacks it under $DOCMONK_INSTALL_DIR/versions, and links `docmonk` onto
# a per-user bin directory. With a key, verification fails closed: a
# missing `minisign` tool, a missing or invalid SHA256SUMS.minisig aborts.
#
#   DOCMONK_VERSION        version to install (default: the latest release)
#   DOCMONK_INSTALL_DIR    default: ~/.docmonk
#   DOCMONK_BIN_DIR        default: ~/.local/bin
#   DOCMONK_HOME           default: ~/.docmonk
#   DOCMONK_DOWNLOAD_BASE  a URL or local directory holding the release
#                          assets (testing and mirrors)
#   DOCMONK_MINISIGN_PUBKEY  a release key to verify with instead of the
#                          one below (testing and mirrors)
#   DOCMONK_INSECURE_SKIP_SIGNATURE=1  install without checking the
#                          signature (checksums only); prints a warning
set -eu

REPO="gzarog/DocMonk"
# The release-signing public key (minisign), committed so the README
# one-liner verifies signatures. Empty: checksums only, with a warning.
# rust-release.yml refuses to publish when it differs from
# vars.MINISIGN_PUBLIC_KEY.
MINISIGN_PUBKEY=""
VERIFY_KEY="${DOCMONK_MINISIGN_PUBKEY:-$MINISIGN_PUBKEY}"
INSTALL_DIR="${DOCMONK_INSTALL_DIR:-$HOME/.docmonk}"
BIN_DIR="${DOCMONK_BIN_DIR:-$HOME/.local/bin}"
DOCMONK_HOME="${DOCMONK_HOME:-$HOME/.docmonk}"

fail() {
    echo "error: $*" >&2
    exit 1
}

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin-arm64 | Darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    *) fail "no DocMonk release for $(uname -s) $(uname -m); build from source (see README.md)" ;;
esac

fetch() { # <url-or-path> <dest>
    case "$1" in
        http://* | https://*)
            attempt=1
            until curl -fsSL "$1" -o "$2"; do
                [ "$attempt" -ge 4 ] && fail "failed to download $1"
                sleep "$attempt"
                attempt=$((attempt + 1))
            done
            ;;
        *) cp "$1" "$2" || fail "missing $1" ;;
    esac
}

VERSION="${DOCMONK_VERSION:-}"
if [ -z "$VERSION" ]; then
    TAG="$(curl -fsSL -H "Accept: application/vnd.github+json" \
        "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -o '"tag_name" *: *"[^"]*"' | head -n1 | sed 's/.*"\([^"]*\)"$/\1/')" \
        || fail "could not determine the latest release"
    VERSION="${TAG#v}"
    case "$VERSION" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) fail "latest release tag '$TAG' is not a MAJOR.MINOR.PATCH version" ;;
    esac
fi
VERSION="${VERSION#v}"
case "$VERSION" in
    *[!0-9A-Za-z.+-]* | "" | .*) fail "invalid version '$VERSION'" ;;
esac
BASE="${DOCMONK_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download/v$VERSION}"
ASSET="docmonk-$VERSION-$TARGET.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading DocMonk $VERSION ($TARGET)..."
fetch "$BASE/$ASSET" "$TMP/$ASSET"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS"

if [ -z "$VERIFY_KEY" ]; then
    echo "warning: this installer carries no release key; verifying checksums only" >&2
elif [ "${DOCMONK_INSECURE_SKIP_SIGNATURE:-}" = 1 ]; then
    echo "warning: DOCMONK_INSECURE_SKIP_SIGNATURE=1: the release signature is NOT verified" >&2
else
    command -v minisign >/dev/null 2>&1 \
        || fail "minisign is required to verify the release signature; install it
  (https://jedisct1.github.io/minisign/, e.g. 'brew install minisign' or
  'apt install minisign') and run this again, or set
  DOCMONK_INSECURE_SKIP_SIGNATURE=1 to install with checksums only"
    # A release without SHA256SUMS.minisig fails here (fetch exits).
    fetch "$BASE/SHA256SUMS.minisig" "$TMP/SHA256SUMS.minisig"
    minisign -Vqm "$TMP/SHA256SUMS" -x "$TMP/SHA256SUMS.minisig" -P "$VERIFY_KEY" \
        || fail "SHA256SUMS signature does not verify; refusing to install"
    echo "Signature verified."
fi

EXPECTED="$(awk -v n="$ASSET" '{ f = $2; sub(/^\*/, "", f); if (f == n) print $1 }' "$TMP/SHA256SUMS")"
[ -n "$EXPECTED" ] || fail "$ASSET is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)"
else
    ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)"
fi
[ "$ACTUAL" = "$EXPECTED" ] \
    || fail "checksum mismatch for $ASSET: expected $EXPECTED, got $ACTUAL; refusing to install"
echo "Checksum verified."

VERSIONS="$INSTALL_DIR/versions"
PARTIAL="$VERSIONS/$VERSION.partial"
mkdir -p "$VERSIONS"
rm -rf "$PARTIAL"
mkdir -p "$PARTIAL"
tar -xzf "$TMP/$ASSET" -C "$PARTIAL" --strip-components=1
[ -x "$PARTIAL/docmonk" ] || fail "$ASSET does not contain the docmonk binary"
"$PARTIAL/docmonk" version >/dev/null || fail "the downloaded binary does not run on this machine"
rm -rf "${VERSIONS:?}/$VERSION"
mv "$PARTIAL" "$VERSIONS/$VERSION"

if [ -d "$VERSIONS/$VERSION/models" ]; then
    mkdir -p "$DOCMONK_HOME/models"
    cp -R "$VERSIONS/$VERSION/models/." "$DOCMONK_HOME/models/"
fi

STATE="$INSTALL_DIR/install_state.json"
OLD=""
[ -f "$STATE" ] && OLD="$(sed -n 's/.*"current" *: *"\([^"]*\)".*/\1/p' "$STATE" | head -n1)"
PREV="null"
if [ -n "$OLD" ] && [ "$OLD" != "$VERSION" ]; then
    PREV="\"$OLD\""
elif [ -f "$STATE" ]; then
    P="$(sed -n 's/.*"previous" *: *"\([^"]*\)".*/\1/p' "$STATE" | head -n1)"
    [ -n "$P" ] && PREV="\"$P\""
fi
ln -sfn "versions/$VERSION" "$INSTALL_DIR/current"
printf '{\n  "current": "%s",\n  "previous": %s\n}\n' "$VERSION" "$PREV" > "$STATE"

mkdir -p "$BIN_DIR"
ln -sf "$INSTALL_DIR/current/docmonk" "$BIN_DIR/docmonk"
echo "DocMonk $VERSION installed: $BIN_DIR/docmonk"

# `docmonk update` reads this to find the install it manages.
mkdir -p "$DOCMONK_HOME"
cat > "$DOCMONK_HOME/install_info.json" <<JSON
{
  "install_method": "native",
  "repository": "$REPO",
  "install_dir": "$INSTALL_DIR",
  "bin_dir": "$BIN_DIR"
}
JSON

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo ""
        echo "warning: $BIN_DIR is not on your PATH."
        echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, ~/.profile, ...):"
        echo "  export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac

echo ""
echo "Get started:"
echo "  docmonk init"
echo "  docmonk source add <path>"
echo "  docmonk index"
