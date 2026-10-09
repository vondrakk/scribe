//! Local desktop notifications for hard seams the engineer would miss while
//! heads-down (PRD §5 "Local desktop notification"). Fire-and-forget: a seam
//! is detected in real time (e.g. a `post-checkout` onto an unkeyed branch) and
//! we surface a native toast so the engineer can bind the ticket inline.
//!
//! Cross-platform, no dependencies:
//!   * Linux : `notify-send` (libnotify) — the standard desktop toast.
//!   * macOS : `osascript -e 'display notification …'` — built in, no install.
//!
//! All of it is best-effort: if the tool is absent, the display is headless, or
//! the user opted out (config `desktop_notifications = false`, or the
//! `SCRIBE_NO_NOTIFY` env var), we silently do nothing. Capture never depends
//! on a notification succeeding.

use std::process::{Command, Stdio};

use crate::config;

/// Send a desktop notification. Returns silently on any failure or opt-out.
pub fn desktop(summary: &str, body: &str) {
    // Opt-out: explicit env override wins, then the config flag.
    if std::env::var("SCRIBE_NO_NOTIFY").map(|v| !v.is_empty()).unwrap_or(false) {
        return;
    }
    if !config::load().desktop_notifications {
        return;
    }
    // A notification is pointless with no display/session attached.
    if !has_display() {
        return;
    }
    let _ = spawn(summary, body);
}

#[cfg(target_os = "macos")]
fn has_display() -> bool {
    // GUI session is the norm on macOS; let osascript decide at spawn time.
    true
}

#[cfg(not(target_os = "macos"))]
fn has_display() -> bool {
    // On Linux a toast needs a graphical session bus. WAYLAND_DISPLAY or
    // DISPLAY being set is a good-enough proxy; avoids spawning into the void
    // on headless servers / bare SSH.
    std::env::var("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false)
        || std::env::var("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn spawn(summary: &str, body: &str) -> std::io::Result<std::process::Child> {
    // Escape double quotes for the AppleScript string literals.
    let s = summary.replace('\\', "\\\\").replace('"', "\\\"");
    let b = body.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{b}\" with title \"Scribe\" subtitle \"{s}\"");
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(not(target_os = "macos"))]
fn spawn(summary: &str, body: &str) -> std::io::Result<std::process::Child> {
    // -a sets the app name, -u normal keeps it non-intrusive, -i a hint icon.
    Command::new("notify-send")
        .args(["-a", "Scribe", "-u", "normal", "-i", "dialog-information", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}
