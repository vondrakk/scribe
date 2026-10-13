//! The logbook — a durable record of handled work.
//!
//! When the operator combines work into a draft, logs time locally, disposes of
//! it, or sends it, the episodes involved become "accounted for": they drop out
//! of the Stream and move to the Logged screen. Each `LogEntry` bundles the
//! episode ids it covers, so the Stream can hide them and the Logged screen can
//! show the bundle broken out by day — and more work can be appended to an
//! existing entry later.
//!
//! Keyed by episode id (stable across reprocess, like timeadj / compose). An
//! entry is removed by "restore", returning its episodes to the Stream.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::db;
use crate::model::now_ts;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct LogEntry {
    pub id: i64,
    pub created_ts: f64,
    pub updated_ts: f64,
    /// worklog | handoff | logged | disposed  (worklog/handoff came from a draft;
    /// logged = local time only; disposed = dismissed).
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub ticket: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub episode_ids: Vec<i64>,
    /// The draft this entry produced, if any (for Send / discard-restore wiring).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub draft_id: Option<i64>,
    /// Snapshot of summed time at creation (display recomputes live).
    #[serde(default)]
    pub secs: f64,
    /// Whether it was pushed to Jira (vs local-only).
    #[serde(default)]
    pub pushed_jira: bool,
    /// The write-up kept with a local-only entry; posted if it is pushed later.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Logged without asking, because its path is on the ignore list.
    #[serde(default)]
    pub silent: bool,
    /// Work added by Scribe after the entry was made (same event, later).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_ids: Vec<i64>,
    /// What that added work was — kept as text so it outlives raw capture.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub addendum: String,
    /// Time per day, kept for the same reason. An event can span days; the
    /// log is always by day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_day: Vec<crate::model::DaySecs>,
}

impl LogEntry {
    /// The write-up plus whatever was added to the event afterwards.
    pub fn text(&self) -> String {
        [self.body.trim(), self.addendum.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n\n")
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Logbook {
    pub seq: i64,
    pub entries: Vec<LogEntry>,
}

pub fn load() -> Logbook {
    let entries: Vec<LogEntry> = db::all(db::LOG);
    let top = entries.iter().map(|e| e.id).max().unwrap_or(0);
    let mut lb = Logbook { seq: db::doc::<i64>("logbook_seq").max(top), entries };
    lb.entries.sort_by_key(|e| e.id);
    lb
}

/// Store the logbook, then bring the Markdown journal in line with it.
fn save(lb: &Logbook) {
    let rows: Vec<db::Row> = lb
        .entries
        .iter()
        .filter_map(|e| Some((e.id, e.updated_ts, e.ticket.clone(), serde_json::to_string(e).ok()?)))
        .collect();
    db::replace(db::LOG, &rows);
    db::put_doc("logbook_seq", &lb.seq);
    crate::journal::sync();
}

/// Every episode id accounted for by the logbook — hidden from the Stream.
pub fn handled_ids() -> HashSet<i64> {
    let mut s = HashSet::new();
    for e in load().entries {
        for id in e.episode_ids {
            s.insert(id);
        }
    }
    s
}

/// Create a new entry; returns its id.
#[allow(clippy::too_many_arguments)]
pub fn add(
    kind: &str,
    title: &str,
    ticket: &str,
    tag: &str,
    project: &str,
    episode_ids: Vec<i64>,
    draft_id: Option<i64>,
    secs: f64,
    pushed_jira: bool,
) -> i64 {
    let mut lb = load();
    lb.seq += 1;
    let id = lb.seq;
    let now = now_ts();
    lb.entries.push(LogEntry {
        id,
        created_ts: now,
        updated_ts: now,
        kind: kind.into(),
        title: title.into(),
        ticket: ticket.into(),
        tag: tag.into(),
        project: project.into(),
        episode_ids,
        draft_id,
        secs,
        pushed_jira,
        ..Default::default()
    });
    save(&lb);
    id
}

/// Store a logbook changed as a whole (and refresh the journal).
pub fn store(lb: &Logbook) {
    save(lb);
}

/// Re-point entries after episode ids changed; ids no longer known are dropped.
pub fn remap_episodes(map: &std::collections::HashMap<i64, i64>) {
    let mut lb = load();
    if lb.entries.iter().all(|e| e.episode_ids.is_empty()) {
        return;
    }
    for e in lb.entries.iter_mut() {
        e.episode_ids = e.episode_ids.iter().filter_map(|id| map.get(id).copied()).collect();
    }
    save(&lb);
}

/// Fold entry `from` into `target` — more of the same event, sent later. The
/// work, the time and the text move over; `from` goes away.
pub fn absorb(target: i64, from: i64) -> bool {
    let mut lb = load();
    let Some(pos) = lb.entries.iter().position(|e| e.id == from) else { return false };
    if target == from || !lb.entries.iter().any(|e| e.id == target) {
        return false;
    }
    let add = lb.entries.remove(pos);
    let Some(e) = lb.entries.iter_mut().find(|e| e.id == target) else { return false };
    for id in add.episode_ids {
        if !e.episode_ids.contains(&id) {
            e.episode_ids.push(id);
        }
    }
    e.secs += add.secs;
    e.updated_ts = now_ts();
    if !add.body.trim().is_empty() {
        let more = add.body.replace("h3. What was done", "h3. Added later");
        // One sign-off line is enough: the addition carries its own.
        let was = e.body.replace("Captured by Scribe and reviewed before posting.", "");
        e.body = [was.trim(), more.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n\n");
    }
    save(&lb);
    true
}

/// Keep a write-up with an entry.
pub fn set_body(entry_id: i64, body: &str) {
    let mut lb = load();
    if let Some(e) = lb.entries.iter_mut().find(|e| e.id == entry_id) {
        e.body = body.to_string();
        save(&lb);
    }
}

/// Append episode ids (deduped) to an existing entry; bumps its time snapshot.
pub fn append(entry_id: i64, new_ids: &[i64], add_secs: f64) -> bool {
    let mut lb = load();
    let mut ok = false;
    if let Some(e) = lb.entries.iter_mut().find(|e| e.id == entry_id) {
        for id in new_ids {
            if !e.episode_ids.contains(id) {
                e.episode_ids.push(*id);
            }
        }
        e.secs += add_secs;
        e.updated_ts = now_ts();
        ok = true;
    }
    if ok {
        save(&lb);
    }
    ok
}

/// Mark a local entry as pushed to Jira (sets its ticket + the pushed flag).
pub fn mark_pushed(entry_id: i64, ticket: &str) -> bool {
    let mut lb = load();
    let mut ok = false;
    if let Some(e) = lb.entries.iter_mut().find(|e| e.id == entry_id) {
        if !ticket.is_empty() {
            e.ticket = ticket.to_string();
        }
        e.pushed_jira = true;
        e.updated_ts = now_ts();
        ok = true;
    }
    if ok {
        save(&lb);
    }
    ok
}

/// Look up one entry by id.
pub fn get(entry_id: i64) -> Option<LogEntry> {
    load().entries.into_iter().find(|e| e.id == entry_id)
}

/// Remove an entry — its episodes return to the Stream.
pub fn restore(entry_id: i64) -> bool {
    let mut lb = load();
    let n = lb.entries.len();
    lb.entries.retain(|e| e.id != entry_id);
    let changed = lb.entries.len() != n;
    if changed {
        save(&lb);
    }
    changed
}

/// Remove the entry a given draft produced (used when that draft is discarded).
pub fn restore_by_draft(draft_id: i64) {
    let mut lb = load();
    let n = lb.entries.len();
    lb.entries.retain(|e| e.draft_id != Some(draft_id));
    if lb.entries.len() != n {
        save(&lb);
    }
}

/// Record (or update) that a draft was published. If an entry already tracks the
/// draft (a composed bundle), mark it pushed; otherwise create one so the sent
/// work leaves the Stream and shows on the Logged screen.
#[allow(clippy::too_many_arguments)]
pub fn mark_published(
    draft_id: i64,
    kind: &str,
    title: &str,
    ticket: &str,
    episode_ids: Vec<i64>,
    secs: f64,
) {
    let mut lb = load();
    if let Some(e) = lb.entries.iter_mut().find(|e| e.draft_id == Some(draft_id)) {
        e.pushed_jira = true;
        e.updated_ts = now_ts();
        save(&lb);
        return;
    }
    drop(lb);
    let lkind = match kind {
        "jira_worklog" => "worklog",
        "confluence_handoff" => "handoff",
        _ => "sent",
    };
    add(lkind, title, ticket, "", "", episode_ids, Some(draft_id), secs, true);
}
