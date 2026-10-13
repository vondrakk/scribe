//! AI prompt capture, recorded to a file indexed by session.
//!
//! Each prompt the engineer sends to an AI tool is appended to
//! `$SCRIBE_HOME/prompts/<session>.jsonl` — one file per session, so prompts
//! are grouped (indexed) by the session they belong to. The full prompt text
//! stays in that per-session file; only a short preview is mirrored into the
//! main event log, so the work timeline reflects AI usage without duplicating
//! (potentially long or sensitive) prompt text.

use serde::{Deserialize, Serialize};

use crate::binding;
use crate::capture;
use crate::model::{self, now_ts, Event};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Prompt {
    pub ts: f64,
    pub session: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ticket: Option<String>,
}

/// Keep session ids filesystem-safe (they become a file name).
fn sanitize(session: &str) -> String {
    let s: String = session
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "default".to_string() } else { s }
}

fn preview(text: &str, n: usize) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

/// Record one AI prompt: append to the per-session file, and drop a compact
/// marker into the main event log so it appears on the work timeline.
pub fn record(session: &str, tool: &str, model: Option<&str>, prompt: &str) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok();
    let (repo, branch) = cwd
        .as_deref()
        .map(capture::repo_branch)
        .unwrap_or((None, None));
    let ticket = binding::get_binding().map(|b| b.ticket).or_else(|| {
        binding::extract_key(branch.as_deref())
    });

    let entry = Prompt {
        ts: now_ts(),
        session: sanitize(session),
        tool: tool.to_string(),
        model: model.map(|s| s.to_string()),
        prompt: prompt.to_string(),
        cwd: cwd.clone(),
        repo: repo.clone(),
        branch: branch.clone(),
        ticket: ticket.clone(),
    };

    crate::db::append(crate::db::PROMPTS, entry.ts, &entry.session, &entry);

    // Compact timeline marker (no full prompt text).
    let label = match model {
        Some(m) => format!("{}/{}", tool, m),
        None => tool.to_string(),
    };
    model::append_event(Event {
        kind: "ai".into(),
        action: Some("prompt".into()),
        cwd,
        repo,
        branch,
        key: ticket,
        msg: Some(format!("{}: {}", label, preview(prompt, 80))),
        ..Default::default()
    });
}

#[derive(Serialize)]
pub struct SessionPrompts {
    pub session: String,
    pub count: usize,
    pub last: f64,
    pub entries: Vec<Prompt>,
}

/// All sessions with their prompts, newest session first.
pub fn all_sessions() -> Vec<SessionPrompts> {
    let mut by_session: std::collections::BTreeMap<String, Vec<Prompt>> = std::collections::BTreeMap::new();
    for p in crate::db::all::<Prompt>(crate::db::PROMPTS) {
        by_session.entry(p.session.clone()).or_default().push(p);
    }
    let mut sessions: Vec<SessionPrompts> = by_session
        .into_iter()
        .map(|(session, entries)| SessionPrompts {
            session,
            count: entries.len(),
            last: entries.iter().map(|p| p.ts).fold(0.0, f64::max),
            entries,
        })
        .collect();
    sessions.sort_by(|a, b| b.last.partial_cmp(&a.last).unwrap_or(std::cmp::Ordering::Equal));
    sessions
}

pub fn total_count() -> usize {
    crate::db::count(crate::db::PROMPTS)
}
