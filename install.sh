#!/usr/bin/env sh
# Scribe installer — copies the self-contained `scribe` binary onto your PATH.
#
# Usage:
#   ./install.sh                     # install to ~/.local/bin (or /usr/local/bin if root)
#   PREFIX=/usr/local ./install.sh   # install to <PREFIX>/bin
#   ./install.sh --uninstall         # remove an installed scribe
set -eu

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BIN_SRC="$SELF_DIR/scribe"

# --- resolve install dir -----------------------------------------------------
if [ -n "${PREFIX:-}" ]; then
    BIN_DIR="$PREFIX/bin"
elif [ "$(id -u)" = "0" ]; then
    BIN_DIR="/usr/local/bin"
else
    BIN_DIR="$HOME/.local/bin"
fi
DEST="$BIN_DIR/scribe"

# --- uninstall ---------------------------------------------------------------
if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    if [ -e "$DEST" ]; then
        rm -f "$DEST"
        echo "Removed $DEST"
    else
        echo "Nothing to remove at $DEST"
    fi
    exit 0
fi

# --- install -----------------------------------------------------------------
if [ ! -f "$BIN_SRC" ]; then
    echo "error: binary not found next to this script ($BIN_SRC)" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
install -m 0755 "$BIN_SRC" "$DEST" 2>/dev/null || { cp "$BIN_SRC" "$DEST" && chmod 0755 "$DEST"; }
echo "Installed scribe -> $DEST"

# --- PATH hint ---------------------------------------------------------------
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo
        echo "NOTE: $BIN_DIR is not on your PATH. Add it, e.g.:"
        echo "      echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc && . ~/.bashrc"
        ;;
esac

# --- next steps --------------------------------------------------------------
cat <<'EOF'

Done. Verify:
    scribe --help

To start capturing (per the README):
    export SCRIBE_HOME="$PWD/.scribe"
    scribe up            # writes the capture snippet, processes, serves the panel
    source "$SCRIBE_HOME/scribe.sh"   # enable capture in your working shell (bash or zsh)
EOF
