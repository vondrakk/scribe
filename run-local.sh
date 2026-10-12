#!/usr/bin/env bash
# Run the Scribe build from this checkout against your real data ($SCRIBE_HOME,
# default ~/.scribe).
#
# Usage:   ./run-local.sh          # read-only check: what is built, what is running
#          ./run-local.sh --start  # build, then launch the panel (opens the browser)
#          ./run-local.sh --demo   # same, but on scripted demo data in /tmp
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/scribe"
HOME_DIR="${SCRIBE_HOME:-$HOME/.scribe}"
PORT=8787
if [ -f "$HOME_DIR/config.json" ]; then
  PORT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("panel_port",8787))' "$HOME_DIR/config.json" 2>/dev/null || echo 8787)
fi

report() {
  echo "checkout : $HERE ($(git -C "$HERE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?'))"
  if [ -x "$BIN" ]; then
    echo "binary   : $BIN ($("$BIN" --version), built $(date -r "$BIN" '+%Y-%m-%d %H:%M'))"
    if [ -n "$(find "$HERE/src" "$HERE/Cargo.toml" -newer "$BIN" -print -quit)" ]; then
      echo "           sources are newer than the binary — --start will rebuild"
    fi
  else
    echo "binary   : not built yet"
  fi
  echo "data     : $HOME_DIR $([ -d "$HOME_DIR" ] && echo '(exists)' || echo '(will be created)')"
  if curl -fsS -m 2 "http://127.0.0.1:$PORT/api/state" >/dev/null 2>&1; then
    echo "panel    : running at http://127.0.0.1:$PORT"
  else
    echo "panel    : not running on port $PORT"
  fi
}

case "${1:-}" in
  "")
    report
    echo
    echo "Nothing was changed. Start it with:  $0 --start"
    ;;
  --start)
    echo "==> Building"
    ( cd "$HERE" && cargo build --release )
    report
    echo "==> Launching (Ctrl-C to stop)"
    exec "$BIN" up
    ;;
  --demo)
    exec "$HERE/demo.sh"
    ;;
  *)
    echo "unknown option: $1" >&2
    sed -n '5,7p' "$0" >&2
    exit 2
    ;;
esac
