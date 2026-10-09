//! Data types + the file-backed store.
//!
//! Two stores, by design:
//!   * events.jsonl  — the raw append-only log. Written by shell/git hooks with a
//!                     single atomic append. Source of truth.
//!   * *.json        — derived state (episodes, drafts, binding, counters),
//!                     rebuildable from the log by `process`.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

pub fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Event {
    pub ts: f64,
    #[serde(rename = "type")]
    pub kind: String, // shell | git | bind | seam
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub msg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sha: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Episode {
    pub id: i64,
    pub start_ts: f64,
    pub end_ts: f64,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub ticket: Option<String>,
    pub activity: String,
    pub kind: String, // planned | interrupt | unaccounted
    pub summary: String,
    pub n_events: i64,
    /// The working directory where this work happened (most frequent cwd) — the
    /// most informative descriptor of what was being done.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The actual shell commands in this episode (control commands filtered).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Commit messages made during this episode.
    #[serde(default)]
    pub commits: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Draft {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub episode_id: Option<i64>,
    pub kind: String,   // jira_new | jira_worklog | confluence_handoff
    pub target: String, // Jira | Confluence
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ref_key: Option<String>,
    pub status: String, // draft | approved | discarded | published
    /// Operator-composed from stream items (not episode-derived). Preserved
    /// verbatim across reprocess instead of being regenerated.
    #[serde(default)]
    pub composed: bool,
    /// For jira_worklog drafts: the summed time (seconds) this worklog records.
    /// Posted as a real Jira worklog when > 0; 0 falls back to a comment.
    #[serde(default)]
    pub dur_secs: f64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Binding {
    pub ticket: String,
    pub source: String,
    pub since: f64,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub binding: Option<Binding>,
    #[serde(default)]
    pub draft_seq: i64,
    #[serde(default)]
    pub publish_seq: i64,
}

// --- append-only event log -------------------------------------------------

/// Atomically append one event to the JSONL log.
pub fn append_event(mut ev: Event) {
    if ev.ts == 0.0 {
        ev.ts = now_ts();
    }
    if let Ok(mut line) = serde_json::to_string(&ev) {
        line.push('\n');
        if let Ok(mut fh) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(config::events_log())
        {
            let _ = fh.write_all(line.as_bytes());
        }
    }
}

pub fn read_events() -> Vec<Event> {
    let mut out = Vec::new();
    if let Ok(s) = std::fs::read_to_string(config::events_log()) {
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(line) {
                out.push(ev);
            }
        }
    }
    out.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// --- derived state (JSON files) -------------------------------------------

pub fn load_state() -> State {
    std::fs::read_to_string(config::state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(st: &State) {
    if let Ok(s) = serde_json::to_string_pretty(st) {
        let _ = std::fs::write(config::state_file(), s);
    }
}

pub fn load_episodes() -> Vec<Episode> {
    std::fs::read_to_string(config::episodes_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_episodes(eps: &[Episode]) {
    if let Ok(s) = serde_json::to_string_pretty(eps) {
        let _ = std::fs::write(config::episodes_file(), s);
    }
}

pub fn load_drafts() -> Vec<Draft> {
    std::fs::read_to_string(config::drafts_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_drafts(ds: &[Draft]) {
    if let Ok(s) = serde_json::to_string_pretty(ds) {
        let _ = std::fs::write(config::drafts_file(), s);
    }
}

/// Wipe captured + derived data for a clean slate. Keeps config.json and the
/// shell snippet. `include_published` also clears the published/ artifacts.
/// Returns (raw_events_removed, episodes_removed, drafts_removed, prompts_removed).
pub fn reset(include_published: bool) -> (usize, usize, usize, usize) {
    let n_ev = read_events().len();
    let n_ep = load_episodes().len();
    let n_dr = load_drafts().len();
    let n_pr = std::fs::read_dir(config::prompts_dir())
        .map(|rd| rd.flatten().filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl")).count())
        .unwrap_or(0);

    for f in [
        config::events_log(),
        config::episodes_file(),
        config::drafts_file(),
        config::state_file(),
        config::binding_file(),
    ] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_dir_all(config::prompts_dir());
    if include_published {
        let _ = std::fs::remove_dir_all(config::published_dir());
    }
    (n_ev, n_ep, n_dr, n_pr)
}
