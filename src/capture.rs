//! Capture helpers: git repo/branch detection, structured event recording from
//! git hooks, and the retention sweep (trust guarantee §7.1).
//!
//! With the file-backed store there is no separate "ingest" step — the pipeline
//! and panel read events.jsonl directly. `sweep` prunes aged raw capture.

use std::process::Command;

use crate::binding;
use crate::config;
use crate::model::{self, Event};

fn git(args: &[&str], cwd: &str) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn repo_branch(cwd: &str) -> (Option<String>, Option<String>) {
    let repo = git(&["rev-parse", "--show-toplevel"], cwd)
        .map(|root| std::path::Path::new(&root).file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(root));
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd);
    (repo, branch)
}

fn hostname() -> String {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        Command::new("hostname")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "localhost".to_string())
    })
}

/// Record a shell command (called by the PROMPT_COMMAND hot path).
pub fn capture_shell(cmd: &str, exit: i64, cwd: &str, repo: &str, branch: &str) {
    let repo = if repo.is_empty() { None } else { Some(repo.to_string()) };
    let branch = if branch.is_empty() { None } else { Some(branch.to_string()) };
    let key = binding::extract_key(branch.as_deref());
    model::append_event(Event {
        kind: "shell".into(),
        cmd: Some(cmd.to_string()),
        exit: Some(exit),
        cwd: Some(cwd.to_string()),
        repo,
        branch,
        host: Some(hostname()),
        key,
        ..Default::default()
    });
}

/// Record a structured git event (called by git hooks).
pub fn capture_event(etype: &str, action: &str, message: Option<&str>, sha: Option<&str>) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let (repo, branch) = repo_branch(&cwd);
    let key = binding::extract_key(branch.as_deref())
        .or_else(|| binding::extract_key(message));
    model::append_event(Event {
        kind: etype.into(),
        action: Some(action.into()),
        cwd: Some(cwd),
        repo: repo.clone(),
        branch: branch.clone(),
        host: Some(hostname()),
        key: key.clone(),
        msg: message.map(|s| s.to_string()),
        sha: sha.map(|s| s.to_string()),
        ..Default::default()
    });

    // post-checkout onto a keyed branch binds silently; unkeyed branch -> hint.
    if action == "checkout" {
        if let Some(k) = &key {
            binding::set_binding(Some(k), "branch", &format!("branch={}", branch.unwrap_or_default()));
            eprintln!("[scribe] bound to {}", k);
        } else {
            let br = branch.clone().unwrap_or_default();
            let cands: Vec<String> = binding::candidate_tickets(5).into_iter().map(|c| c.0).collect();
            let hint = if cands.is_empty() { "none seen yet".into() } else { cands.join(", ") };
            eprintln!(
                "[scribe] unkeyed branch '{}'. Bind with: w <TICKET>   (recent: {})",
                br, hint
            );
            // Hard seam: a context switch onto an unkeyed branch needs the
            // engineer to confirm a ticket. Surface it as a desktop toast so it
            // isn't missed while heads-down (PRD §5).
            crate::notify::desktop(
                &format!("Unbound branch '{}'", br),
                &format!("Bind a ticket:  w <TICKET>   (recent: {})", hint),
            );
        }
    } else if action == "commit" {
        if let Some(k) = &key {
            binding::set_binding(Some(k), "commit", "commit key");
        }
    }
}

/// Count raw events (cheap status helper).
pub fn count_events() -> usize {
    model::read_events().len()
}

/// Drop raw events older than the retention window.
pub fn sweep_retention() -> usize {
    let days = config::load().retention_days as f64;
    let cutoff = model::now_ts() - days * 86400.0;
    crate::context::sweep(cutoff);
    crate::db::delete_before(crate::db::PROMPTS, cutoff);
    crate::db::delete_before(crate::db::EVENTS, cutoff)
}
