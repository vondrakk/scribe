//! Publishing: mock (default) or real Atlassian Cloud.
//!
//! `publish_backend = "mock"` writes an audit record to published/*.json with a
//! fake ref (offline, no credentials). `publish_backend = "atlassian"` posts the
//! approved artifact to real Jira/Confluence *as the engineer* via their own
//! OAuth token (src/atlassian.rs) — PRD §3.3, §7.4. Either way an audit record
//! is written to published/ and the draft is marked published with the real ref.

use serde_json::json;

use crate::model::{self, now_ts};
use crate::{atlassian, config};

pub struct Published {
    pub ok: bool,
    pub error: Option<String>,
    pub target: String,
    pub reference: String,
    pub url: String,
    pub file: String,
}

fn err(target: &str, msg: String) -> Published {
    Published {
        ok: false,
        error: Some(msg),
        target: target.to_string(),
        reference: String::new(),
        url: String::new(),
        file: String::new(),
    }
}

fn mock_ref(target: &str) -> String {
    let mut st = model::load_state();
    st.publish_seq += 1;
    let seq = st.publish_seq;
    model::save_state(&st);
    if target == "Jira" {
        format!("OPS-{}", 9000 + seq)
    } else {
        format!("CONF-{}", 1000 + seq)
    }
}

/// Noon on `day` (local) as unix seconds — None for today, which is logged as now.
fn day_start(day: &str) -> Option<f64> {
    use chrono::TimeZone;
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    if d == chrono::Local::now().date_naive() {
        return None;
    }
    chrono::Local.from_local_datetime(&d.and_hms_opt(12, 0, 0)?).single().map(|t| t.timestamp() as f64)
}

/// Log `total` seconds on a ticket as one worklog per day worked, dated on
/// that day. The write-up goes on the latest day; earlier days point at it.
/// `days` holds the measured split; it is scaled to `total` (which the
/// engineer may have corrected).
pub fn post_worklogs(key: &str, total: f64, days: &[model::DaySecs], body: &str) -> Result<atlassian::Posted, String> {
    let days: Vec<&model::DaySecs> = days.iter().filter(|d| d.secs >= 60.0).collect();
    if days.len() < 2 {
        return atlassian::add_worklog(key, total, body, days.first().and_then(|d| day_start(&d.day)));
    }
    let measured: f64 = days.iter().map(|d| d.secs).sum();
    let last = days.len() - 1;
    let mut posted = None;
    for (n, d) in days.iter().enumerate() {
        let note = if n == last { body.to_string() } else { format!("Part of the work written up on {}.", days[last].day) };
        match atlassian::add_worklog(key, total * d.secs / measured, &note, day_start(&d.day)) {
            Ok(p) => posted = Some(p),
            Err(e) if n == 0 => return Err(e),
            Err(e) => return Err(format!("Jira took {n} of {} days and then failed: {e}", days.len())),
        }
    }
    posted.ok_or_else(|| "nothing to log".to_string())
}

pub fn publish_draft(draft_id: i64, engineer: &str) -> Published {
    let mut drafts = model::load_drafts();
    let idx = match drafts.iter().position(|d| d.id == draft_id) {
        Some(i) => i,
        None => return err("", format!("no draft {}", draft_id)),
    };
    if drafts[idx].status != "approved" && drafts[idx].status != "draft" {
        return err(
            &drafts[idx].target,
            format!("draft {} is {}, not publishable", draft_id, drafts[idx].status),
        );
    }

    let d = drafts[idx].clone();
    let cfg = config::load();
    let real = cfg.publish_backend == "atlassian";

    // Produce (reference, url) by posting for real, or minting a mock ref.
    let (reference, url) = if real {
        let posted = match d.kind.as_str() {
            "jira_worklog" => {
                let key = match &d.ref_key {
                    Some(k) => k,
                    None => return err(&d.target, "worklog draft has no ticket key".into()),
                };
                if d.dur_secs > 0.0 {
                    // Real worklog: push the summed time (with the body as its note).
                    post_worklogs(key, d.dur_secs, &d.days, &d.body)
                } else {
                    // No duration — fall back to a comment (universal, no time dep).
                    atlassian::add_comment(key, &d.body)
                }
            }
            "jira_new" => {
                let summary = d.title.trim_start_matches("[Draft] ").to_string();
                atlassian::create_issue(&summary, &d.body)
            }
            "confluence_handoff" => atlassian::create_page(&d.title, &d.body),
            other => Err(format!("don't know how to publish kind '{other}'")),
        };
        match posted {
            Ok(p) => (p.reference, p.url),
            Err(e) => return err(&d.target, e),
        }
    } else {
        let reference = d.ref_key.clone().unwrap_or_else(|| mock_ref(&d.target));
        let url = if d.target == "Jira" {
            format!("https://example.atlassian.net/browse/{}", reference)
        } else {
            format!("https://example.atlassian.net/wiki/pages/{}", reference)
        };
        (reference, url)
    };

    // Audit record (both paths), draft marked published.
    let payload = json!({
        "target": d.target,
        "kind": d.kind,
        "days": d.days,
        "ref": reference,
        "url": url,
        "title": d.title,
        "body": d.body,
        "attributed_to": engineer,
        "posted_as_engineer": true,
        "backend": if real { "atlassian" } else { "mock" },
        "note": if real { "Published via per-user OAuth token." }
                else { "MOCK publish — no credential used." },
        "ts": now_ts(),
    });
    let fname = format!("{}_{}_{}.json", now_ts() as i64, d.target, reference);
    let fpath = config::published_dir().join(&fname);
    let _ = std::fs::write(&fpath, serde_json::to_string_pretty(&payload).unwrap_or_default());

    drafts[idx].status = "published".into();
    drafts[idx].ref_key = Some(reference.clone());
    model::save_drafts(&drafts);

    // Mark the sent work handled: updates the composed bundle's entry if there is
    // one, else records a new logbook entry so the sent draft's work leaves the
    // Stream and appears on the Logged screen.
    crate::logbook::mark_published(
        draft_id,
        &d.kind,
        &d.title,
        &reference,
        d.episode_id.map(|e| vec![e]).unwrap_or_default(),
        d.dur_secs,
    );

    Published {
        ok: true,
        error: None,
        target: d.target,
        reference,
        url,
        file: fpath.to_string_lossy().to_string(),
    }
}
