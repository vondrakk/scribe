//! Scribe's core: capture, the episode pipeline, review, publishing and the
//! local panel service. Shared by the `scribe` CLI (src/main.rs) and the
//! desktop app (app/).

pub mod atlassian;
pub mod binding;
pub mod capture;
pub mod config;
pub mod context;
pub mod db;
pub mod desk;
pub mod drafts;
pub mod gitlab;
pub mod inbox;
pub mod journal;
pub mod llm;
pub mod logbook;
pub mod model;
pub mod notify;
pub mod pagerduty;
pub mod panel;
pub mod pipeline;
pub mod prompts;
pub mod pty;
pub mod publish;
pub mod review;
pub mod rules;
pub mod secrets;
pub mod stream;
pub mod timeadj;
pub mod workbench;
