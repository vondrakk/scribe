//! The Markdown journal: the logbook, written out as files a person can read,
//! search and keep.
//!
//! The database is the working copy; these files are its readable record and
//! are rewritten from it whenever the logbook changes. Layout, under
//! $SCRIBE_HOME/journal:
//!
//!   days/2026/10/2026-10-13.md   everything logged or sent that day
//!   tickets/OPS-1234.md          the same entries, per ticket, across days
//!
//! Raw capture expires (retention); the journal does not. Skipped work is not
//! written. Files Scribe did not write are never touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};

use crate::logbook::{self, LogEntry};
use crate::model::DaySecs;
use crate::{config, model, stream};

/// First line of every file Scribe writes here — how it knows its own.
const MARK: &str = "<!-- Written by Scribe from its database. Edits here are overwritten. -->";

pub fn dir() -> PathBuf {
    config::home().join("journal")
}

fn when(ts: f64, fmt: &str) -> String {
    Local.timestamp_opt(ts as i64, 0).single().map(|t| t.format(fmt).to_string()).unwrap_or_default()
}

/// Jira wiki markup → Markdown, for the handful of forms Scribe writes.
fn to_markdown(body: &str) -> String {
    let link = regex::Regex::new(r"\[([^|\]]+)\|([^\]]+)\]").unwrap();
    let mono = regex::Regex::new(r"\{\{([^}]+)\}\}").unwrap();
    let mut out: Vec<String> = Vec::new();
    for l in body.lines() {
        let l = l.trim_end();
        if l.starts_with("Captured by Scribe") {
            continue;
        }
        let l = mono.replace_all(&link.replace_all(l, "[$1]($2)"), "`$1`").to_string();
        if let Some(h) = ["h1. ", "h2. ", "h3. "].iter().find_map(|p| l.strip_prefix(p)) {
            out.push(format!("**{h}**"));
            out.push(String::new());
        } else if let Some(b) = l.strip_prefix("* ") {
            out.push(format!("- {b}"));
        } else if !(l.is_empty() && out.last().is_some_and(|p| p.is_empty())) {
            out.push(l);
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn kind_label(e: &LogEntry) -> &'static str {
    match e.kind.as_str() {
        "worklog" => "Worklog",
        "handoff" => "Handoff note",
        "logged" if e.silent => "Logged silently",
        "logged" => "Logged",
        _ => "Update",
    }
}

fn went(e: &LogEntry) -> &'static str {
    match (e.kind.as_str(), e.pushed_jira) {
        ("handoff", true) => "Sent to Confluence",
        (_, true) => "Sent to Jira",
        ("logged", false) => "Kept locally — not posted",
        _ => "Not sent yet",
    }
}

/// How long the entry took, and how that falls across days. What was sent
/// wins over what was measured, since the engineer may have corrected it.
fn timing(e: &LogEntry) -> (f64, Vec<DaySecs>) {
    let live = stream::secs_for_ids(&e.episode_ids);
    let sent = e.pushed_jira && e.auto_ids.is_empty() && e.secs > 0.0;
    let total = if sent || live <= 0.0 { e.secs } else { live };
    let measured: f64 = e.by_day.iter().map(|d| d.secs).sum();
    let days = e
        .by_day
        .iter()
        .map(|d| DaySecs { day: d.day.clone(), secs: if measured > 0.0 { d.secs * total / measured } else { d.secs } })
        .collect();
    (total, days)
}

/// One entry: on a day's page (`page_day`), or on its ticket's page.
fn render(e: &LogEntry, body: &str, page_day: Option<&str>) -> String {
    let (total, days) = timing(e);
    let logged = when(e.created_ts, "%Y-%m-%d");
    let (stamp, secs) = match page_day {
        // Work written up on a later day is still logged under the day it was done.
        Some(day) => (
            if day == logged { when(e.created_ts, "%H:%M") } else { format!("logged {logged}") },
            days.iter().find(|d| d.day == day).map(|d| d.secs).unwrap_or(total),
        ),
        None => (when(e.created_ts, "%Y-%m-%d %H:%M"), total),
    };
    let mut head = vec![stamp];
    if page_day.is_some() && !e.ticket.is_empty() {
        head.push(e.ticket.clone());
    }
    head.push(if e.title.is_empty() { kind_label(e).to_string() } else { e.title.clone() });
    let mut facts = vec![kind_label(e).to_string(), went(e).to_string()];
    if secs >= 60.0 {
        facts.insert(1, stream::fmt_dur(secs));
    }
    if page_day.is_none() && days.len() > 1 {
        facts.push(days.iter().map(|d| format!("{} {}", d.day, stream::fmt_dur(d.secs))).collect::<Vec<_>>().join(", "));
    }
    if !e.project.is_empty() {
        facts.push(format!("`{}`", e.project));
    }
    let mut s = format!("## {}\n\n_{}_\n", head.join(" · "), facts.join(" · "));
    let md = to_markdown(body);
    if !md.is_empty() {
        s.push('\n');
        s.push_str(&md);
        s.push('\n');
    }
    s
}

fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Every journal file as it should be now: path → contents.
fn pages() -> BTreeMap<PathBuf, String> {
    let drafts = model::load_drafts();
    let mut entries: Vec<LogEntry> = logbook::load().entries.into_iter().filter(|e| e.kind != "disposed").collect();
    entries.sort_by(|a, b| a.created_ts.total_cmp(&b.created_ts));
    let root = dir();
    let mut days: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tickets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &entries {
        let mut body = e.text();
        if body.is_empty() {
            body = e.draft_id.and_then(|id| drafts.iter().find(|d| d.id == id)).map(|d| d.body.clone()).unwrap_or_default();
        }
        if !e.ticket.is_empty() {
            tickets.entry(e.ticket.clone()).or_default().push(render(e, &body, None));
        }
        // An event that ran over several days is logged under each of them
        // with that day's time; the write-up sits on the last.
        if e.by_day.len() < 2 {
            let day = e.by_day.first().map(|d| d.day.clone()).unwrap_or_else(|| when(e.created_ts, "%Y-%m-%d"));
            days.entry(day.clone()).or_default().push(render(e, &body, Some(&day)));
            continue;
        }
        let last = &e.by_day[e.by_day.len() - 1].day;
        for d in &e.by_day {
            let text = if &d.day == last { body.clone() } else { format!("Continued — written up on {last}.") };
            days.entry(d.day.clone()).or_default().push(render(e, &text, Some(&d.day)));
        }
    }
    let mut out = BTreeMap::new();
    for (day, parts) in days {
        if day.len() < 10 {
            continue;
        }
        let title = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")
            .ok()
            .map(|d| d.format("%A, %-d %B %Y").to_string())
            .unwrap_or_else(|| day.clone());
        let path = root.join("days").join(&day[0..4]).join(&day[5..7]).join(format!("{day}.md"));
        out.insert(path, format!("{MARK}\n# {title}\n\n{}", parts.join("\n")));
    }
    for (ticket, parts) in tickets {
        let path = root.join("tickets").join(format!("{}.md", safe(&ticket)));
        out.insert(path, format!("{MARK}\n# {ticket}\n\n{}", parts.join("\n")));
    }
    out
}

fn ours(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.starts_with(MARK)).unwrap_or(false)
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    for ent in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk(&p, found);
        } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
            found.push(p);
        }
    }
}

/// Make the files match the logbook: write what changed, and remove pages of
/// Scribe's own whose entries are gone. Returns how many files were written.
pub fn sync() -> usize {
    let want = pages();
    let mut written = 0;
    for (path, text) in &want {
        if std::fs::read_to_string(path).map(|old| &old == text).unwrap_or(false) {
            continue;
        }
        // Never write over a file someone else put at this path.
        if path.exists() && !ours(path) {
            continue;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("md.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok() {
            written += 1;
        }
    }
    let mut existing = Vec::new();
    walk(&dir(), &mut existing);
    for p in existing {
        if !want.contains_key(&p) && ours(&p) {
            let _ = std::fs::remove_file(&p);
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_markup_becomes_markdown() {
        let md = to_markdown("h3. What was done\n* Ran {{df -h}} on [db|https://x.example]\n\nCaptured by Scribe and reviewed before posting.");
        assert_eq!(md, "**What was done**\n\n- Ran `df -h` on [db](https://x.example)");
    }
}
