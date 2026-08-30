//! The numbers a lane can show beside its state: how much of the context
//! window is used, and how much of the five-hour and seven-day limits.
//!
//! Hooks carry none of these — on either side. They come from two other
//! places, and this module is where both are turned into one shape:
//!
//! - **Claude** hands them to its status-line command, on every assistant
//!   message. The hook's `--status` mode projects three percentages out of
//!   that JSON and posts them as a `StatusLine` record; nothing else in the
//!   status JSON leaves the machine, and the JSON itself goes on to the
//!   user's own status line untouched.
//! - **Codex** writes them into its session rollout, one `token_count` line
//!   after each model response. Every Codex hook names that file
//!   (`transcript_path`), and the app reads its tail — never the hook, which
//!   is a Windows process even for a WSL agent and cannot see `/home`.
//!
//! The limits are an account's, not a lane's: every lane of one account
//! reads the same. Unknown is unknown, never zero.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// What the Codex TUI subtracts from both the usage and the window before
/// showing "context left": the tokens every conversation starts with.
pub const BASELINE_TOKENS: i64 = 12_000;

/// How much of a rollout's end is read. The last `token_count` sits before
/// the response items that follow it, and a tool's output among them can run
/// past sixty kilobytes; a quarter megabyte over WSL's file server is tens
/// of milliseconds on the worker thread, never on the accept path.
const TAIL: u64 = 256 * 1024;

/// Three percentages, each of them possibly unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gauges {
    pub context_used: Option<u8>,
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
}

impl Gauges {
    /// The wire shape: `{"ctx": 42, "h5": 10, "d7": 3}`, any key absent.
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            context_used: object.get("ctx").and_then(percent),
            five_hour: object.get("h5").and_then(percent),
            seven_day: object.get("d7").and_then(percent),
        })
    }

    pub fn to_json(self) -> Value {
        let mut object = Map::new();
        if let Some(value) = self.context_used {
            object.insert("ctx".to_owned(), Value::from(value));
        }
        if let Some(value) = self.five_hour {
            object.insert("h5".to_owned(), Value::from(value));
        }
        if let Some(value) = self.seven_day {
            object.insert("d7".to_owned(), Value::from(value));
        }
        Value::Object(object)
    }

    pub fn is_empty(self) -> bool {
        self.context_used.is_none() && self.five_hour.is_none() && self.seven_day.is_none()
    }

    /// Takes what `newer` knows and keeps what it does not.
    pub fn merge(&mut self, newer: Self) {
        if newer.context_used.is_some() {
            self.context_used = newer.context_used;
        }
        if newer.five_hour.is_some() {
            self.five_hour = newer.five_hour;
        }
        if newer.seven_day.is_some() {
            self.seven_day = newer.seven_day;
        }
    }

    /// One line for the window: `ctx 42% · 5h — · 7d 3%`. Nothing at all
    /// when nothing is known — a lane with no numbers is two lines, not
    /// three dashes.
    pub fn sentence(self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let show = |value: Option<u8>| match value {
            Some(value) => format!("{value}%"),
            None => "—".to_owned(),
        };
        Some(format!(
            "ctx {} · 5h {} · 7d {}",
            show(self.context_used),
            show(self.five_hour),
            show(self.seven_day)
        ))
    }
}

/// A JSON number as a whole percentage, rounded and clamped. Anything that is
/// not a finite number is unknown.
pub fn percent(value: &Value) -> Option<u8> {
    let number = value.as_f64()?;
    if !number.is_finite() {
        return None;
    }
    Some(number.round().clamp(0.0, 100.0) as u8)
}

/// One rollout line, if it is a `token_count`, as gauges: context by the
/// TUI's arithmetic, and each limit told apart by the length of its window
/// rather than by which slot it came in — the five-hour window is
/// `primary` on one plan and absent on another.
pub fn codex_gauges(line: &str) -> Option<Gauges> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let mut gauges = Gauges::default();
    if let Some(info) = payload.get("info").filter(|info| info.is_object()) {
        let total = info
            .pointer("/last_token_usage/total_tokens")
            .and_then(Value::as_i64);
        let window = info.get("model_context_window").and_then(Value::as_i64);
        if let (Some(total), Some(window)) = (total, window)
            && window > BASELINE_TOKENS
        {
            let used = (total - BASELINE_TOKENS).max(0) as f64;
            let room = (window - BASELINE_TOKENS) as f64;
            gauges.context_used = Some((100.0 * used / room).round().clamp(0.0, 100.0) as u8);
        }
    }
    if let Some(limits) = payload.get("rate_limits").filter(|l| l.is_object()) {
        for slot in ["primary", "secondary"] {
            let Some(window) = limits.get(slot).filter(|w| w.is_object()) else {
                continue;
            };
            let used = window.get("used_percent").and_then(percent);
            match window.get("window_minutes").and_then(Value::as_i64) {
                Some(minutes) if minutes <= 720 => gauges.five_hour = used.or(gauges.five_hour),
                Some(minutes) if minutes >= 7200 => gauges.seven_day = used.or(gauges.seven_day),
                _ => {}
            }
        }
    }
    Some(gauges)
}

/// Where a Codex transcript can be opened from Windows. A Windows agent's
/// path is used as it is; a WSL agent's is a Linux path, reachable through
/// `\\wsl.localhost\<distro>` — one candidate per distribution, since the
/// hook cannot say which one it ran in. Claude transcripts are never read.
pub fn candidates(source: &str, transcript_path: &str, distros: &[String]) -> Vec<PathBuf> {
    if source.starts_with("codex-win") {
        return vec![PathBuf::from(transcript_path)];
    }
    if source.starts_with("codex-wsl") && transcript_path.starts_with('/') {
        return distros
            .iter()
            .map(|distro| {
                PathBuf::from(format!(
                    r"\\wsl.localhost\{distro}{}",
                    transcript_path.replace('/', "\\")
                ))
            })
            .collect();
    }
    Vec::new()
}

/// The gauges in a rollout's tail. Read from the end: the last line may be
/// mid-write, and a `token_count` may carry limits without usage or the
/// other way round, so each field is taken from the newest line that has
/// it. `None` when the tail has no complete `token_count` at all.
pub fn from_rollout(path: &Path) -> Option<Gauges> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    fold(text.lines().rev())
}

/// The newest value of each field across `lines`, newest first.
fn fold<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Gauges> {
    let mut found = Gauges::default();
    for line in lines {
        let Some(gauges) = codex_gauges(line) else {
            continue;
        };
        if found.context_used.is_none() {
            found.context_used = gauges.context_used;
        }
        if found.five_hour.is_none() {
            found.five_hour = gauges.five_hour;
        }
        if found.seven_day.is_none() {
            found.seven_day = gauges.seven_day;
        }
        if found.context_used.is_some() && found.five_hour.is_some() && found.seven_day.is_some() {
            break;
        }
    }
    (!found.is_empty()).then_some(found)
}

/// The worker's memory between events: which distributions exist (asked
/// once — `wsl.exe` is not free) and where each transcript turned out to be
/// (asked once per file — a stopped distribution's share can stall).
#[derive(Default)]
pub struct Rollouts {
    distros: Option<Vec<String>>,
    resolved: HashMap<String, PathBuf>,
}

impl Rollouts {
    /// Adds `gauges` to a Codex event that names its rollout, from that
    /// rollout's tail. Any other event is left exactly as it was.
    pub fn attach(&mut self, value: &mut Value) {
        let Some(source) = value.get("src").and_then(Value::as_str) else {
            return;
        };
        if !source.starts_with("codex") {
            return;
        }
        let Some(path) = value
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let source = source.to_owned();
        let resolved = match self.resolved.get(&path) {
            Some(known) => known.clone(),
            None => {
                let distros = self.distros.get_or_insert_with(crate::agents::wsl_distros);
                let Some(found) = candidates(&source, &path, distros)
                    .into_iter()
                    .find(|candidate| candidate.exists())
                else {
                    return;
                };
                self.resolved.insert(path, found.clone());
                found
            }
        };
        if let Some(gauges) = from_rollout(&resolved)
            && !gauges.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("gauges".to_owned(), gauges.to_json());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real line from a real rollout, numbers included.
    const REAL: &str = r#"{"timestamp":"2026-08-25T23:59:00.000Z","ordinal":17,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":27000000,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":239,"reasoning_output_tokens":87,"total_tokens":27299811},"last_token_usage":{"input_tokens":213000,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":239,"reasoning_output_tokens":87,"total_tokens":213922},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":5.0,"window_minutes":10080,"resets_at":1788295730},"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"plan_type":"prolite"}}}"#;

    /// A directory of the test's own, gone again when the test is.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("agent-frow-gauges-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn codex_context_follows_the_tui_formula() {
        let gauges = codex_gauges(REAL).unwrap();
        // (213922 − 12000) / (258400 − 12000) = 0.8195…
        assert_eq!(gauges.context_used, Some(82));
    }

    #[test]
    fn codex_limits_are_told_apart_by_their_window() {
        let gauges = codex_gauges(REAL).unwrap();
        assert_eq!(gauges.seven_day, Some(5), "10080 minutes is the week");
        assert_eq!(
            gauges.five_hour, None,
            "and this plan has no five-hour window"
        );

        let both = REAL.replace(
            r#""secondary":null"#,
            r#""secondary":{"used_percent":41.6,"window_minutes":10080,"resets_at":1}"#,
        );
        let both = both.replace(
            r#""window_minutes":10080,"resets_at":1788295730"#,
            r#""window_minutes":300,"resets_at":1788295730"#,
        );
        let gauges = codex_gauges(&both).unwrap();
        assert_eq!(gauges.five_hour, Some(5));
        assert_eq!(gauges.seven_day, Some(42));
    }

    #[test]
    fn a_line_without_usage_still_yields_limits() {
        let limits_only = json!({
            "type": "event_msg",
            "payload": { "type": "token_count", "info": null,
                "rate_limits": { "primary": { "used_percent": 12, "window_minutes": 299 } } }
        })
        .to_string();
        let gauges = codex_gauges(&limits_only).unwrap();
        assert_eq!(gauges.context_used, None);
        assert_eq!(gauges.five_hour, Some(12));
        assert!(
            codex_gauges(r#"{"type":"event_msg","payload":{"type":"turn_started"}}"#).is_none()
        );
        assert!(codex_gauges("not json").is_none());
    }

    #[test]
    fn the_last_complete_token_count_wins() {
        let scratch = Scratch::new("last");
        let dir = scratch.0.clone();
        let path = dir.join("rollout.jsonl");
        let older = REAL.replace(r#""total_tokens":213922"#, r#""total_tokens":100000"#);
        let truncated = &REAL[..REAL.len() / 2];
        std::fs::write(
            &path,
            format!("{{\"type\":\"session_meta\"}}\n{older}\n{REAL}\n{truncated}"),
        )
        .unwrap();
        let gauges = from_rollout(&path).unwrap();
        assert_eq!(
            gauges.context_used,
            Some(82),
            "the complete newer line, not the older or the cut one"
        );
    }

    #[test]
    fn earlier_lines_fill_what_the_last_one_lacks() {
        let scratch = Scratch::new("fill");
        let dir = scratch.0.clone();
        let path = dir.join("rollout.jsonl");
        let limits_only = json!({
            "type": "event_msg",
            "payload": { "type": "token_count", "info": null,
                "rate_limits": { "primary": { "used_percent": 7, "window_minutes": 10080 } } }
        })
        .to_string();
        std::fs::write(&path, format!("{REAL}\n{limits_only}\n")).unwrap();
        let gauges = from_rollout(&path).unwrap();
        assert_eq!(gauges.seven_day, Some(7), "the newest limit");
        assert_eq!(
            gauges.context_used,
            Some(82),
            "the newest usage, one line back"
        );
    }

    #[test]
    fn a_wsl_transcript_is_reached_through_wsl_localhost() {
        let found = candidates(
            "codex-wsl",
            "/home/me/.codex/sessions/2026/08/25/rollout-x.jsonl",
            &["Ubuntu".to_owned(), "Debian".to_owned()],
        );
        assert_eq!(
            found,
            [
                PathBuf::from(
                    r"\\wsl.localhost\Ubuntu\home\me\.codex\sessions\2026\08\25\rollout-x.jsonl"
                ),
                PathBuf::from(
                    r"\\wsl.localhost\Debian\home\me\.codex\sessions\2026\08\25\rollout-x.jsonl"
                ),
            ]
        );
    }

    #[test]
    fn a_windows_transcript_is_used_as_is() {
        let found = candidates("codex-win", r"C:\Users\me\.codex\sessions\r.jsonl", &[]);
        assert_eq!(
            found,
            [PathBuf::from(r"C:\Users\me\.codex\sessions\r.jsonl")]
        );
    }

    #[test]
    fn claude_transcripts_are_never_read() {
        assert!(
            candidates(
                "claude-wsl",
                "/home/me/.claude/projects/x/t.jsonl",
                &["Ubuntu".to_owned()]
            )
            .is_empty()
        );
        assert!(candidates("claude-win", r"C:\Users\me\.claude\t.jsonl", &[]).is_empty());
    }

    #[test]
    fn a_percentage_is_rounded_and_clamped() {
        assert_eq!(percent(&json!(42.6)), Some(43));
        assert_eq!(percent(&json!(140)), Some(100));
        assert_eq!(percent(&json!(-3)), Some(0));
        assert_eq!(percent(&json!("x")), None);
        assert_eq!(percent(&Value::Null), None);
    }

    #[test]
    fn the_wire_shape_round_trips() {
        let gauges = Gauges {
            context_used: Some(42),
            five_hour: None,
            seven_day: Some(3),
        };
        let json = gauges.to_json();
        assert_eq!(json, json!({"ctx": 42, "d7": 3}));
        assert_eq!(Gauges::from_json(&json), Some(gauges));
        assert_eq!(Gauges::from_json(&json!("no")), None);
        let mut merged = Gauges {
            five_hour: Some(9),
            ..Default::default()
        };
        merged.merge(gauges);
        assert_eq!(
            merged,
            Gauges {
                context_used: Some(42),
                five_hour: Some(9),
                seven_day: Some(3)
            }
        );
    }

    #[test]
    fn the_window_sentence_dashes_the_unknown() {
        let gauges = Gauges {
            context_used: Some(42),
            five_hour: None,
            seven_day: Some(3),
        };
        assert_eq!(gauges.sentence().as_deref(), Some("ctx 42% · 5h — · 7d 3%"));
        assert_eq!(Gauges::default().sentence(), None);
    }
}
