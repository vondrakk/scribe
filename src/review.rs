//! The review screen: Scribe proposes, the engineer decides.
//!
//! Unhandled work is grouped into one card per ticket per day (or per project
//! for work with no ticket), joined with the conversation context that explains
//! it (src/context.rs) and the AI prompts sent while doing it (src/prompts.rs),
//! and given a ready-to-post Jira update. The engineer
//! sends it, edits it, or skips it — nothing else is asked of them. Nothing
//! here publishes without that explicit send (§7.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context::{self, ContextItem};
use crate::model::{self, now_ts, Draft, Episode};
use crate::prompts::{self, Prompt};
use crate::logbook::LogEntry;
use crate::{config, logbook, publish, stream};

/// Prompts are stamped a moment apart from their timeline marker.
const PROMPT_SLACK: f64 = 2.0;
/// How many prompts the proposed update quotes before summarising the rest.
const PROMPTS_QUOTED: usize = 6;

/// Under "auto", a pause this long across a change of day ends an event.
const AUTO_BREAK_SECS: f64 = 6.0 * 3600.0;
/// A notification this long before work starts may be what triggered it.
const LEAD_SECS: f64 = 600.0;

#[derive(Serialize, Default, Clone)]
pub struct CtxView {
    pub id: i64,
    pub source: String,
    pub who: String,
    pub channel: String,
    pub ts: f64,
    pub text: String,
    pub link: String,
    /// The line this item contributes to the proposed update.
    pub line: String,
    /// Clips are in the proposal by default; notifications are only offered.
    pub included: bool,
}

#[derive(Serialize, Default, Clone)]
pub struct PromptView {
    pub ts: f64,
    pub tool: String,
    pub text: String,
}

#[derive(Serialize, Default)]
pub struct Card {
    pub id: String,
    pub kind: String, // work | interrupt | untracked | thread
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub title: String,
    pub place: String,
    /// The History entry this continues, when the event was already sent:
    /// sending posts only the addition and adds it to that entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follows: Option<i64>,
    pub follows_ts: f64,
    /// The full working directory, for "ignore this path".
    pub path: String,
    pub start_ts: f64,
    pub end_ts: f64,
    pub secs: f64,
    /// The newest episode is still within the session gap — work may continue.
    pub active: bool,
    pub episode_ids: Vec<i64>,
    pub commits: Vec<String>,
    pub commands: Vec<String>,
    pub note: String,
    /// What was asked of AI tools during this work, oldest first.
    pub prompts: Vec<PromptView>,
    pub context: Vec<CtxView>,
    /// The proposed Jira update, in the wiki-ish markup publish understands.
    pub body: String,
}

fn local_day(ts: f64) -> i64 {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| chrono::Datelike::num_days_from_ce(&d.with_timezone(&chrono::Local).date_naive()) as i64)
        .unwrap_or(0)
}

fn pretty_home(p: &str) -> String {
    if let Ok(h) = std::env::var("HOME") {
        if let Some(rest) = p.strip_prefix(&h) {
            return format!("~{rest}");
        }
    }
    // Outside home, the last two components say enough.
    let parts: Vec<&str> = p.split('/').filter(|x| !x.is_empty()).collect();
    if parts.len() > 3 {
        return format!("…/{}", parts[parts.len() - 2..].join("/"));
    }
    p.to_string()
}

/// One line of plain text: whitespace collapsed, markup characters dropped
/// (`*` toggles bold in the Jira conversion), capped.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ").replace('*', "");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

fn ctx_line(it: &ContextItem) -> String {
    let chan = if it.channel.is_empty() { String::new() } else { format!(" in {}", it.channel) };
    let link = if it.link.is_empty() { String::new() } else { format!(" (thread: {})", it.link) };
    if !it.summary.is_empty() {
        let who = if it.who.is_empty() { "Chat".to_string() } else { it.who.clone() };
        return format!("* {who}{chan}: {}{link}", one_line(&it.summary, 400));
    }
    // No model to summarise with: give the opening message (the ask) and who
    // else took part, rather than the whole exchange.
    if it.source == "notification" {
        // Stored as "<summary>\n<body>"; the body is the message preview.
        let body = it.text.split_once('\n').map(|(_, b)| b).unwrap_or(&it.text);
        let said = body.strip_prefix(&format!("{}: ", it.who)).unwrap_or(body);
        let who = if it.who.is_empty() { "Chat".to_string() } else { it.who.clone() };
        return format!("* {who}{chan}: {}", one_line(said, 280));
    }
    let msgs = context::clip_messages(&it.text);
    match msgs.first() {
        Some((asker, ask)) => {
            let ask = if it.link.is_empty() { ask.clone() } else { ask.replace(&it.link, "") };
            let mut others: Vec<&str> = Vec::new();
            for (n, _) in &msgs {
                if n != asker && !others.contains(&n.as_str()) {
                    others.push(n);
                }
            }
            let also = if others.is_empty() { String::new() } else { format!(" Discussed with {}.", others.join(", ")) };
            format!("* Raised by {asker}{chan}: {}{also}{link}", one_line(&ask, 240))
        }
        None => {
            let who = if it.who.is_empty() { "Chat".to_string() } else { it.who.clone() };
            format!("* {who}{chan}: {}{link}", one_line(&it.text, 280))
        }
    }
}

pub(crate) fn ctx_view(it: &ContextItem) -> CtxView {
    CtxView {
        id: it.id,
        source: it.source.clone(),
        who: it.who.clone(),
        channel: it.channel.clone(),
        ts: it.ts,
        text: ctx_line(it).trim_start_matches("* ").to_string(),
        link: it.link.clone(),
        line: ctx_line(it),
        included: it.source == "clip" || it.filed,
    }
}

fn in_episode(p: &Prompt, e: &Episode) -> bool {
    p.ts >= e.start_ts - PROMPT_SLACK && p.ts <= e.end_ts + PROMPT_SLACK
}

fn compose_body(card: &Card, eps: &[&Episode], llm: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let why: Vec<&CtxView> = card.context.iter().filter(|c| c.included).collect();
    if !why.is_empty() || !card.note.is_empty() {
        lines.push("h3. Context".into());
        if !card.note.is_empty() {
            lines.push(format!("* {}", one_line(&card.note, 280)));
        }
        lines.extend(why.iter().map(|c| c.line.clone()));
        lines.push(String::new());
    }
    if !card.prompts.is_empty() {
        let mut tools: Vec<&str> = Vec::new();
        for p in &card.prompts {
            if !tools.contains(&p.tool.as_str()) {
                tools.push(&p.tool);
            }
        }
        lines.push(format!("h3. Asked of {}", tools.join(", ")));
        lines.extend(card.prompts.iter().take(PROMPTS_QUOTED).map(|p| format!("* {}", one_line(&p.text, 200))));
        if card.prompts.len() > PROMPTS_QUOTED {
            lines.push(format!("* …and {} more prompt(s)", card.prompts.len() - PROMPTS_QUOTED));
        }
        lines.push(String::new());
    }
    let did_something = !card.commits.is_empty() || eps.iter().any(|e| !e.commands.is_empty());
    if did_something {
        lines.push("h3. What was done".into());
        if llm {
            // A real model wrote the summaries: they read better than counts.
            lines.extend(eps.iter().map(|e| format!("* {}", one_line(&e.summary, 400))));
        }
        lines.extend(card.commits.iter().map(|c| format!("* Committed: {}", one_line(c, 160))));
        if !llm {
            let n: usize = eps.iter().map(|e| e.commands.len()).sum();
            if n > 0 {
                let place = if card.place.is_empty() { String::new() } else { format!(" in {}", card.place) };
                lines.push(format!("* Ran {n} command(s){place}"));
            }
        }
        lines.push(String::new());
    }
    lines.push("Captured by Scribe and reviewed before posting.".into());
    lines.join("\n")
}

/// What makes two pieces of work the same event: the ticket, else the project.
/// An unticketed interruption is its own thing.
fn event_key(e: &Episode) -> String {
    match (&e.ticket, e.kind.as_str()) {
        (Some(t), _) => format!("t:{t}"),
        (None, "interrupt") => format!("i:{}", e.id),
        (None, _) => format!("p:{}", e.repo.clone().or_else(|| e.cwd.clone()).unwrap_or_else(|| "untracked".into())),
    }
}

/// Does `b` start a new event rather than continue the one `a` belongs to?
fn breaks(split: &str, a: &Episode, b: &Episode) -> bool {
    let new_day = local_day(a.end_ts) != local_day(b.start_ts);
    match split {
        "never" => false,
        "day" => new_day,
        _ => new_day && b.start_ts - a.end_ts >= AUTO_BREAK_SECS,
    }
}

/// All captured work as events: (event id, its episodes in order). An event is
/// a logical thing — it can run across days; logging is what goes by day.
fn segments<'a>(episodes: &'a [Episode], asked: &[Prompt], split: &str) -> Vec<(String, Vec<&'a Episode>)> {
    let mut by_key: BTreeMap<String, Vec<&Episode>> = BTreeMap::new();
    for e in episodes {
        // Nothing was done in it (e.g. only the capture snippet being sourced):
        // not worth a decision. Prompting an AI tool counts as doing something.
        if e.commands.is_empty() && e.commits.is_empty() && e.note.is_none() && !asked.iter().any(|p| in_episode(p, e)) {
            continue;
        }
        by_key.entry(event_key(e)).or_default().push(e);
    }
    let mut out: Vec<(String, Vec<&Episode>)> = Vec::new();
    for (key, mut eps) in by_key {
        eps.sort_by(|a, b| a.start_ts.total_cmp(&b.start_ts));
        let mut cur: Vec<&Episode> = Vec::new();
        for e in eps {
            if cur.last().is_some_and(|last| breaks(split, last, e)) {
                out.push((format!("{key}:{}", local_day(cur[0].start_ts)), std::mem::take(&mut cur)));
            }
            cur.push(e);
        }
        out.push((format!("{key}:{}", local_day(cur[0].start_ts)), cur));
    }
    out
}

/// Is this work under a path the engineer said to log without asking?
fn ignored(paths: &[String], e: &Episode) -> bool {
    let Some(cwd) = e.cwd.as_deref() else { return false };
    let home = std::env::var("HOME").unwrap_or_default();
    paths.iter().any(|p| {
        let p = p.trim().trim_end_matches('/');
        let p = match p.strip_prefix('~') {
            Some(rest) => format!("{home}{rest}"),
            None => p.to_string(),
        };
        !p.is_empty() && (cwd == p || cwd.starts_with(&format!("{p}/")))
    })
}

/// What work added to an entry after the fact amounted to, as update text.
fn addendum(e: &LogEntry, episodes: &[Episode]) -> String {
    let mine: Vec<&Episode> = episodes.iter().filter(|x| e.auto_ids.contains(&x.id)).collect();
    if mine.is_empty() {
        return e.addendum.clone(); // raw capture has expired: keep what was recorded
    }
    let mut lines = vec![if e.body.trim().is_empty() { "h3. What was done" } else { "h3. Added automatically" }.to_string()];
    lines.extend(mine.iter().flat_map(|x| x.commits.iter()).map(|c| format!("* Committed: {}", one_line(c, 160))));
    let n: usize = mine.iter().map(|x| x.commands.len()).sum();
    if n > 0 {
        let place = mine.iter().rev().find_map(|x| x.cwd.clone()).map(|c| format!(" in {}", pretty_home(&c))).unwrap_or_default();
        lines.push(format!("* Ran {n} command(s){place}"));
    }
    lines.extend(mine.iter().filter_map(|x| x.note.as_ref()).map(|n| format!("* {}", one_line(n, 280))));
    lines.join("\n")
}

/// Put new work where it already belongs, without asking:
///   * more work on an event that was logged locally joins that entry — both
///     what happened and the time;
///   * work under an ignored path is logged silently, one entry per event.
/// Work on an event already sent to Jira is left as a card: nothing is posted
/// anywhere without a Send. Also refreshes each entry's per-day time.
pub fn settle() {
    let cfg = config::load();
    let episodes = model::load_episodes();
    let asked: Vec<Prompt> = prompts::all_sessions().into_iter().flat_map(|s| s.entries).collect();
    let mut lb = logbook::load();
    let mut changed = false;
    let now = now_ts();

    for (_, eps) in segments(&episodes, &asked, &cfg.event_split) {
        let owner = |lb: &logbook::Logbook, id: i64| lb.entries.iter().position(|e| e.episode_ids.contains(&id));
        let free: Vec<&Episode> = eps.iter().copied().filter(|e| owner(&lb, e.id).is_none()).collect();
        if free.is_empty() {
            continue;
        }
        let mut owners: Vec<usize> = eps.iter().filter_map(|e| owner(&lb, e.id)).collect();
        owners.sort_by(|a, b| lb.entries[*a].created_ts.total_cmp(&lb.entries[*b].created_ts));
        owners.dedup();
        let latest = owners.last().copied();
        let local = latest.filter(|i| {
            let e = &lb.entries[*i];
            e.kind == "logged" && !e.pushed_jira && !e.silent
        });
        let mut quiet = owners.iter().rev().copied().find(|i| lb.entries[*i].silent && !lb.entries[*i].pushed_jira);
        for e in free {
            let target = if ignored(&cfg.ignored_paths, e) {
                if quiet.is_none() {
                    lb.seq += 1;
                    let place = e.cwd.clone().unwrap_or_default();
                    lb.entries.push(LogEntry {
                        id: lb.seq,
                        created_ts: now,
                        updated_ts: now,
                        kind: "logged".into(),
                        title: format!("Work in {}", pretty_home(&place)),
                        ticket: e.ticket.clone().unwrap_or_default(),
                        project: place,
                        silent: true,
                        ..Default::default()
                    });
                    quiet = Some(lb.entries.len() - 1);
                }
                quiet
            } else {
                local
            };
            if let Some(i) = target {
                let entry = &mut lb.entries[i];
                entry.episode_ids.push(e.id);
                entry.auto_ids.push(e.id);
                entry.updated_ts = now;
                changed = true;
            }
        }
    }

    for e in lb.entries.iter_mut().filter(|e| e.kind != "disposed" && !e.episode_ids.is_empty()) {
        if !e.episode_ids.iter().any(|id| episodes.iter().any(|x| x.id == *id)) {
            continue; // nothing left to measure from; the stored figures stand
        }
        let by_day = stream::days_for_ids(&e.episode_ids);
        let more = addendum(e, &episodes);
        if by_day != e.by_day || more != e.addendum {
            if !e.auto_ids.is_empty() {
                e.secs = by_day.iter().map(|d| d.secs).sum();
            }
            e.by_day = by_day;
            e.addendum = more;
            changed = true;
        }
    }
    if changed {
        logbook::store(&lb);
    }
}

/// Build the cards, newest first.
pub fn build() -> Vec<Card> {
    let cfg = config::load();
    let llm = cfg.llm_backend != "stub";
    let episodes = model::load_episodes();
    let handled = logbook::handled_ids();
    let now = now_ts();
    let mut asked: Vec<Prompt> = prompts::all_sessions().into_iter().flat_map(|s| s.entries).collect();
    asked.sort_by(|a, b| a.ts.total_cmp(&b.ts));

    // One card per event, holding whatever of it is not yet accounted for.
    let lb = logbook::load();
    // The latest update already sent for an event, if any: (entry id, when, ticket).
    let mut sent_for: BTreeMap<String, (i64, f64, String)> = BTreeMap::new();
    let groups: Vec<(String, Vec<&Episode>)> = segments(&episodes, &asked, &cfg.event_split)
        .into_iter()
        .map(|(id, eps)| {
            let prior = lb
                .entries
                .iter()
                .filter(|x| x.pushed_jira && !x.ticket.is_empty() && x.kind != "handoff")
                .filter(|x| eps.iter().any(|e| x.episode_ids.contains(&e.id)))
                .max_by(|a, b| a.created_ts.total_cmp(&b.created_ts));
            if let Some(x) = prior {
                sent_for.insert(id.clone(), (x.id, x.updated_ts, x.ticket.clone()));
            }
            (id, eps.into_iter().filter(|e| !handled.contains(&e.id)).collect::<Vec<_>>())
        })
        .filter(|(_, eps)| !eps.is_empty())
        .collect();

    let mut cards: Vec<(Card, Vec<&Episode>)> = Vec::new();
    for (id, eps) in groups {
        let first = eps[0];
        let ids: Vec<i64> = eps.iter().map(|e| e.id).collect();
        let interrupt = eps.iter().any(|e| e.kind == "interrupt");
        let place = eps
            .iter()
            .rev()
            .find_map(|e| e.cwd.clone())
            .map(|c| pretty_home(&c))
            .unwrap_or_default();
        let note = eps.iter().filter_map(|e| e.note.clone()).collect::<Vec<_>>().join("; ");
        let end_ts = eps.iter().map(|e| e.end_ts).fold(0.0, f64::max);
        let title = match &first.ticket {
            Some(t) => t.clone(),
            None if interrupt && !note.is_empty() => note.clone(),
            None if interrupt => "Interruption".into(),
            None if !place.is_empty() => place.clone(),
            None => "Untracked work".into(),
        };
        let prior = sent_for.get(&id);
        // Work that became a ticket when it was first sent keeps going there.
        let ticket = first.ticket.clone().or_else(|| prior.map(|p| p.2.clone()));
        let card = Card {
            follows: prior.map(|p| p.0),
            follows_ts: prior.map(|p| p.1).unwrap_or(0.0),
            id,
            kind: if interrupt { "interrupt" } else if ticket.is_some() { "work" } else { "untracked" }.into(),
            ticket,
            title,
            path: eps.iter().rev().find_map(|e| e.cwd.clone()).unwrap_or_default(),
            place,
            start_ts: eps.iter().map(|e| e.start_ts).fold(f64::MAX, f64::min),
            end_ts,
            secs: stream::secs_for_ids(&ids),
            active: now - end_ts < cfg.episode_gap_seconds,
            episode_ids: ids,
            commits: eps.iter().flat_map(|e| e.commits.clone()).collect(),
            commands: eps.iter().flat_map(|e| e.commands.clone()).collect(),
            note,
            prompts: asked
                .iter()
                .filter(|p| eps.iter().any(|e| in_episode(p, e)))
                .map(|p| PromptView { ts: p.ts, tool: p.tool.clone(), text: p.prompt.clone() })
                .collect(),
            ..Default::default()
        };
        cards.push((card, eps));
    }

    // Attach context. A stated or mentioned ticket wins; otherwise it belongs to
    // the work it overlaps in time. A clip nothing claims becomes its own card.
    let mut orphans: Vec<ContextItem> = Vec::new();
    for it in context::load().into_iter().filter(|i| !i.used) {
        let lead = if it.source == "clip" { LEAD_SECS } else { 0.0 };
        let by_ticket = (!it.ticket.is_empty())
            .then(|| {
                cards
                    .iter()
                    .enumerate()
                    .filter(|(_, (c, _))| c.ticket.as_deref() == Some(it.ticket.as_str()))
                    .max_by(|a, b| a.1 .0.end_ts.total_cmp(&b.1 .0.end_ts))
                    .map(|(i, _)| i)
            })
            .flatten();
        let by_time = || {
            cards
                .iter()
                .enumerate()
                .filter(|(_, (_, eps))| {
                    eps.iter().any(|e| it.ts >= e.start_ts - LEAD_SECS && it.ts <= e.end_ts + lead)
                })
                .max_by(|a, b| a.1 .0.end_ts.total_cmp(&b.1 .0.end_ts))
                .map(|(i, _)| i)
        };
        // Filed by hand under one card: that beats mentions and timing.
        let pinned = (!it.pin.is_empty()).then(|| cards.iter().position(|(c, _)| c.id == it.pin)).flatten();
        // A thread about a ticket with no open work is not evidence for other work.
        let target = pinned.or_else(|| if it.ticket.is_empty() { by_time() } else { by_ticket });
        match target {
            Some(i) => cards[i].0.context.push(ctx_view(&it)),
            None if it.source == "clip" => orphans.push(it),
            None => {}
        }
    }

    let mut out: Vec<Card> = cards
        .into_iter()
        .map(|(mut c, eps)| {
            c.body = compose_body(&c, &eps, llm);
            c
        })
        .collect();
    for it in orphans {
        let mut c = Card {
            id: format!("c:{}", it.id),
            kind: "thread".into(),
            ticket: (!it.ticket.is_empty()).then(|| it.ticket.clone()),
            title: if it.ticket.is_empty() { "Chat thread".into() } else { it.ticket.clone() },
            start_ts: it.ts,
            end_ts: it.ts,
            context: vec![ctx_view(&it)],
            ..Default::default()
        };
        c.body = compose_body(&c, &[], llm);
        out.push(c);
    }
    out.sort_by(|a, b| b.end_ts.total_cmp(&a.end_ts));
    out
}

/// Today's auto-generated handoff note, if it is still waiting to be sent.
pub fn handoff() -> Option<Draft> {
    model::load_drafts()
        .into_iter()
        .find(|d| d.kind == "confluence_handoff" && !d.composed && d.status == "draft")
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SendReq {
    pub episode_ids: Vec<i64>,
    pub context_ids: Vec<i64>,
    pub ticket: String,
    pub title: String,
    pub body: String,
    pub secs: f64,
    /// Keep it in the local log only; nothing is posted.
    pub local: bool,
    /// This is more work on an event already sent: once posted, fold it into
    /// that History entry instead of making a second one.
    pub append_to: i64,
}

#[derive(Serialize)]
pub struct Sent {
    pub reference: String,
    pub url: String,
    /// A line to paste back into the chat thread, so it points at the ticket.
    pub reply: String,
    pub mock: bool,
}

/// Send one card: an update on its ticket (a worklog when there is time to log,
/// else a comment), or a new issue when it has no ticket. On failure nothing is
/// recorded, so the card stays on the review screen.
pub fn send(req: &SendReq) -> Result<Sent, String> {
    if req.body.trim().is_empty() {
        return Err("the update is empty".into());
    }
    let ticket = req.ticket.trim().to_string();
    let has_work = !req.episode_ids.is_empty();
    let secs = if has_work { req.secs.max(0.0) } else { 0.0 };
    if req.local {
        return Ok(log_locally(req, &ticket, secs));
    }
    let (kind, title, ref_key) = if !ticket.is_empty() {
        let title = if secs > 0.0 {
            format!("Worklog on {ticket} — {}", stream::fmt_dur(secs))
        } else {
            format!("Update on {ticket}")
        };
        ("jira_worklog", title, Some(ticket.clone()))
    } else if has_work {
        let title = if req.title.trim().is_empty() { "Unplanned work" } else { req.title.trim() };
        ("jira_new", title.to_string(), None)
    } else {
        return Err("which ticket does this thread belong to? Enter its key".into());
    };

    let mut st = model::load_state();
    st.draft_seq += 1;
    let id = st.draft_seq;
    model::save_state(&st);
    let mut drafts = model::load_drafts();
    drafts.push(Draft {
        id,
        episode_id: None,
        kind: kind.into(),
        target: "Jira".into(),
        title: title.clone(),
        body: req.body.clone(),
        ref_key,
        status: "approved".into(),
        composed: true,
        dur_secs: secs,
        days: stream::days_for_ids(&req.episode_ids),
    });
    model::save_drafts(&drafts);
    let project = project_of(&req.episode_ids);
    let lkind = if kind == "jira_worklog" { "worklog" } else { "sent" };
    let entry = logbook::add(lkind, &title, &ticket, "", &project, req.episode_ids.clone(), Some(id), secs, false);

    let p = publish::publish_draft(id, "engineer");
    if !p.ok {
        let mut drafts = model::load_drafts();
        drafts.retain(|d| d.id != id);
        model::save_drafts(&drafts);
        logbook::restore_by_draft(id);
        return Err(p.error.unwrap_or_else(|| "publish failed".into()));
    }
    logbook::set_body(entry, &req.body);
    logbook::mark_pushed(entry, &p.reference);
    if req.append_to > 0 {
        logbook::absorb(req.append_to, entry);
    }
    context::mark_used(&req.context_ids);
    settle();
    Ok(Sent {
        reply: format!("Written up in {}: {}", p.reference, p.url),
        reference: p.reference,
        url: p.url,
        mock: config::load().publish_backend != "atlassian",
    })
}

fn project_of(episode_ids: &[i64]) -> String {
    model::load_episodes()
        .iter()
        .rev()
        .find(|e| episode_ids.contains(&e.id))
        .and_then(|e| e.cwd.clone())
        .unwrap_or_default()
}

/// Keep the write-up and its time in History without posting anywhere. It can
/// still be pushed to Jira from there later.
fn log_locally(req: &SendReq, ticket: &str, secs: f64) -> Sent {
    let title = if !ticket.is_empty() {
        format!("Update on {ticket}")
    } else if !req.title.trim().is_empty() {
        req.title.trim().to_string()
    } else {
        "Chat thread".to_string()
    };
    let project = project_of(&req.episode_ids);
    // History works time out from the work itself, so a corrected figure is
    // kept as an adjustment on that work (spread in proportion).
    let measured = stream::secs_for_ids(&req.episode_ids);
    if (secs - measured).abs() >= 30.0 {
        let eps = model::load_episodes();
        for (n, id) in req.episode_ids.iter().enumerate() {
            let own = stream::secs_for_ids(&[*id]);
            let share = if measured > 0.0 { secs * own / measured } else if n == 0 { secs } else { 0.0 };
            if eps.iter().any(|e| e.id == *id) {
                crate::timeadj::set(*id, share);
            }
        }
    }
    let entry = logbook::add("logged", &title, ticket, "", &project, req.episode_ids.clone(), None, secs, false);
    logbook::set_body(entry, &req.body);
    context::mark_used(&req.context_ids);
    settle(); // records which days the time falls on
    Sent { reference: "your local log".into(), url: String::new(), reply: String::new(), mock: false }
}

/// The update as a chat message: Slack's own markup, short enough to post.
/// Scribe has no Slack access, so this is handed back to be pasted.
pub fn slack_post(req: &SendReq) -> String {
    slack_text(req, &crate::secrets::load().atlassian.site)
}

fn slack_text(req: &SendReq, site: &str) -> String {
    let link = regex::Regex::new(r"\[([^|\]]+)\|([^\]]+)\]").unwrap();
    let mono = regex::Regex::new(r"\{\{([^}]+)\}\}").unwrap();
    let inline = |s: &str| mono.replace_all(&link.replace_all(s, "$1 ($2)"), "`$1`").to_string();
    let ticket = req.ticket.trim();
    let mut out: Vec<String> = Vec::new();
    let head = if !ticket.is_empty() { format!("Update on {ticket}") } else { req.title.trim().to_string() };
    if !head.is_empty() {
        out.push(format!("*{head}*"));
    }
    for l in req.body.lines() {
        let l = l.trim_end();
        if l.starts_with("Captured by Scribe") {
            continue;
        }
        if let Some(h) = ["h1. ", "h2. ", "h3. "].iter().find_map(|p| l.strip_prefix(p)) {
            out.push(format!("*{}*", inline(h)));
        } else if let Some(b) = l.strip_prefix("* ").or_else(|| l.strip_prefix("- ")) {
            out.push(format!("• {}", inline(b)));
        } else {
            out.push(inline(l));
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut foot: Vec<String> = Vec::new();
    if req.secs >= 60.0 {
        foot.push(format!("Time: {}", stream::fmt_dur(req.secs)));
    }
    if !ticket.is_empty() && !site.is_empty() {
        foot.push(format!("{}/browse/{ticket}", site.trim_end_matches('/')));
    }
    if !foot.is_empty() {
        out.push(String::new());
        out.push(foot.join(" · "));
    }
    out.join("\n")
}

/// Skip a card: its work moves to History as dismissed (restorable there) and
/// its context is dropped from review.
pub fn skip(episode_ids: &[i64], context_ids: &[i64]) {
    if !episode_ids.is_empty() {
        let ids: Vec<String> = episode_ids.iter().map(|i| format!("ep-{i}")).collect();
        let _ = stream::dispose(&ids);
    }
    context::mark_used(context_ids);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_break_by_the_chosen_rule() {
        use chrono::TimeZone;
        let at = |d: u32, h: u32| chrono::Local.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap().timestamp() as f64;
        let ep = |start: f64, end: f64| Episode { start_ts: start, end_ts: end, ..Default::default() };
        let (evening, late, morning) = (ep(at(12, 22), at(12, 23)), ep(at(13, 0), at(13, 1)), ep(at(13, 9), at(13, 10)));
        // Working past midnight is one event; coming back in the morning is a new one.
        assert!(!breaks("auto", &evening, &late));
        assert!(!breaks("auto", &late, &morning));
        assert!(breaks("auto", &evening, &morning));
        assert!(breaks("day", &evening, &late));
        assert!(!breaks("never", &evening, &morning));
    }

    #[test]
    fn slack_post_uses_slack_markup() {
        let req = SendReq {
            ticket: "OPS-12".into(),
            body: "h3. What was done\n* Ran {{df -h}} on [db-prod|https://x.example/db]\n\nCaptured by Scribe and reviewed before posting.".into(),
            secs: 2700.0,
            ..Default::default()
        };
        assert_eq!(
            slack_text(&req, "https://co.atlassian.net/"),
            "*Update on OPS-12*\n*What was done*\n• Ran `df -h` on db-prod (https://x.example/db)\n\nTime: 45m · https://co.atlassian.net/browse/OPS-12"
        );
    }
}
