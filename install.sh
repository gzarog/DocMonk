#!/bin/sh
# Installs the native DocMonk CLI: downloads the release archive
# for this platform, verifies it against the release's SHA256SUMS (and the
# minisign signature over it, when a key is configured below and
# `minisign` is installed), unpacks it under $DOCMONK_INSTALL_DIR/versions,
# and links `docmonk` onto a per-user bin directory.
#
#   DOCMONK_VERSION        version to install (default: the latest release)
#   DOCMONK_INSTALL_DIR    default: ~/.docmonk
#   DOCMONK_BIN_DIR        default: ~/.local/bin
#   DOCMONK_HOME           default: ~/.docmonk
#   DOCMONK_DOWNLOAD_BASE  a URL or local directory holding the release
#                          assets (testing and mirrors)
set -eu

REPO="gzarog/DocMonk"
# The release-signing public key (minisign). Empty: checksums only.
MINISIGN_PUBKEY=""
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

if [ -n "$MINISIGN_PUBKEY" ]; then
    if command -v minisign >/dev/null 2>&1; then
        fetch "$BASE/SHA256SUMS.minisig" "$TMP/SHA256SUMS.minisig"
        minisign -Vqm "$TMP/SHA256SUMS" -P "$MINISIGN_PUBKEY" \
            || fail "SHA256SUMS signature does not verify; refusing to install"
        echo "Signature verified."
    else
        echo "note: install minisign to also verify the release signature" >&2
    fi
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
