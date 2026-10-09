//! The operator's stream: one chronological feed that threads everything so the
//! events make sense in order, and lets the operator select related items,
//! combine them, and publish a single artifact back out.
//!
//! Today it threads what Scribe already has — captured work (episodes) and
//! outbound artifacts (drafts + published records). Inbound *pull* connectors
//! (GitLab MRs, assigned Jira issues, incoming handoffs) add items to the same
//! feed later; `dir: "in"` is reserved for them.

use serde::Serialize;

use crate::model::{self, now_ts, Draft};

#[derive(Serialize, Default)]
pub struct StreamItem {
    pub id: String,     // "ep-3", "dr-5", "pub-OPS-12"
    pub ts: f64,
    pub kind: String,   // episode | jira_worklog | jira_new | confluence_handoff | published
    pub dir: String,    // work | out | in
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Git repo name for work items — the unit the Stream groups "projects" by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Active duration of this work (end − start), seconds. 0 for non-work.
    #[serde(default)]
    pub dur: f64,
}

/// Build the merged, newest-first stream.
pub fn build() -> Vec<StreamItem> {
    let episodes = model::load_episodes();
    let mut items: Vec<StreamItem> = Vec::new();

    // Captured work. Duration is the episode span unless the operator has set a
    // manual override (timeadj) for that episode. Episodes recorded in the
    // logbook (combined / logged / disposed / sent) are "handled" and hidden —
    // they live on the Logged screen now.
    let overrides = crate::timeadj::load();
    let handled = crate::logbook::handled_ids();
    for e in &episodes {
        if handled.contains(&e.id) {
            continue;
        }
        let dur = overrides
            .get(&e.id)
            .copied()
            .unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0));
        items.push(StreamItem {
            id: format!("ep-{}", e.id),
            ts: e.end_ts,
            kind: "episode".into(),
            dir: "work".into(),
            title: format!(
                "{} · {}",
                cap_first(&e.activity),
                e.ticket.clone().unwrap_or_else(|| "unbound".into())
            ),
            detail: e.summary.clone(),
            ticket: e.ticket.clone(),
            status: Some(e.kind.clone()),
            url: None,
            draft_id: None,
            cwd: e.cwd.clone(),
            repo: e.repo.clone(),
            branch: e.branch.clone(),
            commands: e.commands.clone(),
            commits: e.commits.clone(),
            tags: crate::rules::tags_for(e.cwd.as_deref(), e.repo.as_deref()),
            dur,
        });
    }

    // Outbound drafts (pending), timestamped by their source episode. Composed
    // bundles and drafts whose work is already handled live on the Logged screen,
    // not here — the Stream keeps only the per-episode draft suggestions for work
    // that still needs attention.
    let latest_ep = episodes.iter().map(|e| e.end_ts).fold(0.0, f64::max);
    for d in model::load_drafts() {
        if d.status == "discarded" {
            continue;
        }
        if d.composed || d.episode_id.map(|eid| handled.contains(&eid)).unwrap_or(false) {
            continue;
        }
        let ts = d
            .episode_id
            .and_then(|eid| episodes.iter().find(|e| e.id == eid).map(|e| e.end_ts))
            .unwrap_or(latest_ep);
        items.push(StreamItem {
            id: format!("dr-{}", d.id),
            ts,
            kind: d.kind.clone(),
            dir: "out".into(),
            title: d.title.clone(),
            detail: d.body.lines().next().unwrap_or("").to_string(),
            ticket: d.ref_key.clone(),
            status: Some(d.status.clone()),
            url: None,
            draft_id: Some(d.id),
            ..Default::default()
        });
    }

    // Inbound artifacts (cached): MRs and assigned issues flowing TO the operator.
    for it in crate::inbox::load().items {
        items.push(StreamItem {
            id: format!("in-{}-{}", it.source, it.url),
            ts: it.updated_ts,
            kind: it.kind.clone(),
            dir: "in".into(),
            title: it.title.clone(),
            detail: it.author.clone().unwrap_or_default(),
            ticket: it.ticket.clone(),
            status: Some("inbox".into()),
            url: if it.url.is_empty() { None } else { Some(it.url.clone()) },
            ..Default::default()
        });
    }

    // Recently-sent confirmations: handled bundles that went out (pushed to Jira
    // or a published draft) in the last 12h show as muted "Sent ✓" markers, so
    // the operator gets confirmation in the feed without the Stream re-cluttering.
    let cutoff = now_ts() - 12.0 * 3600.0;
    for e in crate::logbook::load().entries {
        let went_out = e.pushed_jira || e.kind == "sent";
        if !went_out || e.updated_ts < cutoff {
            continue;
        }
        let ticket = if e.ticket.is_empty() { None } else { Some(e.ticket.clone()) };
        items.push(StreamItem {
            id: format!("sent-{}", e.id),
            ts: e.updated_ts,
            kind: "published".into(),
            dir: "out".into(),
            title: e.title.clone(),
            detail: format!("sent{}", ticket.as_ref().map(|t| format!(" as {t}")).unwrap_or_default()),
            ticket,
            status: Some("published".into()),
            ..Default::default()
        });
    }

    items.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
    items
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Combine selected stream items into one new draft artifact (back out).
/// `ids` are stream item ids; only episodes contribute content today.
/// `kind` is "confluence_handoff" or "jira_worklog". Returns the new draft id.
pub fn compose(ids: &[String], kind: &str, title: &str, ticket: &str) -> Result<i64, String> {
    let episodes = model::load_episodes();
    let picked: Vec<_> = ids
        .iter()
        .filter_map(|id| id.strip_prefix("ep-"))
        .filter_map(|n| n.parse::<i64>().ok())
        .filter_map(|eid| episodes.iter().find(|e| e.id == eid))
        .collect();
    if picked.is_empty() {
        return Err("select at least one work item (episode) to combine".into());
    }

    let overrides = crate::timeadj::load();
    let secs_of = |e: &model::Episode| -> f64 {
        overrides
            .get(&e.id)
            .copied()
            .unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0))
    };

    let (draft_kind, target, draft_title, body, ref_key, worklog_secs) = match kind {
        "jira_worklog" => {
            // Prefer the given ticket, else the single common ticket of the picks.
            let t = if !ticket.trim().is_empty() {
                ticket.trim().to_string()
            } else {
                let tickets: std::collections::BTreeSet<_> =
                    picked.iter().filter_map(|e| e.ticket.clone()).collect();
                if tickets.len() == 1 {
                    tickets.into_iter().next().unwrap()
                } else {
                    return Err("a combined worklog needs one ticket — set it, or pick items that share a ticket".into());
                }
            };
            let total: f64 = picked.iter().map(|e| secs_of(e)).sum();
            let mut lines = vec![
                format!("*Combined worklog* — {}", fmt_dur(total)),
                String::new(),
            ];
            for e in &picked {
                lines.push(format!("* ({}) {} [{}]", e.activity, e.summary, fmt_dur(secs_of(e))));
            }
            (
                "jira_worklog".to_string(),
                "Jira".to_string(),
                format!("Worklog on {t} — {}", fmt_dur(total)),
                lines.join("\n"),
                Some(t),
                total,
            )
        }
        _ => {
            // Confluence handoff combining the selected work.
            let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let heading = if title.trim().is_empty() {
                format!("Combined handoff — {day}")
            } else {
                title.trim().to_string()
            };
            let mut lines = vec![format!("h1. {heading}"), String::new(), "h2. Work".to_string()];
            for e in &picked {
                lines.push(format!(
                    "* *{}* ({}): {}",
                    e.ticket.clone().unwrap_or_else(|| "unbound".into()),
                    e.activity,
                    e.summary
                ));
            }
            lines.push(String::new());
            lines.push("h2. State / open items".to_string());
            lines.push("_Review and edit before publishing._".to_string());
            (
                "confluence_handoff".to_string(),
                "Confluence".to_string(),
                heading,
                lines.join("\n"),
                None,
                0.0,
            )
        }
    };

    // Append the composed draft.
    let mut st = model::load_state();
    st.draft_seq += 1;
    let id = st.draft_seq;
    model::save_state(&st);

    let mut drafts = model::load_drafts();
    drafts.push(Draft {
        id,
        episode_id: None,
        kind: draft_kind.clone(),
        target,
        title: draft_title.clone(),
        body,
        ref_key: ref_key.clone(),
        status: "draft".into(),
        composed: true,
        dur_secs: worklog_secs,
    });
    model::save_drafts(&drafts);

    // Record the bundle in the logbook so its work drops out of the Stream and
    // shows on the Logged screen.
    let eids: Vec<i64> = picked.iter().map(|e| e.id).collect();
    let project = most_common(picked.iter().filter_map(|e| e.cwd.clone()));
    let tag = picked
        .iter()
        .flat_map(|e| crate::rules::tags_for(e.cwd.as_deref(), e.repo.as_deref()))
        .next()
        .unwrap_or_default();
    let lkind = if draft_kind == "jira_worklog" { "worklog" } else { "handoff" };
    crate::logbook::add(
        lkind,
        &draft_title,
        ref_key.as_deref().unwrap_or(""),
        &tag,
        &project,
        eids,
        Some(id),
        worklog_secs,
        false,
    );
    let _ = now_ts();
    Ok(id)
}

/// Format a duration (seconds) like the UI: "2h 4m" / "19m" / "45s".
fn fmt_dur(s: f64) -> String {
    let s = s.round() as i64;
    if s < 60 {
        return format!("{s}s");
    }
    let m = (s as f64 / 60.0).round() as i64;
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h {}m", m / 60, m % 60)
}

/// Push the summed time of the selected work items to the LOCAL worklog ledger
/// (never to Jira). Returns (seconds_logged, project_headline).
pub fn save_local_worklog(ids: &[String], ticket: &str) -> Result<(f64, String), String> {
    let episodes = model::load_episodes();
    let picked: Vec<_> = ids
        .iter()
        .filter_map(|id| id.strip_prefix("ep-"))
        .filter_map(|n| n.parse::<i64>().ok())
        .filter_map(|eid| episodes.iter().find(|e| e.id == eid))
        .collect();
    if picked.is_empty() {
        return Err("select at least one work item to log".into());
    }
    let overrides = crate::timeadj::load();
    let secs: f64 = picked
        .iter()
        .map(|e| {
            overrides
                .get(&e.id)
                .copied()
                .unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0))
        })
        .sum();

    // Project = most common cwd of the picks; tag = first tag found.
    let project = most_common(picked.iter().filter_map(|e| e.cwd.clone()));
    let tag = picked
        .iter()
        .flat_map(|e| crate::rules::tags_for(e.cwd.as_deref(), e.repo.as_deref()))
        .next()
        .unwrap_or_default();
    let ticket = if !ticket.trim().is_empty() {
        ticket.trim().to_string()
    } else {
        picked.iter().filter_map(|e| e.ticket.clone()).next().unwrap_or_default()
    };
    let title = if !ticket.is_empty() {
        format!("Logged time · {ticket}")
    } else if !project.is_empty() {
        format!("Logged time · {}", project)
    } else {
        "Logged time".to_string()
    };

    crate::logbook::add(
        "logged",
        &title,
        &ticket,
        &tag,
        &project,
        picked.iter().map(|e| e.id).collect(),
        None,
        secs,
        false,
    );
    Ok((secs, project))
}

/// Summed time (seconds, with manual overrides applied) for a set of episode ids.
pub fn secs_for_ids(ids: &[i64]) -> f64 {
    let eps = model::load_episodes();
    let ov = crate::timeadj::load();
    ids.iter()
        .filter_map(|id| eps.iter().find(|e| e.id == *id))
        .map(|e| ov.get(&e.id).copied().unwrap_or_else(|| (e.end_ts - e.start_ts).max(0.0)))
        .sum()
}

/// Dispose of selected work items — mark them handled (dismissed) so they leave
/// the Stream. Returns how many episodes were disposed.
pub fn dispose(ids: &[String]) -> Result<usize, String> {
    let episodes = model::load_episodes();
    let picked: Vec<_> = ids
        .iter()
        .filter_map(|id| id.strip_prefix("ep-"))
        .filter_map(|n| n.parse::<i64>().ok())
        .filter_map(|eid| episodes.iter().find(|e| e.id == eid))
        .collect();
    if picked.is_empty() {
        return Err("select at least one work item to dispose".into());
    }
    let project = most_common(picked.iter().filter_map(|e| e.cwd.clone()));
    let eids: Vec<i64> = picked.iter().map(|e| e.id).collect();
    crate::logbook::add("disposed", "Dismissed", "", "", &project, eids, None, 0.0, false);
    Ok(picked.len())
}

/// Append selected work items to an existing logbook entry (keeps the event, adds
/// more work — the Logged screen keeps it broken out by day).
pub fn append_to_entry(entry_id: i64, ids: &[String]) -> Result<usize, String> {
    let eids: Vec<i64> = ids
        .iter()
        .filter_map(|id| id.strip_prefix("ep-"))
        .filter_map(|n| n.parse::<i64>().ok())
        .collect();
    if eids.is_empty() {
        return Err("select at least one work item to add".into());
    }
    let add_secs = secs_for_ids(&eids);
    if crate::logbook::append(entry_id, &eids, add_secs) {
        Ok(eids.len())
    } else {
        Err("no such event".into())
    }
}

fn most_common<I: Iterator<Item = String>>(it: I) -> String {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for s in it {
        *counts.entry(s).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(s, _)| s)
        .unwrap_or_default()
}
