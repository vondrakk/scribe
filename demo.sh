#!/usr/bin/env bash
# Scribe one-shot demo: builds the binary, captures a scripted work session
# (real shell + git-hook capture) in an isolated sandbox, runs the pipeline, and
# launches the control panel. Nothing touches your real repos or shell config.
#
# Usage:   ./demo.sh            # run the demo, then open the panel
#          ./demo.sh --no-panel # run the demo, print a summary, don't serve
#
# Everything lives under $SCRIBE_HOME (a temp dir); remove it to clean up.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SCRIBE_HOME="${SCRIBE_HOME:-/tmp/scribe-demo}"
SANDBOX="${SANDBOX:-/tmp/scribe-demo-work}"
REPO="$SANDBOX/demo-service"
REPO2="$SANDBOX/infra-repo"
BIN="$HERE/target/release/scribe"

echo "==> Building scribe (release)"
( cd "$HERE" && cargo build --release >/dev/null )

echo "==> Fresh sandbox: $SCRIBE_HOME  (work repos in $SANDBOX)"
rm -rf "$SCRIBE_HOME" "$SANDBOX"
mkdir -p "$REPO" "$REPO2"

git -C "$REPO"  init -q
git -C "$REPO"  config user.email demo@example.com; git -C "$REPO"  config user.name demo
git -C "$REPO"  commit -qm "init" --allow-empty
git -C "$REPO2" init -q
git -C "$REPO2" config user.email demo@example.com; git -C "$REPO2" config user.name demo
git -C "$REPO2" commit -qm "init" --allow-empty

"$BIN" init >/dev/null
"$BIN" install-hooks "$REPO"  >/dev/null
"$BIN" install-hooks "$REPO2" >/dev/null

echo "==> Capturing a scripted work session (real interactive shell + git hooks)"
# PROMPT_COMMAND fires _scribe_log after each command — this is the real path.
# Commands are kept comment-free so the captured command text stays clean;
# see the narration in the echo lines above/below instead.
bash -i >/dev/null 2>&1 <<EOF
source "$SCRIBE_HOME/scribe.sh"
cd $REPO
git checkout -b feat/OPS-1234-login
echo "def login(): ..." > login.py
git add login.py
git -c commit.gpgsign=false commit -qm "OPS-1234 add login handler"
grep -n login login.py
pytest
cd $REPO2
git checkout -b hotfix/PLAT-77-restart
w !
kubectl get pods --context prod
echo "noting what I touched during the interrupt"
w OPS-1234
sleep 0.2
EOF

echo "==> Processing"
"$BIN" process
echo
echo "==> Episodes"
python3 - "$SCRIBE_HOME/episodes.json" <<'PY'
import json, sys
for e in json.load(open(sys.argv[1])):
    print(f"  [{e['kind']:<10}] {e['activity']:<11} {str(e['ticket'] or '-'):<9}")
    print(f"     {e['summary']}")
PY
echo
echo "==> Draft artifacts (all require approval in the panel)"
python3 - "$SCRIBE_HOME/drafts.json" <<'PY'
import json, sys
for d in json.load(open(sys.argv[1])):
    print(f"  {d['target']:<10} {d['kind']:<18} {d['status']:<8} {d['title']}")
PY

if [ "${1:-}" = "--no-panel" ]; then
  echo
  echo "Done. Review in the panel with:  SCRIBE_HOME=$SCRIBE_HOME $BIN panel"
  echo "Clean up with:                   rm -rf $SCRIBE_HOME $SANDBOX"
  exit 0
fi

echo
echo "==> Launching the GUI (opens your browser at http://127.0.0.1:8787)"
echo "    (Tip: real LLM summaries — Settings tab → backend=anthropic, set ANTHROPIC_API_KEY)"
exec "$BIN" up
