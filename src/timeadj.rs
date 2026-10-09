//! Manual time adjustments: per-episode duration overrides.
//!
//! Episode durations are derived (end − start), but real work isn't always
//! wall-clock contiguous — an engineer may want to correct how long a piece of
//! work actually took, especially when several things ran concurrently. These
//! overrides are keyed by episode id and applied in `stream::build`; they
//! survive reprocess as long as episode ids are stable.

use std::collections::HashMap;

use crate::config;

/// Load the override map (episode id → seconds).
pub fn load() -> HashMap<i64, f64> {
    std::fs::read_to_string(config::time_overrides_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(m: &HashMap<i64, f64>) {
    if let Ok(s) = serde_json::to_string_pretty(m) {
        let _ = std::fs::write(config::time_overrides_file(), s);
    }
}

/// Set (or clear, when secs < 0) the override for an episode.
pub fn set(episode_id: i64, secs: f64) {
    let mut m = load();
    if secs < 0.0 {
        m.remove(&episode_id);
    } else {
        m.insert(episode_id, secs);
    }
    save(&m);
}
