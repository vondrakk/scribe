//! Tag rules: user-defined {path fragment → tag}. A rule tags every work item
//! whose working directory (or repo) matches it, so related work auto-groups
//! over time — the "system learns from the user's rules" idea. The user tags
//! one item by its directory; every future item in that directory inherits the
//! tag. Stored in $SCRIBE_HOME/rules.json.

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Serialize, Deserialize, Clone)]
pub struct Rule {
    /// A path/repo fragment to match against a work item's cwd + repo.
    pub prefix: String,
    pub tag: String,
}

fn path() -> std::path::PathBuf {
    config::home().join("rules.json")
}

pub fn load() -> Vec<Rule> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(rules: &[Rule]) {
    if let Ok(s) = serde_json::to_string_pretty(rules) {
        let _ = std::fs::write(path(), s);
    }
}

pub fn add(prefix: &str, tag: &str) {
    let prefix = prefix.trim();
    let tag = tag.trim();
    if prefix.is_empty() || tag.is_empty() {
        return;
    }
    let mut rules = load();
    if !rules.iter().any(|r| r.prefix == prefix && r.tag == tag) {
        rules.push(Rule { prefix: prefix.to_string(), tag: tag.to_string() });
        save(&rules);
    }
}

pub fn remove(prefix: &str, tag: &str) {
    let mut rules = load();
    rules.retain(|r| !(r.prefix == prefix && r.tag == tag));
    save(&rules);
}

/// Tags whose rule matches the given cwd or repo.
pub fn tags_for(cwd: Option<&str>, repo: Option<&str>) -> Vec<String> {
    let hay = format!("{} {}", cwd.unwrap_or(""), repo.unwrap_or(""));
    let mut out: Vec<String> = Vec::new();
    for r in load() {
        if !r.prefix.is_empty() && hay.contains(&r.prefix) && !out.contains(&r.tag) {
            out.push(r.tag);
        }
    }
    out
}
