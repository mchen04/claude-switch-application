#!/usr/bin/env sh
# cs installer — downloads the latest macOS release and installs to ~/.local/bin.
# Usage: curl -fsSL https://raw.githubusercontent.com/mchen04/claude-switch/main/install.sh | sh

set -eu

REPO="mchen04/claude-switch"
BIN_DIR="${CS_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
if [ "$OS" != "Darwin" ]; then
    echo "cs is currently macOS-only (detected: $OS)" >&2
    exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
    arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
    x86_64)        TARGET="x86_64-apple-darwin" ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

for cmd in curl tar; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "missing required tool: $cmd" >&2
        exit 1
    fi
done

API_URL="https://api.github.com/repos/$REPO/releases/latest"
TAG="$(curl -fsSL "$API_URL" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
if [ -z "$TAG" ]; then
    echo "could not resolve latest release tag from $API_URL" >&2
    exit 1
fi

ASSET="cs-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading $ASSET ($TAG)..."
curl -fsSL "$URL" -o "$TMP/$ASSET"

# Optional checksum verification (skip silently if shasum missing).
if command -v shasum >/dev/null 2>&1; then
    if curl -fsSL "$URL.sha256" -o "$TMP/$ASSET.sha256" 2>/dev/null; then
        ( cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null )
    fi
fi

tar -xzf "$TMP/$ASSET" -C "$TMP"

mkdir -p "$BIN_DIR"
mv "$TMP/cs" "$BIN_DIR/cs"
chmod +x "$BIN_DIR/cs"

# Strip the macOS Gatekeeper quarantine flag so the unsigned binary runs.
xattr -d com.apple.quarantine "$BIN_DIR/cs" 2>/dev/null || true

echo "installed cs to $BIN_DIR/cs"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on your PATH — add it to your shell rc" ;;
esac

echo
echo "next: run \`cs setup\` to install the shell wrapper."
