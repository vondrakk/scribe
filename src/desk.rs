//! The stream: one row per piece of work, with everything known about it.
//!
//! A work item is a ticket (or, when there is no ticket, an incident, a merge
//! request, or a stretch of untracked work). Each row pulls together what the
//! other systems say about it — its Jira state, merge requests, PagerDuty
//! incidents, Confluence pages, chat about it — and the work captured on this
//! machine, and is ranked by whether it needs the operator now.
//!
//! Reads come from the local caches (inbox.json, the capture store), so the
//! stream is instant and works offline. Writes go straight to the source
//! system, only on an explicit action, and only when posting is switched on.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

use crate::inbox::{self, InboxItem};
use crate::review::{self, Card, CtxView};
use crate::{atlassian, config, context, gitlab, logbook, pagerduty};

/// Unfiled chat shown at once; older messages are still kept until they expire.
const MAX_LOOSE: usize = 30;

#[derive(Serialize, Default)]
pub struct Item {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub title: String,
    /// Which worlds this item has something in: jira, gitlab, pagerduty,
    /// confluence, chat, work.
    pub sources: Vec<String>,
    /// Why it needs the operator, most pressing first. Empty = nothing to do.
    pub needs: Vec<String>,
    pub rank: i64,
    pub updated_ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jira: Option<InboxItem>,
    pub incidents: Vec<InboxItem>,
    pub mrs: Vec<InboxItem>,
    pub pages: Vec<InboxItem>,
    /// Chat that mentions the ticket and isn't already part of a write-up.
    pub chat: Vec<CtxView>,
    /// Captured work waiting to be written up (the review cards).
    pub cards: Vec<Card>,
}

#[derive(Serialize)]
pub struct Desk {
    pub items: Vec<Item>,
    /// How many items need the operator.
    pub needs: usize,
    /// Recent chat that no row has claimed, for the operator to file.
    pub loose: Vec<CtxView>,
    pub fetched_ts: f64,
    /// Writes reach the real systems (false = practice mode).
    pub live: bool,
}

fn need(item: &mut Item, rank: i64, why: &str) {
    if !item.needs.iter().any(|n| n == why) {
        item.needs.push(why.to_string());
    }
    item.rank = item.rank.max(rank);
}

fn source(item: &mut Item, name: &str) {
    if !item.sources.iter().any(|s| s == name) {
        item.sources.push(name.to_string());
    }
}

pub fn live() -> bool {
    config::load().publish_backend == "atlassian"
}

pub fn build() -> Desk {
    let inbox = inbox::load();
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    // Everything about a ticket shares a row; anything else gets its own.
    let slot = |key: &Option<String>, own: String| match key {
        Some(k) => format!("k:{k}"),
        None => own,
    };

    for it in &inbox.items {
        let own = match it.source.as_str() {
            "pagerduty" => format!("pd:{}", it.ref_id),
            _ => format!("in:{}", it.url),
        };
        let item = items.entry(slot(&it.ticket, own)).or_default();
        item.key = item.key.clone().or_else(|| it.ticket.clone());
        item.updated_ts = item.updated_ts.max(it.updated_ts);
        match it.source.as_str() {
            "jira" => {
                source(item, "jira");
                item.title = it.title.clone();
                item.jira = Some(it.clone());
                item.rank = item.rank.max(10);
            }
            "pagerduty" => {
                source(item, "pagerduty");
                if it.status == "triggered" {
                    need(item, 100, "Paging you");
                } else {
                    need(item, 70, "Incident open");
                }
                item.incidents.push(it.clone());
            }
            "gitlab" => {
                source(item, "gitlab");
                if it.kind == "mr-review" {
                    need(item, 60, "Review requested");
                } else {
                    item.rank = item.rank.max(20);
                }
                item.mrs.push(it.clone());
            }
            "confluence" => {
                source(item, "confluence");
                item.pages.push(it.clone());
            }
            _ => {}
        }
    }

    let mut in_cards: Vec<i64> = Vec::new();
    for card in review::build() {
        in_cards.extend(card.context.iter().map(|c| c.id));
        let item = items.entry(slot(&card.ticket, card.id.clone())).or_default();
        item.key = item.key.clone().or_else(|| card.ticket.clone());
        item.updated_ts = item.updated_ts.max(card.end_ts);
        if card.kind == "thread" {
            source(item, "chat");
            need(item, 40, "Chat thread to file");
        } else {
            source(item, "work");
            need(item, 50, if card.ticket.is_some() { "Update ready to send" } else { "Untracked work to write up" });
        }
        if item.title.is_empty() && card.ticket.is_none() {
            item.title = card.title.clone();
        }
        item.cards.push(card);
    }

    // Chat that names a ticket but belongs to no write-up yet; the rest is loose.
    let mut loose: Vec<CtxView> = Vec::new();
    for c in context::load().into_iter().filter(|c| !c.used && !in_cards.contains(&c.id)) {
        if c.ticket.is_empty() {
            loose.push(review::ctx_view(&c));
            continue;
        }
        let item = items.entry(format!("k:{}", c.ticket)).or_default();
        item.key = Some(c.ticket.clone());
        item.updated_ts = item.updated_ts.max(c.ts);
        source(item, "chat");
        need(item, 40, "Mentioned in chat");
        item.chat.push(review::ctx_view(&c));
    }

    let mut out: Vec<Item> = items
        .into_iter()
        .map(|(id, mut item)| {
            item.id = id;
            if item.title.is_empty() {
                item.title = item
                    .incidents
                    .first()
                    .or(item.mrs.first())
                    .map(|i| i.title.clone())
                    .or_else(|| item.key.clone())
                    .unwrap_or_else(|| "Untitled".into());
            }
            item
        })
        .collect();
    out.sort_by(|a, b| b.rank.cmp(&a.rank).then(b.updated_ts.total_cmp(&a.updated_ts)));
    Desk {
        needs: out.iter().filter(|i| !i.needs.is_empty()).count(),
        items: out,
        loose: {
            loose.sort_by(|a, b| b.ts.total_cmp(&a.ts));
            loose.truncate(MAX_LOOSE);
            loose
        },
        fetched_ts: inbox.fetched_ts,
        live: live(),
    }
}

/// How many items need the operator — for the tab badge and the tray.
pub fn needs_count() -> usize {
    build().needs
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim()
}

/// Record an action taken in another system, so History shows what went out.
fn record(title: &str, ticket: &str) {
    let in_jira = title.contains(ticket) && !ticket.is_empty();
    logbook::add("sent", title, ticket, "", "", Vec::new(), None, 0.0, in_jira);
}

/// Carry out one outbound action. `what` is `<system>/<verb>`.
pub fn act(what: &str, v: &Value) -> Result<Value, String> {
    if !live() {
        return Err("Practice mode is on — switch on posting in Settings → Connections to act on other systems.".into());
    }
    let key = text(v, "key");
    let body = text(v, "body");
    let need_body = || if body.is_empty() { Err("write something first".to_string()) } else { Ok(()) };
    match what {
        "jira/comment" => {
            need_body()?;
            let p = atlassian::add_comment(key, body)?;
            record(&format!("Commented on {key}"), key);
            Ok(json!({ "ok": true, "url": p.url }))
        }
        "jira/transition" => {
            atlassian::transition(key, text(v, "id"))?;
            record(&format!("Moved {key} to {}", text(v, "name")), key);
            Ok(json!({ "ok": true }))
        }
        "gitlab/comment" => {
            need_body()?;
            gitlab::comment(text(v, "host"), text(v, "ref_id"), body)?;
            record(&format!("Commented on merge request {}", text(v, "title")), key);
            Ok(json!({ "ok": true }))
        }
        "gitlab/approve" => {
            gitlab::approve(text(v, "host"), text(v, "ref_id"))?;
            record(&format!("Approved merge request {}", text(v, "title")), key);
            Ok(json!({ "ok": true }))
        }
        "pagerduty/ack" | "pagerduty/resolve" => {
            let (status, verb) = if what.ends_with("ack") { ("acknowledged", "Acknowledged") } else { ("resolved", "Resolved") };
            pagerduty::set_status(text(v, "ref_id"), status)?;
            record(&format!("{verb} incident {}", text(v, "title")), key);
            Ok(json!({ "ok": true }))
        }
        "pagerduty/note" => {
            need_body()?;
            pagerduty::add_note(text(v, "ref_id"), body)?;
            record(&format!("Added a note to incident {}", text(v, "title")), key);
            Ok(json!({ "ok": true }))
        }
        "confluence/append" => {
            need_body()?;
            let p = atlassian::append_page(text(v, "ref_id"), body)?;
            record(&format!("Appended to page {}", p.reference), key);
            Ok(json!({ "ok": true, "url": p.url }))
        }
        _ => Err(format!("unknown action {what}")),
    }
}
