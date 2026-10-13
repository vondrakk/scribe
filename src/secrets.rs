//! Access tokens entered in the panel's Settings.
//!
//! Stored in $SCRIBE_HOME/secrets.json, readable by the owner only (0600), and
//! kept apart from config.json so the config stays safe to show or share. A
//! token is write-only from the panel's side: the API reports whether one is
//! set, never its value. A stored token is tied to the address it was entered
//! for — changing the address drops it, so it can't be redirected to another
//! host without being typed again.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::config;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct GitlabConn {
    /// Label shown in the panel (e.g. "work", "gitlab.com").
    pub name: String,
    pub base_url: String,
    /// Personal access token with `read_api`.
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AtlassianConn {
    /// https://yourco.atlassian.net
    pub site: String,
    pub email: String,
    /// API token from id.atlassian.com → Security → API tokens.
    pub token: String,
}

impl AtlassianConn {
    pub fn is_set(&self) -> bool {
        !self.site.is_empty() && !self.email.is_empty() && !self.token.is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct PagerdutyConn {
    /// Sent as the `From` header PagerDuty requires when acting on incidents.
    pub email: String,
    /// Personal REST API key (PagerDuty → My Profile → User Settings).
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Secrets {
    pub gitlab: Vec<GitlabConn>,
    pub atlassian: AtlassianConn,
    pub pagerduty: PagerdutyConn,
}

pub fn secrets_file() -> PathBuf {
    config::home().join("secrets.json")
}

pub fn load() -> Secrets {
    std::fs::read_to_string(secrets_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(s: &Secrets) -> Result<(), String> {
    let path = secrets_file();
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    write_private(&tmp, &body).map_err(|e| format!("cannot write {}: {e}", tmp.to_string_lossy()))?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // mode() only applies on creation; tighten a leftover file too.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    f.write_all(body.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)
}

fn norm_url(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

/// What the panel may see: everything except the tokens themselves.
pub fn view() -> Value {
    let s = load();
    let gitlab: Vec<Value> = s
        .gitlab
        .iter()
        .map(|g| json!({ "name": g.name, "base_url": g.base_url, "token_set": !g.token.is_empty() }))
        .collect();
    json!({
        "gitlab": gitlab,
        "atlassian": {
            "site": s.atlassian.site,
            "email": s.atlassian.email,
            "token_set": !s.atlassian.token.is_empty(),
        },
        "pagerduty": {
            "email": s.pagerduty.email,
            "token_set": !s.pagerduty.token.is_empty(),
        },
    })
}

/// Merge a Settings submission into `old`. A blank token keeps the stored one,
/// but only while the address it was entered for is unchanged.
fn merge(old: &Secrets, v: &Value) -> Secrets {
    let mut out = old.clone();
    let text = |x: &Value, k: &str| x.get(k).and_then(|y| y.as_str()).unwrap_or("").trim().to_string();

    if let Some(rows) = v.get("gitlab").and_then(|x| x.as_array()) {
        out.gitlab = Vec::new();
        for row in rows {
            let base_url = norm_url(&text(row, "base_url"));
            if base_url.is_empty() {
                continue; // a cleared row removes the connection
            }
            let mut token = text(row, "token");
            if token.is_empty() {
                let prev = row
                    .get("id")
                    .and_then(|x| x.as_u64())
                    .and_then(|i| old.gitlab.get(i as usize));
                if let Some(p) = prev.filter(|p| p.base_url == base_url) {
                    token = p.token.clone();
                }
            }
            out.gitlab.push(GitlabConn { name: text(row, "name"), base_url, token });
        }
    }

    if let Some(a) = v.get("atlassian") {
        let site = norm_url(&text(a, "site"));
        let mut token = text(a, "token");
        if token.is_empty() && site == old.atlassian.site {
            token = old.atlassian.token.clone();
        }
        out.atlassian = if site.is_empty() {
            AtlassianConn::default()
        } else {
            AtlassianConn { site, email: text(a, "email"), token }
        };
    }

    if let Some(p) = v.get("pagerduty") {
        // PagerDuty has one fixed address, so there is nothing to re-pin.
        let mut token = text(p, "token");
        if token.is_empty() {
            token = old.pagerduty.token.clone();
        }
        out.pagerduty = if p.get("clear").and_then(|x| x.as_bool()).unwrap_or(false) {
            PagerdutyConn::default()
        } else {
            PagerdutyConn { email: text(p, "email"), token }
        };
    }
    out
}

pub fn apply(v: &Value) -> Result<(), String> {
    save(&merge(&load(), v))
}

/// Call each service with its stored token and report who it says we are.
pub fn test() -> Value {
    let s = load();
    let outcome = |r: Result<String, String>| match r {
        Ok(who) => json!({ "ok": true, "detail": who }),
        Err(e) => json!({ "ok": false, "detail": e }),
    };
    let gitlab: Vec<Value> = s
        .gitlab
        .iter()
        .map(|g| {
            let r = if g.token.is_empty() {
                Err("no token saved".to_string())
            } else {
                crate::gitlab::whoami(&g.base_url, &g.token)
            };
            outcome(r)
        })
        .collect();
    let atlassian = if s.atlassian.site.is_empty() {
        Value::Null
    } else {
        outcome(crate::atlassian::whoami())
    };
    let pagerduty = if s.pagerduty.token.is_empty() {
        Value::Null
    } else {
        outcome(crate::pagerduty::whoami())
    };
    json!({ "gitlab": gitlab, "atlassian": atlassian, "pagerduty": pagerduty })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored() -> Secrets {
        Secrets {
            gitlab: vec![
                GitlabConn { name: "work".into(), base_url: "https://gitlab.work".into(), token: "t-work".into() },
                GitlabConn { name: "public".into(), base_url: "https://gitlab.com".into(), token: "t-pub".into() },
            ],
            atlassian: AtlassianConn {
                site: "https://co.atlassian.net".into(),
                email: "me@co.com".into(),
                token: "t-atl".into(),
            },
            pagerduty: PagerdutyConn { email: "me@co.com".into(), token: "t-pd".into() },
        }
    }

    #[test]
    fn blank_token_keeps_the_stored_one() {
        let v = json!({
            "gitlab": [
                { "id": 0, "name": "work", "base_url": "https://gitlab.work/", "token": "" },
                { "id": 1, "name": "public", "base_url": "https://gitlab.com", "token": "new" },
            ],
            "atlassian": { "site": "https://co.atlassian.net", "email": "me@co.com", "token": "" },
        });
        let s = merge(&stored(), &v);
        assert_eq!(s.gitlab[0].token, "t-work");
        assert_eq!(s.gitlab[1].token, "new");
        assert_eq!(s.atlassian.token, "t-atl");
    }

    #[test]
    fn changing_the_address_drops_the_token() {
        let v = json!({
            "gitlab": [{ "id": 0, "name": "work", "base_url": "https://elsewhere.example", "token": "" }],
            "atlassian": { "site": "https://other.atlassian.net", "email": "me@co.com", "token": "" },
        });
        let s = merge(&stored(), &v);
        assert_eq!(s.gitlab.len(), 1);
        assert_eq!(s.gitlab[0].token, "");
        assert_eq!(s.atlassian.token, "");
    }

    #[test]
    fn cleared_rows_remove_connections() {
        let v = json!({ "gitlab": [{ "id": 0, "base_url": "" }], "atlassian": { "site": "" } });
        let s = merge(&stored(), &v);
        assert!(s.gitlab.is_empty());
        assert!(!s.atlassian.is_set());
    }
}
