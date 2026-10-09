//! Processing pipeline: sessionize -> classify -> summarize -> emit drafts.
//!
//! Deterministic, offline stub throughout (LLM backend = "stub"). `summarize`
//! is the pluggable seam where an Anthropic / OpenAI-compatible backend drops
//! in. `process` rebuilds episodes/drafts from the event log idempotently.

use regex::Regex;

use crate::binding;
use crate::config;
use crate::drafts;
use crate::model::{self, Episode, Event};

// --- classification rules --------------------------------------------------

const ACTIVITY_RULES: &[(&str, &[&str])] = &[
    ("deploy", &["kubectl", "helm", "docker", "podman", "terraform", "ansible",
                 "aws ", "gcloud", "ssh ", "systemctl", "deploy"]),
    ("debug", &["pytest", "test", "gdb", "strace", "tail -f", "journalctl",
                "logs", "dmesg", "curl localhost"]),
    ("review", &["gh pr", "git diff", "git log", "git show", "review"]),
    ("investigate", &["grep", "rg ", "find ", "cat ", "less ", "man ", "which",
                      "ps ", "top", "htop", "df ", "du "]),
    ("develop", &["vim", "nvim", "code ", "git commit", "git add", "make",
                  "npm", "pip", "python", "go build", "cargo"]),
];

pub fn classify(events: &[Event]) -> String {
    use std::collections::HashMap;
    let mut score: HashMap<&str, i32> = HashMap::new();
    for ev in events {
        let text = format!(
            "{} {}",
            ev.cmd.clone().unwrap_or_default(),
            ev.action.clone().unwrap_or_default()
        )
        .to_lowercase();
        for (activity, needles) in ACTIVITY_RULES {
            if needles.iter().any(|n| text.contains(n)) {
                *score.entry(activity).or_insert(0) += 1;
            }
        }
    }
    score
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| "develop".to_string())
}

// --- sessionization + seam detection --------------------------------------

fn is_seam(prev: &Event, cur: &Event, gap_limit: f64) -> bool {
    if cur.kind == "seam" {
        return true;
    }
    if cur.ts - prev.ts > gap_limit {
        return true;
    }
    // Repo change is a seam only when both events carry a repo.
    if let (Some(pr), Some(qr)) = (&cur.repo, &prev.repo) {
        if pr != qr {
            return true;
        }
    }
    let pk = binding::extract_key(cur.branch.as_deref());
    let qk = binding::extract_key(prev.branch.as_deref());
    if let (Some(a), Some(b)) = (pk, qk) {
        if a != b {
            return true;
        }
    }
    false
}

fn most_common_cwd(events: &[Event]) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in events {
        if let Some(c) = e.cwd.as_deref() {
            if !c.is_empty() {
                *counts.entry(c).or_insert(0) += 1;
            }
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c.to_string())
}

pub fn sessionize(events: &[Event], gap_limit: f64) -> Vec<Vec<Event>> {
    let mut groups: Vec<Vec<Event>> = Vec::new();
    let mut cur: Vec<Event> = Vec::new();
    for ev in events {
        if let Some(last) = cur.last() {
            if is_seam(last, ev, gap_limit) {
                groups.push(std::mem::take(&mut cur));
            }
        }
        cur.push(ev.clone());
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

fn episode_ticket(events: &[Event]) -> Option<String> {
    for ev in events {
        if let Some(k) = &ev.key {
            return Some(k.clone());
        }
    }
    for ev in events {
        if let Some(k) = binding::extract_key(ev.branch.as_deref()) {
            return Some(k);
        }
    }
    None
}

fn episode_kind(events: &[Event], ticket: &Option<String>) -> String {
    if events.iter().any(|e| e.kind == "seam" && e.action.as_deref() == Some("interrupt")) {
        return "interrupt".to_string();
    }
    if ticket.is_none() {
        return "unaccounted".to_string();
    }
    "planned".to_string()
}

// --- summarization (pluggable backend) -------------------------------------

fn control_re() -> Regex {
    Regex::new(r"^\s*(w(\s|$|!|\?)|source\b.*scribe|_scribe)").unwrap()
}
fn infra_re() -> Regex {
    Regex::new(r"\b(kubectl|helm|ssh|scp|aws|gcloud|az|terraform|docker|podman)\b").unwrap()
}

/// Deterministic rule-based narrative — always computed, and used as the
/// fallback whenever a real LLM backend is unreachable (see `llm::summarize`).
pub fn stub_summary(events: &[Event], activity: &str, ticket: &Option<String>) -> String {
    let cre = control_re();
    let ire = infra_re();
    let is_control = |c: &str| cre.is_match(c);

    let cmds: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "shell")
        .filter_map(|e| e.cmd.clone())
        .filter(|c| !is_control(c))
        .collect();
    let fails = events
        .iter()
        .filter(|e| e.kind == "shell" && e.exit.unwrap_or(0) != 0)
        .filter(|e| !e.cmd.as_deref().map(|c| is_control(c)).unwrap_or(false))
        .count();
    let mut repos: Vec<String> = events.iter().filter_map(|e| e.repo.clone()).collect();
    repos.sort();
    repos.dedup();
    let commits: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == "git" && e.action.as_deref() == Some("commit"))
        .collect();
    let mut infra: Vec<String> = cmds
        .iter()
        .filter_map(|c| ire.find(c).map(|m| m.as_str().to_string()))
        .collect();
    infra.sort();
    infra.dedup();

    let anchor = ticket.clone().unwrap_or_else(|| "unbound work".to_string());
    let mut parts: Vec<String> = Vec::new();
    let mut act = activity.to_string();
    if let Some(first) = act.get_mut(0..1) {
        let upper = first.to_uppercase();
        act = format!("{}{}", upper, &activity[1..]);
    }
    parts.push(format!("{} episode on {}.", act, anchor));
    if !repos.is_empty() {
        parts.push(format!("Repos: {}.", repos.join(", ")));
    }
    parts.push(format!("{} commands run.", cmds.len()));
    if !commits.is_empty() {
        let msgs: Vec<String> = commits
            .iter()
            .map(|c| c.msg.clone().unwrap_or_default().lines().next().unwrap_or("").to_string())
            .collect();
        parts.push(format!("{} commit(s): {}.", commits.len(), msgs.join("; ")));
    }
    if fails > 0 {
        parts.push(format!("{} command(s) exited non-zero (likely debugging).", fails));
    }
    let ai = events.iter().filter(|e| e.kind == "ai").count();
    if ai > 0 {
        parts.push(format!("{} AI prompt(s).", ai));
    }
    let head: Vec<String> = cmds.iter().take(6).cloned().collect();
    if !head.is_empty() {
        parts.push(format!("Notable: {}.", head.join(" | ")));
    }
    if !infra.is_empty() {
        parts.push(format!("Infra/remote tools: {}.", infra.join(", ")));
    }
    parts.join(" ")
}

// --- top-level ------------------------------------------------------------

pub struct ProcessResult {
    pub episodes: usize,
    pub drafts: usize,
}

pub fn process() -> ProcessResult {
    let cfg = config::load();
    let all = model::read_events();
    // `bind` events are pure metadata; they inform the active ticket but are
    // not activity and must not seed/fragment episodes. `seam` events stay —
    // they mark interrupt boundaries.
    let events: Vec<Event> = all.into_iter().filter(|e| e.kind != "bind").collect();

    let groups = sessionize(&events, cfg.episode_gap_seconds);
    let mut episodes: Vec<Episode> = Vec::new();
    let mut ep_id: i64 = 0;
    let mut built: Vec<(Episode, Vec<Event>)> = Vec::new();
    for grp in groups {
        if grp.is_empty() {
            continue;
        }
        let ticket = episode_ticket(&grp);
        let activity = classify(&grp);
        let kind = episode_kind(&grp, &ticket);
        // Deterministic summary is always computed; the configured LLM backend
        // may replace it, falling back to this on any error.
        let stub = stub_summary(&grp, &activity, &ticket);
        let summary = crate::llm::summarize(&grp, &activity, &ticket, &stub);
        let mut repos: Vec<String> = grp.iter().filter_map(|e| e.repo.clone()).collect();
        repos.sort();
        repos.dedup();
        let mut branches: Vec<String> = grp.iter().filter_map(|e| e.branch.clone()).collect();
        branches.sort();
        branches.dedup();
        // The actual commands (what was being done), the commits, and the most
        // representative working directory for this episode.
        let cre = control_re();
        let commands: Vec<String> = grp
            .iter()
            .filter(|e| e.kind == "shell")
            .filter_map(|e| e.cmd.clone())
            .filter(|c| !cre.is_match(c))
            .take(60)
            .collect();
        let commits: Vec<String> = grp
            .iter()
            .filter(|e| e.kind == "git" && e.action.as_deref() == Some("commit"))
            .filter_map(|e| e.msg.clone())
            .map(|m| m.lines().next().unwrap_or("").to_string())
            .collect();
        let cwd = most_common_cwd(&grp);
        ep_id += 1;
        let ep = Episode {
            id: ep_id,
            start_ts: grp.first().map(|e| e.ts).unwrap_or(0.0),
            end_ts: grp.last().map(|e| e.ts).unwrap_or(0.0),
            repo: repos.into_iter().next(),
            branch: branches.into_iter().next(),
            ticket,
            activity,
            kind,
            summary,
            n_events: grp.len() as i64,
            cwd,
            commands,
            commits,
        };
        episodes.push(ep.clone());
        built.push((ep, grp));
    }
    model::save_episodes(&episodes);

    let n_drafts = drafts::rebuild(&built, &episodes);
    ProcessResult {
        episodes: episodes.len(),
        drafts: n_drafts,
    }
}
