//! GitLab inbound connector: merge requests assigned to, or awaiting review by,
//! the authenticated user. Read-only. Base URL from config.gitlab_base_url, token
//! from the GITLAB_TOKEN env var (a personal access token with `read_api`).
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

/// Fetch open MRs assigned to me and awaiting my review.
pub fn fetch() -> Result<Vec<InboxItem>, String> {
    let cfg = config::load();
    let base = cfg.gitlab_base_url.trim_end_matches('/').to_string();
    let token = std::env::var("GITLAB_TOKEN").unwrap_or_default();
    if base.is_empty() || token.is_empty() {
        return Err(String::new()); // not configured — contribute nothing
    }

    let mut items: Vec<InboxItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Assigned to me.
    let assigned = get(
        &format!("{base}/api/v4/merge_requests?scope=assigned_to_me&state=opened&per_page=50"),
        &token,
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
    if let Ok(user) = get(&format!("{base}/api/v4/user"), &token) {
        if let Some(uname) = user["username"].as_str() {
            if let Ok(rev) = get(
                &format!("{base}/api/v4/merge_requests?reviewer_username={uname}&state=opened&per_page=50"),
                &token,
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
