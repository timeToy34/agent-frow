//! When each flavor last reached us.
//!
//! This is the one diagnostic the previous application lacked and needed most.
//! A configuration file can only say a hook is *registered*; it can never say
//! the agent has actually run it. "Registered, but never seen" is the signature
//! of a Codex trust review nobody has done yet, and it looks exactly like a
//! broken app until you can tell the two apart.
//!
//! Kept on disk so the answer survives a restart and can be printed by
//! `doctor` when the tray app is not running.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

/// Reads the recorded times, keyed by `--source` name.
pub fn load(path: &Path) -> BTreeMap<String, u64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    map.into_iter()
        .filter_map(|(key, value)| Some((key, value.as_u64()?)))
        .collect()
}

/// Records that `source` was heard from at `now_ms`.
///
/// Written through a temp file so a crash mid-write cannot leave a file that
/// fails to parse and silently resets every timestamp to "never".
pub fn record(path: &Path, source: &str, now_ms: u64) {
    let mut seen = load(path);
    seen.insert(source.to_owned(), now_ms);

    let mut map = Map::new();
    for (key, value) in seen {
        map.insert(key, Value::from(value));
    }
    let Ok(text) = serde_json::to_string_pretty(&Value::Object(map)) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

/// "4m ago", "never", and so on — the column a person actually reads.
pub fn describe(seen: Option<u64>, now_ms: u64) -> String {
    let Some(at) = seen else {
        return "never".to_owned();
    };
    let secs = now_ms.saturating_sub(at) / 1000;
    match secs {
        0..=5 => "just now".to_owned(),
        6..=90 => format!("{secs}s ago"),
        91..=5400 => format!("{}m ago", secs / 60),
        5401..=172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::describe;

    #[test]
    fn never_is_distinct_from_a_long_time_ago() {
        // These two mean completely different things: one is a pending trust
        // review or a broken install, the other is an agent nobody has used
        // today. Collapsing them is what made the old diagnostics useless.
        assert_eq!(describe(None, 1_000_000), "never");
        assert_eq!(describe(Some(0), 1_000_000_000), "11d ago");
    }

    #[test]
    fn recent_events_read_as_recent() {
        let now = 1_000_000_000;
        assert_eq!(describe(Some(now - 2_000), now), "just now");
        assert_eq!(describe(Some(now - 30_000), now), "30s ago");
        assert_eq!(describe(Some(now - 600_000), now), "10m ago");
        assert_eq!(describe(Some(now - 7_200_000), now), "2h ago");
    }
}
