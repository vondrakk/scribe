//! GitLab inbound connector: merge requests assigned to, or awaiting review by,
//! the authenticated user. Read-only. Any number of GitLab instances can be
//! connected in Settings (address + a personal access token with `read_api`,
//! see secrets.rs); config.gitlab_base_url + the GITLAB_TOKEN env var still work
//! as one more.
//!
//! Returns Err("") when not configured, so the inbox simply stays empty.

use serde_json::Value;

use crate::config;
use crate::inbox::InboxItem;

fn get(url: &str, token: &str) -> Result<Value, String> {
    match ureq::get(url)
        .set("PRIVATE-TOKEN", token)
        .set("accept", "application/json")
        .call()
    {
        Ok(resp) => {
            let txt = resp.into_string().map_err(|e| format!("read body: {e}"))?;
            serde_json::from_str(&txt).map_err(|e| format!("bad json: {e}"))
        }
        Err(ureq::Error::Status(code, resp)) => {
            Err(format!("HTTP {code}: {}", resp.into_string().unwrap_or_default()))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

fn rfc3339_to_ts(s: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}

fn mr_to_item(v: &Value, reviewing: bool) -> InboxItem {
    let title = v["title"].as_str().unwrap_or("").to_string();
    let reference = v["references"]["full"].as_str().unwrap_or("").to_string();
    let branch = v["source_branch"].as_str().unwrap_or("");
    let ticket = crate::binding::extract_key(Some(&title))
        .or_else(|| crate::binding::extract_key(Some(branch)));
    InboxItem {
        source: "gitlab".into(),
        kind: if reviewing { "mr-review".into() } else { "mr".into() },
        title: if reference.is_empty() { title } else { format!("{reference}  {title}") },
        url: v["web_url"].as_str().unwrap_or("").to_string(),
        ticket,
        author: v["author"]["name"].as_str().map(String::from),
        updated_ts: rfc3339_to_ts(v["updated_at"].as_str().unwrap_or("")),
    }
}

/// Every configured instance as (label, base URL, token).
fn connections() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = crate::secrets::load()
        .gitlab
        .into_iter()
        .filter(|g| !g.base_url.is_empty() && !g.token.is_empty())
        .map(|g| {
            let label = if g.name.is_empty() { g.base_url.clone() } else { g.name };
            (label, g.base_url, g.token)
        })
        .collect();
    let base = config::load().gitlab_base_url.trim_end_matches('/').to_string();
    let token = std::env::var("GITLAB_TOKEN").unwrap_or_default();
    if !base.is_empty() && !token.is_empty() && !out.iter().any(|c| c.1 == base && c.2 == token) {
        out.push((base.clone(), base, token));
    }
    out
}

pub fn is_configured() -> bool {
    !connections().is_empty()
}

/// The username a token authenticates as — used by Settings' connection test.
pub fn whoami(base: &str, token: &str) -> Result<String, String> {
    let user = get(&format!("{}/api/v4/user", base.trim_end_matches('/')), token)?;
    user["username"]
        .as_str()
        .map(|u| format!("signed in as {u}"))
        .ok_or_else(|| "unexpected reply from GitLab".to_string())
}

/// Fetch open MRs assigned to me and awaiting my review, from every instance.
/// One instance failing doesn't hide the others' results.
pub fn fetch() -> Result<Vec<InboxItem>, String> {
    let conns = connections();
    if conns.is_empty() {
        return Err(String::new()); // not configured — contribute nothing
    }
    let mut items: Vec<InboxItem> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (label, base, token) in &conns {
        match fetch_one(base, token) {
            Ok(v) => items.extend(v),
            Err(e) => errors.push(format!("{label}: {e}")),
        }
    }
    if items.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(items)
}

fn fetch_one(base: &str, token: &str) -> Result<Vec<InboxItem>, String> {
    let base = base.trim_end_matches('/');

    let mut items: Vec<InboxItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Assigned to me.
    let assigned = get(
        &format!("{base}/api/v4/merge_requests?scope=assigned_to_me&state=opened&per_page=50"),
        token,
    )?;
    if let Some(arr) = assigned.as_array() {
        for mr in arr {
            let it = mr_to_item(mr, false);
            if seen.insert(it.url.clone()) {
                items.push(it);
            }
        }
    }

    // Awaiting my review (needs my username).
    if let Ok(user) = get(&format!("{base}/api/v4/user"), token) {
        if let Some(uname) = user["username"].as_str() {
            if let Ok(rev) = get(
                &format!("{base}/api/v4/merge_requests?reviewer_username={uname}&state=opened&per_page=50"),
                token,
            ) {
                if let Some(arr) = rev.as_array() {
                    for mr in arr {
                        let it = mr_to_item(mr, true);
                        if seen.insert(it.url.clone()) {
                            items.push(it);
                        }
                    }
                }
            }
        }
    }

    Ok(items)
}
