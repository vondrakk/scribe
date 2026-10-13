//! Manual time adjustments: per-episode duration overrides.
//!
//! Episode durations are derived (end − start), but real work isn't always
//! wall-clock contiguous — an engineer may want to correct how long a piece of
//! work actually took, especially when several things ran concurrently. These
//! overrides are keyed by episode id and applied in `stream::build`; they
//! survive reprocess as long as episode ids are stable.

use std::collections::HashMap;


/// Load the override map (episode id → seconds).
pub fn load() -> HashMap<i64, f64> {
    crate::db::doc("time_overrides")
}

fn save(m: &HashMap<i64, f64>) {
    crate::db::put_doc("time_overrides", m);
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

/// Re-key the corrections after episode ids changed; unknown ids are dropped.
pub fn remap(map: &HashMap<i64, i64>) {
    let old = load();
    if old.is_empty() {
        return;
    }
    save(&old.into_iter().filter_map(|(id, secs)| Some((*map.get(&id)?, secs))).collect());
}
