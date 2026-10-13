//! PagerDuty connector: open incidents assigned to me, and the three things an
//! on-call engineer does to one — acknowledge, resolve, add a note.
//!
//! Uses a personal REST API key entered in Settings (secrets.rs). The email is
//! sent as the `From` header PagerDuty requires on writes, and is the fallback
//! for finding "me" when the key is an account-level one.

use serde_json::{json, Value};

use crate::inbox::InboxItem;
use crate::secrets;

/// PagerDuty's API address. `SCRIBE_PAGERDUTY_API` overrides it — for accounts
/// in the EU service region (https://api.eu.pagerduty.com) and for tests.
fn api() -> String {
    std::env::var("SCRIBE_PAGERDUTY_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.pagerduty.com".to_string())
}

fn creds() -> Option<secrets::PagerdutyConn> {
    let c = secrets::load().pagerduty;
    (!c.token.is_empty()).then_some(c)
}

pub fn is_configured() -> bool {
    creds().is_some()
}

fn call(method: &str, path: &str, c: &secrets::PagerdutyConn, body: Option<&Value>) -> Result<Value, String> {
    let mut req = ureq::request(method, &format!("{}{path}", api().trim_end_matches('/')))
        .set("authorization", &format!("Token token={}", c.token))
        .set("accept", "application/vnd.pagerduty+json;version=2")
        .set("content-type", "application/json");
    if !c.email.is_empty() {
        req = req.set("from", &c.email);
    }
    let resp = match body {
        Some(b) => req.send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let txt = r.into_string().map_err(|e| format!("read body: {e}"))?;
            if txt.trim().is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_str(&txt).map_err(|e| format!("bad json: {e}"))
        }
        Err(ureq::Error::Status(code, r)) => {
            let txt = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&txt)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or(txt);
            Err(format!("HTTP {code}: {msg}"))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

/// (user id, display name) the key acts as.
fn me(c: &secrets::PagerdutyConn) -> Result<(String, String), String> {
    let pick = |u: &Value| {
        Some((u["id"].as_str()?.to_string(), u["name"].as_str().unwrap_or("").to_string()))
    };
    if let Ok(v) = call("GET", "/users/me", c, None) {
        if let Some(u) = pick(&v["user"]) {
            return Ok(u);
        }
    }
    // An account-level key has no "me": look the user up by email.
    if c.email.is_empty() {
        return Err("this key isn't tied to a user — add your PagerDuty email in Settings".into());
    }
    let v = call("GET", &format!("/users?query={}", c.email.replace('@', "%40").replace('+', "%2B")), c, None)?;
    v["users"]
        .as_array()
        .and_then(|a| a.iter().find(|u| u["email"].as_str() == Some(c.email.as_str())))
        .and_then(pick)
        .ok_or_else(|| format!("no PagerDuty user with email {}", c.email))
}

pub fn whoami() -> Result<String, String> {
    let c = creds().ok_or("no key saved")?;
    me(&c).map(|(_, name)| format!("signed in as {name}"))
}

fn ts(s: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp() as f64).unwrap_or(0.0)
}

/// Open (triggered or acknowledged) incidents assigned to me. Err("") when not
/// configured, so the inbox simply stays empty.
pub fn fetch() -> Result<Vec<InboxItem>, String> {
    let c = creds().ok_or_else(String::new)?;
    let (uid, _) = me(&c)?;
    let v = call(
        "GET",
        &format!("/incidents?statuses%5B%5D=triggered&statuses%5B%5D=acknowledged&user_ids%5B%5D={uid}&limit=50&sort_by=created_at%3Adesc"),
        &c,
        None,
    )?;
    let mut out = Vec::new();
    for inc in v["incidents"].as_array().into_iter().flatten() {
        let title = inc["title"].as_str().unwrap_or("").to_string();
        let number = inc["incident_number"].as_i64().unwrap_or(0);
        out.push(InboxItem {
            source: "pagerduty".into(),
            kind: "incident".into(),
            ticket: crate::binding::extract_key(Some(&title)),
            title,
            url: inc["html_url"].as_str().unwrap_or("").to_string(),
            author: inc["service"]["summary"].as_str().map(String::from),
            updated_ts: ts(inc["created_at"].as_str().unwrap_or("")),
            status: inc["status"].as_str().unwrap_or("").to_string(),
            ref_id: inc["id"].as_str().unwrap_or("").to_string(),
            detail: format!("#{number} · {} urgency", inc["urgency"].as_str().unwrap_or("?")),
            ..Default::default()
        });
    }
    Ok(out)
}

/// Move an incident to `acknowledged` or `resolved`.
pub fn set_status(id: &str, status: &str) -> Result<(), String> {
    let c = creds().ok_or("PagerDuty is not connected")?;
    if c.email.is_empty() {
        return Err("PagerDuty needs your email (Settings → Connections) to act on incidents".into());
    }
    let body = json!({ "incident": { "type": "incident_reference", "status": status } });
    call("PUT", &format!("/incidents/{id}"), &c, Some(&body)).map(|_| ())
}

pub fn add_note(id: &str, note: &str) -> Result<(), String> {
    let c = creds().ok_or("PagerDuty is not connected")?;
    if c.email.is_empty() {
        return Err("PagerDuty needs your email (Settings → Connections) to add notes".into());
    }
    let body = json!({ "note": { "content": note } });
    call("POST", &format!("/incidents/{id}/notes"), &c, Some(&body)).map(|_| ())
}
