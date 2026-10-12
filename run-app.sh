#!/usr/bin/env bash
# Build and run the Scribe desktop app (native window + tray menu) from this
# checkout, or package it as an installable .deb.
#
# Usage:   ./run-app.sh            # read-only check: what is built, what is missing
#          ./run-app.sh --start    # build the CLI and the app, then open the app
#          ./run-app.sh --stop     # quit the running app
#          ./run-app.sh --deps     # install the packaging prerequisites (asks for sudo)
#          ./run-app.sh --bundle   # build Scribe_<version>_amd64.deb
#          ./run-app.sh --install  # install that .deb system-wide (asks for sudo)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI="$HERE/target/release/scribe"
APP="$HERE/target/release/scribe-app"
DEB_DIR="$HERE/target/release/bundle/deb"
HOME_DIR="${SCRIBE_HOME:-$HOME/.scribe}"
PORT=8787
if [ -f "$HOME_DIR/config.json" ]; then
  PORT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("panel_port",8787))' "$HOME_DIR/config.json" 2>/dev/null || echo 8787)
fi

latest_deb() { ls -t "$DEB_DIR"/*.deb 2>/dev/null | head -1; }
has_pc() { pkg-config --exists "$1" 2>/dev/null; }

has_tray() {
  dbus-send --session --dest=org.freedesktop.DBus --print-reply /org/freedesktop/DBus \
    org.freedesktop.DBus.NameHasOwner string:org.kde.StatusNotifierWatcher 2>/dev/null | grep -q 'boolean true'
}

report() {
  echo "checkout  : $HERE ($(git -C "$HERE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?'))"
  for b in "$CLI" "$APP"; do
    if [ -x "$b" ]; then
      echo "built     : $(basename "$b") ($(date -r "$b" '+%Y-%m-%d %H:%M'))"
    else
      echo "built     : $(basename "$b") — not yet"
    fi
  done
  if pgrep -x scribe-app >/dev/null; then
    echo "app       : running"
  else
    echo "app       : not running"
  fi
  if curl -fsS -m 2 "http://127.0.0.1:$PORT/api/state" >/dev/null 2>&1; then
    echo "service   : answering on port $PORT (the app joins it rather than starting its own)"
  else
    echo "service   : not running on port $PORT (the app starts it)"
  fi
  if has_tray; then
    echo "tray      : available — closing the window keeps Scribe in the tray"
  else
    echo "tray      : NOT available on this desktop — closing the window minimizes it instead"
    echo "            (GNOME needs the 'Ubuntu AppIndicators' extension switched on)"
  fi
  if has_pc webkit2gtk-4.1; then
    echo "build deps: webkit2gtk-4.1 found"
  else
    echo "build deps: webkit2gtk-4.1 MISSING — run $0 --deps"
  fi
  if cargo tauri --version >/dev/null 2>&1 && has_pc ayatana-appindicator3-0.1; then
    echo "packaging : ready"
  else
    echo "packaging : prerequisites missing — run $0 --deps"
  fi
  local deb; deb="$(latest_deb)"
  if [ -n "$deb" ]; then
    echo "package   : $deb ($(date -r "$deb" '+%Y-%m-%d %H:%M'))"
  else
    echo "package   : none built yet"
  fi
  if dpkg -s scribe >/dev/null 2>&1; then
    echo "installed : scribe $(dpkg -s scribe | sed -n 's/^Version: //p') (system-wide)"
  else
    echo "installed : no"
  fi
}

build() {
  ( cd "$HERE" && cargo build --release && cargo build --release -p scribe-app )
}

case "${1:-}" in
  "")
    report
    echo
    echo "Nothing was changed. Open the app with:  $0 --start"
    ;;
  --start)
    build
    if pgrep -x scribe-app >/dev/null; then
      echo "Scribe is already running; bringing its window forward."
    fi
    nohup "$APP" >/dev/null 2>&1 &
    sleep 3
    echo
    report
    ;;
  --stop)
    if pkill -x scribe-app; then echo "Stopped the app."; else echo "The app was not running."; fi
    sleep 1
    echo
    report
    ;;
  --deps)
    sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
    cargo tauri --version >/dev/null 2>&1 || cargo install tauri-cli --version "^2" --locked
    echo
    report
    ;;
  --bundle)
    cargo tauri --version >/dev/null 2>&1 && has_pc ayatana-appindicator3-0.1 || {
      echo "Packaging prerequisites are missing. Run:  $0 --deps" >&2; exit 1; }
    ( cd "$HERE" && cargo build --release )
    # The CLI ships inside the package, next to the app.
    cp "$CLI" "$HERE/app/bin/scribe-$(rustc -vV | sed -n 's/^host: //p')"
    ( cd "$HERE/app" && cargo tauri build --bundles deb --config tauri.bundle.conf.json )
    echo
    report
    ;;
  --install)
    deb="$(latest_deb)"
    [ -n "$deb" ] || { echo "No package built yet. Run:  $0 --bundle" >&2; exit 1; }
    sudo apt-get install -y "$deb"
    echo
    report
    echo
    echo "Scribe is now in your applications menu. The CLI is at /usr/bin/scribe."
    ;;
  *)
    echo "Unknown option: $1" >&2
    sed -n '5,10p' "$0" >&2
    exit 2
    ;;
esac
