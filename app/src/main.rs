//! Scribe desktop app: the review panel in a native window, with a tray menu.
//!
//! The app runs Scribe's panel service in-process on loopback (or joins one
//! that is already running, e.g. `scribe up`) and shows it in a webview. The
//! window closes to the tray; the tray menu covers what you want without
//! opening it: what you're working on, how many things need you, "I got
//! interrupted", and "add the chat thread I just copied".
//!
//! Shell and git capture still go through the `scribe` CLI, which ships next
//! to this binary.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use scribe::{binding, config, context, desk, notify, panel};

/// Passed by the login item, so starting at login doesn't pop a window.
const HIDDEN_FLAG: &str = "--hidden";
const TRAY_REFRESH: Duration = Duration::from_secs(15);

/// Links that leave the panel (Jira, GitLab, chat threads) are turned into
/// plain navigations, which `on_navigation` hands to the system browser.
const EXTERNAL_LINKS_JS: &str = r#"
document.addEventListener('click', function (e) {
  var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
  if (a && a.host && a.host !== location.host) { e.preventDefault(); location.href = a.href; }
}, true);
window.open = function (u) { if (u) location.href = u; return null; };
"#;

fn panel_url() -> String {
    let cfg = config::load();
    format!("http://{}:{}", cfg.panel_host, cfg.panel_port)
}

/// Is a Scribe panel answering at `url`?
fn is_scribe(url: &str) -> bool {
    ureq::get(&format!("{url}/api/state"))
        .timeout(Duration::from_secs(2))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|v| v.get("counts").is_some())
        .unwrap_or(false)
}

/// Make sure the panel service is up: join a running one, else serve in-process.
fn start_service(url: &str) -> bool {
    if is_scribe(url) {
        return true;
    }
    std::thread::spawn(|| {
        if let Err(e) = panel::try_serve() {
            eprintln!("scribe: {e}");
        }
    });
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if is_scribe(url) {
            return true;
        }
    }
    false
}

/// The `scribe` CLI shipped beside this binary, if it is there.
fn sibling_cli() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cli = exe.parent()?.join("scribe");
    cli.is_file().then_some(cli)
}

/// First run from an installed app: write the shell-capture snippet so
/// enabling terminal capture is one line in the shell's startup file.
fn ensure_snippet() {
    if config::home().join("scribe.sh").exists() {
        return;
    }
    if let Some(cli) = sibling_cli() {
        let _ = std::process::Command::new(cli)
            .args(["init", "--shell"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn open_external(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// Does the desktop show tray icons? macOS always has a menu bar. On Linux the
/// icon only appears when something hosts the StatusNotifier protocol — stock
/// GNOME needs the AppIndicator extension for that.
#[cfg(target_os = "linux")]
fn has_tray() -> bool {
    std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.DBus",
            "--print-reply",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            "string:org.kde.StatusNotifierWatcher",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("boolean true"))
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn has_tray() -> bool {
    true
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn status_text() -> (String, usize) {
    let waiting = desk::needs_count();
    let work = match binding::get_binding() {
        Some(b) => format!("Working on {}", b.ticket),
        None => "No ticket set".to_string(),
    };
    let review = match waiting {
        0 => "nothing needs you".to_string(),
        1 => "1 needs you".to_string(),
        n => format!("{n} need you"),
    };
    (format!("{work} · {review}"), waiting)
}

fn main() {
    let url = panel_url();
    let up = start_service(&url);
    ensure_snippet();
    let start_hidden = std::env::args().any(|a| a == HIDDEN_FLAG) && has_tray();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_FLAG])))
        .setup(move |app| {
            let origin = url.clone();
            let target = if up {
                WebviewUrl::External(url.parse().expect("panel url"))
            } else {
                WebviewUrl::App("index.html".into())
            };
            WebviewWindowBuilder::new(app, "main", target)
                .title("Scribe")
                .inner_size(1180.0, 860.0)
                .min_inner_size(720.0, 520.0)
                .visible(!start_hidden)
                .initialization_script(EXTERNAL_LINKS_JS)
                .on_navigation(move |to| {
                    let local = to.as_str().starts_with(&origin) || !matches!(to.scheme(), "http" | "https");
                    if !local {
                        open_external(to.as_str());
                    }
                    local
                })
                .build()?;

            let (text, waiting) = status_text();
            let status = MenuItem::with_id(app, "status", text, false, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "Open Scribe", true, None::<&str>)?;
            let interrupt = MenuItem::with_id(app, "interrupt", "I got interrupted", true, None::<&str>)?;
            let clip = MenuItem::with_id(app, "clip", "Add copied chat thread", true, None::<&str>)?;
            let login = CheckMenuItem::with_id(
                app,
                "login",
                "Start at login",
                true,
                app.autolaunch().is_enabled().unwrap_or(false),
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Quit Scribe", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &status,
                    &PredefinedMenuItem::separator(app)?,
                    &open,
                    &interrupt,
                    &clip,
                    &PredefinedMenuItem::separator(app)?,
                    &login,
                    &quit,
                ],
            )?;

            let badge = |n: usize| (n > 0).then(|| n.to_string());
            let tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("app icon").clone())
                .tooltip("Scribe")
                .title(badge(waiting).unwrap_or_default())
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "interrupt" => {
                        binding::mark_interrupt(None);
                        notify::desktop("Interruption marked", "Open Scribe to say what it was.");
                    }
                    "clip" => match context::read_clipboard().and_then(|t| context::add_clip(&t, "")) {
                        Ok(_) => notify::desktop("Chat thread added", "It is on the Review screen."),
                        Err(e) => notify::desktop("Couldn't add the thread", &e),
                    },
                    "login" => {
                        let auto = app.autolaunch();
                        let on = auto.is_enabled().unwrap_or(false);
                        let _ = if on { auto.disable() } else { auto.enable() };
                        let _ = login.set_checked(auto.is_enabled().unwrap_or(!on));
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Keep the tray current while the window is closed.
            std::thread::spawn(move || loop {
                std::thread::sleep(TRAY_REFRESH);
                let (text, waiting) = status_text();
                let _ = status.set_text(text);
                let _ = tray.set_title(badge(waiting));
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window leaves Scribe running: in the tray where the
            // desktop has one, otherwise minimized so it stays reachable.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = if has_tray() { window.hide() } else { window.minimize() };
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to start Scribe");

    app.run(|_app, _event| {
        // Clicking the dock icon brings the window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            show_window(_app);
        }
    });
}
