//! Real Atlassian Cloud publishing: OAuth 2.0 (3LO), per-user tokens stored
//! locally, and Jira/Confluence write calls — the production path behind the
//! publish seam (PRD §3.3, §7.4: posts *as the engineer*, no admin token).
//!
//! Endpoints verified against developer.atlassian.com:
//!   * authorize:  https://auth.atlassian.com/authorize
//!   * token:      https://auth.atlassian.com/oauth/token
//!   * resources:  GET https://api.atlassian.com/oauth/token/accessible-resources
//!   * API:        https://api.atlassian.com/ex/{jira|confluence}/{cloudId}/...
//!
//! The client secret is read from ATLASSIAN_CLIENT_SECRET (never stored in
//! config). Tokens live in $SCRIBE_HOME/atlassian.json.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config;
use crate::model::now_ts;

const AUTHORIZE_URL: &str = "https://auth.atlassian.com/authorize";
const TOKEN_URL: &str = "https://auth.atlassian.com/oauth/token";
const RESOURCES_URL: &str = "https://api.atlassian.com/oauth/token/accessible-resources";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: f64,
    pub cloud_id: String,
    pub site_url: String,
}

fn load_tokens() -> Option<Tokens> {
    std::fs::read_to_string(config::atlassian_token_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn save_tokens(t: &Tokens) {
    if let Ok(s) = serde_json::to_string_pretty(t) {
        let _ = std::fs::write(config::atlassian_token_file(), s);
    }
}

fn client_secret() -> Result<String, String> {
    std::env::var("ATLASSIAN_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "ATLASSIAN_CLIENT_SECRET is not set".to_string())
}

pub fn is_authenticated() -> bool {
    if crate::secrets::load().atlassian.is_set() {
        return true;
    }
    let cfg = config::load();
    if !cfg.atlassian_conf_file.is_empty() {
        return creds_from_conf(&cfg.atlassian_conf_file).is_ok();
    }
    load_tokens().map(|t| !t.access_token.is_empty()).unwrap_or(false)
}

// --- OAuth 3LO flow (scribe auth) -----------------------------------------

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn parse_query(q: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            m.insert(k.to_string(), v.replace('+', " "));
        }
    }
    m
}

/// Run the interactive authorization flow and persist tokens + cloudId.
pub fn authorize() -> Result<String, String> {
    let cfg = config::load();
    if cfg.atlassian_client_id.is_empty() {
        return Err("set atlassian_client_id in config (Settings tab) first".into());
    }
    let secret = client_secret()?;
    let port = cfg.atlassian_redirect_port;
    let redirect = format!("http://127.0.0.1:{}/callback", port);
    let state = format!("{:x}", (now_ts() * 1000.0) as u64 ^ std::process::id() as u64);

    let auth_url = format!(
        "{AUTHORIZE_URL}?audience=api.atlassian.com&client_id={}&scope={}&redirect_uri={}&state={}&response_type=code&prompt=consent",
        url_encode(&cfg.atlassian_client_id),
        url_encode(&cfg.atlassian_scopes),
        url_encode(&redirect),
        url_encode(&state),
    );

    // One-shot loopback listener for the redirect.
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port} for the OAuth redirect: {e}"))?;

    println!("Opening your browser to authorize Scribe. If it doesn't open, visit:\n{auth_url}\n");
    let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
        .arg(&auth_url)
        .spawn();

    let code = loop {
        let request = server.recv().map_err(|e| format!("redirect listener: {e}"))?;
        let url = request.url().to_string();
        if let Some((_, qs)) = url.split_once('?') {
            let q = parse_query(qs);
            let page = "<html><body style='font-family:sans-serif'>Scribe is authorized. \
                        You can close this tab.</body></html>";
            let _ = request.respond(
                tiny_http::Response::from_string(page).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap(),
                ),
            );
            if q.get("state").map(|s| s.as_str()) != Some(state.as_str()) {
                return Err("OAuth state mismatch (possible CSRF) — aborted".into());
            }
            if let Some(c) = q.get("code") {
                break c.clone();
            }
            return Err(format!("authorization failed: {qs}"));
        } else {
            let _ = request.respond(tiny_http::Response::from_string("waiting for /callback"));
        }
    };

    // Exchange the code for tokens.
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": cfg.atlassian_client_id,
        "client_secret": secret,
        "code": code,
        "redirect_uri": redirect,
    });
    let v = post_json(TOKEN_URL, None, &body)?;
    let access = v["access_token"].as_str().unwrap_or_default().to_string();
    let refresh = v["refresh_token"].as_str().unwrap_or_default().to_string();
    let expires_in = v["expires_in"].as_f64().unwrap_or(3600.0);
    if access.is_empty() {
        return Err(format!("no access_token in response: {v}"));
    }

    // Discover the cloudId + site url.
    let resources = get_json(RESOURCES_URL, &format!("Bearer {access}"))?;
    let site = resources
        .as_array()
        .and_then(|a| a.first())
        .ok_or("no accessible Atlassian sites for this account")?;
    let cloud_id = site["id"].as_str().unwrap_or_default().to_string();
    let site_url = site["url"].as_str().unwrap_or_default().to_string();

    let tokens = Tokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: now_ts() + expires_in,
        cloud_id,
        site_url: site_url.clone(),
    };
    save_tokens(&tokens);
    Ok(site_url)
}

/// A valid access token, refreshing if the stored one is near expiry.
fn valid_token() -> Result<Tokens, String> {
    let mut t = load_tokens().ok_or("not authenticated — run `scribe auth`")?;
    if now_ts() < t.expires_at - 60.0 {
        return Ok(t);
    }
    // Refresh.
    let cfg = config::load();
    let secret = client_secret()?;
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": cfg.atlassian_client_id,
        "client_secret": secret,
        "refresh_token": t.refresh_token,
    });
    let v = post_json(TOKEN_URL, None, &body)?;
    if let Some(a) = v["access_token"].as_str() {
        t.access_token = a.to_string();
        t.expires_at = now_ts() + v["expires_in"].as_f64().unwrap_or(3600.0);
        if let Some(r) = v["refresh_token"].as_str() {
            t.refresh_token = r.to_string();
        }
        save_tokens(&t);
        Ok(t)
    } else {
        Err(format!("token refresh failed: {v}"))
    }
}

// --- credential resolution: OAuth token store OR a conf file ---------------

struct Creds {
    /// Full Authorization header value ("Bearer …" or "Basic …").
    auth: String,
    /// Jira REST v3 base, e.g. {site}/rest/api/3 or api.atlassian.com/ex/jira/{cloud}/rest/api/3.
    jira_base: String,
    /// Confluence v2 base.
    conf_base: String,
    /// Site URL for browse/webui links.
    site_url: String,
}

fn resolve_creds() -> Result<Creds, String> {
    // An API token saved in Settings wins, then a conf file, then OAuth.
    let saved = crate::secrets::load().atlassian;
    if saved.is_set() {
        return Ok(Creds {
            auth: format!("Basic {}", b64(&format!("{}:{}", saved.email, saved.token))),
            jira_base: format!("{}/rest/api/3", saved.site),
            conf_base: format!("{}/wiki/api/v2", saved.site),
            site_url: saved.site,
        });
    }
    let cfg = config::load();
    if !cfg.atlassian_conf_file.is_empty() {
        return creds_from_conf(&cfg.atlassian_conf_file);
    }
    let t = valid_token()?;
    Ok(Creds {
        auth: format!("Bearer {}", t.access_token),
        jira_base: format!("https://api.atlassian.com/ex/jira/{}/rest/api/3", t.cloud_id),
        conf_base: format!("https://api.atlassian.com/ex/confluence/{}/wiki/api/v2", t.cloud_id),
        site_url: t.site_url,
    })
}

/// The account the current credentials authenticate as — used by Settings'
/// connection test.
pub fn whoami() -> Result<String, String> {
    let c = resolve_creds()?;
    let me = get_json(&format!("{}/myself", c.jira_base), &c.auth)?;
    let name = me["displayName"].as_str().or_else(|| me["emailAddress"].as_str());
    name.map(|n| format!("signed in as {n}"))
        .ok_or_else(|| "unexpected reply from Jira".to_string())
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return format!("{h}/{rest}");
        }
    }
    p.to_string()
}

fn creds_from_conf(path: &str) -> Result<Creds, String> {
    let raw = std::fs::read_to_string(expand_tilde(path))
        .map_err(|e| format!("cannot read conf {path}: {e}"))?;
    let (site, email, token, bearer) = parse_conf(&raw);
    let site = site.trim_end_matches('/').to_string();
    if site.is_empty() {
        return Err("conf is missing the site URL (site=https://you.atlassian.net)".into());
    }
    let auth = if !bearer.is_empty() {
        format!("Bearer {bearer}")
    } else if !email.is_empty() && !token.is_empty() {
        format!("Basic {}", b64(&format!("{email}:{token}")))
    } else {
        return Err("conf needs email+token (API token) or a bearer token".into());
    };
    Ok(Creds {
        jira_base: format!("{site}/rest/api/3"),
        conf_base: format!("{site}/wiki/api/v2"),
        auth,
        site_url: site,
    })
}

/// Parse a conf file (JSON or key=value) → (site, email, token, bearer).
fn parse_conf(raw: &str) -> (String, String, String, String) {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        let g = |keys: &[&str]| -> String {
            for k in keys {
                if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                    return s.to_string();
                }
            }
            String::new()
        };
        return (
            g(&["site", "base_url", "url", "host"]),
            g(&["email", "user", "username"]),
            g(&["token", "api_token", "apitoken", "password", "pat"]),
            g(&["bearer", "access_token"]),
        );
    }
    let (mut site, mut email, mut token, mut bearer) =
        (String::new(), String::new(), String::new(), String::new());
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, val)) = line.split_once('=') {
            let k = k.trim().to_lowercase();
            let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.as_str() {
                "site" | "base_url" | "url" | "host" => site = val,
                "email" | "user" | "username" => email = val,
                "token" | "api_token" | "apitoken" | "password" | "pat" => token = val,
                "bearer" | "access_token" => bearer = val,
                _ => {}
            }
        }
    }
    (site, email, token, bearer)
}

fn b64(s: &str) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = s.as_bytes();
    let mut out = String::new();
    for ch in bytes.chunks(3) {
        let b0 = ch[0];
        let b1 = *ch.get(1).unwrap_or(&0);
        let b2 = *ch.get(2).unwrap_or(&0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if ch.len() > 1 { A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if ch.len() > 2 { A[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

// --- HTTP helpers ----------------------------------------------------------

/// POST JSON with an optional full Authorization header value.
fn post_json(url: &str, auth: Option<&str>, body: &Value) -> Result<Value, String> {
    let mut req = ureq::post(url).set("content-type", "application/json");
    if let Some(a) = auth {
        req = req.set("authorization", a);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => parse_body(resp),
        Err(ureq::Error::Status(code, resp)) => {
            let txt = resp.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {txt}"))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

/// GET JSON with a full Authorization header value.
fn get_json(url: &str, auth: &str) -> Result<Value, String> {
    match ureq::get(url)
        .set("authorization", auth)
        .set("accept", "application/json")
        .call()
    {
        Ok(resp) => parse_body(resp),
        Err(ureq::Error::Status(code, resp)) => {
            Err(format!("HTTP {code}: {}", resp.into_string().unwrap_or_default()))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

fn parse_body(resp: ureq::Response) -> Result<Value, String> {
    let txt = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    if txt.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&txt).map_err(|e| format!("bad json: {e}: {txt}"))
}

// --- Jira + Confluence writes ---------------------------------------------

pub struct Posted {
    pub reference: String,
    pub url: String,
}

/// Create a Jira issue. Returns its key + browse URL.
pub fn create_issue(summary: &str, description_wiki: &str) -> Result<Posted, String> {
    let cfg = config::load();
    if cfg.jira_project_key.is_empty() {
        return Err("set jira_project_key in config first".into());
    }
    let c = resolve_creds()?;
    let body = json!({
        "fields": {
            "project": { "key": cfg.jira_project_key },
            "issuetype": { "name": cfg.jira_issue_type },
            "summary": summary,
            "description": to_adf(description_wiki),
        }
    });
    let v = post_json(&format!("{}/issue", c.jira_base), Some(&c.auth), &body)?;
    let key = v["key"].as_str().ok_or_else(|| format!("no key in response: {v}"))?;
    Ok(Posted {
        reference: key.to_string(),
        url: format!("{}/browse/{}", c.site_url.trim_end_matches('/'), key),
    })
}

/// Add a comment to an existing issue.
pub fn add_comment(issue_key: &str, body_wiki: &str) -> Result<Posted, String> {
    let c = resolve_creds()?;
    let body = json!({ "body": to_adf(body_wiki) });
    post_json(&format!("{}/issue/{issue_key}/comment", c.jira_base), Some(&c.auth), &body)?;
    Ok(Posted {
        reference: issue_key.to_string(),
        url: format!("{}/browse/{}", c.site_url.trim_end_matches('/'), issue_key),
    })
}

/// Log work (real Jira worklog) against an issue: records time spent, with an
/// optional comment. `secs` is clamped to Jira's 60s minimum.
/// `started` dates the worklog (unix seconds); None means now.
pub fn add_worklog(issue_key: &str, secs: f64, comment_wiki: &str, started: Option<f64>) -> Result<Posted, String> {
    let c = resolve_creds()?;
    let time_spent = (secs.round() as i64).max(60);
    // Jira wants the start time as e.g. 2026-10-06T12:00:00.000+0000.
    let at = started
        .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0))
        .unwrap_or_else(chrono::Utc::now);
    let started = at.format("%Y-%m-%dT%H:%M:%S%.3f%z").to_string();
    let mut body = json!({
        "timeSpentSeconds": time_spent,
        "started": started,
    });
    if !comment_wiki.trim().is_empty() {
        body["comment"] = to_adf(comment_wiki);
    }
    post_json(&format!("{}/issue/{issue_key}/worklog", c.jira_base), Some(&c.auth), &body)?;
    Ok(Posted {
        reference: issue_key.to_string(),
        url: format!("{}/browse/{}", c.site_url.trim_end_matches('/'), issue_key),
    })
}

/// Create a Confluence page (v2 API) in the configured space.
pub fn create_page(title: &str, body_wiki: &str) -> Result<Posted, String> {
    let cfg = config::load();
    if cfg.confluence_space_id.is_empty() {
        return Err("set confluence_space_id in config first".into());
    }
    let c = resolve_creds()?;
    let body = json!({
        "spaceId": cfg.confluence_space_id,
        "status": "current",
        "title": title,
        "body": { "representation": "storage", "value": to_storage(body_wiki) },
    });
    let v = post_json(&format!("{}/pages", c.conf_base), Some(&c.auth), &body)?;
    let id = v["id"].as_str().map(|s| s.to_string())
        .or_else(|| v["id"].as_i64().map(|n| n.to_string()))
        .unwrap_or_default();
    let webui = v["_links"]["webui"].as_str().unwrap_or("");
    let base = v["_links"]["base"].as_str().unwrap_or(c.site_url.trim_end_matches('/'));
    Ok(Posted {
        reference: if id.is_empty() { title.to_string() } else { id },
        url: format!("{base}{webui}"),
    })
}

// --- inbound: issues assigned to me ---------------------------------------

fn jira_date_to_ts(s: &str) -> f64 {
    chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z")
        .map(|d| d.timestamp() as f64)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp() as f64))
        .unwrap_or(0.0)
}

/// Pull issues matching config.jira_jql (default: assigned to me, not done).
/// Returns Err("") when not authenticated, so the inbox stays empty silently.
pub fn pull_assigned() -> Result<Vec<crate::inbox::InboxItem>, String> {
    let c = match resolve_creds() {
        Ok(c) => c,
        Err(_) => return Err(String::new()),
    };
    let jql = config::load().jira_jql;
    let url = format!(
        "{}/search/jql?jql={}&fields=summary,status,updated&maxResults=50",
        c.jira_base,
        url_encode(&jql)
    );
    let v = get_json(&url, &c.auth)?;
    let site = c.site_url.trim_end_matches('/');
    let mut out = Vec::new();
    if let Some(issues) = v["issues"].as_array() {
        for iss in issues {
            let key = iss["key"].as_str().unwrap_or("").to_string();
            if key.is_empty() {
                continue;
            }
            out.push(crate::inbox::InboxItem {
                source: "jira".into(),
                kind: "issue".into(),
                title: iss["fields"]["summary"].as_str().unwrap_or("").to_string(),
                url: format!("{site}/browse/{key}"),
                ticket: Some(key),
                author: iss["fields"]["status"]["name"].as_str().map(String::from),
                updated_ts: jira_date_to_ts(iss["fields"]["updated"].as_str().unwrap_or("")),
                status: iss["fields"]["status"]["name"].as_str().unwrap_or("").to_string(),
                ..Default::default()
            });
        }
    }
    Ok(out)
}

// --- one ticket, in depth ---------------------------------------------------

/// Plain text of an ADF document (Jira's rich-text format), paragraphs kept.
fn adf_text(v: &Value, out: &mut String) {
    if let Some(t) = v["text"].as_str() {
        out.push_str(t);
    }
    if v["type"].as_str() == Some("hardBreak") {
        out.push('\n');
    }
    for child in v["content"].as_array().into_iter().flatten() {
        adf_text(child, out);
    }
    if matches!(v["type"].as_str(), Some("paragraph" | "heading" | "listItem")) && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// What the stream shows when a ticket is opened: its state, the moves
/// available from here, and the latest comments.
pub fn issue_detail(key: &str) -> Result<Value, String> {
    let c = resolve_creds()?;
    let v = get_json(
        &format!("{}/issue/{key}?fields=summary,status,assignee,priority,comment,updated", c.jira_base),
        &c.auth,
    )?;
    let f = &v["fields"];
    let all = f["comment"]["comments"].as_array().cloned().unwrap_or_default();
    let comments: Vec<Value> = all
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|cm| {
            let mut text = String::new();
            adf_text(&cm["body"], &mut text);
            json!({
                "author": cm["author"]["displayName"].as_str().unwrap_or(""),
                "ts": jira_date_to_ts(cm["created"].as_str().unwrap_or("")),
                "text": text.trim(),
            })
        })
        .collect();
    let moves = get_json(&format!("{}/issue/{key}/transitions", c.jira_base), &c.auth)
        .map(|t| {
            t["transitions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| json!({ "id": x["id"].as_str().unwrap_or(""), "name": x["name"].as_str().unwrap_or("") }))
                .collect::<Vec<Value>>()
        })
        .unwrap_or_default();
    Ok(json!({
        "key": key,
        "summary": f["summary"].as_str().unwrap_or(""),
        "status": f["status"]["name"].as_str().unwrap_or(""),
        "assignee": f["assignee"]["displayName"].as_str().unwrap_or("Unassigned"),
        "priority": f["priority"]["name"].as_str().unwrap_or(""),
        "url": format!("{}/browse/{key}", c.site_url.trim_end_matches('/')),
        "comments": comments,
        "comment_total": all.len(),
        "transitions": moves,
    }))
}

/// Move a ticket through its workflow (`id` from `issue_detail`'s transitions).
pub fn transition(key: &str, id: &str) -> Result<(), String> {
    let c = resolve_creds()?;
    let body = json!({ "transition": { "id": id } });
    post_json(&format!("{}/issue/{key}/transitions", c.jira_base), Some(&c.auth), &body).map(|_| ())
}

// --- Confluence: pages about the tickets in play ---------------------------

/// Pages whose text mentions any of `keys`. Err("") when not authenticated.
pub fn search_pages(keys: &[String]) -> Result<Vec<crate::inbox::InboxItem>, String> {
    let c = match resolve_creds() {
        Ok(c) => c,
        Err(_) => return Err(String::new()),
    };
    // Keys come from our own ticket pattern; keep the query to what it expects.
    let keys: Vec<&String> = keys
        .iter()
        .filter(|k| k.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'))
        .take(20)
        .collect();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let terms: Vec<String> = keys.iter().map(|k| format!("text ~ \"{k}\"")).collect();
    let cql = format!("type = page AND ({}) ORDER BY lastmodified DESC", terms.join(" OR "));
    let base = c.conf_base.replace("/api/v2", "/rest/api");
    let v = get_json(&format!("{base}/search?cql={}&limit=30", url_encode(&cql)), &c.auth)?;
    let link_base = v["_links"]["base"].as_str().unwrap_or("").to_string();
    let mut out = Vec::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let clean = |s: &str| s.replace("@@@hl@@@", "").replace("@@@endhl@@@", "");
        let title = clean(r["title"].as_str().unwrap_or(""));
        let excerpt = clean(r["excerpt"].as_str().unwrap_or(""));
        let hay = format!("{title} {excerpt}").to_uppercase();
        // Only keep a page we can tie to a ticket by what the search showed us.
        let Some(key) = keys.iter().find(|k| hay.contains(&k.to_uppercase())) else {
            continue;
        };
        out.push(crate::inbox::InboxItem {
            source: "confluence".into(),
            kind: "page".into(),
            title,
            url: format!("{link_base}{}", r["url"].as_str().unwrap_or("")),
            ticket: Some((*key).clone()),
            author: r["resultGlobalContainer"]["title"].as_str().map(String::from),
            updated_ts: jira_date_to_ts(r["lastModified"].as_str().unwrap_or("")),
            ref_id: r["content"]["id"].as_str().unwrap_or("").to_string(),
            detail: excerpt.split_whitespace().collect::<Vec<_>>().join(" "),
            ..Default::default()
        });
    }
    Ok(out)
}

/// Append to an existing page (a runbook, an incident log): the new text goes
/// at the end as a new page version.
pub fn append_page(page_id: &str, body_wiki: &str) -> Result<Posted, String> {
    if page_id.is_empty() || !page_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("bad page id".into());
    }
    let c = resolve_creds()?;
    let url = format!("{}/pages/{page_id}", c.conf_base);
    let page = get_json(&format!("{url}?body-format=storage"), &c.auth)?;
    let title = page["title"].as_str().ok_or("could not read the page")?;
    let version = page["version"]["number"].as_i64().ok_or("could not read the page version")?;
    let current = page["body"]["storage"]["value"].as_str().unwrap_or("");
    let body = json!({
        "id": page_id,
        "status": "current",
        "title": title,
        "body": { "representation": "storage", "value": format!("{current}{}", to_storage(body_wiki)) },
        "version": { "number": version + 1, "message": "Appended from Scribe" },
    });
    let v = match ureq::put(&url)
        .set("content-type", "application/json")
        .set("authorization", &c.auth)
        .send_string(&body.to_string())
    {
        Ok(resp) => parse_body(resp)?,
        Err(ureq::Error::Status(code, resp)) => {
            return Err(format!("HTTP {code}: {}", resp.into_string().unwrap_or_default()))
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };
    let base = v["_links"]["base"].as_str().unwrap_or(c.site_url.trim_end_matches('/'));
    Ok(Posted {
        reference: title.to_string(),
        url: format!("{base}{}", v["_links"]["webui"].as_str().unwrap_or("")),
    })
}

// --- format conversion: Jira wiki-ish markup -> ADF / storage --------------

enum Line<'a> {
    Heading(u8, &'a str),
    Bullet(&'a str),
    Para(&'a str),
    Blank,
}

fn classify_line(line: &str) -> Line<'_> {
    let l = line.trim_end();
    if l.is_empty() {
        return Line::Blank;
    }
    for lvl in 1..=3u8 {
        let p = format!("h{lvl}. ");
        if let Some(rest) = l.strip_prefix(&p) {
            return Line::Heading(lvl, rest);
        }
    }
    if let Some(rest) = l.strip_prefix("* ").or_else(|| l.strip_prefix("- ")) {
        return Line::Bullet(rest);
    }
    Line::Para(l)
}

/// Split a run of text on `*bold*` into ADF text nodes with optional strong mark.
fn inline_adf(text: &str) -> Vec<Value> {
    let mut nodes = Vec::new();
    let mut bold = false;
    // Bare URLs become real links (a thread permalink, a page, an MR).
    let url = regex::Regex::new(r"https?://[^\s)\]>]+").unwrap();
    let mut push = |s: &str, href: Option<&str>, bold: bool| {
        if s.is_empty() {
            return;
        }
        let mut marks: Vec<Value> = Vec::new();
        if let Some(h) = href {
            marks.push(json!({ "type": "link", "attrs": { "href": h } }));
        }
        if bold {
            marks.push(json!({ "type": "strong" }));
        }
        let mut node = json!({ "type": "text", "text": s });
        if !marks.is_empty() {
            node["marks"] = json!(marks);
        }
        nodes.push(node);
    };
    for seg in text.split('*') {
        let mut last = 0;
        for m in url.find_iter(seg) {
            let u = m.as_str().trim_end_matches(['.', ',', ';', ':']);
            push(&seg[last..m.start()], None, bold);
            push(u, Some(u), bold);
            last = m.start() + u.len();
        }
        push(&seg[last..], None, bold);
        bold = !bold; // each '*' toggles bold state
    }
    if nodes.is_empty() {
        // ADF text nodes can't be empty; fall back to the raw text.
        nodes.push(json!({ "type": "text", "text": text }));
    }
    nodes
}

pub fn to_adf(wiki: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut bullets: Vec<Value> = Vec::new();
    let flush_bullets = |content: &mut Vec<Value>, bullets: &mut Vec<Value>| {
        if !bullets.is_empty() {
            content.push(json!({ "type": "bulletList", "content": bullets.clone() }));
            bullets.clear();
        }
    };
    for line in wiki.lines() {
        match classify_line(line) {
            Line::Heading(lvl, t) => {
                flush_bullets(&mut content, &mut bullets);
                content.push(json!({
                    "type": "heading", "attrs": { "level": lvl },
                    "content": inline_adf(t)
                }));
            }
            Line::Bullet(t) => {
                bullets.push(json!({
                    "type": "listItem",
                    "content": [ { "type": "paragraph", "content": inline_adf(t) } ]
                }));
            }
            Line::Para(t) => {
                flush_bullets(&mut content, &mut bullets);
                content.push(json!({ "type": "paragraph", "content": inline_adf(t) }));
            }
            Line::Blank => flush_bullets(&mut content, &mut bullets),
        }
    }
    flush_bullets(&mut content, &mut bullets);
    if content.is_empty() {
        content.push(json!({ "type": "paragraph", "content": [] }));
    }
    json!({ "type": "doc", "version": 1, "content": content })
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Convert `*bold*` runs to <strong> within an already HTML-escaped line.
fn inline_storage(text: &str) -> String {
    let mut out = String::new();
    let mut bold = false;
    for seg in esc_html(text).split('*') {
        if !seg.is_empty() {
            if bold {
                out.push_str("<strong>");
                out.push_str(seg);
                out.push_str("</strong>");
            } else {
                out.push_str(seg);
            }
        }
        bold = !bold;
    }
    out
}

pub fn to_storage(wiki: &str) -> String {
    let mut out = String::new();
    let mut in_list = false;
    for line in wiki.lines() {
        match classify_line(line) {
            Line::Heading(lvl, t) => {
                if in_list {
                    out.push_str("</ul>");
                    in_list = false;
                }
                out.push_str(&format!("<h{lvl}>{}</h{lvl}>", inline_storage(t)));
            }
            Line::Bullet(t) => {
                if !in_list {
                    out.push_str("<ul>");
                    in_list = true;
                }
                out.push_str(&format!("<li>{}</li>", inline_storage(t)));
            }
            Line::Para(t) => {
                if in_list {
                    out.push_str("</ul>");
                    in_list = false;
                }
                out.push_str(&format!("<p>{}</p>", inline_storage(t)));
            }
            Line::Blank => {
                if in_list {
                    out.push_str("</ul>");
                    in_list = false;
                }
            }
        }
    }
    if in_list {
        out.push_str("</ul>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adf_headings_bullets_bold() {
        let adf = to_adf("h2. Planned\n* *OPS-1* did a thing\nplain line");
        let c = adf["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "heading");
        assert_eq!(c[0]["attrs"]["level"], 2);
        assert_eq!(c[1]["type"], "bulletList");
        let runs = c[1]["content"][0]["content"][0]["content"].as_array().unwrap();
        assert!(runs.iter().any(|n| n["marks"].is_array()), "bold run present");
        assert_eq!(c[2]["type"], "paragraph");
    }

    #[test]
    fn storage_escapes_and_bold() {
        let s = to_storage("h1. T<x>\n* *b* & more");
        assert!(s.contains("<h1>T&lt;x&gt;</h1>"), "{s}");
        assert!(s.contains("<strong>b</strong>"), "{s}");
        assert!(s.contains("&amp;"), "{s}");
    }
}
