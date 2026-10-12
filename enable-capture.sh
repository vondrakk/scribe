#!/usr/bin/env bash
# Switch on Scribe's terminal capture by loading its hook from ~/.bashrc.
# Scribe records nothing from a terminal until that hook is loaded.
#
# Usage:   ./enable-capture.sh            # read-only check: is capture on?
#          ./enable-capture.sh --enable   # add the hook line to ~/.bashrc
#          ./enable-capture.sh --disable  # remove it again
set -euo pipefail

HOME_DIR="${SCRIBE_HOME:-$HOME/.scribe}"
SNIPPET="$HOME_DIR/scribe.sh"
RC="$HOME/.bashrc"
MARK="# scribe capture"
LINE="[ -r \"$SNIPPET\" ] && source \"$SNIPPET\"  $MARK"

report() {
  if [ -r "$SNIPPET" ]; then
    echo "hook file : $SNIPPET"
  else
    echo "hook file : MISSING ($SNIPPET) — start Scribe once with ./run-local.sh --start"
  fi
  if grep -qF "$MARK" "$RC" 2>/dev/null; then
    echo "bashrc    : loads the hook (line $(grep -nF "$MARK" "$RC" | head -1 | cut -d: -f1) of $RC)"
  else
    echo "bashrc    : does NOT load the hook — capture is off"
  fi
  local ev="$HOME_DIR/events.jsonl"
  if [ -s "$ev" ]; then
    echo "captured  : $(wc -l < "$ev") event(s), last at $(date -r "$ev" '+%Y-%m-%d %H:%M')"
  else
    echo "captured  : nothing yet"
  fi
}

case "${1:-}" in
  "")
    report
    echo
    echo "Nothing was changed. Switch capture on with:  $0 --enable"
    ;;
  --enable)
    [ -r "$SNIPPET" ] || { echo "No hook file at $SNIPPET — start Scribe once first (./run-local.sh --start)." >&2; exit 1; }
    if grep -qF "$MARK" "$RC" 2>/dev/null; then
      echo "Already enabled; $RC not changed."
    else
      [ -f "$RC" ] && cp -p "$RC" "$RC.before-scribe"
      printf '\n%s\n' "$LINE" >> "$RC"
      echo "Added the hook line to $RC (previous version kept as $RC.before-scribe)."
    fi
    echo
    report
    echo
    echo "Capture starts in every terminal you open from now on."
    echo "Terminals that are already open keep running without it."
    ;;
  --disable)
    if grep -qF "$MARK" "$RC" 2>/dev/null; then
      cp -p "$RC" "$RC.before-scribe"
      grep -vF "$MARK" "$RC.before-scribe" > "$RC" || true
      echo "Removed the hook line from $RC (previous version kept as $RC.before-scribe)."
    else
      echo "Not enabled; $RC not changed."
    fi
    echo
    report
    ;;
  *)
    echo "Unknown option: $1" >&2
    sed -n '5,7p' "$0" >&2
    exit 2
    ;;
esac
