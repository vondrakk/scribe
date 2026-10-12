//! Scribe — passive per-engineer work capture → draft artifacts (prototype).

mod binding;
mod capture;
mod atlassian;
mod config;
mod context;
mod drafts;
mod gitlab;
mod inbox;
mod llm;
mod model;
mod notify;
mod prompts;
mod pty;
mod rules;
mod logbook;
mod stream;
mod timeadj;
mod workbench;
mod panel;
mod pipeline;
mod publish;
mod review;
mod secrets;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "scribe", version, about = "Passive work capture → draft Jira/Confluence artifacts")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print shell snippet to source + write git-hook templates.
    Init {
        #[arg(long)]
        shell: bool,
        #[arg(long)]
        git: bool,
    },
    /// Copy git hooks into a repo's .git/hooks.
    InstallHooks { repo: String },
    /// Record a shell command (called by the PROMPT_COMMAND hot path).
    CaptureShell {
        cmd: String,
        exit: i64,
        cwd: String,
        #[arg(default_value = "")]
        repo: String,
        #[arg(default_value = "")]
        branch: String,
    },
    /// Record a structured git event (called by git hooks).
    Event {
        #[arg(long = "type", default_value = "git")]
        etype: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        sha: Option<String>,
    },
    /// Binding verb: `w` show · `w OPS-1234` bind · `w ?` candidates ·
    /// `w ! [what was asked, by whom]` interrupt.
    W {
        arg: Option<String>,
        /// Free-text reason for an interrupt (`w ! maria asked about db-prod`).
        rest: Vec<String>,
    },
    /// Emit the prompt-line segment (bound ticket / unbound glyph).
    Prompt,
    /// Record an AI prompt to the per-session prompt log. Prompt text is a
    /// positional arg, or read from stdin when omitted or "-".
    Ai {
        prompt: Option<String>,
        /// AI tool name (claude, copilot, chatgpt, …).
        #[arg(long, default_value = "ai")]
        tool: String,
        #[arg(long)]
        model: Option<String>,
        /// Session id. Defaults to $SCRIBE_SESSION, else "default".
        #[arg(long)]
        session: Option<String>,
    },
    /// Add a chat thread as context for your current work. Reads the clipboard
    /// when no text is given; "-" reads stdin.
    Clip {
        text: Option<String>,
        /// Ticket this thread belongs to (default: a key found in the text,
        /// else whatever you were working on at the time).
        #[arg(long)]
        ticket: Option<String>,
    },
    /// Record chat notifications your desktop shows, as local context (Linux).
    /// The panel does this itself when the setting is on; this runs it alone.
    Listen,
    /// Sessionize → classify → summarize → emit drafts.
    Process,
    /// Apply retention (drop raw capture older than N days).
    Sweep,
    /// Pull inbound artifacts (GitLab MRs, assigned Jira issues) into the inbox.
    Pull,
    /// Launch the local control panel.
    Panel,
    /// One command to run it all: ensure capture is set up, process, open the
    /// browser, and serve the panel (auto-reprocessing as activity is captured).
    Up,
    /// Authorize Scribe with Atlassian Cloud (OAuth 2.0 3LO) so publishing posts
    /// as you. Needs atlassian_client_id configured + ATLASSIAN_CLIENT_SECRET set.
    Auth,
    /// Wipe captured + derived data for a clean slate (keeps config + snippet).
    Reset {
        /// Also remove published/ artifacts.
        #[arg(long)]
        all: bool,
    },
    /// Print binding + counts.
    Status,
}

fn bin_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "scribe".to_string())
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { shell, git } => cmd_init(shell, git),
        Cmd::InstallHooks { repo } => cmd_install_hooks(&repo),
        Cmd::CaptureShell { cmd, exit, cwd, repo, branch } => {
            capture::capture_shell(&cmd, exit, &cwd, &repo, &branch)
        }
        Cmd::Event { etype, action, message, sha } => {
            capture::capture_event(&etype, &action, message.as_deref(), sha.as_deref())
        }
        Cmd::W { arg, rest } => cmd_w(arg.as_deref(), &rest.join(" ")),
        Cmd::Clip { text, ticket } => cmd_clip(text, ticket),
        Cmd::Listen => {
            let mut cfg = config::load();
            if !cfg.capture_notifications {
                cfg.capture_notifications = true;
                config::save(&cfg);
                println!("turned on capture_notifications in config");
            }
            println!("listening for notifications from: {}  (Ctrl-C to stop)", cfg.notification_apps);
            if let Err(e) = context::listen() {
                eprintln!("listen failed: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Prompt => cmd_prompt(),
        Cmd::Ai { prompt, tool, model, session } => cmd_ai(prompt, tool, model, session),
        Cmd::Process => {
            let r = pipeline::process();
            println!("episodes={} drafts={}", r.episodes, r.drafts);
        }
        Cmd::Sweep => {
            println!("dropped {} aged raw event(s)", capture::sweep_retention());
        }
        Cmd::Pull => {
            let r = inbox::refresh();
            println!("inbox: {} GitLab MR(s), {} Jira issue(s)", r.gitlab, r.jira);
            for e in r.errors {
                eprintln!("  {e}");
            }
        }
        Cmd::Panel => panel::serve(),
        Cmd::Up => cmd_up(),
        Cmd::Auth => match atlassian::authorize() {
            Ok(site) => println!("Authorized with Atlassian site: {site}\nSet publish_backend=atlassian (Settings) to publish for real."),
            Err(e) => {
                eprintln!("auth failed: {e}");
                std::process::exit(1);
            }
        },
        Cmd::Reset { all } => {
            let (ev, ep, dr, pr) = model::reset(all);
            println!(
                "reset: removed {ev} event(s), {ep} episode(s), {dr} draft(s), {pr} prompt session(s){}",
                if all { ", and published artifacts" } else { "" }
            );
        }
        Cmd::Status => cmd_status(),
    }
}

fn cmd_clip(text: Option<String>, ticket: Option<String>) {
    let raw = match text {
        Some(t) if t != "-" => Ok(t),
        Some(_) => {
            use std::io::Read;
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            Ok(s)
        }
        None => context::read_clipboard(),
    };
    match raw.and_then(|t| context::add_clip(&t, ticket.as_deref().unwrap_or(""))) {
        Ok(item) => {
            let lines = item.text.lines().count();
            let target = if item.ticket.is_empty() {
                "your current work".to_string()
            } else {
                item.ticket.clone()
            };
            println!("added thread ({lines} line(s)) as context for {target} — review it in the panel");
        }
        Err(e) => {
            eprintln!("clip failed: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_w(arg: Option<&str>, reason: &str) {
    match arg {
        None => match binding::get_binding() {
            Some(b) => println!("bound to {} ({})", b.ticket, b.source),
            None => println!("no active ticket (⚠ unbound)"),
        },
        Some("?") => {
            for (k, last, n) in binding::candidate_tickets(10) {
                let when = chrono::Local
                    .timestamp_opt(last as i64, 0)
                    .single()
                    .map(|t| t.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                println!("  {:12} last={}  seen={}", k, when, n);
            }
        }
        Some("!") => {
            binding::mark_interrupt(Some(reason));
            println!("marked interrupt — current activity split into its own episode");
        }
        Some(a) => {
            let key = binding::extract_key(Some(a)).unwrap_or_else(|| a.to_string());
            binding::set_binding(Some(&key), "manual", "");
            println!("bound to {}", key);
        }
    }
}

use chrono::TimeZone;

fn cmd_ai(prompt: Option<String>, tool: String, model: Option<String>, session: Option<String>) {
    let raw = match prompt {
        Some(p) if p != "-" => p,
        _ => {
            use std::io::Read;
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            s
        }
    };

    let mut text = raw.trim().to_string();
    let mut tool = tool;
    let mut session = session;

    // Claude Code's UserPromptSubmit hook pipes a JSON object on stdin (fields:
    // text, session_id, hook_event_name, cwd, …). Detect it and pull out the
    // prompt + session so the hook can be wired as simply `scribe ai -`.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        // Claude Code's UserPromptSubmit payload names the prompt `prompt`;
        // accept `text` too for other tools that use it.
        let field = v
            .get("prompt")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("text").and_then(|x| x.as_str()));
        if let Some(t) = field {
            text = t.trim().to_string();
            if v.get("hook_event_name").is_some() && tool == "ai" {
                tool = "claude-code".to_string();
            }
            if session.is_none() {
                if let Some(sid) = v.get("session_id").and_then(|x| x.as_str()) {
                    session = Some(sid.to_string());
                }
            }
        }
    }

    // Always exit 0: this may run as a Claude Code hook, where a non-zero exit
    // would block the user's prompt. An empty prompt is simply a no-op.
    if text.is_empty() {
        return;
    }
    let session = session
        .or_else(|| std::env::var("SCRIBE_SESSION").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "default".to_string());
    prompts::record(&session, &tool, model.as_deref(), &text);
}

fn cmd_prompt() {
    let ticket = std::fs::read_to_string(config::binding_file())
        .unwrap_or_default()
        .trim()
        .to_string();
    if !ticket.is_empty() {
        print!("[{}]", ticket);
        return;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let (_repo, branch) = capture::repo_branch(&cwd);
    if branch.is_some() {
        print!("[⚠ unbound]");
    }
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

fn cmd_up() {
    // Ensure the shell-capture snippet exists (idempotent — same as `init`).
    let bin = bin_path();
    let snippet = config::home().join("scribe.sh");
    let _ = std::fs::write(&snippet, shell_snippet(&bin));

    let cfg = config::load();
    let url = format!("http://{}:{}", cfg.panel_host, cfg.panel_port);
    // Open the browser shortly after the server starts binding.
    let u = url.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        open_browser(&u);
    });
    println!("Scribe up → {url}");
    println!("To capture your work, run once in the terminal you work in:");
    println!("    source \"{}\"", snippet.to_string_lossy());
    panel::serve();
}

fn cmd_status() {
    let b = binding::get_binding();
    let eps = model::load_episodes();
    let open = model::load_drafts().iter().filter(|d| d.status == "draft").count();
    let v = serde_json::json!({
        "home": config::home().to_string_lossy(),
        "binding": b,
        "events": capture::count_events(),
        "episodes": eps.len(),
        "open_drafts": open,
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

// --- installers ------------------------------------------------------------

fn shell_snippet(bin: &str) -> String {
    format!(
        r#"# >>> scribe capture >>>
# Passive shell capture for Scribe. Appends one JSONL line per command to
# $SCRIBE_HOME/events.jsonl via a fast native call (no interpreter spawn).
# Works in both bash and zsh (macOS's default shell) — the hook self-detects.
export SCRIBE_HOME="${{SCRIBE_HOME:-$HOME/.scribe}}"
# A stable session id per terminal, so AI prompts are indexed by session.
export SCRIBE_SESSION="${{SCRIBE_SESSION:-$(date +%Y%m%d-%H%M%S)-$$}}"
_SCRIBE_BIN="{bin}"
_scribe_log() {{
  local ec=$?
  local last
  if [ -n "$ZSH_VERSION" ]; then
    # zsh: the last history entry, numbering/leading-space stripped.
    last=$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//')
  else
    last=$(HISTTIMEFORMAT= history 1 2>/dev/null | sed 's/^[ ]*[0-9]*[ ]*//')
  fi
  [ -z "$last" ] && return $ec
  [ "$last" = "$_SCRIBE_LAST" ] && return $ec
  _SCRIBE_LAST="$last"
  local root repo branch
  root=$(git rev-parse --show-toplevel 2>/dev/null); repo=""; [ -n "$root" ] && repo=$(basename "$root")
  branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
  "$_SCRIBE_BIN" capture-shell "$last" "$ec" "$PWD" "$repo" "$branch" 2>/dev/null
  return $ec
}}
if [ -n "$ZSH_VERSION" ]; then
  # zsh: register _scribe_log as a precmd hook (runs before each prompt).
  autoload -Uz add-zsh-hook 2>/dev/null
  if whence add-zsh-hook >/dev/null 2>&1; then
    add-zsh-hook precmd _scribe_log
  elif [ -z "${{precmd_functions[(r)_scribe_log]}}" ]; then
    precmd_functions+=(_scribe_log)
  fi
else
  # bash: prepend to PROMPT_COMMAND, once.
  case "$PROMPT_COMMAND" in
    *_scribe_log*) : ;;
    *) PROMPT_COMMAND="_scribe_log${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}" ;;
  esac
fi
# Binding verb.
w() {{ "$_SCRIBE_BIN" w "$@"; }}
# Record an AI prompt in this session:  ai "how do I ..."   (or:  ai - <<<"$text")
ai() {{ "$_SCRIBE_BIN" ai "$@"; }}
# <<< scribe capture <<<
"#,
        bin = bin
    )
}

fn git_hooks(bin: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "post-checkout",
            format!(
                "#!/usr/bin/env bash\n# scribe: bind on branch switch\n\"{bin}\" event --type git --action checkout >/dev/null 2>&1 || true\n",
                bin = bin
            ),
        ),
        (
            "commit-msg",
            format!(
                "#!/usr/bin/env bash\n# scribe: capture commit key from message\nmsg=$(cat \"$1\" 2>/dev/null)\n\"{bin}\" event --type git --action commit-msg --message \"$msg\" >/dev/null 2>&1 || true\n",
                bin = bin
            ),
        ),
        (
            "post-commit",
            format!(
                "#!/usr/bin/env bash\n# scribe: capture commit\nsha=$(git rev-parse HEAD 2>/dev/null)\nmsg=$(git log -1 --pretty=%B 2>/dev/null)\n\"{bin}\" event --type git --action commit --sha \"$sha\" --message \"$msg\" >/dev/null 2>&1 || true\n",
                bin = bin
            ),
        ),
    ]
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}
#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) {}

fn cmd_init(shell: bool, git: bool) {
    let (shell, git) = if !shell && !git { (true, true) } else { (shell, git) };
    let bin = bin_path();
    let snippet_path = config::home().join("scribe.sh");
    let _ = std::fs::write(&snippet_path, shell_snippet(&bin));

    if shell {
        println!("Shell capture (bash or zsh). Add this line to your ~/.bashrc");
        println!("(or ~/.zshrc on macOS / zsh):\n");
        println!("    source \"{}\"\n", snippet_path.to_string_lossy());
        println!("Then open a new shell (or re-source the rc file). The `w` verb");
        println!("becomes available and capture starts immediately. The snippet");
        println!("self-detects the shell, so the same line works in both.\n");
    }
    if git {
        println!("Git hooks. In each repo you want captured, run:\n");
        println!("    scribe install-hooks <repo>\n");
    }
}

fn cmd_install_hooks(repo: &str) {
    let repo = std::path::Path::new(repo);
    let gitdir = repo.join(".git");
    if !gitdir.exists() {
        eprintln!("not a git repo: {}", repo.to_string_lossy());
        std::process::exit(1);
    }
    let dst = gitdir.join("hooks");
    let _ = std::fs::create_dir_all(&dst);
    let bin = bin_path();
    for (name, body) in git_hooks(&bin) {
        let target = dst.join(name);
        let _ = std::fs::write(&target, body);
        make_executable(&target);
    }
    println!("installed hooks into {}", dst.to_string_lossy());
}
