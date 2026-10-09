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
pub fn add_worklog(issue_key: &str, secs: f64, comment_wiki: &str) -> Result<Posted, String> {
    let c = resolve_creds()?;
    let time_spent = (secs.round() as i64).max(60);
    // Jira wants the start time as e.g. 2026-10-06T12:00:00.000+0000.
    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3f%z").to_string();
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
            });
        }
    }
    Ok(out)
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
    for seg in text.split('*') {
        if !seg.is_empty() {
            let mut node = json!({ "type": "text", "text": seg });
            if bold {
                node["marks"] = json!([{ "type": "strong" }]);
            }
            nodes.push(node);
        }
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
