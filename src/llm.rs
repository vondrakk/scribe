//! Pluggable LLM backend for episode summaries (PRD §8 "LLM strategy").
//!
//! `summarize` dispatches on `config.llm_backend`:
//!   * "stub"      → the deterministic rule-based summary (returned as `fallback`)
//!   * "anthropic" → Claude Messages API (raw HTTP; Rust has no official SDK)
//!   * "openai"    → any OpenAI-compatible /chat/completions endpoint, incl. a
//!                   local server (llama.cpp / Ollama / vLLM)
//!
//! Non-stub backends **fall back to the deterministic stub** on any missing
//! credential, network failure, non-2xx response, parse error, refusal, or
//! empty completion — the pipeline must never break because the LLM is
//! unreachable (the PRD runs batched end-of-session, offline-capable).
//!
//! Raw HTTP is the correct surface here: the claude-api skill specifies the
//! official Anthropic SDK for supported languages and raw HTTP for languages
//! without one — Rust has no official SDK.

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::model::Event;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Whether a model accepts `output_config.effort`. Haiku 4.5 and Sonnet 4.5
/// reject it with a 400; Opus, Sonnet 5.x, and Fable accept it.
fn supports_effort(model: &str) -> bool {
    let m = model.to_lowercase();
    !(m.contains("haiku") || m.contains("sonnet-4-5"))
}

const SYSTEM_PROMPT: &str = "You write a concise, factual engineering work-log \
note for a Jira ticket from captured terminal and git activity. Write ONE or \
TWO sentences describing what the engineer actually did. Be specific and \
grounded strictly in the facts given — never invent file names, outcomes, or \
details not present. No preamble, no markdown, no bullet points.";

/// Public entry point. `fallback` is the deterministic stub summary the pipeline
/// already computed; it is returned verbatim for the "stub" backend and used as
/// the safety net for every non-stub failure path.
pub fn summarize(
    events: &[Event],
    activity: &str,
    ticket: &Option<String>,
    fallback: &str,
) -> String {
    let cfg = config::load();
    match cfg.llm_backend.as_str() {
        "anthropic" => try_backend(call_anthropic(&cfg, events, activity, ticket), fallback),
        "openai" => try_backend(call_openai(&cfg, events, activity, ticket), fallback),
        _ => fallback.to_string(),
    }
}

fn try_backend(result: Result<String, String>, fallback: &str) -> String {
    match result {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        Ok(_) => {
            eprintln!("[scribe] LLM returned empty text; using stub summary");
            fallback.to_string()
        }
        Err(e) => {
            eprintln!("[scribe] LLM backend error ({e}); using stub summary");
            fallback.to_string()
        }
    }
}

// --- prompt construction ---------------------------------------------------

fn control_re() -> Regex {
    Regex::new(r"^\s*(w(\s|$|!|\?)|source\b.*scribe|_scribe)").unwrap()
}

/// The raw material handed to the model: the same facts the stub uses, as a
/// compact bulleted brief (never free prose, to keep the model grounded).
fn build_user_prompt(events: &[Event], activity: &str, ticket: &Option<String>) -> String {
    let cre = control_re();
    let cmds: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "shell")
        .filter_map(|e| e.cmd.clone())
        .filter(|c| !cre.is_match(c))
        .collect();
    let fails: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "shell" && e.exit.unwrap_or(0) != 0)
        .filter_map(|e| e.cmd.clone())
        .filter(|c| !cre.is_match(c))
        .collect();
    let mut repos: Vec<String> = events.iter().filter_map(|e| e.repo.clone()).collect();
    repos.sort();
    repos.dedup();
    let commits: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "git" && e.action.as_deref() == Some("commit"))
        .filter_map(|e| e.msg.clone())
        .map(|m| m.lines().next().unwrap_or("").to_string())
        .collect();

    let mut p = String::new();
    p.push_str(&format!("Activity type: {activity}\n"));
    p.push_str(&format!(
        "Ticket: {}\n",
        ticket.clone().unwrap_or_else(|| "none (unbound work)".into())
    ));
    if !repos.is_empty() {
        p.push_str(&format!("Repositories: {}\n", repos.join(", ")));
    }
    if !commits.is_empty() {
        p.push_str(&format!("Commits: {}\n", commits.join(" | ")));
    }
    if !fails.is_empty() {
        p.push_str(&format!("Commands that failed (non-zero exit): {}\n", fails.join(" | ")));
    }
    p.push_str("Commands run (in order):\n");
    for c in cmds.iter().take(40) {
        p.push_str(&format!("  - {c}\n"));
    }
    p.push_str("\nWrite the work-log note now.");
    p
}

// --- Anthropic backend -----------------------------------------------------

fn call_anthropic(
    cfg: &Config,
    events: &[Event],
    activity: &str,
    ticket: &Option<String>,
) -> Result<String, String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or("ANTHROPIC_API_KEY not set")?;
    let user = build_user_prompt(events, activity, ticket);

    let mut body = json!({
        "model": cfg.llm_model,
        "max_tokens": cfg.llm_max_tokens,
        "system": SYSTEM_PROMPT,
        "messages": [ { "role": "user", "content": user } ],
    });
    // `output_config.effort` keeps cost down on Opus / Sonnet 5.x / Fable, but
    // it ERRORS (400) on Haiku 4.5 and Sonnet 4.5, which have no effort control.
    // Only send it where it's accepted; Haiku runs without thinking by default,
    // which is exactly right for a short, factual summary.
    if supports_effort(&cfg.llm_model) {
        body["output_config"] = json!({ "effort": "low" });
    }

    let resp = ureq::post(ANTHROPIC_URL)
        .set("content-type", "application/json")
        .set("x-api-key", &key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .send_string(&body.to_string())
        .map_err(|e| format!("request failed: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;

    // Safety classifiers can decline with HTTP 200 + stop_reason "refusal".
    if v.get("stop_reason").and_then(|s| s.as_str()) == Some("refusal") {
        return Err("model refused".into());
    }
    let out = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    Ok(out)
}

// --- OpenAI-compatible backend (incl. local servers) -----------------------

fn call_openai(
    cfg: &Config,
    events: &[Event],
    activity: &str,
    ticket: &Option<String>,
) -> Result<String, String> {
    let base = if cfg.llm_base_url.is_empty() {
        "http://localhost:11434/v1".to_string() // Ollama default
    } else {
        cfg.llm_base_url.trim_end_matches('/').to_string()
    };
    let url = format!("{base}/chat/completions");
    let user = build_user_prompt(events, activity, ticket);

    let body = json!({
        "model": cfg.llm_model,
        "max_tokens": cfg.llm_max_tokens,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user },
        ],
    });

    // A local server often needs no key; a hosted one reads OPENAI_API_KEY.
    let mut req = ureq::post(&url).set("content-type", "application/json");
    if let Ok(k) = std::env::var("OPENAI_API_KEY") {
        if !k.is_empty() {
            req = req.set("authorization", &format!("Bearer {k}"));
        }
    }
    let resp = req
        .send_string(&body.to_string())
        .map_err(|e| format!("request failed: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
    let out = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(out)
}
