//! Inbound artifacts that flow TO the operator — Jira issues, GitLab merge
//! requests, PagerDuty incidents and the Confluence pages about them — cached
//! locally and threaded into the stream (desk.rs).
//!
//! The connectors are not hit on every panel poll; instead a refresh (manual
//! button, `scribe pull`, or a periodic timer) fetches and writes inbox.json,
//! and the stream reads the cache. Connectors that are unconfigured or
//! unauthenticated simply contribute nothing — no errors.

use serde::{Deserialize, Serialize};

use crate::model::now_ts;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct InboxItem {
    pub source: String, // jira | gitlab | pagerduty | confluence
    pub kind: String,   // issue | mr | mr-review | incident | page
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ticket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub author: Option<String>,
    pub updated_ts: f64,
    /// State in the source system (Jira status, incident status, MR state).
    #[serde(default)]
    pub status: String,
    /// What the source calls it, for acting on it: an incident id, a page id,
    /// or `<project id>!<iid>` for a merge request.
    #[serde(default)]
    pub ref_id: String,
    /// The instance it came from (GitLab base URL) — picks the token to act with.
    #[serde(default)]
    pub host: String,
    /// One line of secondary detail (branches, urgency, excerpt).
    #[serde(default)]
    pub detail: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Inbox {
    pub fetched_ts: f64,
    pub items: Vec<InboxItem>,
}

pub fn load() -> Inbox {
    crate::db::doc("inbox")
}

fn save(inbox: &Inbox) {
    crate::db::put_doc("inbox", inbox);
}

pub struct RefreshResult {
    pub gitlab: usize,
    pub jira: usize,
    pub pagerduty: usize,
    pub confluence: usize,
    pub errors: Vec<String>,
}

/// Fetch from all configured connectors and rewrite the cache.
pub fn refresh() -> RefreshResult {
    let mut items: Vec<InboxItem> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let gitlab = match crate::gitlab::fetch() {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                errors.push(format!("gitlab: {e}"));
            }
            Vec::new()
        }
    };
    let jira = match crate::atlassian::pull_assigned() {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                errors.push(format!("jira: {e}"));
            }
            Vec::new()
        }
    };
    let pagerduty = match crate::pagerduty::fetch() {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                errors.push(format!("pagerduty: {e}"));
            }
            Vec::new()
        }
    };
    let (ng, nj, np) = (gitlab.len(), jira.len(), pagerduty.len());
    items.extend(gitlab);
    items.extend(jira);
    items.extend(pagerduty);

    // Confluence is looked up by the tickets in play, so it goes last.
    let mut keys: Vec<String> = items.iter().filter_map(|i| i.ticket.clone()).collect();
    keys.extend(crate::review::build().into_iter().filter_map(|c| c.ticket));
    keys.extend(crate::binding::get_binding().map(|b| b.ticket));
    keys.sort();
    keys.dedup();
    let pages = match crate::atlassian::search_pages(&keys) {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                errors.push(format!("confluence: {e}"));
            }
            Vec::new()
        }
    };
    let nc = pages.len();
    items.extend(pages);
    items.sort_by(|a, b| b.updated_ts.partial_cmp(&a.updated_ts).unwrap_or(std::cmp::Ordering::Equal));

    save(&Inbox { fetched_ts: now_ts(), items });
    RefreshResult { gitlab: ng, jira: nj, pagerduty: np, confluence: nc, errors }
}
