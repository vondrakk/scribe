//! The review screen: Scribe proposes, the engineer decides.
//!
//! Unhandled work is grouped into one card per ticket per day (or per project
//! for work with no ticket), joined with the conversation context that explains
//! it (src/context.rs), and given a ready-to-post Jira update. The engineer
//! sends it, edits it, or skips it — nothing else is asked of them. Nothing
//! here publishes without that explicit send (§7.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context::{self, ContextItem};
use crate::model::{self, now_ts, Draft, Episode};
use crate::{config, logbook, publish, stream};

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

#[derive(Serialize, Default)]
pub struct Card {
    pub id: String,
    pub kind: String, // work | interrupt | untracked | thread
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub title: String,
    pub place: String,
    pub start_ts: f64,
    pub end_ts: f64,
    pub secs: f64,
    /// The newest episode is still within the session gap — work may continue.
    pub active: bool,
    pub episode_ids: Vec<i64>,
    pub commits: Vec<String>,
    pub commands: Vec<String>,
    pub note: String,
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

fn ctx_view(it: &ContextItem) -> CtxView {
    CtxView {
        id: it.id,
        source: it.source.clone(),
        who: it.who.clone(),
        channel: it.channel.clone(),
        ts: it.ts,
        text: ctx_line(it).trim_start_matches("* ").to_string(),
        link: it.link.clone(),
        line: ctx_line(it),
        included: it.source == "clip",
    }
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
    if !eps.is_empty() {
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

/// Build the cards, newest first.
pub fn build() -> Vec<Card> {
    let cfg = config::load();
    let llm = cfg.llm_backend != "stub";
    let episodes = model::load_episodes();
    let handled = logbook::handled_ids();
    let now = now_ts();

    // Group unhandled work: per ticket per day, else per project per day; an
    // unticketed interrupt is its own thing.
    let mut groups: BTreeMap<String, Vec<&Episode>> = BTreeMap::new();
    for e in episodes.iter().filter(|e| !handled.contains(&e.id)) {
        // Nothing was done in it (e.g. only the capture snippet being sourced):
        // not worth a decision.
        if e.commands.is_empty() && e.commits.is_empty() && e.note.is_none() {
            continue;
        }
        let day = local_day(e.end_ts);
        let key = match (&e.ticket, e.kind.as_str()) {
            (Some(t), _) => format!("t:{t}:{day}"),
            (None, "interrupt") => format!("i:{}", e.id),
            (None, _) => format!(
                "p:{}:{day}",
                e.repo.clone().or_else(|| e.cwd.clone()).unwrap_or_else(|| "untracked".into())
            ),
        };
        groups.entry(key).or_default().push(e);
    }

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
        let card = Card {
            id,
            kind: if interrupt { "interrupt" } else if first.ticket.is_some() { "work" } else { "untracked" }.into(),
            ticket: first.ticket.clone(),
            title,
            place,
            start_ts: eps.iter().map(|e| e.start_ts).fold(f64::MAX, f64::min),
            end_ts,
            secs: stream::secs_for_ids(&ids),
            active: now - end_ts < cfg.episode_gap_seconds,
            episode_ids: ids,
            commits: eps.iter().flat_map(|e| e.commits.clone()).collect(),
            commands: eps.iter().flat_map(|e| e.commands.clone()).collect(),
            note,
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
        // A thread about a ticket with no open work is not evidence for other work.
        let target = if it.ticket.is_empty() { by_time() } else { by_ticket };
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
    });
    model::save_drafts(&drafts);
    let project = model::load_episodes()
        .iter()
        .rev()
        .find(|e| req.episode_ids.contains(&e.id))
        .and_then(|e| e.cwd.clone())
        .unwrap_or_default();
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
    logbook::mark_pushed(entry, &p.reference);
    context::mark_used(&req.context_ids);
    Ok(Sent {
        reply: format!("Written up in {}: {}", p.reference, p.url),
        reference: p.reference,
        url: p.url,
        mock: config::load().publish_backend != "atlassian",
    })
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
