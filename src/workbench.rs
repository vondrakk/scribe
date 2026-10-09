//! The engineer's workbench: a scratchpad and an async command/Ansible runner.
//!
//! Commands run in a background thread (the panel server is single-threaded, so
//! a long Ansible run must not block it) and their output is polled. True to the
//! "work in the pane → captured" idea, every run is also recorded as a shell
//! event, so it flows into the timeline and episodes like any other activity.
//!
//! The panel binds to loopback only, so the runner is reachable just from this
//! machine, by this user — the same trust boundary as the engineer's own shell.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::capture;
use crate::config;
use crate::model::now_ts;

const OUTPUT_CAP: usize = 200_000;

#[derive(Clone)]
pub struct Run {
    pub cmd: String,
    pub cwd: String,
    pub output: String,
    pub exit: Option<i64>,
    pub done: bool,
    pub started: f64,
}

fn registry() -> &'static Mutex<HashMap<String, Run>> {
    static R: OnceLock<Mutex<HashMap<String, Run>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cap(mut s: String) -> String {
    if s.len() > OUTPUT_CAP {
        s.truncate(OUTPUT_CAP);
        s.push_str("\n… (output truncated)");
    }
    s
}

/// Start a command running in the background. Returns a run id to poll.
pub fn start_run(cmd: &str, cwd: &str) -> String {
    let cwd = if cwd.trim().is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    } else {
        cwd.to_string()
    };
    let id = format!("{:x}", (now_ts() * 1_000_000.0) as u64 ^ std::process::id() as u64);

    if let Ok(mut m) = registry().lock() {
        m.insert(
            id.clone(),
            Run {
                cmd: cmd.to_string(),
                cwd: cwd.clone(),
                output: String::new(),
                exit: None,
                done: false,
                started: now_ts(),
            },
        );
    }

    let cmd_s = cmd.to_string();
    let id_s = id.clone();
    std::thread::spawn(move || {
        let out = Command::new("bash")
            .arg("-lc")
            .arg(&cmd_s)
            .current_dir(&cwd)
            .output();
        let (text, exit) = match out {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                let err = String::from_utf8_lossy(&o.stderr);
                if !err.is_empty() {
                    if !s.is_empty() && !s.ends_with('\n') {
                        s.push('\n');
                    }
                    s.push_str(&err);
                }
                (cap(s), o.status.code().map(|c| c as i64).or(Some(-1)))
            }
            Err(e) => (format!("failed to run: {e}"), Some(-1)),
        };

        if let Ok(mut m) = registry().lock() {
            if let Some(r) = m.get_mut(&id_s) {
                r.output = text;
                r.exit = exit;
                r.done = true;
            }
        }

        // Capture the run as a shell event (same path as the shell snippet), so
        // work done in the pane shows up in the timeline and episodes.
        let (repo, branch) = capture::repo_branch(&cwd);
        capture::capture_shell(
            &cmd_s,
            exit.unwrap_or(0),
            &cwd,
            repo.as_deref().unwrap_or(""),
            branch.as_deref().unwrap_or(""),
        );
    });

    id
}

pub fn get_run(id: &str) -> Option<Run> {
    registry().lock().ok().and_then(|m| m.get(id).cloned())
}

// --- scratchpad ------------------------------------------------------------

fn scratchpad_path() -> std::path::PathBuf {
    config::home().join("scratchpad.md")
}

pub fn read_scratchpad() -> String {
    std::fs::read_to_string(scratchpad_path()).unwrap_or_default()
}

pub fn write_scratchpad(text: &str) {
    let _ = std::fs::write(scratchpad_path(), text);
}
