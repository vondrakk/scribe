//! Local control panel — multi-tab web UI (Nanny pattern) over tiny_http.
//!
//! Review is the one screen that matters: a card per piece of work with a
//! proposed update to send, edit or skip. History and Settings sit beside it;
//! the pipeline internals and the workbench live under "More". Nothing
//! publishes without an explicit click here (§7.3).
//! Binds to loopback only.

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};

use crate::{atlassian, binding, config, context, desk, inbox, logbook, model, pipeline, prompts, pty, publish, review, rules, stream, timeadj, workbench};

const INDEX_HTML: &str = include_str!("panel_index.html");
const XTERM_JS: &str = include_str!("../assets/xterm.js");
const XTERM_CSS: &str = include_str!("../assets/xterm.css");
const ADDON_FIT_JS: &str = include_str!("../assets/addon-fit.js");

fn ctype(mime: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()).unwrap()
}
fn json_header() -> Header {
    ctype("application/json")
}
fn html_header() -> Header {
    ctype("text/html; charset=utf-8")
}

fn state_json() -> Value {
    let drafts = model::load_drafts();
    let episodes = model::load_episodes();
    let events = model::read_events();
    let n_drafts = drafts.iter().filter(|d| d.status == "draft").count();
    let n_un = episodes.iter().filter(|e| e.kind == "unaccounted").count();
    // "capture live" = at least one event in the last 10 minutes — tells the UI
    // whether the shell snippet is actually sourced and recording.
    let cutoff = model::now_ts() - 600.0;
    let recent = events.iter().filter(|e| e.ts >= cutoff).count();
    json!({
        "binding": binding::get_binding(),
        "config": config::load(),
        "counts": {
            "review": review::build().len(),
            "desk": desk::needs_count(),
            "drafts": n_drafts,
            "episodes": episodes.len(),
            "unaccounted": n_un,
            "timeline": events.len(),
            "prompts": prompts::total_count(),
            "logged": logbook::load().entries.len(),
        },
        "capture_live": recent > 0,
        "atlassian_authed": atlassian::is_authenticated(),
        "inbox": {
            "count": inbox::load().items.len(),
            "fetched_ts": inbox::load().fetched_ts,
            "gitlab": crate::gitlab::is_configured(),
            "pagerduty": crate::pagerduty::is_configured(),
            "jira": atlassian::is_authenticated(),
        },
        "connections": crate::secrets::view(),
        "snippet": config::home().join("scribe.sh").to_string_lossy(),
        "now": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
    })
}

fn candidates_json() -> Value {
    let cands: Vec<Value> = binding::candidate_tickets(12)
        .into_iter()
        .map(|(k, last, n)| json!({ "key": k, "last": last, "n": n }))
        .collect();
    json!({ "candidates": cands })
}

fn timeline_json() -> Value {
    let mut events = model::read_events();
    events.reverse();
    events.truncate(200);
    json!({ "events": events })
}

fn episodes_json() -> Value {
    let mut eps = model::load_episodes();
    eps.sort_by(|a, b| b.start_ts.partial_cmp(&a.start_ts).unwrap_or(std::cmp::Ordering::Equal));
    json!({ "episodes": eps })
}

fn drafts_json() -> Value {
    let mut ds: Vec<_> = model::load_drafts()
        .into_iter()
        .filter(|d| d.status != "discarded")
        .collect();
    ds.sort_by(|a, b| b.id.cmp(&a.id));
    json!({ "drafts": ds })
}

/// The Logged screen: handled bundles enriched with live time + a per-day
/// breakdown, plus per-project totals. The front-end groups entries by day and
/// labels timestamps.
fn logbook_json() -> Value {
    let lb = logbook::load();
    let eps = model::load_episodes();
    let ov = timeadj::load();
    let drafts = model::load_drafts();
    let secs_of = |id: i64| -> f64 {
        eps.iter()
            .find(|e| e.id == id)
            .map(|e| ov.get(&e.id).copied().unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0)))
            .unwrap_or(0.0)
    };
    let end_of = |id: i64| -> f64 { eps.iter().find(|e| e.id == id).map(|e| e.end_ts).unwrap_or(0.0) };

    let mut entries: Vec<Value> = Vec::new();
    let mut totals: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();

    let mut sorted = lb.entries.clone();
    sorted.sort_by(|a, b| b.updated_ts.partial_cmp(&a.updated_ts).unwrap_or(std::cmp::Ordering::Equal));

    for e in &sorted {
        let live_secs: f64 = e.episode_ids.iter().map(|id| secs_of(*id)).sum();
        // Per-day breakdown (group episode time by calendar day).
        let mut byday: std::collections::BTreeMap<i64, (f64, i64, f64)> = std::collections::BTreeMap::new();
        for id in &e.episode_ids {
            let end = end_of(*id);
            let daykey = (end / 86400.0).floor() as i64;
            let ent = byday.entry(daykey).or_insert((0.0, 0, end));
            ent.0 += secs_of(*id);
            ent.1 += 1;
            if end > ent.2 { ent.2 = end; }
        }
        let mut breakdown: Vec<Value> = byday
            .values()
            .map(|(s, n, ts)| json!({ "ts": ts, "secs": s, "count": n }))
            .collect();
        breakdown.sort_by(|a, b| b["ts"].as_f64().unwrap_or(0.0)
            .partial_cmp(&a["ts"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));

        let draft_status = e.draft_id
            .and_then(|did| drafts.iter().find(|d| d.id == did))
            .map(|d| d.status.clone());

        if e.kind == "worklog" || e.kind == "logged" {
            let key = if !e.project.is_empty() { e.project.clone() }
                else if !e.tag.is_empty() { e.tag.clone() }
                else if !e.ticket.is_empty() { e.ticket.clone() }
                else { "(unlabeled)".to_string() };
            *totals.entry(key).or_insert(0.0) += live_secs;
        }

        entries.push(json!({
            "id": e.id,
            "kind": e.kind,
            "title": e.title,
            "ticket": e.ticket,
            "tag": e.tag,
            "project": e.project,
            "created_ts": e.created_ts,
            "updated_ts": e.updated_ts,
            "secs": live_secs,
            "count": e.episode_ids.len(),
            "draft_id": e.draft_id,
            "draft_status": draft_status,
            "pushed_jira": e.pushed_jira,
            "body": e.text(),
            "silent": e.silent,
            "auto": e.auto_ids.len(),
            "breakdown": breakdown,
        }));
    }

    let totals: Vec<Value> = {
        let mut v: Vec<(String, f64)> = totals.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter().map(|(k, s)| json!({ "project": k, "secs": s })).collect()
    };
    json!({ "entries": entries, "totals": totals })
}

fn pretty_home(p: &str) -> String {
    if let Ok(h) = std::env::var("HOME") {
        if let Some(rest) = p.strip_prefix(&h) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

fn fmt_hm(secs: f64) -> String {
    let m = (secs / 60.0).round() as i64;
    if m <= 0 {
        return "—".into();
    }
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h {}m", m / 60, m % 60)
}

/// A per-project, per-day timesheet (markdown table) over the last `days` days,
/// built from the logbook — ready to paste into a standup or a timesheet tool.
fn timesheet_md(days: i64) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let lb = logbook::load();
    let eps = model::load_episodes();
    let ov = timeadj::load();
    let now = model::now_ts();
    let start = now - (days as f64) * 86400.0;
    let secs_of = |id: i64| -> f64 {
        eps.iter().find(|e| e.id == id)
            .map(|e| ov.get(&e.id).copied().unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0)))
            .unwrap_or(0.0)
    };
    let end_of = |id: i64| -> f64 { eps.iter().find(|e| e.id == id).map(|e| e.end_ts).unwrap_or(0.0) };

    // Key by local calendar day (matches the UI's day grouping).
    let local_day = |ts: f64| -> i64 {
        chrono::DateTime::from_timestamp(ts as i64, 0)
            .map(|d| {
                chrono::Datelike::num_days_from_ce(&d.with_timezone(&chrono::Local).date_naive()) as i64
            })
            .unwrap_or(0)
    };

    let mut matrix: BTreeMap<String, BTreeMap<i64, f64>> = BTreeMap::new();
    let mut dayset: BTreeSet<i64> = BTreeSet::new();
    for e in &lb.entries {
        if e.kind == "disposed" {
            continue;
        }
        let proj = if !e.project.is_empty() { pretty_home(&e.project) }
            else if !e.ticket.is_empty() { e.ticket.clone() }
            else { "(unlabeled)".into() };
        for id in &e.episode_ids {
            let end = end_of(*id);
            if end < start || end <= 0.0 {
                continue;
            }
            let dk = local_day(end);
            dayset.insert(dk);
            *matrix.entry(proj.clone()).or_default().entry(dk).or_insert(0.0) += secs_of(*id);
        }
    }

    // Drop projects whose total rounds to nothing (noise).
    matrix.retain(|_, byday| byday.values().sum::<f64>() >= 30.0);

    let through = chrono::Local::now().format("%Y-%m-%d");
    if matrix.is_empty() {
        return format!("# Timesheet — last {days} days (through {through})\n\n_No logged time in this window._\n");
    }
    let dayvec: Vec<i64> = dayset.into_iter().collect();
    let day_label = |dk: i64| -> String {
        chrono::NaiveDate::from_num_days_from_ce_opt(dk as i32)
            .map(|d| d.format("%a %-m/%-d").to_string())
            .unwrap_or_default()
    };

    let mut out = String::new();
    out.push_str(&format!("# Timesheet — last {days} days (through {through})\n\n"));
    // header row
    out.push_str("| Project |");
    for dk in &dayvec {
        out.push_str(&format!(" {} |", day_label(*dk)));
    }
    out.push_str(" Total |\n|---|");
    for _ in &dayvec {
        out.push_str("---|");
    }
    out.push_str("---|\n");

    let mut col_totals: BTreeMap<i64, f64> = BTreeMap::new();
    let mut grand = 0.0;
    for (proj, byday) in &matrix {
        out.push_str(&format!("| {proj} |"));
        let mut row_total = 0.0;
        for dk in &dayvec {
            let s = byday.get(dk).copied().unwrap_or(0.0);
            row_total += s;
            *col_totals.entry(*dk).or_insert(0.0) += s;
            out.push_str(&format!(" {} |", fmt_hm(s)));
        }
        grand += row_total;
        out.push_str(&format!(" {} |\n", fmt_hm(row_total)));
    }
    // totals row
    out.push_str("| **Total** |");
    for dk in &dayvec {
        out.push_str(&format!(" {} |", fmt_hm(col_totals.get(dk).copied().unwrap_or(0.0))));
    }
    out.push_str(&format!(" **{}** |\n", fmt_hm(grand)));
    out
}

fn set_draft_status(id: i64, status: &str) {
    let mut ds = model::load_drafts();
    if let Some(d) = ds.iter_mut().find(|d| d.id == id) {
        d.status = status.to_string();
    }
    model::save_drafts(&ds);
}

fn edit_draft(id: i64, body: &str) {
    let mut ds = model::load_drafts();
    if let Some(d) = ds.iter_mut().find(|d| d.id == id) {
        d.body = body.to_string();
    }
    model::save_drafts(&ds);
}

fn apply_config(v: &Value) {
    let mut c = config::load();
    if let Some(x) = v.get("episode_gap_seconds").and_then(|x| x.as_f64()) {
        c.episode_gap_seconds = x;
    }
    if let Some(x) = v.get("retention_days").and_then(|x| x.as_i64()) {
        c.retention_days = x;
    }
    if let Some(x) = v.get("ticket_key_pattern").and_then(|x| x.as_str()) {
        c.ticket_key_pattern = x.to_string();
    }
    if let Some(x) = v.get("llm_backend").and_then(|x| x.as_str()) {
        c.llm_backend = x.to_string();
    }
    if let Some(x) = v.get("llm_model").and_then(|x| x.as_str()) {
        c.llm_model = x.to_string();
    }
    if let Some(x) = v.get("llm_base_url").and_then(|x| x.as_str()) {
        c.llm_base_url = x.to_string();
    }
    if let Some(x) = v.get("publish_backend").and_then(|x| x.as_str()) {
        c.publish_backend = x.to_string();
    }
    if let Some(x) = v.get("atlassian_client_id").and_then(|x| x.as_str()) {
        c.atlassian_client_id = x.to_string();
    }
    if let Some(x) = v.get("atlassian_conf_file").and_then(|x| x.as_str()) {
        c.atlassian_conf_file = x.to_string();
    }
    if let Some(x) = v.get("jira_project_key").and_then(|x| x.as_str()) {
        c.jira_project_key = x.to_string();
    }
    if let Some(x) = v.get("jira_issue_type").and_then(|x| x.as_str()) {
        c.jira_issue_type = x.to_string();
    }
    if let Some(x) = v.get("confluence_space_id").and_then(|x| x.as_str()) {
        c.confluence_space_id = x.to_string();
    }
    if let Some(x) = v.get("gitlab_base_url").and_then(|x| x.as_str()) {
        c.gitlab_base_url = x.to_string();
    }
    if let Some(x) = v.get("jira_jql").and_then(|x| x.as_str()) {
        c.jira_jql = x.to_string();
    }
    if let Some(x) = v.get("inbox_refresh_seconds").and_then(|x| x.as_i64()) {
        c.inbox_refresh_seconds = x;
    }
    if let Some(x) = v.get("capture_notifications").and_then(|x| x.as_bool()) {
        c.capture_notifications = x;
    }
    if let Some(x) = v.get("notification_apps").and_then(|x| x.as_str()) {
        c.notification_apps = x.to_string();
    }
    if let Some(x) = v.get("event_split").and_then(|x| x.as_str()) {
        if ["auto", "day", "never"].contains(&x) {
            c.event_split = x.to_string();
        }
    }
    if let Some(x) = v.get("ignored_paths").and_then(|x| x.as_array()) {
        let mut paths: Vec<String> =
            x.iter().filter_map(|p| p.as_str()).map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        paths.dedup();
        c.ignored_paths = paths;
    }
    config::save(&c);
    // Either setting can move work between the Stream and the log.
    review::settle();
}

/// Background thread: periodically refresh the inbox so inbound artifacts flow in
/// without a manual click. Network calls happen here, off the request thread, so
/// the panel never blocks. Interval comes from config (0 disables), re-read each
/// loop so a Settings change takes effect without a restart. Only fires when a
/// connector is actually configured.
fn spawn_inbox_refresher() {
    std::thread::spawn(|| loop {
        let cfg = config::load();
        let secs = cfg.inbox_refresh_seconds;
        if secs <= 0 {
            // Disabled — idle-poll the setting so re-enabling takes effect.
            std::thread::sleep(std::time::Duration::from_secs(60));
            continue;
        }
        let configured = crate::gitlab::is_configured()
            || crate::pagerduty::is_configured()
            || atlassian::is_authenticated();
        if configured {
            let _ = inbox::refresh();
        }
        std::thread::sleep(std::time::Duration::from_secs(secs.max(30) as u64));
    });
}

/// True unless the request was made by another web page: a browser names the
/// calling page in Origin, and for the panel's own calls that is the address
/// it was served from.
fn same_origin(request: &tiny_http::Request) -> bool {
    let header = |name: &str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str().to_string())
    };
    match (header("origin"), header("host")) {
        (None, _) => true, // not a cross-site browser request (CLI, curl)
        (Some(origin), Some(host)) => origin == format!("http://{host}"),
        (Some(_), None) => false,
    }
}

pub fn serve() {
    if let Err(e) = try_serve() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Serve the panel until the process ends. Returns only if it could not start.
pub fn try_serve() -> Result<(), String> {
    let cfg = config::load();
    let addr = format!("{}:{}", cfg.panel_host, cfg.panel_port);
    let server = Server::http(&addr).map_err(|e| format!("could not bind {addr}: {e}"))?;
    // Background inbox auto-refresh (off the request thread).
    spawn_inbox_refresher();
    // Opt-in chat-notification listener (idles while the setting is off).
    context::spawn_listener();

    // Process once at startup so the panel opens with current content.
    let r0 = pipeline::process();
    println!(
        "Scribe panel → http://{}  ({} episodes, {} drafts)  (Ctrl-C to stop)",
        addr, r0.episodes, r0.drafts
    );

    // Auto-process when new activity is captured. The event log only grows via
    // appends, so a size change means new events → re-sessionize. This runs on
    // the single request-handling thread (tiny_http is sequential), so there is
    // no concurrent file access to guard against.
    let mut last_sz = crate::db::last_id(crate::db::EVENTS);

    for mut request in server.incoming_requests() {
        let sz = crate::db::last_id(crate::db::EVENTS);
        if sz != last_sz {
            pipeline::process();
            last_sz = sz;
        }

        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();

        // Returns (status_code, body_string, is_html)
        let (code, out, is_html): (u16, String, bool) = match (&method, path.as_str()) {
            (Method::Get, "/") => (200, INDEX_HTML.to_string(), true),
            (Method::Get, "/xterm.js") => (200, XTERM_JS.to_string(), false),
            (Method::Get, "/addon-fit.js") => (200, ADDON_FIT_JS.to_string(), false),
            (Method::Get, "/xterm.css") => (200, XTERM_CSS.to_string(), false),
            (Method::Post, "/api/pty/open") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
                let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
                let cwd = v.get("cwd").and_then(|x| x.as_str()).unwrap_or("");
                match pty::open(cols, rows, cwd) {
                    Ok(id) => (200, json!({"ok": true, "id": id}).to_string(), false),
                    Err(e) => (500, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/pty/read") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let from = v.get("from").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let (data, next, alive) = pty::read(id, from);
                (200, json!({"data": data, "next": next, "alive": alive}).to_string(), false)
            }
            (Method::Post, "/api/pty/write") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let data = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
                pty::write(id, data);
                (200, json!({"ok": true}).to_string(), false)
            }
            (Method::Post, "/api/pty/resize") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
                let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
                pty::resize(id, cols, rows);
                (200, json!({"ok": true}).to_string(), false)
            }
            (Method::Post, "/api/pty/close") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                pty::close(v.get("id").and_then(|x| x.as_str()).unwrap_or(""));
                (200, json!({"ok": true}).to_string(), false)
            }
            (Method::Get, "/api/state") => (200, state_json().to_string(), false),
            (Method::Get, "/api/timeline") => (200, timeline_json().to_string(), false),
            (Method::Get, "/api/episodes") => (200, episodes_json().to_string(), false),
            (Method::Get, "/api/drafts") => (200, drafts_json().to_string(), false),
            (Method::Get, "/api/candidates") => (200, candidates_json().to_string(), false),
            (Method::Get, "/api/prompts") => {
                (200, json!({ "sessions": prompts::all_sessions() }).to_string(), false)
            }
            (Method::Get, "/api/stream") => {
                (200, json!({ "items": stream::build() }).to_string(), false)
            }
            (Method::Get, "/api/review") => {
                (200, json!({ "cards": review::build(), "handoff": review::handoff() }).to_string(), false)
            }
            (Method::Post, "/api/review/send") => {
                let req: review::SendReq = serde_json::from_str(&body).unwrap_or_default();
                match review::send(&req) {
                    Ok(s) => (200, json!({"ok": true, "ref": s.reference, "url": s.url,
                        "reply": s.reply, "mock": s.mock}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/review/slack") => {
                let req: review::SendReq = serde_json::from_str(&body).unwrap_or_default();
                (200, json!({"ok": true, "text": review::slack_post(&req)}).to_string(), false)
            }
            (Method::Post, "/api/context/assign") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
                match context::assign(v.get("id").and_then(|x| x.as_i64()).unwrap_or(-1), s("ticket"), s("pin")) {
                    Ok(()) => (200, json!({"ok": true}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/ignore") => {
                // Stop asking about a path: its work is logged silently from now on.
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                if path.is_empty() {
                    (400, json!({"ok": false, "error": "no path"}).to_string(), false)
                } else {
                    let mut c = config::load();
                    if !c.ignored_paths.contains(&path) {
                        c.ignored_paths.push(path);
                        config::save(&c);
                    }
                    review::settle();
                    (200, json!({"ok": true}).to_string(), false)
                }
            }
            (Method::Post, "/api/review/skip") => {
                let req: review::SendReq = serde_json::from_str(&body).unwrap_or_default();
                review::skip(&req.episode_ids, &req.context_ids);
                (200, json!({"ok": true}).to_string(), false)
            }
            (Method::Post, "/api/context/clip") => {
                // In-panel replacement for `scribe clip`.
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                let ticket = v.get("ticket").and_then(|x| x.as_str()).unwrap_or("");
                match context::add_clip(text, ticket) {
                    Ok(_) => (200, json!({"ok": true}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Get, "/api/rules") => {
                (200, json!({ "rules": rules::load() }).to_string(), false)
            }
            (Method::Post, "/api/inbox/refresh") => {
                let r = inbox::refresh();
                (200, json!({"ok": true, "gitlab": r.gitlab, "jira": r.jira,
                    "pagerduty": r.pagerduty, "confluence": r.confluence,
                    "errors": r.errors}).to_string(), false)
            }
            (Method::Get, "/api/desk") => {
                let d = desk::build();
                (200, json!({ "desk": d, "handoff": review::handoff() }).to_string(), false)
            }
            (Method::Get, "/api/desk/jira") => {
                // ?key=OPS-123 — the ticket in depth, fetched when a row is opened.
                let key = url.split_once("key=").map(|(_, k)| k.split('&').next().unwrap_or("")).unwrap_or("");
                let ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                match ok.then(|| atlassian::issue_detail(key)) {
                    Some(Ok(v)) => (200, v.to_string(), false),
                    Some(Err(e)) => (200, json!({"error": e}).to_string(), false),
                    None => (400, json!({"error": "bad key"}).to_string(), false),
                }
            }
            (Method::Post, "/api/rules") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let prefix = v.get("prefix").and_then(|x| x.as_str()).unwrap_or("");
                let tag = v.get("tag").and_then(|x| x.as_str()).unwrap_or("");
                rules::add(prefix, tag);
                (200, json!({ "ok": true }).to_string(), false)
            }
            (Method::Post, "/api/rules/delete") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                rules::remove(
                    v.get("prefix").and_then(|x| x.as_str()).unwrap_or(""),
                    v.get("tag").and_then(|x| x.as_str()).unwrap_or(""),
                );
                (200, json!({ "ok": true }).to_string(), false)
            }
            (Method::Post, "/api/stream/time") => {
                // Manual time adjust for a (possibly collapsed) work group. ids are
                // stream ids like "ep-3". The entered total goes on the first
                // episode and the rest are zeroed, so the group's shown/summed
                // duration equals the entered value. secs < 0 clears all overrides.
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let ids: Vec<i64> = v.get("ids").and_then(|x| x.as_array())
                    .map(|a| a.iter()
                        .filter_map(|i| i.as_str())
                        .filter_map(|s| s.strip_prefix("ep-"))
                        .filter_map(|n| n.parse::<i64>().ok())
                        .collect())
                    .unwrap_or_default();
                let secs = v.get("secs").and_then(|x| x.as_f64()).unwrap_or(-1.0);
                if ids.is_empty() {
                    (400, json!({"ok": false, "error": "no work items"}).to_string(), false)
                } else {
                    for (i, eid) in ids.iter().enumerate() {
                        if secs < 0.0 {
                            timeadj::set(*eid, -1.0); // clear
                        } else {
                            timeadj::set(*eid, if i == 0 { secs } else { 0.0 });
                        }
                    }
                    (200, json!({"ok": true}).to_string(), false)
                }
            }
            (Method::Get, "/api/logbook") => (200, logbook_json().to_string(), false),
            (Method::Post, "/api/logbook/log") => {
                // Log summed time of selected work to the LOCAL ledger only.
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let ids: Vec<String> = v.get("ids").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let ticket = v.get("ticket").and_then(|x| x.as_str()).unwrap_or("");
                match stream::save_local_worklog(&ids, ticket) {
                    Ok((secs, project)) => (200, json!({"ok": true, "secs": secs,
                        "project": project}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/logbook/dispose") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let ids: Vec<String> = v.get("ids").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                match stream::dispose(&ids) {
                    Ok(n) => (200, json!({"ok": true, "n": n}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/logbook/append") => {
                // Add selected work to an existing event (kept broken out by day).
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let entry_id = v.get("entry_id").and_then(|x| x.as_i64()).unwrap_or(-1);
                let ids: Vec<String> = v.get("ids").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                match stream::append_to_entry(entry_id, &ids) {
                    Ok(n) => (200, json!({"ok": true, "n": n}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/logbook/push") => {
                // Push a locally-logged entry to Jira as a real worklog on demand.
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let entry_id = v.get("entry_id").and_then(|x| x.as_i64()).unwrap_or(-1);
                let ticket_in = v.get("ticket").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                match logbook::get(entry_id) {
                    None => (404, json!({"ok": false, "error": "no such entry"}).to_string(), false),
                    Some(e) => {
                        let ticket = if !ticket_in.is_empty() { ticket_in } else { e.ticket.clone() };
                        if ticket.is_empty() {
                            (400, json!({"ok": false, "error": "set a ticket first"}).to_string(), false)
                        } else {
                            let secs = stream::secs_for_ids(&e.episode_ids);
                            let comment = if !e.text().is_empty() { e.text() }
                                else if e.title.is_empty() { "Logged via Scribe".into() } else { e.title.clone() };
                            match crate::publish::post_worklogs(&ticket, secs, &stream::days_for_ids(&e.episode_ids), &comment) {
                                Ok(p) => {
                                    logbook::mark_pushed(entry_id, &ticket);
                                    (200, json!({"ok": true, "ref": p.reference, "url": p.url}).to_string(), false)
                                }
                                Err(err) => (400, json!({"ok": false, "error": err}).to_string(), false),
                            }
                        }
                    }
                }
            }
            (Method::Get, "/api/logbook/timesheet") => {
                let days = url.split('?').nth(1)
                    .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("days=")))
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(7);
                (200, json!({ "md": timesheet_md(days.clamp(1, 90)) }).to_string(), false)
            }
            (Method::Post, "/api/logbook/restore") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let entry_id = v.get("entry_id").and_then(|x| x.as_i64()).unwrap_or(-1);
                let ok = logbook::restore(entry_id);
                (200, json!({"ok": ok}).to_string(), false)
            }
            (Method::Post, "/api/stream/compose") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let ids: Vec<String> = v.get("ids").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("confluence_handoff");
                let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
                let ticket = v.get("ticket").and_then(|x| x.as_str()).unwrap_or("");
                match stream::compose(&ids, kind, title, ticket) {
                    Ok(id) => (200, json!({"ok": true, "draft_id": id}).to_string(), false),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string(), false),
                }
            }
            (Method::Post, "/api/run") => {
                let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                let cmd = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                let cwd = v.get("cwd").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if cmd.is_empty() {
                    (400, json!({"ok": false, "error": "empty command"}).to_string(), false)
                } else {
                    let id = workbench::start_run(&cmd, &cwd);
                    (200, json!({"ok": true, "id": id}).to_string(), false)
                }
            }
            (Method::Post, "/api/process") => {
                let r = pipeline::process();
                (200, json!({"ok": true, "episodes": r.episodes, "drafts": r.drafts}).to_string(), false)
            }
            (Method::Post, "/api/reset") => {
                let all = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("all").and_then(|x| x.as_bool()))
                    .unwrap_or(false);
                let (ev, ep, dr, pr) = model::reset(all);
                (200, json!({"ok": true, "events": ev, "episodes": ep,
                    "drafts": dr, "prompts": pr}).to_string(), false)
            }
            (Method::Post, "/api/bind") => {
                // In-panel replacement for `w <TICKET>` — no CLI needed.
                let ticket = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("ticket").and_then(|x| x.as_str()).map(String::from))
                    .unwrap_or_default();
                if let Some(k) = binding::extract_key(Some(&ticket)).or_else(|| {
                    (!ticket.trim().is_empty()).then(|| ticket.trim().to_string())
                }) {
                    binding::set_binding(Some(&k), "manual", "");
                    (200, json!({"ok": true, "ticket": k}).to_string(), false)
                } else {
                    (400, json!({"ok": false, "error": "empty ticket"}).to_string(), false)
                }
            }
            (Method::Post, "/api/interrupt") => {
                // In-panel replacement for `w ! [reason]`.
                let reason = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("reason").and_then(|x| x.as_str()).map(String::from));
                binding::mark_interrupt(reason.as_deref());
                (200, json!({"ok": true}).to_string(), false)
            }
            (Method::Post, "/api/secrets") => {
                if !same_origin(&request) {
                    (403, json!({"error": "tokens can only be saved from the panel itself"}).to_string(), false)
                } else {
                    match serde_json::from_str::<Value>(&body)
                        .map_err(|e| e.to_string())
                        .and_then(|v| crate::secrets::apply(&v))
                    {
                        Ok(()) => (200, json!({"ok": true}).to_string(), false),
                        Err(e) => (400, json!({"error": e}).to_string(), false),
                    }
                }
            }
            (Method::Post, "/api/secrets/test") => {
                (200, crate::secrets::test().to_string(), false)
            }
            (Method::Post, "/api/config") => {
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    apply_config(&v);
                }
                (200, json!({"ok": true}).to_string(), false)
            }
            _ => {
                // POST /api/desk/act/<system>/<verb> — act on another system.
                if method == Method::Post && parts.len() == 5 && parts[1] == "desk" && parts[2] == "act" {
                    if !same_origin(&request) {
                        (403, json!({"error": "actions can only be taken from the panel itself"}).to_string(), false)
                    } else {
                        let v = serde_json::from_str::<Value>(&body).unwrap_or(json!({}));
                        match desk::act(&format!("{}/{}", parts[3], parts[4]), &v) {
                            Ok(out) => (200, out.to_string(), false),
                            Err(e) => (200, json!({"error": e}).to_string(), false),
                        }
                    }
                } else
                // GET /api/run/<id> — poll a running/finished command.
                if method == Method::Get && parts.len() == 3 && parts[1] == "run" {
                    match workbench::get_run(parts[2]) {
                        Some(r) => (200, json!({
                            "cmd": r.cmd, "cwd": r.cwd, "output": r.output,
                            "exit": r.exit, "done": r.done, "started": r.started
                        }).to_string(), false),
                        None => (404, json!({"error": "no such run"}).to_string(), false),
                    }
                } else
                // /api/drafts/<id>/<action>
                if method == Method::Post && parts.len() == 4 && parts[1] == "drafts" {
                    let did: i64 = parts[2].parse().unwrap_or(-1);
                    match parts[3] {
                        "approve" => {
                            set_draft_status(did, "approved");
                            (200, json!({"ok": true}).to_string(), false)
                        }
                        "discard" => {
                            set_draft_status(did, "discarded");
                            // If this draft was a composed bundle, discarding it
                            // returns its work to the Stream.
                            logbook::restore_by_draft(did);
                            (200, json!({"ok": true}).to_string(), false)
                        }
                        "edit" => {
                            let b = serde_json::from_str::<Value>(&body)
                                .ok()
                                .and_then(|v| v.get("body").and_then(|x| x.as_str()).map(String::from))
                                .unwrap_or_default();
                            edit_draft(did, &b);
                            (200, json!({"ok": true}).to_string(), false)
                        }
                        "publish" => {
                            let p = publish::publish_draft(did, "engineer");
                            if p.ok {
                                (200, json!({"ok": true, "ref": p.reference, "url": p.url,
                                    "target": p.target, "file": p.file}).to_string(), false)
                            } else {
                                (400, json!({"ok": false, "error": p.error}).to_string(), false)
                            }
                        }
                        _ => (404, json!({"error": "not found"}).to_string(), false),
                    }
                } else {
                    (404, json!({"error": "not found"}).to_string(), false)
                }
            }
        };

        let header = if path.ends_with(".js") {
            ctype("application/javascript; charset=utf-8")
        } else if path.ends_with(".css") {
            ctype("text/css; charset=utf-8")
        } else if is_html {
            html_header()
        } else {
            json_header()
        };
        let response = Response::from_string(out)
            .with_status_code(code)
            .with_header(header);
        let _ = request.respond(response);
    }
    Ok(())
}
