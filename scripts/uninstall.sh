#!/bin/sh
# thoth uninstaller for Linux and macOS.
# usage:  curl -fsSL https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/uninstall.sh | sh
# purge:  ... | sh -s -- --purge   (also removes config and project memory dirs)
set -eu

INSTALL_DIR="${THOTH_INSTALL_DIR:-$HOME/.local/bin}"
PURGE=0
for arg in "$@"; do
    [ "$arg" = "--purge" ] && PURGE=1
done

if [ -f "$INSTALL_DIR/thoth" ]; then
    rm -f "$INSTALL_DIR/thoth"
    echo "removed $INSTALL_DIR/thoth"
else
    echo "thoth binary not found in $INSTALL_DIR (nothing to remove)"
fi

if [ "$PURGE" = "1" ]; then
    rm -rf "$HOME/.thoth"
    rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/thoth"
    echo "removed ~/.thoth and the thoth config directory"
else
    echo "kept config and state:"
    echo "  ~/.thoth                (editor state, session recaps)"
    echo "  ~/.config/thoth         (config.toml)"
    echo "run with --purge to remove them too"
fi
