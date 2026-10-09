//! Configuration and paths.
//!
//! Runtime data lives under SCRIBE_HOME (default ~/.scribe). Everything the
//! agent captures stays inside this directory; it leaves only through the
//! explicit mock-publish step.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

// `#[serde(default)]` lets an older config.json (missing the newer llm_* fields)
// still deserialize — missing fields fall back to Default — instead of erroring.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Sessionization: a gap larger than this (seconds) ends an episode.
    pub episode_gap_seconds: f64,
    /// Raw capture auto-expiry (days). Trust guarantee §7.1.
    pub retention_days: i64,
    /// Branch/commit key pattern, e.g. OPS-1234.
    pub ticket_key_pattern: String,
    pub panel_host: String,
    pub panel_port: u16,
    /// Pluggable LLM backend: "stub" (deterministic, offline, default),
    /// "anthropic" (Claude Messages API), or "openai" (OpenAI-compatible,
    /// incl. a local server). Non-stub backends fall back to the stub on any
    /// missing-credential / network / API error, so the pipeline never breaks.
    pub llm_backend: String,
    /// Model id for the LLM backend. Default is Haiku 4.5 — summaries are a
    /// cheap, high-volume, final-only task (PRD §8 cost-aware intent). Set to
    /// claude-sonnet-5-5 for more judgment, or claude-opus-5-5 for the best.
    pub llm_model: String,
    /// Base URL for the "openai" backend (e.g. http://localhost:11434/v1 for a
    /// local OpenAI-compatible server). Ignored by the "anthropic" backend.
    pub llm_base_url: String,
    /// Max output tokens for a summary (short by design).
    pub llm_max_tokens: u32,

    /// Publishing backend: "mock" (default, writes published/*.json) or
    /// "atlassian" (real Jira/Confluence via per-user OAuth 2.0 3LO).
    pub publish_backend: String,
    /// OAuth 2.0 (3LO) client id from your Atlassian developer console app.
    /// The client secret is read from the ATLASSIAN_CLIENT_SECRET env var.
    pub atlassian_client_id: String,
    /// Space-separated OAuth scopes requested at `scribe auth`. Defaults to the
    /// classic scopes; adjust if your app uses granular scopes.
    pub atlassian_scopes: String,
    /// Loopback port for the OAuth redirect. The app's callback URL must be
    /// http://127.0.0.1:<port>/callback.
    pub atlassian_redirect_port: u16,
    /// Jira project key new (interrupt/unplanned) issues are created in.
    pub jira_project_key: String,
    /// Jira issue type for created issues.
    pub jira_issue_type: String,
    /// Confluence space id handoff pages are created in (numeric id, v2 API).
    pub confluence_space_id: String,
    /// Optional path to a conf file holding an Atlassian token, used instead of
    /// the OAuth flow. Supports JSON or key=value with: site=https://you.atlassian.net
    /// and either (email= + token=) for an API token, or bearer= for an access token.
    pub atlassian_conf_file: String,
    /// GitLab base URL for the inbound MR connector (e.g. https://gitlab.yourco.com).
    /// The token is read from the GITLAB_TOKEN env var.
    pub gitlab_base_url: String,
    /// JQL for the inbound Jira connector (issues that flow to you).
    pub jira_jql: String,
    /// Background inbox auto-refresh interval (seconds). 0 disables it; the panel
    /// then only refreshes on the manual button / `scribe pull`. Default 300 (5m).
    pub inbox_refresh_seconds: i64,
    /// Fire a local desktop notification on a hard seam the engineer would miss
    /// while heads-down — e.g. a branch switch onto an unkeyed branch that needs
    /// binding (PRD §5). Linux uses `notify-send`, macOS uses `osascript`. The
    /// `SCRIBE_NO_NOTIFY` env var overrides this to off. Default true.
    pub desktop_notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            episode_gap_seconds: 15.0 * 60.0,
            retention_days: 14,
            ticket_key_pattern: r"[A-Z][A-Z0-9]+-\d+".to_string(),
            panel_host: "127.0.0.1".to_string(),
            panel_port: 8787,
            llm_backend: "stub".to_string(),
            llm_model: "claude-haiku-4-5".to_string(),
            llm_base_url: String::new(),
            llm_max_tokens: 512,
            publish_backend: "mock".to_string(),
            atlassian_client_id: String::new(),
            atlassian_scopes:
                "offline_access read:me read:jira-work write:jira-work \
                 read:confluence-content.all write:confluence-content"
                    .to_string(),
            atlassian_redirect_port: 8799,
            jira_project_key: String::new(),
            jira_issue_type: "Task".to_string(),
            confluence_space_id: String::new(),
            atlassian_conf_file: String::new(), // wired: resolve_creds reads it (Basic/bearer)
            gitlab_base_url: String::new(),
            jira_jql: "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
                .to_string(),
            inbox_refresh_seconds: 300,
            desktop_notifications: true,
        }
    }
}

/// The logbook: handled-work bundles (combined / logged / disposed / sent). The
/// Stream hides episodes recorded here; the Logged screen shows them.
pub fn logbook_file() -> PathBuf {
    home().join("logbook.json")
}

/// Manual per-episode time overrides (episode id → seconds).
pub fn time_overrides_file() -> PathBuf {
    home().join("time_overrides.json")
}

/// Where the Atlassian OAuth token store lives.
pub fn atlassian_token_file() -> PathBuf {
    home().join("atlassian.json")
}

pub fn home() -> PathBuf {
    let p = match std::env::var("SCRIBE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let h = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(h).join(".scribe")
        }
    };
    let _ = fs::create_dir_all(&p);
    p
}

/// Append-only raw event log (JSONL). Shell + git hooks write here.
pub fn events_log() -> PathBuf {
    home().join("events.jsonl")
}

pub fn episodes_file() -> PathBuf {
    home().join("episodes.json")
}

pub fn drafts_file() -> PathBuf {
    home().join("drafts.json")
}

pub fn state_file() -> PathBuf {
    home().join("state.json")
}

/// Plain-text current binding, read by the fast prompt-line segment.
pub fn binding_file() -> PathBuf {
    home().join("binding")
}

pub fn published_dir() -> PathBuf {
    let p = home().join("published");
    let _ = fs::create_dir_all(&p);
    p
}

/// AI prompts are recorded here, one JSONL file per session (indexed by
/// session): prompts/<session>.jsonl.
pub fn prompts_dir() -> PathBuf {
    let p = home().join("prompts");
    let _ = fs::create_dir_all(&p);
    p
}

pub fn config_file() -> PathBuf {
    home().join("config.json")
}

pub fn load() -> Config {
    if let Ok(s) = fs::read_to_string(config_file()) {
        if let Ok(c) = serde_json::from_str::<Config>(&s) {
            return c;
        }
    }
    Config::default()
}

pub fn save(c: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(c) {
        let _ = fs::write(config_file(), s);
    }
}
