//! Inbound artifacts that flow TO the operator — GitLab MRs and assigned Jira
//! issues — cached locally and threaded into the stream (dir: "in").
//!
//! The connectors are not hit on every panel poll; instead a refresh (manual
//! button, `scribe pull`, or a periodic timer) fetches and writes inbox.json,
//! and the stream reads the cache. Connectors that are unconfigured or
//! unauthenticated simply contribute nothing — no errors.

use serde::{Deserialize, Serialize};

use crate::config;
use crate::model::now_ts;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct InboxItem {
    pub source: String, // gitlab | jira
    pub kind: String,   // mr | issue
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ticket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub author: Option<String>,
    pub updated_ts: f64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Inbox {
    pub fetched_ts: f64,
    pub items: Vec<InboxItem>,
}

fn path() -> std::path::PathBuf {
    config::home().join("inbox.json")
}

pub fn load() -> Inbox {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(inbox: &Inbox) {
    // Write-then-rename so a concurrent reader (the panel's request thread) never
    // sees a half-written file while the background refresh thread writes.
    if let Ok(s) = serde_json::to_string_pretty(inbox) {
        let final_path = path();
        let tmp = final_path.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &final_path);
        }
    }
}

pub struct RefreshResult {
    pub gitlab: usize,
    pub jira: usize,
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
    let (ng, nj) = (gitlab.len(), jira.len());
    items.extend(gitlab);
    items.extend(jira);
    items.sort_by(|a, b| b.updated_ts.partial_cmp(&a.updated_ts).unwrap_or(std::cmp::Ordering::Equal));

    save(&Inbox { fetched_ts: now_ts(), items });
    RefreshResult { gitlab: ng, jira: nj, errors }
}
