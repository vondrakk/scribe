//! Draft artifact generation (PRD §9). Every draft requires approval.
//!
//!   * planned episode + ticket -> Jira worklog/comment update
//!   * interrupt / unaccounted   -> draft NEW Jira ticket (unplanned work)
//!   * end of session            -> Confluence handoff doc (the flagship win)
//!
//! Nothing here publishes. On reprocess, drafts the engineer already acted on
//! (approved / published / discarded) keep their status, matched by signature.

use chrono::Utc;

use crate::model::{self, Draft, Episode, Event};

/// Identity for carrying engineer-acted status across reprocess. Deliberately
/// excludes ref_key: publishing *assigns* a ref (e.g. CONF-1001), so including
/// it would make a published draft fail to match its regenerated twin. The
/// title already encodes the identity (ticket key / handoff date).
fn sig(d: &Draft) -> (String, String) {
    (d.kind.clone(), d.title.clone())
}

fn gen_for_episode(ep: &Episode) -> Option<Draft> {
    if ep.kind == "planned" {
        if let Some(t) = &ep.ticket {
            let body = format!(
                "*Worklog ({})*\n\n{}\n\n_Captured by Scribe from local activity. \
                 Posts as the engineer on approval._",
                ep.activity, ep.summary
            );
            return Some(Draft {
                id: 0,
                episode_id: Some(ep.id),
                kind: "jira_worklog".into(),
                target: "Jira".into(),
                title: format!("Worklog on {}", t),
                body,
                ref_key: Some(t.clone()),
                status: "draft".into(),
                composed: false,
                dur_secs: 0.0,
            });
        }
    } else {
        // interrupt | unaccounted
        let label = if ep.kind == "interrupt" { "Interrupt" } else { "Unaccounted" };
        let body = format!(
            "h2. Summary\n{}\n\nh2. Type\nUnplanned / {} work detected by Scribe.\n\n\
             h2. Suggested fields\n* Activity: {}\n* Source: automatic seam detection",
            ep.summary,
            label.to_lowercase(),
            ep.activity
        );
        return Some(Draft {
            id: 0,
            episode_id: Some(ep.id),
            kind: "jira_new".into(),
            target: "Jira".into(),
            title: format!("[Draft] {} work: {}", label, ep.activity),
            body,
            ref_key: None,
            status: "draft".into(),
            composed: false,
            dur_secs: 0.0,
        });
    }
    None
}

fn gen_handoff(episodes: &[Episode]) -> Option<Draft> {
    if episodes.is_empty() {
        return None;
    }
    let day = Utc::now().format("%Y-%m-%d").to_string();
    let mut lines: Vec<String> = vec![format!("h1. Handoff — {}", day), String::new()];

    let planned: Vec<&Episode> = episodes.iter().filter(|e| e.kind == "planned").collect();
    let interrupts: Vec<&Episode> = episodes.iter().filter(|e| e.kind == "interrupt").collect();
    let unacc: Vec<&Episode> = episodes.iter().filter(|e| e.kind == "unaccounted").collect();

    lines.push("h2. Planned work".into());
    if planned.is_empty() {
        lines.push("_None bound._".into());
    } else {
        for e in &planned {
            lines.push(format!(
                "* *{}* ({}): {}",
                e.ticket.clone().unwrap_or_default(),
                e.activity,
                e.summary
            ));
        }
    }
    lines.push(String::new());
    lines.push("h2. Interrupts".into());
    if interrupts.is_empty() {
        lines.push("_None._".into());
    } else {
        for e in &interrupts {
            lines.push(format!("* ({}): {}", e.activity, e.summary));
        }
    }
    if !unacc.is_empty() {
        lines.push(String::new());
        lines.push("h2. Needs triage (Unaccounted)".into());
        for e in &unacc {
            lines.push(format!("* ({}): {}", e.activity, e.summary));
        }
    }
    lines.push(String::new());
    lines.push("h2. Current state / open items".into());
    lines.push("_Review and edit before publishing._".into());

    Some(Draft {
        id: 0,
        episode_id: None,
        kind: "confluence_handoff".into(),
        target: "Confluence".into(),
        title: format!("Async handoff — {}", day),
        body: lines.join("\n"),
        ref_key: None,
        status: "draft".into(),
        composed: false,
        dur_secs: 0.0,
    })
}

/// Regenerate drafts from episodes, preserving engineer-acted status.
pub fn rebuild(built: &[(Episode, Vec<Event>)], episodes: &[Episode]) -> usize {
    // Operator-composed drafts are not episode-derived — keep them verbatim and
    // out of the signature matching entirely.
    let all = model::load_drafts();
    let composed: Vec<Draft> = all.iter().filter(|d| d.composed).cloned().collect();

    // The rest are matched by signature: carry over the id (so ids stay stable
    // across reprocess) and ref_key, and inherit *status* only from
    // engineer-acted drafts (unacted ones reset to a fresh "draft").
    let mut existing: Vec<Draft> = all.into_iter().filter(|d| !d.composed).collect();
    let mut consumed = vec![false; existing.len()];

    let mut candidates: Vec<Draft> = Vec::new();
    for (ep, _ev) in built {
        if let Some(d) = gen_for_episode(ep) {
            candidates.push(d);
        }
    }
    if let Some(h) = gen_handoff(episodes) {
        candidates.push(h);
    }

    // Carry over id/ref_key (always) and status (only if acted) by signature.
    for cand in candidates.iter_mut() {
        let csig = sig(cand);
        for (i, e) in existing.iter().enumerate() {
            if !consumed[i] && sig(e) == csig {
                cand.id = e.id;
                if e.status != "draft" {
                    cand.status = e.status.clone();
                    cand.ref_key = e.ref_key.clone();
                }
                consumed[i] = true;
                break;
            }
        }
    }

    // Assign fresh ids to genuinely new drafts.
    let mut st = model::load_state();
    for cand in candidates.iter_mut() {
        if cand.id == 0 {
            st.draft_seq += 1;
            cand.id = st.draft_seq;
        }
    }

    // Keep acted drafts that no longer regenerate (e.g. a published record whose
    // episode shifted) so history is never lost. Unconsumed *unacted* drafts are
    // dropped — they are stale and will be freshly regenerated if still valid.
    let mut final_list = candidates;
    for (i, e) in existing.drain(..).enumerate() {
        if !consumed[i] && e.status != "draft" {
            final_list.push(e);
        }
    }
    // Operator-composed drafts are always kept.
    final_list.extend(composed);

    model::save_state(&st);
    model::save_drafts(&final_list);
    final_list.iter().filter(|d| d.status != "discarded").count()
}
