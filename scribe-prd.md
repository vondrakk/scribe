# Scribe — Product Requirements Document

> **Name:** Scribe (formerly working title "Ridealong")
> **Audience:** Claude Code (implementation) + maintainers
> **Status:** Draft v1
> **Owner:** Viktor

---

## 1. Summary

Scribe is a per-engineer tool that **passively captures an engineer's work and turns it into structured artifacts** — a work timeline, draft Jira tickets, Jira ticket updates, async handoff documents, and action items for teammates.

The organizing principle is that **scheduled sprint work (from Jira) is the frame**, and the engineer's only active job is to confirm *context* when it changes — never to log. Capture is passive and local. Interrupts to planned work are detected automatically and attributed to their own work episode.

This is **not** a surveillance or productivity-scoring tool. That is enforced by architecture, not policy (see §7).

### What it is like
A sibling to the existing "Nanny" supervised on-call tool: same deployment shape (Python, Podman, pluggable LLM backend, multi-tab local web control panel). Where Nanny *acts*, Scribe *observes and documents*.

---

## 2. Goals / Non-Goals

### Goals
- Capture terminal + git + related CLI activity with near-zero engineer effort.
- Bind captured work to the correct Jira sprint ticket automatically where possible.
- Detect interruptions to planned work and capture them as distinct episodes.
- Produce human-approved draft artifacts: Jira tickets, Jira updates/worklogs, Confluence handoffs, teammate action items.
- Make async handoff across timezones (Manila / Cebu / Palawan / Berlin / US) effortless.
- Surface **aggregate, team-level** interrupt-vs-planned-work load for retros.

### Non-Goals (v1)
- **No Slack integration.** All interaction is local (terminal + local notifications + local panel).
- **No real-time streaming** of artifacts — batched end-of-session digest.
- **No per-engineer activity feed or velocity/productivity scoring.** Structurally excluded.
- **No desktop screen capture / OCR.** Terminal-first only.
- Incident/postmortem flows are secondary (nice-to-have, not v1 focus).

---

## 3. Architecture

Three components:

### 3.1 Local agent (per engineer)
Runs on the engineer's machine. Owns: capture, sessionization, classification, summarization, draft generation, and all LLM calls. All raw data stays here.

### 3.2 Local control panel (multi-tab web UI, Nanny pattern)
Tabs: **Timeline**, **Episodes**, **Drafts** (review/approve/edit/discard), **Unaccounted** (end-of-session triage), **Settings**. This is where artifacts are approved before anything leaves the machine.

### 3.3 Shared publishing service (thin)
The *only* networked component. Responsibilities, deliberately narrow:
- Broker **per-user OAuth tokens** for Atlassian (Jira + Confluence).
- Publish **approved** artifacts, attributed to the engineer, using their token.
- Host the **aggregate** team timeline (no per-engineer drilldown).

It never receives raw capture. It only ever handles approved, published artifacts.

---

## 4. Capture (terminal-first)

| Source | Mechanism | Captured |
|---|---|---|
| Shell | wrapper / `tmux pipe-pane` / `script` | command, output, exit code, cwd, timestamp |
| Git | `post-checkout`, `commit-msg`, `post-commit` hooks | branch switches, commit keys, diffs |
| Remote/infra | parse invocations | SSH / kubectl / cloud-CLI targets (host, cluster, account) |
| PagerDuty | API/webhook (read) | incident trigger/ack/resolve events (seam signal) |

All events land in a single **append-only local event log**.

---

## 5. Context binding model

**The active Jira ticket is the anchor.** Everything binds to it.

### Binding signals (in priority order)
1. **Branch key** — `feat/OPS-1234-…` binds all activity in that worktree to `OPS-1234`. Primary signal; makes binding near-automatic.
2. In-progress + assigned Jira sprint issue for the engineer.
3. Keys found in commit / PR text.

### Engineer surface (minimal — two of three are rare)
- **Confirm active ticket** — once per ticket, usually auto-inferred.
- **Answer a seam prompt** — one action, only when a context switch is detected.
- **Clear the Unaccounted bucket** — end of session.

### Interaction mechanisms (Slack-free)
- **Git hooks (main path):** `post-checkout` binds silently on a keyed branch; prompts (listing in-progress sprint issues) on a new/unkeyed branch. `commit-msg` confirms the key.
- **Prompt-line segment** (`PROMPT_COMMAND` / zsh `precmd`): shows the bound ticket; shows a warning glyph on unbound activity. Resolve inline with a short verb:
  - `w OPS-1250` — bind to ticket
  - `w ?` — list candidate tickets
  - `w !` — mark interrupt
- **Local desktop notification** (`notify-send` / libnotify) with action buttons for hard seams the engineer would miss while heads-down.

---

## 6. Seam (interrupt) detection

An interrupt = the active ticket changing. Detect the seam; ask at most one question.

**Signals:** branch/repo/cwd change to an unrelated key · new SSH/kubectl/cloud target · PagerDuty trigger · calendar event start · idle-gap-then-different-context.

**On a hard seam:** desktop notification — *"Still on OPS-1234, or something else?"* → `[Same]` · `[Interrupt → pick/new]` · `[Not work]`. One action.

An interrupt spins up its own episode, and a draft unplanned-work ticket if it looks real. Anything that won't bind confidently drops into **Unaccounted** for batch triage at end of session — never nagged in real time.

---

## 7. Trust guarantees (hard requirements)

These are non-negotiable and must be structurally enforced:

1. **Raw capture is local-only** and **auto-expires** (configurable, default 7–30 days). Only approved, published artifacts persist or leave the machine.
2. **No manager-facing per-engineer activity feed.** The only shared surface is normal Jira/Confluence that everyone already sees.
3. **Review-before-send, always.** Nothing is published without explicit engineer approval in the local panel.
4. **Per-user OAuth, not a god-mode admin token.** Artifacts post *as the engineer*, from their own authorized token. The publishing service brokers per-user tokens; it holds no org-wide Atlassian admin credential.
5. **Aggregate-only metrics.** Interrupt-vs-planned load is reported at sprint/team level. Per-engineer velocity/scoring must be structurally impossible to derive from the shared service.

---

## 8. Processing pipeline

1. **Ingest** → append-only local event log.
2. **Sessionize** → heuristic pre-segmentation (time gaps, cwd/repo/branch changes) refined by LLM into coherent *work episodes*.
3. **Classify + enrich** → activity type (develop / debug / deploy / review / investigate) + entities (service, host, ticket key, incident).
4. **Summarize** → LLM-generated human-readable episode narrative.
5. **Emit** → map episodes to draft artifacts (§9).

### LLM strategy (cost-aware)
- **Local / cheap model:** sessionization + classification.
- **Cloud API (Anthropic):** final episode summaries only.
- **Batched end-of-session digest**, not streaming.
- **Pluggable LLM backend** (reuse Nanny: Anthropic API or any OpenAI-compatible local server).

---

## 9. Outputs (all drafts require approval)

- **Timeline / work log** — passive, always on, local.
- **Draft new Jira tickets** — for detected unplanned/interrupt work.
- **Jira worklog / comment updates** — when a key (`OPS-1234`) is detected in branch, commit, or PR. Maps cleanly to Jira worklogs if time tracking is in use.
- **Async handoff doc → Confluence** — overnight / end-of-session: what was touched, current state, open items. *This is the flagship win for the distributed team.*
- **Action items for others** — emitted as **Jira** (assigned subtask or @mention in a comment), attributed to the engineer, human-approved. (Slack being out of scope, Jira is the delivery channel.)
- **(Secondary) Incident postmortem → Confluence**, assembled from PagerDuty timeline + Jira + episode narrative.

---

## 10. Tech stack

- **Language:** Python (consistent with Nanny, ~baseline for reuse).
- **Packaging/deploy:** Podman bundle per engineer; thin shared service separately deployed.
- **Control panel:** multi-tab web UI (Nanny pattern).
- **LLM:** pluggable backend (Anthropic / OpenAI-compatible local).
- **Integrations:** Atlassian OAuth (Jira + Confluence, one grant), PagerDuty read-only key (shared service).
- **Optional:** Prometheus/Alertmanager (reuse Nanny stack) for agent health only — not engineer metrics.

---

## 11. Milestones

| Milestone | Deliverable | Acceptance criteria |
|---|---|---|
| **M0 — Capture** | Shell + git-hook capture → append-only local event log | Commands, output, exit codes, cwd, timestamps, and branch switches land in the log; survives restart; auto-expiry works |
| **M1 — Binding** | Active-ticket model + branch-key inference + `w` CLI + prompt-line segment | Keyed branch binds silently; unkeyed branch prompts with sprint issues; prompt line shows bound ticket + unbound-activity glyph |
| **M2 — Pipeline** | Sessionization + classification + summarization | Raw log segments into episodes; each episode has type, entities, and a readable summary; LLM split (local vs cloud) in place |
| **M3 — Drafts + panel** | Draft generation + local review/approve/edit/discard UI + Unaccounted bucket | Engineer can review every draft and the exact captured context before anything is published; nothing publishes without approval |
| **M4 — Publishing** | Shared service + per-user Atlassian OAuth + Jira write | Approved tickets/updates post to Jira **as the engineer**; no admin token used |
| **M5 — Handoff + seams** | Confluence handoff doc + seam detection + desktop notifications | EOD handoff doc generated and publishable; hard seams fire a notification; interrupts captured as distinct episodes |
| **M6 — Aggregate metrics** | Team-level interrupt-vs-planned reporting | Sprint/team aggregate only; per-engineer drilldown structurally absent |

---

## 12. Open questions (confirm before/early in build)

1. **Jira flavor:** Cloud vs Data Center/Server? (Determines OAuth flow and API.)
2. **Atlassian time tracking:** in use? (Determines whether episodes emit as worklogs.)
3. **Agent target OS:** Linux-only, or Mac in the mix? (Affects shell hooks + `notify-send` equivalent.)
4. **Calendar source** for seam detection (Google / Exchange / none in v1?).
5. **Retention window** default — 7, 14, or 30 days?
6. **Branch-naming adoption:** can PH + Ops standardize on `key-in-branch`? (A prompting git hook can enforce; binding quality depends heavily on this.)
7. **Shared service hosting** — where does the thin publishing service run (cloud account, on-prem)?
