//! Active-ticket binding model and the `w` verbs (PRD §5).
//!
//! Binding priority: branch key (near-automatic) > manual `w OPS-1234` > commit key.
//! The current binding is mirrored to a plain-text file so the shell prompt
//! segment reads it with zero process spawn.

use regex::Regex;

use crate::config;
use crate::model::{self, Binding, Event};

pub fn key_re() -> Regex {
    Regex::new(&config::load().ticket_key_pattern).unwrap_or_else(|_| {
        Regex::new(r"[A-Z][A-Z0-9]+-\d+").unwrap()
    })
}

pub fn extract_key(text: Option<&str>) -> Option<String> {
    let t = text?;
    key_re().find(t).map(|m| m.as_str().to_string())
}

pub fn get_binding() -> Option<Binding> {
    model::load_state().binding
}

fn mirror_to_file(b: &Option<Binding>) {
    let txt = b.as_ref().map(|x| x.ticket.clone()).unwrap_or_default();
    let _ = std::fs::write(config::binding_file(), txt);
}

pub fn set_binding(ticket: Option<&str>, source: &str, note: &str) {
    let mut st = model::load_state();
    let b = ticket.map(|t| Binding {
        ticket: t.to_string(),
        source: source.to_string(),
        since: model::now_ts(),
        note: note.to_string(),
    });
    st.binding = b.clone();
    model::save_state(&st);
    mirror_to_file(&b);
    model::append_event(Event {
        kind: "bind".into(),
        action: Some(if ticket.is_some() { "bind" } else { "unbind" }.into()),
        key: ticket.map(|s| s.to_string()),
        msg: Some(format!("source={} {}", source, note).trim().to_string()),
        ..Default::default()
    });
}

/// Recent distinct ticket keys seen in capture — the `w ?` list. (In production
/// this would be the engineer's in-progress assigned sprint issues from Jira.)
pub fn candidate_tickets(limit: usize) -> Vec<(String, f64, usize)> {
    use std::collections::HashMap;
    let mut last: HashMap<String, f64> = HashMap::new();
    let mut n: HashMap<String, usize> = HashMap::new();
    for ev in model::read_events() {
        if let Some(k) = &ev.key {
            let e = last.entry(k.clone()).or_insert(0.0);
            if ev.ts > *e {
                *e = ev.ts;
            }
            *n.entry(k.clone()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, f64, usize)> = last
        .into_iter()
        .map(|(k, t)| {
            let c = *n.get(&k).unwrap_or(&0);
            (k, t, c)
        })
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(limit);
    v
}

pub const DEFAULT_INTERRUPT_MSG: &str = "manual w! interrupt";

/// `w !` — flag the current activity as an unplanned interrupt. Records a seam
/// event; the pipeline splits a distinct interrupt episode at this boundary.
/// `reason` is the engineer's own words for what was asked, and by whom.
pub fn mark_interrupt(reason: Option<&str>) {
    let cur = get_binding();
    model::append_event(Event {
        kind: "seam".into(),
        action: Some("interrupt".into()),
        key: cur.map(|b| b.ticket),
        msg: Some(
            reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or(DEFAULT_INTERRUPT_MSG)
                .to_string(),
        ),
        ..Default::default()
    });
    set_binding(None, "interrupt", "");
}
