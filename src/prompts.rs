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
use crate::config;
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

fn session_file(session: &str) -> std::path::PathBuf {
    config::prompts_dir().join(format!("{}.jsonl", sanitize(session)))
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

    if let Ok(mut line) = serde_json::to_string(&entry) {
        line.push('\n');
        use std::io::Write;
        if let Ok(mut fh) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_file(session))
        {
            let _ = fh.write_all(line.as_bytes());
        }
    }

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

fn read_file(path: &std::path::Path) -> Vec<Prompt> {
    let mut out = Vec::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(p) = serde_json::from_str::<Prompt>(line) {
                out.push(p);
            }
        }
    }
    out
}

/// All sessions with their prompts, newest session first.
pub fn all_sessions() -> Vec<SessionPrompts> {
    let mut sessions: Vec<SessionPrompts> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(config::prompts_dir()) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let entries = read_file(&path);
            if entries.is_empty() {
                continue;
            }
            let session = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let last = entries.iter().map(|p| p.ts).fold(0.0, f64::max);
            sessions.push(SessionPrompts {
                session,
                count: entries.len(),
                last,
                entries,
            });
        }
    }
    sessions.sort_by(|a, b| b.last.partial_cmp(&a.last).unwrap_or(std::cmp::Ordering::Equal));
    sessions
}

pub fn total_count() -> usize {
    all_sessions().iter().map(|s| s.count).sum()
}
