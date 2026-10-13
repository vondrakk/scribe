//! Conversation context — the "why" behind captured work.
//!
//! Scribe sees commands and commits, but the request, the diagnosis and the
//! decision usually happen in chat. With no Slack API access, context gets in
//! two ways, both local and both as the engineer:
//!
//!   * clip         — the engineer copies a thread and runs `scribe clip` (or
//!                    pastes it in the panel). Deliberate, so it is trusted.
//!   * notification — an opt-in listener records chat notifications the desktop
//!                    already shows (sender, channel, preview). Passive, so it
//!                    is only ever *offered* on a card, never auto-included.
//!
//! Items live in context.json under SCRIBE_HOME, expire with raw capture
//! (§7.1), and leave the machine only inside an artifact the engineer sends.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::binding;
use crate::config;
use crate::db;
use crate::model::now_ts;

/// Stored text is capped: a clip is a thread, not an archive.
const MAX_CLIP_CHARS: usize = 12_000;
const MAX_PREVIEW_CHARS: usize = 500;
const MAX_ITEMS: usize = 500;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ContextItem {
    pub id: i64,
    pub ts: f64,
    pub source: String, // clip | notification
    #[serde(default)]
    pub app: String,
    /// People involved (comma-separated), best-effort.
    #[serde(default)]
    pub who: String,
    #[serde(default)]
    pub channel: String,
    /// The pasted thread, or the notification preview.
    pub text: String,
    /// Distilled problem / decision / outcome (LLM backends only; else empty).
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub ticket: String,
    /// Already sent with (or dismissed from) a card — hidden from review.
    #[serde(default)]
    pub used: bool,
    /// The review card the engineer filed this under, when it isn't a ticket's.
    #[serde(default)]
    pub pin: String,
    /// Filed by hand, so it is trusted like a clip and goes into the update.
    #[serde(default)]
    pub filed: bool,
}

#[derive(Default)]
struct Store {
    seq: i64,
    items: Vec<ContextItem>,
}

// The listener thread and the request thread both write the store.
static LOCK: Mutex<()> = Mutex::new(());

fn load_store() -> Store {
    let items: Vec<ContextItem> = db::all(db::CONTEXT);
    let top = items.iter().map(|i| i.id).max().unwrap_or(0);
    // The counter outlives swept items, so ids are never reused.
    Store { seq: db::doc::<i64>("context_seq").max(top), items }
}

fn save_store(st: &Store) {
    let rows: Vec<db::Row> = st
        .items
        .iter()
        .filter_map(|i| Some((i.id, i.ts, i.ticket.clone(), serde_json::to_string(i).ok()?)))
        .collect();
    db::replace(db::CONTEXT, &rows);
    db::put_doc("context_seq", &st.seq);
}

pub fn load() -> Vec<ContextItem> {
    load_store().items
}

fn push(mut item: ContextItem) -> ContextItem {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load_store();
    st.seq += 1;
    item.id = st.seq;
    st.items.push(item.clone());
    if st.items.len() > MAX_ITEMS {
        let drop = st.items.len() - MAX_ITEMS;
        st.items.drain(0..drop);
    }
    save_store(&st);
    item
}

pub fn mark_used(ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load_store();
    for it in st.items.iter_mut() {
        if ids.contains(&it.id) {
            it.used = true;
        }
    }
    save_store(&st);
}

/// File an item under a ticket, or under one review card (`pin`), overriding
/// where time and mentions would have put it. Brings a dismissed item back.
pub fn assign(id: i64, ticket: &str, pin: &str) -> Result<(), String> {
    let ticket = match ticket.trim() {
        "" => String::new(),
        t => binding::extract_key(Some(t)).ok_or_else(|| format!("{t} doesn't look like a ticket key"))?,
    };
    if ticket.is_empty() && pin.trim().is_empty() {
        return Err("choose where it belongs".into());
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load_store();
    let it = st.items.iter_mut().find(|i| i.id == id).ok_or("that message is no longer kept")?;
    it.ticket = ticket;
    it.pin = pin.trim().to_string();
    it.filed = true;
    it.used = false;
    save_store(&st);
    Ok(())
}

/// Drop items older than `cutoff` (same retention window as raw capture).
pub fn sweep(cutoff: f64) -> usize {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load_store();
    let n = st.items.len();
    st.items.retain(|i| i.ts >= cutoff);
    let dropped = n - st.items.len();
    if dropped > 0 {
        save_store(&st);
    }
    dropped
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// --- clips -----------------------------------------------------------------

/// Split a copied Slack thread into (author, message) pairs. An author sits on
/// their own line, followed by a line that is just a timestamp ("2:02 PM" /
/// "14:02"). Empty when the text does not have that shape.
pub fn clip_messages(text: &str) -> Vec<(String, String)> {
    let time = Regex::new(r"^\s*\d{1,2}:\d{2}(\s?[AaPp][Mm])?\s*$").unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let name = lines[i].trim();
        let is_header = i + 1 < lines.len()
            && time.is_match(lines[i + 1])
            && !name.is_empty()
            && name.chars().count() <= 40;
        if is_header {
            out.push((name.to_string(), String::new()));
            i += 2;
            continue;
        }
        if let Some((_, msg)) = out.last_mut() {
            if !msg.is_empty() {
                msg.push(' ');
            }
            msg.push_str(lines[i].trim());
        }
        i += 1;
    }
    out
}

fn clip_people(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in clip_messages(text) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Store a pasted thread. `ticket` is optional; a key mentioned in the text is
/// used when it is omitted.
pub fn add_clip(text: &str, ticket: &str) -> Result<ContextItem, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to add — the text is empty".into());
    }
    let link = Regex::new(r"https://[A-Za-z0-9.-]+\.slack\.com/archives/\S+")
        .unwrap()
        .find(text)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let channel = Regex::new(r"(^|\s)(#[a-z][a-z0-9_-]+)")
        .unwrap()
        .captures(text)
        .map(|c| c[2].to_string())
        .unwrap_or_default();
    let ticket = if !ticket.trim().is_empty() {
        binding::extract_key(Some(ticket)).unwrap_or_else(|| ticket.trim().to_string())
    } else {
        binding::extract_key(Some(text)).unwrap_or_default()
    };
    let stored = truncate(text, MAX_CLIP_CHARS);
    let summary = crate::llm::distill(&stored).unwrap_or_default();
    Ok(push(ContextItem {
        ts: now_ts(),
        source: "clip".into(),
        app: "slack".into(),
        who: clip_people(text).join(", "),
        channel,
        text: stored,
        summary,
        link,
        ticket,
        ..Default::default()
    }))
}

/// Read the desktop clipboard with whatever tool is present.
pub fn read_clipboard() -> Result<String, String> {
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
    } else {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    };
    for (bin, args) in tools {
        if let Ok(out) = Command::new(bin).args(*args).stderr(Stdio::null()).output() {
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
        }
    }
    Err("could not read the clipboard (need wl-paste, xclip or xsel) — pipe the text in instead: scribe clip -".into())
}

// --- notifications ---------------------------------------------------------

fn app_matches(app: &str, body: &str, needles: &str) -> bool {
    let app = app.to_lowercase();
    if app == "scribe" {
        return false;
    }
    // Browser-delivered notifications carry the origin on the body's first line.
    let origin = body.lines().next().unwrap_or("").to_lowercase();
    needles
        .split(',')
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .any(|n| app.contains(&n) || origin.contains(&n))
}

fn record_notification(app: &str, summary: &str, body: &str) {
    let cfg = config::load();
    // Re-read each time so switching it off in Settings takes effect at once.
    if !cfg.capture_notifications || !app_matches(app, body, &cfg.notification_apps) {
        return;
    }
    let channel = Regex::new(r"#[A-Za-z0-9_-]+")
        .unwrap()
        .find(summary)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let who = match summary.split(" from ").nth(1) {
        Some(w) => w.trim().to_string(),
        None => match body.split_once(": ") {
            Some((w, _)) if w.chars().count() <= 40 && !w.contains('\n') => w.trim().to_string(),
            _ => String::new(),
        },
    };
    let text = truncate(format!("{}\n{}", summary.trim(), body.trim()).trim(), MAX_PREVIEW_CHARS);
    // A re-sent toast (same text within a few seconds) is one notification.
    let now = now_ts();
    if load().iter().rev().take(5).any(|i| i.source == "notification" && i.text == text && now - i.ts < 10.0) {
        return;
    }
    let ticket = binding::extract_key(Some(&text)).unwrap_or_default();
    push(ContextItem {
        ts: now,
        source: "notification".into(),
        app: app.to_string(),
        who,
        channel,
        text,
        ticket,
        ..Default::default()
    });
}

/// Parses `dbus-monitor` text output for Notify calls. The call's top-level
/// string arguments are, in order: app_name, app_icon, summary, body.
#[derive(Default)]
struct NotifyParser {
    collecting: bool,
    strings: Vec<String>,
    partial: Option<String>,
    /// The last string closed on a `"` that may have been part of its text.
    reopenable: bool,
}

/// A line that begins a new argument or a new message, as opposed to the
/// continuation of a multi-line string.
fn is_arg_start(line: &str) -> bool {
    const HEADERS: [&str; 4] = ["method call ", "method return ", "signal ", "error "];
    const ARGS: [&str; 8] =
        ["string \"", "uint32 ", "int32 ", "array [", "boolean ", "byte ", "variant ", "dict entry("];
    HEADERS.iter().any(|h| line.starts_with(h))
        || (line.starts_with("   ") && ARGS.iter().any(|a| line.trim_start().starts_with(a)))
}

impl NotifyParser {
    /// Feed one line; returns (app, summary, body) when a call completes.
    fn feed(&mut self, line: &str) -> Option<(String, String, String)> {
        // dbus-monitor does not escape quotes, so a line ending in `"` only
        // closes a string if what follows is a new argument.
        if self.collecting && self.reopenable && self.partial.is_none() && !is_arg_start(line) {
            if let Some(last) = self.strings.pop() {
                self.partial = Some(last + "\"");
            }
        }
        self.reopenable = false;
        if let Some(mut p) = self.partial.take() {
            p.push('\n');
            match line.strip_suffix('"') {
                Some(rest) => {
                    p.push_str(rest);
                    self.strings.push(p);
                    self.reopenable = true;
                }
                None => {
                    p.push_str(line);
                    self.partial = Some(p);
                }
            }
            return None;
        }
        if line.starts_with("method call") {
            self.collecting = line.contains("member=Notify");
            self.strings.clear();
            return None;
        }
        if !self.collecting {
            return None;
        }
        if let Some(rest) = line.strip_prefix("   string \"") {
            match rest.strip_suffix('"') {
                Some(s) => {
                    self.strings.push(s.to_string());
                    self.reopenable = true;
                }
                None => self.partial = Some(rest.to_string()),
            }
        } else if line.starts_with("   array [") {
            // The actions array follows the body: the strings are complete.
            self.collecting = false;
            if self.strings.len() >= 4 {
                let s = std::mem::take(&mut self.strings);
                return Some((s[0].clone(), s[2].clone(), s[3].clone()));
            }
        }
        None
    }
}

/// Watch the session bus for desktop notifications and record matching ones.
/// Blocks until the monitor exits.
pub fn listen() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return Err("the notification listener is Linux-only (it reads the D-Bus session bus)".into());
    }
    let mut child = Command::new("dbus-monitor")
        .args(["--session", "interface='org.freedesktop.Notifications',member='Notify'"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start dbus-monitor: {e}"))?;
    let out = child.stdout.take().ok_or("no dbus-monitor output")?;
    let mut parser = NotifyParser::default();
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        if let Some((app, summary, body)) = parser.feed(&line) {
            record_notification(&app, &summary, &body);
        }
    }
    let _ = child.wait();
    Err("dbus-monitor exited".into())
}

/// Background listener for the panel process. Idles while the setting is off.
pub fn spawn_listener() {
    if cfg!(target_os = "macos") {
        return;
    }
    std::thread::spawn(|| loop {
        if config::load().capture_notifications {
            if let Err(e) = listen() {
                eprintln!("[scribe] notification listener: {e}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notify_call() {
        let dump = "method call time=1.0 sender=:1.5 -> destination=:1.2 serial=7 path=/org/freedesktop/Notifications; interface=org.freedesktop.Notifications; member=Notify\n   string \"Slack\"\n   uint32 0\n   string \"\"\n   string \"New message in #ops\"\n   string \"Maria: disk is \"full\"\non db-prod\"\n   array [\n   ]\n";
        let mut p = NotifyParser::default();
        let got: Vec<_> = dump.lines().filter_map(|l| p.feed(l)).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "Slack");
        assert_eq!(got[0].1, "New message in #ops");
        assert_eq!(got[0].2, "Maria: disk is \"full\"\non db-prod");
    }

    #[test]
    fn matches_app_or_browser_origin() {
        assert!(app_matches("Slack", "hi", "slack"));
        assert!(app_matches("Google Chrome", "app.slack.com\nMaria: hi", "slack"));
        assert!(!app_matches("Scribe", "slack", "slack"));
        assert!(!app_matches("Thunderbird", "hello", "slack"));
    }

    #[test]
    fn finds_people_in_copied_thread() {
        let t = "Maria Santos\n  2:02 PM\ndb-prod disk is full\nBen\n14:05\non it";
        assert_eq!(clip_people(t), vec!["Maria Santos", "Ben"]);
        assert_eq!(clip_messages(t)[0].1, "db-prod disk is full");
        assert!(clip_messages("just some pasted text").is_empty());
    }
}
