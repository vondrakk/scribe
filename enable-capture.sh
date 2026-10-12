#!/usr/bin/env bash
# Switch on Scribe's capture.
#   Terminal: loads Scribe's hook from ~/.bashrc; nothing typed in a terminal is
#             recorded until then.
#   Prompts:  adds a UserPromptSubmit hook to ~/.claude/settings.json, so every
#             prompt you send in Claude Code is recorded (kept on this machine).
#
# Usage:   ./enable-capture.sh               # read-only check: what is on?
#          ./enable-capture.sh --enable      # terminal capture on
#          ./enable-capture.sh --disable     # terminal capture off
#          ./enable-capture.sh --claude      # Claude Code prompt capture on
#          ./enable-capture.sh --claude-off  # Claude Code prompt capture off
set -euo pipefail

HOME_DIR="${SCRIBE_HOME:-$HOME/.scribe}"
SNIPPET="$HOME_DIR/scribe.sh"
RC="$HOME/.bashrc"
MARK="# scribe capture"
CLAUDE_CFG="$HOME/.claude/settings.json"
BIN="$(sed -n 's/^_SCRIBE_BIN="\(.*\)"$/\1/p' "$SNIPPET" 2>/dev/null | head -1)"

# claude_hook status|on|off — edits only Scribe's own entry, leaves the rest.
claude_hook() {
  python3 - "$1" "$CLAUDE_CFG" "$BIN" <<'PY'
import json, os, shutil, sys
action, path, binary = sys.argv[1:4]
cfg = json.load(open(path)) if os.path.exists(path) else {}
groups = cfg.get("hooks", {}).get("UserPromptSubmit", [])
ours = lambda h: h.get("type") == "command" and h.get("command", "").rstrip().endswith("scribe\" ai -")
present = any(ours(h) for g in groups for h in g.get("hooks", []))
if action == "status":
    print("on" if present else "off")
    sys.exit(0)
if (action == "on") == present:
    print("unchanged")
    sys.exit(0)
if os.path.exists(path):
    shutil.copy2(path, path + ".before-scribe")
if action == "on":
    groups.append({"hooks": [{"type": "command", "command": '"%s" ai -' % binary, "timeout": 5}]})
else:
    for g in groups:
        g["hooks"] = [h for h in g.get("hooks", []) if not ours(h)]
    groups = [g for g in groups if g.get("hooks")]
hooks = cfg.setdefault("hooks", {})
if groups:
    hooks["UserPromptSubmit"] = groups
else:
    hooks.pop("UserPromptSubmit", None)
    if not hooks:
        cfg.pop("hooks")
os.makedirs(os.path.dirname(path), exist_ok=True)
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
os.replace(tmp, path)
print("changed")
PY
}

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
  if [ "$(claude_hook status)" = on ]; then
    echo "claude    : prompts are recorded (hook in $CLAUDE_CFG)"
  else
    echo "claude    : prompts are NOT recorded — no hook in $CLAUDE_CFG"
  fi
  local np=0
  [ -d "$HOME_DIR/prompts" ] && np=$(cat "$HOME_DIR/prompts"/*.jsonl 2>/dev/null | wc -l)
  echo "prompts   : $np recorded"
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
    echo "Nothing was changed. Switch on with:  $0 --enable   and   $0 --claude"
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
  --claude)
    [ -x "$BIN" ] || { echo "No Scribe binary found via $SNIPPET — start Scribe once first (./run-local.sh --start)." >&2; exit 1; }
    case "$(claude_hook on)" in
      changed) echo "Added the prompt hook to $CLAUDE_CFG (previous version kept as $CLAUDE_CFG.before-scribe)." ;;
      *)       echo "Already enabled; $CLAUDE_CFG not changed." ;;
    esac
    echo
    report
    echo
    echo "Prompts are recorded in Claude Code sessions started from now on."
    ;;
  --claude-off)
    case "$(claude_hook off)" in
      changed) echo "Removed the prompt hook from $CLAUDE_CFG (previous version kept as $CLAUDE_CFG.before-scribe)." ;;
      *)       echo "Not enabled; $CLAUDE_CFG not changed." ;;
    esac
    echo
    report
    ;;
  *)
    echo "Unknown option: $1" >&2
    sed -n '8,12p' "$0" >&2
    exit 2
    ;;
esac
