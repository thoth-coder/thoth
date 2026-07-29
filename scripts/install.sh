#!/bin/sh
# thoth installer for Linux and macOS.
# usage:  curl -fsSL https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.sh | sh
# env:    THOTH_VERSION=v0.1.0   install a specific version (default: latest)
#         THOTH_INSTALL_DIR=...  install location (default: ~/.local/bin)
set -eu

REPO="thoth-coder/thoth"
INSTALL_DIR="${THOTH_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${THOTH_VERSION:-latest}"

if [ "$VERSION" = "latest" ]; then
    TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
else
    TAG="$VERSION"
fi
if [ -z "${TAG:-}" ]; then
    echo "error: could not resolve the latest release tag" >&2
    exit 1
fi

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64)        TARGET="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
            *) echo "error: unsupported architecture: $ARCH" >&2; exit 1 ;;
        esac ;;
    Darwin)
        case "$ARCH" in
            x86_64) TARGET="x86_64-apple-darwin" ;;
            arm64)  TARGET="aarch64-apple-darwin" ;;
            *) echo "error: unsupported architecture: $ARCH" >&2; exit 1 ;;
        esac ;;
    *)
        echo "error: unsupported OS: $OS (on Windows use install.ps1)" >&2
        exit 1 ;;
esac

NAME="thoth-$TAG-$TARGET"
URL="https://github.com/$REPO/releases/download/$TAG/$NAME.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "downloading $URL"
curl -fsSL "$URL" -o "$TMP/$NAME.tar.gz"

# verify checksum when the tool and the file are available
if curl -fsSL "$URL.sha256" -o "$TMP/expected.sha256" 2>/dev/null; then
    EXPECTED=$(awk '{print $1}' "$TMP/expected.sha256")
    ACTUAL=$( (cd "$TMP" && (sha256sum "$NAME.tar.gz" 2>/dev/null || shasum -a 256 "$NAME.tar.gz")) | awk '{print $1}')
    if [ "$EXPECTED" != "$ACTUAL" ]; then
        echo "error: checksum mismatch, aborting" >&2
        exit 1
    fi
fi

tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/$NAME/thoth" "$INSTALL_DIR/thoth"
echo "installed thoth $TAG -> $INSTALL_DIR/thoth"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo ""
        echo "note: $INSTALL_DIR is not in your PATH. add this to your shell profile:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
