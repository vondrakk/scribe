# Scribe — prototype (Rust)

Passive per-engineer work capture → draft Jira/Confluence artifacts, reviewed in
a local panel before anything leaves the machine. This is an **end-to-end thin
slice** of [`scribe-prd.md`](scribe-prd.md): every stage of the flow is
real and wired together, each stage deliberately shallow.

```
shell + git capture ─▶ append-only event log ─▶ sessionize ─▶ classify
   ─▶ summarize (rule-based) ─▶ draft artifacts ─▶ local panel (review/approve)
   ─▶ mock publish (as the engineer)
```

**Prototype choices:** real live capture · deterministic/offline summarizer (no
LLM credentials) · mock publishing (writes artifacts to `$SCRIBE_HOME/published/`
instead of calling Atlassian). One static ~4.4 MB binary, no runtime deps —
ideal for the "distribute a per-engineer agent" shape the PRD wants.

> Ported from an initial Python prototype at Viktor's request. The PRD suggested
> Python for Nanny-reuse; Rust was chosen for the single-binary distribution and
> a native shell-capture hot path (no interpreter spawn per prompt).

## Build

```bash
cargo build --release          # -> target/release/scribe
```

## One-shot demo

```bash
./demo.sh            # builds, captures a scripted session, runs the pipeline, opens the panel
./demo.sh --no-panel # same, but prints episodes/drafts and exits (no server)
```

Everything runs in an isolated sandbox (`/tmp/scribe-demo*`); your real repos
and shell config are never touched. Remove the sandbox dirs to clean up.

## Desktop app (Linux + macOS)

The same panel as a native app: its own window, a launcher/dock icon, a tray
(menu-bar) menu and start-at-login. It lives in `app/` (Tauri) and runs the
panel service in-process, so there is no browser tab and nothing to start by
hand. If `scribe up` is already running it joins that instead.

- **Tray menu** — what you're working on and how many updates wait for review,
  *I got interrupted*, *Add copied chat thread*, *Start at login*, *Quit*.
- **Closing the window** keeps Scribe running in the tray. On a desktop with no
  tray (stock GNOME without the AppIndicator extension) it minimizes instead.
- **Links** to Jira, GitLab and chat threads open in your browser.
- The `scribe` CLI is packaged beside the app (`/usr/bin/scribe` from the
  `.deb`, `Scribe.app/Contents/MacOS/scribe` on macOS) — the shell and git
  hooks call that, so capture works the same.

```bash
./run-app.sh            # check what is built / missing (changes nothing)
./run-app.sh --start    # build and open the app from this checkout
./run-app.sh --deps     # one-time: packaging prerequisites (sudo)
./run-app.sh --bundle   # build the .deb
./run-app.sh --install  # install it (sudo)
```

Releases attach `scribe-app-<version>-linux-amd64.deb` and
`scribe-app-<version>-macos-arm64.dmg`. The macOS build is not signed or
notarized, so the first launch needs right-click → Open.

## Quick start (GUI-first)

```bash
export SCRIBE_HOME="$PWD/.scribe"          # where all local data lives
BIN=target/release/scribe

# 1. launch everything: writes the capture snippet, processes, opens the browser,
#    and serves the panel (which auto-reprocesses as new activity is captured).
$BIN up                                          # http://127.0.0.1:8787

# 2. enable capture in the terminal you work in (one-time, prints in `up`):
source "$SCRIBE_HOME/scribe.sh"            # bash or zsh — the snippet self-detects
#   add the same line to ~/.bashrc (or ~/.zshrc on macOS) to make it permanent
$BIN install-hooks /path/to/repo                 # optional: git branch/commit events

# ...work normally: cd, git checkout feat/OPS-1234-x, commit, test...
# the panel's capture dot turns green and episodes/drafts appear live.
```

Everything after launch happens in the **panel**, on one screen: **Review**.
Scribe groups your captured work into one card per ticket per day (or per
project when no ticket was set) and writes the Jira update for it. You **Send**
it, **Edit** it first, or **Skip** it — nothing is posted until you press Send.
The top bar shows the active ticket and a live capture indicator, with **Change
ticket** and **I got interrupted** (which asks what was asked, and by whom).
**History** keeps what you sent or skipped; **Settings** connects Jira. The
manual builder, the workbench (Terminal / Ansible / Scratchpad) and the
pipeline views (Drafts, Episodes, Timeline, …) are under **More**. The
lower-level commands (`init`, `process`, `panel`, `w`) still exist for
scripting/headless use.

## Chat context (the "why") — no Slack app needed

The request and the decision usually live in chat. Scribe brings them into the
Jira update without any Slack API access:

- **Add a thread** — copy the messages in Slack, then run `scribe clip` (reads
  the clipboard; `scribe clip -` reads stdin; `--ticket OPS-1234` to say where
  it belongs) or use **＋ Add a chat thread** on Review. It attaches to the
  ticket it names, else to the work you were doing at the time, and goes into
  the update under *Context*. With an LLM backend the thread is distilled to
  problem / decision / outcome; with the stub backend the update carries the
  opening message, who took part, and the permalink if one was copied.
- **Notifications (opt-in, Linux)** — Settings → *Chat context* records the
  sender, channel and preview of chat notifications your desktop already
  shows (via the D-Bus session bus). They are only *offered* on the matching
  card; nothing is added to an update unless you add it.
- **Reply line** — after Send, Review shows a one-liner to paste back into the
  thread ("Written up in OPS-1234: …") so chat points at Jira.

All of it stays in `$SCRIBE_HOME/context.json`, expires with raw capture
(`sweep`), and leaves the machine only inside an update you send.

The generated `scribe.sh` and the git hooks embed the absolute path of the
binary that produced them, so `scribe` need not be on `PATH`.

## The `w` verb (binding)

The active Jira ticket is the anchor. A keyed branch (`feat/OPS-1234-…`) binds
silently via the git hook; otherwise resolve inline:

| command | effect |
|---|---|
| `w` | show the current binding |
| `w OPS-1250` | bind to a ticket |
| `w ?` | list candidate tickets (recent keys seen) |
| `w ! [reason]` | mark an interrupt — splits a distinct episode; the reason (what was asked, by whom) goes into its update |

The prompt-line segment (`scribe prompt`) prints `[OPS-1234]` when bound, or
`[⚠ unbound]` for unbound activity inside a repo.

## CLI

| command | what it does |
|---|---|
| `init` | write the shell snippet + print how to enable capture |
| `install-hooks <repo>` | copy git hooks into a repo's `.git/hooks` |
| `capture-shell <cmd> <exit> <cwd> [repo] [branch]` | record a shell command (the PROMPT_COMMAND hot path) |
| `event --type .. --action .. [--message ..] [--sha ..]` | record a git event (git hooks call this) |
| `w [TICKET\|?\|!]` | binding verbs |
| `clip [TEXT\|-] [--ticket KEY]` | add a chat thread (clipboard by default) as context for your work |
| `listen` | record chat notifications as local context (Linux; the panel does this itself when enabled) |
| `ai [TEXT] [--tool --model --session]` | record an AI prompt (TEXT or stdin) to the per-session prompt log |
| `prompt` | emit the prompt-line segment |
| `process` | sessionize → classify → summarize → emit drafts |
| `sweep` | apply retention (drop raw capture older than N days) |
| `up` | GUI-first launcher: snippet + process + open browser + serve (auto-reprocess) |
| `auth` | Atlassian OAuth 2.0 (3LO) sign-in for real publishing |
| `reset [--all]` | wipe captured + derived data for a clean slate |
| `panel` | launch the control panel (also auto-reprocesses on new activity) |
| `status` | print binding + counts |

## Capturing AI prompts (indexed by session)

Record the prompts you send to AI tools alongside your shell/git activity:

```bash
ai "how do I make this retry idempotent?"          # the `ai` shell helper
scribe ai "refactor to tokio::select" --tool claude --model claude-haiku-4-5
echo "$long_prompt" | scribe ai - --tool copilot    # or pipe via stdin
```

Each prompt is appended to `$SCRIBE_HOME/prompts/<session>.jsonl` — **one file
per session**, so prompts are indexed by the session they belong to. The session
id comes from `$SCRIBE_SESSION` (the shell snippet sets a stable one per
terminal). The full prompt text stays in that per-session file; a compact marker
also lands in the main event log, so the **Timeline** shows AI usage, episode
**summaries count AI prompts**, and the panel's **Prompts** tab lists them
grouped by session (tagged with tool/model and the active ticket).

Prompts also show on the **Review** screen: each card lists what was asked
while that work was happening and quotes the first few in the proposed update,
and a session that was only prompts (no terminal commands) gets a card of its
own. Sending or skipping the card clears them.

> Automate it for Claude Code: `./enable-capture.sh --claude` adds a
> `UserPromptSubmit` hook that calls `scribe ai -`, so capture is hands-free.

## LLM backend (episode summaries)

`pipeline::stub_summary` is always computed; the configured backend may replace
it. Set it in the panel's **Settings** tab or in `config.json`:

| `llm_backend` | behavior | credentials |
|---|---|---|
| `stub` (default) | deterministic, rule-based, offline | none |
| `anthropic` | Claude Messages API (raw HTTP; Rust has no official SDK) | `ANTHROPIC_API_KEY` env |
| `openai` | any OpenAI-compatible `/chat/completions` (incl. a local llama.cpp / Ollama / vLLM server) | optional `OPENAI_API_KEY`; set `llm_base_url` |

- `llm_model` — default `claude-haiku-4-5` (cheapest, ideal for short
  high-volume summaries — the PRD's cost-aware intent). Use `claude-sonnet-5-5`
  for more judgment, or `claude-opus-5-5` for the best. The request adapts to
  the model: `effort: low` is sent only on models that accept it (Opus / Sonnet
  5.x / Fable), omitted for Haiku so it isn't rejected.
- **Any** non-stub failure (missing key, network error, non-2xx, refusal, empty
  completion) logs one line to stderr and falls back to the deterministic stub —
  the pipeline never breaks, matching the PRD's offline-capable batched digest.

```bash
export ANTHROPIC_API_KEY=sk-ant-...
# Settings → backend=anthropic, or:
# edit $SCRIBE_HOME/config.json: "llm_backend": "anthropic"
scribe process     # episode summaries now come from Claude, stub on failure
```

## Connections (GitLab + Atlassian tokens)

**Settings → Connections** is where the accounts go:

- **GitLab** — one row per GitLab instance (name, address, personal access
  token with `read_api`). Add as many as you use; merge requests from all of
  them land in the inbox.
- **Atlassian** — site, email and an API token (id.atlassian.com → Security →
  API tokens). Used for Jira and Confluence, both reading and posting.
- **Post to the real Jira and Confluence** — off is practice mode (`mock`).

Tokens are stored in `$SCRIBE_HOME/secrets.json` (mode 0600), apart from
`config.json`, and the panel never sends them back to the browser. Changing an
address drops its saved token, so it must be typed again. **Test connections**
shows who each token signs in as. `GITLAB_TOKEN` + `gitlab_base_url`, the conf
file and OAuth below still work.

## Publishing (mock → real Atlassian Cloud)

`publish_backend = "mock"` (default) writes an audit record to `published/*.json`
offline. `publish_backend = "atlassian"` posts for real to Jira/Confluence **as
you**, via your own OAuth token (PRD §3.3/§7.4 — no admin token). Drafts map to:
worklog → a comment on the ticket; interrupt → a new Jira issue; handoff → a
Confluence page (bodies converted to ADF / storage format).

One-time setup:
1. In the Atlassian developer console, create an **OAuth 2.0 (3LO)** app with
   callback URL `http://127.0.0.1:8799/callback` and scopes
   `offline_access read:me read:jira-work write:jira-work read:confluence-content.all write:confluence-content`.
2. In the panel's **Settings → Publishing**: set backend to `atlassian`, paste
   the **client id**, and set the **Jira project key** and **Confluence space id**.
3. `export ATLASSIAN_CLIENT_SECRET=…` then run **`scribe auth`** — it opens the
   consent page, captures the redirect on loopback, and stores your tokens
   (+ cloudId) in `~/.scribe/atlassian.json` (auto-refreshed).

After that, **Publish** in the panel posts to real Jira/Confluence. Any failure
(not authenticated, API error) surfaces in the panel and writes nothing.

## Where things live (`$SCRIBE_HOME`, default `~/.scribe`)

- `events.jsonl` — the append-only raw event log. Shell + git hooks only ever
  *append* here. Source of truth; everything else is rebuildable from it.
- `episodes.json` / `drafts.json` — derived state, rebuilt by `process`.
- `state.json` — binding + id counters.
- `binding` — plain-text current ticket, read by the fast prompt segment.
- `published/` — mock-published artifacts (what *would* post to Jira/Confluence).
- `config.json` — tunables (episode gap, retention, key pattern, LLM backend).

## Source layout (`src/`)

| file | role |
|---|---|
| `lib.rs` | the core as a library, shared by the CLI and `app/` (desktop app) |
| `config.rs` | paths + config (`$SCRIBE_HOME`) |
| `model.rs` | data types + file-backed store (append-only JSONL + JSON state) |
| `capture.rs` | git detection, event recording, retention sweep |
| `binding.rs` | active-ticket model, branch-key inference, `w` verbs |
| `pipeline.rs` | sessionize → classify → `stub_summary` (deterministic baseline/fallback) |
| `llm.rs` | pluggable summary backend: stub / Anthropic / OpenAI-compatible, with fallback |
| `drafts.rs` | Jira worklog / draft ticket / Confluence handoff + status carryover |
| `publish.rs` | mock publish "as the engineer" (per-user OAuth, no admin token) |
| `context.rs` | chat context: pasted threads (`clip`) + the opt-in notification listener |
| `review.rs` | the Review screen: cards per ticket/day, proposed update, send / skip |
| `panel.rs` + `panel_index.html` | tiny_http server + the web UI (Review · History · Settings · More) |
| `main.rs` | clap CLI + shell/hook installers |

## How this maps to the PRD

| PRD | In this prototype |
|---|---|
| §4 Capture (terminal-first) | bash `PROMPT_COMMAND` **and** zsh `precmd` (macOS default) → `scribe capture-shell` + `post-checkout`/`commit-msg`/`post-commit` hooks → `events.jsonl`. SSH/kubectl/cloud targets parsed from command text; PagerDuty out. |
| §5 Binding model | branch-key inference (silent), `w` verb, prompt segment, candidate list |
| §5 Desktop notification | `notify.rs`: a `post-checkout` onto an **unkeyed** branch fires a local toast (Linux `notify-send`, macOS `osascript`) prompting `w <TICKET>`. Config `desktop_notifications` / env `SCRIBE_NO_NOTIFY` turn it off. |
| §6 Seam detection | repo/branch-key change, time gap, and explicit `w !` split episodes; unbound activity → Unaccounted |
| §8 Pipeline | `pipeline::process()`: sessionize → classify → summarize |
| §8 LLM strategy | `llm.rs` wires the pluggable backend: deterministic `stub`, real Anthropic Claude, or OpenAI-compatible (incl. local); non-stub falls back to stub on any error |
| §9 Outputs | Jira worklog, draft Jira ticket (interrupt/unplanned), Confluence handoff doc |
| §3.2 Control panel | `panel.rs` — Timeline / Episodes / Drafts / Unaccounted / Settings |
| §3.3 Publishing | `publish.rs` + `atlassian.rs`: real Jira/Confluence writes via per-user OAuth 2.0 3LO (local tokens, no admin token), or `mock` offline. `scribe auth` runs the consent flow. |
| §7 Trust guarantees | raw capture local-only + `sweep` retention; review-before-send enforced in panel; publish attributed to the engineer; Settings tab states the guarantees |

## Not in this slice (next steps)

- PagerDuty, calendar seams, and the aggregate team timeline (§3.3, §11 M6).
- Packaging (static musl build / container).
