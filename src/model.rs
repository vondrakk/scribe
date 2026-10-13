//! Data types + their storage in Scribe's database (src/db.rs).
//!
//! Two kinds of data, by design:
//!   * events  — the raw append-only capture, written by shell/git hooks one
//!               row at a time. Source of truth.
//!   * derived — episodes, drafts, binding, counters; rebuildable from the
//!               events by `process`.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{config, db};

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
    /// Its row in the database (not stored in the record itself).
    #[serde(skip)]
    pub row: i64,
}

/// Time spent on one calendar day (local), as `YYYY-MM-DD`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct DaySecs {
    pub day: String,
    pub secs: f64,
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
    /// What the engineer said interrupted them (`w ! <reason>`), if anything.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
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
    /// How that time falls across days, when the work spans more than one:
    /// it is logged as one worklog per day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<DaySecs>,
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

// --- captured events (append-only) -----------------------------------------

/// Record one event.
pub fn append_event(mut ev: Event) {
    if ev.ts == 0.0 {
        ev.ts = now_ts();
    }
    db::append(db::EVENTS, ev.ts, &ev.kind, &ev);
}

pub fn read_events() -> Vec<Event> {
    db::all_with_ids::<Event>(db::EVENTS)
        .into_iter()
        .map(|(row, mut ev)| {
            ev.row = row;
            ev
        })
        .collect()
}

// --- derived state -----------------------------------------------------------

pub fn load_state() -> State {
    db::doc("state")
}

pub fn save_state(st: &State) {
    db::put_doc("state", st);
}

pub fn load_episodes() -> Vec<Episode> {
    db::doc("episodes")
}

pub fn save_episodes(eps: &[Episode]) {
    db::put_doc("episodes", eps);
}

pub fn load_drafts() -> Vec<Draft> {
    db::doc("drafts")
}

pub fn save_drafts(ds: &[Draft]) {
    db::put_doc("drafts", ds);
}

/// Wipe captured + derived data for a clean slate. Keeps config.json and the
/// shell snippet. `include_published` also clears the published/ artifacts.
/// Returns (raw_events_removed, episodes_removed, drafts_removed, prompts_removed).
pub fn reset(include_published: bool) -> (usize, usize, usize, usize) {
    let n_ev = read_events().len();
    let n_ep = load_episodes().len();
    let n_dr = load_drafts().len();
    let n_pr = db::count(db::PROMPTS);

    for t in [db::EVENTS, db::PROMPTS, db::CONTEXT] {
        db::clear(t);
    }
    for d in ["episodes", "drafts", "state", "context_seq"] {
        db::drop_doc(d);
    }
    let _ = std::fs::remove_file(config::binding_file());
    if include_published {
        let _ = std::fs::remove_dir_all(config::published_dir());
    }
    (n_ev, n_ep, n_dr, n_pr)
}
