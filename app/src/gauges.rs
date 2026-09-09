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
//! Limits describe the general Codex allowance, not a model-specific bucket
//! such as Spark. Unknown is unknown, never zero.
//! On a Codex Stop, the same tail also supplies the proposed-plan flag when
//! the hook's final message omits it. Only a completed Plan for that turn
//! counts; no transcript content is retained or forwarded.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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
/// TUI's arithmetic, and limits from the general Codex bucket only. Within
/// that bucket, window length tells the limits apart — the five-hour window
/// is `primary` on one plan and absent on another.
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
    // Newer rollouts can keep a model-specific snapshot in rate_limits.
    // Its zero is not the account's zero. Missing/null ids are the legacy
    // single-bucket shape; an explicit different or malformed id is ignored.
    if let Some(limits) = payload
        .get("rate_limits")
        .filter(|l| l.is_object())
        .filter(|l| {
            l.get("limit_id")
                .is_none_or(|id| id.is_null() || id == "codex")
        })
    {
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
    let text = rollout_tail(path)?;
    fold(text.lines().rev())
}

fn rollout_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Codex 0.153.2 records the completed Plan separately from the final
/// message, which can be absent from Stop. Match the turn explicitly:
/// searching for any plan in the tail would revive an earlier prompt.
fn completed_plan(line: &str, turn_id: &str) -> bool {
    if !line.contains("\"Plan\"") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let payload = &value["payload"];
    value["type"] == "event_msg"
        && payload["type"] == "item_completed"
        && payload["turn_id"].as_str() == Some(turn_id)
        && payload["item"]["type"] == "Plan"
        && payload["item"]["text"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
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
    metadata: HashMap<PathBuf, RolloutMetadata>,
}

struct RolloutMetadata {
    session_id: String,
    cli: bool,
}

impl RolloutMetadata {
    fn read(path: &Path) -> Option<Self> {
        // Only the first complete record, bounded even if a file is malformed.
        // Cache identities, never the transcript or any of its messages.
        let file = std::fs::File::open(path).ok()?;
        let mut line = Vec::new();
        BufReader::new(file.take(64 * 1024))
            .read_until(b'\n', &mut line)
            .ok()?;
        if line.last() != Some(&b'\n') {
            return None;
        }
        let value: Value = serde_json::from_slice(&line).ok()?;
        if value["type"] != "session_meta" {
            return None;
        }
        let session_id = value["payload"]["id"].as_str()?.trim();
        if session_id.is_empty() || value["payload"].get("source").is_none() {
            return None;
        }
        Some(Self {
            session_id: session_id.to_owned(),
            cli: value["payload"]["source"] == "cli",
        })
    }
}

impl Rollouts {
    /// Adds `gauges` to a Codex event that names its rollout and recovers
    /// `proposed_plan` on Stop from a completed Plan with the same turn id.
    /// Both use one bounded tail read. The first complete metadata record
    /// supplies `codex_cli`, cached per file and matched to the session id.
    pub fn attach(&mut self, value: &mut Value) {
        // This fact is derived locally, not accepted from an ingress payload.
        if let Some(object) = value.as_object_mut() {
            object.remove("codex_cli");
        }
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
        if !self.metadata.contains_key(&resolved)
            && let Some(metadata) = RolloutMetadata::read(&resolved)
        {
            self.metadata.insert(resolved.clone(), metadata);
        }
        if let Some(metadata) = self.metadata.get(&resolved)
            && metadata.cli
            && value["session_id"].as_str() == Some(metadata.session_id.as_str())
            && let Some(object) = value.as_object_mut()
        {
            object.insert("codex_cli".to_owned(), Value::Bool(true));
        }
        let Some(text) = rollout_tail(&resolved) else {
            return;
        };
        if let Some(gauges) = fold(text.lines().rev())
            && !gauges.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("gauges".to_owned(), gauges.to_json());
        }
        if value["hook_event_name"] == "Stop"
            && let Some(turn_id) = value
                .get("turn_id")
                .and_then(Value::as_str)
                .or_else(|| value.get("prompt_id").and_then(Value::as_str))
                .filter(|id| !id.trim().is_empty())
            && text.lines().rev().any(|line| completed_plan(line, turn_id))
            && let Some(object) = value.as_object_mut()
        {
            object.insert("proposed_plan".to_owned(), Value::Bool(true));
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
    fn a_model_specific_limit_cannot_replace_the_codex_account_limit() {
        let scratch = Scratch::new("limit-buckets");
        let path = scratch.0.join("rollout.jsonl");
        let account = REAL.replace(r#""used_percent":5.0"#, r#""used_percent":65.0"#);
        let mut spark: Value = serde_json::from_str(REAL).unwrap();
        spark["payload"]["rate_limits"] = json!({
            "limit_id": "codex_bengalfox", "limit_name": "GPT-5.3-Codex-Spark",
            "primary": { "used_percent": 0.0, "window_minutes": 300 },
            "secondary": { "used_percent": 0.0, "window_minutes": 10080 }
        });
        spark["payload"]["info"]["last_token_usage"]["total_tokens"] = json!(100_000);
        std::fs::write(&path, format!("{account}\n{spark}\n")).unwrap();

        // Real failure: the newer Spark snapshot supplied both zeroes even
        // though neither of them described the general Codex allowance.
        let mut event = json!({
            "src": "codex-win", "hook_event_name": "PostToolUse",
            "transcript_path": path.to_str().unwrap()
        });
        Rollouts::default().attach(&mut event);
        assert_eq!(event["gauges"], json!({ "ctx": 36, "d7": 65 }));

        // If the bounded tail contains only another bucket, the account's
        // limits are unknown. Context still belongs to this conversation.
        std::fs::write(&path, format!("{spark}\n")).unwrap();
        let gauges = from_rollout(&path).unwrap();
        assert_eq!(gauges.context_used, Some(36));
        assert_eq!(gauges.five_hour, None);
        assert_eq!(gauges.seven_day, None);
    }

    #[test]
    fn codex_limit_ids_support_legacy_records_without_accepting_other_buckets() {
        for (id, expected) in [
            (json!("codex"), Some(5)),
            (Value::Null, Some(5)),
            (json!("codex_bengalfox"), None),
            (json!("another-model"), None),
            (json!(""), None),
            (json!(42), None),
        ] {
            let mut value: Value = serde_json::from_str(REAL).unwrap();
            value["payload"]["rate_limits"]["limit_id"] = id.clone();
            let gauges = codex_gauges(&value.to_string()).unwrap();
            assert_eq!(gauges.seven_day, expected, "limit_id={id}");
            assert_eq!(gauges.context_used, Some(82));
        }
        let mut legacy: Value = serde_json::from_str(REAL).unwrap();
        legacy["payload"]["rate_limits"]
            .as_object_mut()
            .unwrap()
            .remove("limit_id");
        assert_eq!(
            codex_gauges(&legacy.to_string()).unwrap().seven_day,
            Some(5)
        );

        // A real zero in the general bucket remains a known zero.
        legacy["payload"]["rate_limits"]["limit_id"] = json!("codex");
        legacy["payload"]["rate_limits"]["primary"]["used_percent"] = json!(0);
        assert_eq!(
            codex_gauges(&legacy.to_string()).unwrap().seven_day,
            Some(0)
        );
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
    fn a_codex_stop_recovers_a_plan_missing_from_its_final_message() {
        use crate::event::{Event, Parsed};
        use crate::state::{self, State, Step};

        let scratch = Scratch::new("proposed-plan");
        let path = scratch.0.join("rollout.jsonl");
        // Codex 0.153.2, measured 2026-09-05: the Plan item is completed,
        // but task_complete.last_agent_message is null. The Stop hook's
        // message-based flag alone cannot report the implementation prompt.
        let plan = json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed", "turn_id": "turn-plan",
                "item": { "type": "Plan", "id": "turn-plan-plan", "text": "PRIVATE PLAN" }
            }
        });
        let complete = json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete", "turn_id": "turn-plan", "last_agent_message": null
            }
        });
        std::fs::write(&path, format!("{plan}\n{REAL}\n{complete}\n")).unwrap();
        let mut rollouts = Rollouts::default();
        for id_field in ["turn_id", "prompt_id"] {
            let mut stop = json!({
                "src": "codex-win", "hook_event_name": "Stop", "session_id": "s1",
                "transcript_path": path.to_str().unwrap(), id_field: "turn-plan"
            });
            rollouts.attach(&mut stop);
            assert_eq!(stop.get("proposed_plan"), Some(&json!(true)), "{id_field}");
            assert_eq!(stop["gauges"]["ctx"], 82);
            assert!(!stop.to_string().contains("PRIVATE PLAN"));
            let Parsed::Event(event) = Event::parse(&stop, 1_000) else {
                panic!("expected a Stop event");
            };
            assert_eq!(
                state::step(State::Running, &event),
                Step::Set(State::Waiting)
            );
            assert_eq!(state::adopt(&event), Some(State::Waiting));

            stop["hook_event_name"] = json!("UserPromptSubmit");
            stop.as_object_mut().unwrap().remove("proposed_plan");
            rollouts.attach(&mut stop);
            assert!(stop.get("proposed_plan").is_none());
            let Parsed::Event(answered) = Event::parse(&stop, 2_000) else {
                panic!("expected a prompt event");
            };
            assert_eq!(
                state::step(State::Waiting, &answered),
                Step::Set(State::Running)
            );
        }
    }

    #[test]
    fn only_matching_cli_metadata_enables_terminal_handoffs() {
        use crate::event::{Event, Parsed};

        let scratch = Scratch::new("cli-metadata");
        let mut rollouts = Rollouts::default();
        for (index, source) in [
            json!("cli"),
            json!("vscode"),
            json!("exec"),
            json!({"subagent": {}}),
            Value::Null,
        ]
        .into_iter()
        .enumerate()
        {
            let path = scratch.0.join(format!("{index}.jsonl"));
            let metadata = json!({"type": "session_meta", "payload": {
                "id": "s1", "source": source, "cwd": "PRIVATE PATH"
            }});
            std::fs::write(&path, format!("{metadata}\n")).unwrap();
            for session in ["s1", "different-session"] {
                let mut event = json!({
                    "src": "codex-win", "session_id": session, "hook_event_name": "SessionStart",
                    "transcript_path": path.to_str().unwrap(), "codex_cli": true
                });
                rollouts.attach(&mut event);
                assert_eq!(
                    event["codex_cli"] == true,
                    source == "cli" && session == "s1"
                );
                assert!(!event.to_string().contains("PRIVATE PATH"));
                let Parsed::Event(parsed) = Event::parse(&event, 1) else {
                    panic!("expected event")
                };
                assert_eq!(parsed.codex_cli, source == "cli" && session == "s1");
            }
        }
        for source in ["codex-win", "claude-win"] {
            let mut forged = json!({"src": source, "codex_cli": true});
            rollouts.attach(&mut forged);
            assert!(forged.get("codex_cli").is_none());
        }
    }

    #[test]
    fn unfinished_metadata_is_retried_and_oversized_headers_are_bounded() {
        let scratch = Scratch::new("metadata-retry");
        let path = scratch.0.join("rollout.jsonl");
        let metadata = json!({"type": "session_meta", "payload": {"id": "s1", "source": "cli"}});
        std::fs::write(&path, metadata.to_string()).unwrap();
        let mut event = json!({"src": "codex-win", "session_id": "s1", "transcript_path": path.to_str().unwrap()});
        let mut rollouts = Rollouts::default();
        rollouts.attach(&mut event);
        assert!(event.get("codex_cli").is_none());
        std::fs::write(&path, format!("{metadata}\n")).unwrap();
        rollouts.attach(&mut event);
        assert_eq!(event["codex_cli"], true);

        let huge = scratch.0.join("oversized.jsonl");
        std::fs::write(&huge, format!("{}{metadata}\n", " ".repeat(64 * 1024))).unwrap();
        assert!(RolloutMetadata::read(&huge).is_none());
        // A later record cannot pretend to be the file's session metadata.
        std::fs::write(
            &huge,
            format!("{{\"type\":\"response_item\"}}\n{metadata}\n"),
        )
        .unwrap();
        assert!(RolloutMetadata::read(&huge).is_none());
    }

    #[test]
    fn a_rollout_plan_only_marks_its_own_codex_stop() {
        let scratch = Scratch::new("plan-turn");
        let path = scratch.0.join("rollout.jsonl");
        // No token_count: recovering a plan must not depend on finding gauges.
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",",
                "\"turn_id\":\"old\",\"item\":{\"type\":\"Plan\",\"text\":\"A plan\"}}}\n"
            ),
        )
        .unwrap();
        let mut rollouts = Rollouts::default();
        for (source, kind, turn_id, prompt_id, expected) in [
            ("codex-win", "Stop", Some("old"), None, true),
            ("codex-win", "Stop", Some("new"), None, false),
            ("codex-win", "Stop", None, None, false),
            ("codex-win", "Stop", Some(""), None, false),
            ("codex-win", "Stop", Some("new"), Some("old"), false),
            ("codex-win", "PostToolUse", Some("old"), None, false),
            ("claude-win", "Stop", Some("old"), None, false),
        ] {
            let mut event = json!({
                "src": source, "hook_event_name": kind, "turn_id": turn_id,
                "prompt_id": prompt_id, "transcript_path": path.to_str().unwrap()
            });
            rollouts.attach(&mut event);
            assert_eq!(
                event
                    .get("proposed_plan")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                expected,
                "{source} {kind} {turn_id:?} {prompt_id:?}"
            );
        }
    }

    #[test]
    fn plan_text_and_unfinished_items_are_not_completed_plans() {
        let scratch = Scratch::new("not-a-plan");
        let path = scratch.0.join("rollout.jsonl");
        let records = [
            json!({ "type": "response_item", "payload": {
                "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "<proposed_plan>example</proposed_plan>" }
                ]
            }}),
            json!({ "type": "event_msg", "payload": {
                "type": "item_started", "turn_id": "t",
                "item": { "type": "Plan", "text": "Still being written" }
            }}),
            json!({ "type": "event_msg", "payload": {
                "type": "item_completed", "turn_id": "t",
                "item": { "type": "AgentMessage", "text": "<proposed_plan>example</proposed_plan>" }
            }}),
            json!({ "type": "event_msg", "payload": {
                "type": "item_completed", "turn_id": "t",
                "item": { "type": "Plan", "text": " \n " }
            }}),
        ];
        let mut text = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        text.push_str(r#"{"type":"event_msg","payload":{"type":"item_completed","turn_id":"t","item":{"type":"Plan","text":"cut"#);
        std::fs::write(&path, text).unwrap();
        let mut event = json!({
            "src": "codex-win", "hook_event_name": "Stop", "turn_id": "t",
            "transcript_path": path.to_str().unwrap()
        });
        let mut rollouts = Rollouts::default();
        rollouts.attach(&mut event);
        assert!(event.get("proposed_plan").is_none());

        // The original hook flag remains authoritative when the tail lacks
        // a matching item, or the transcript cannot be read at all.
        event["proposed_plan"] = json!(true);
        rollouts.attach(&mut event);
        assert_eq!(event["proposed_plan"], true);
        event["transcript_path"] = json!(scratch.0.join("missing.jsonl").to_str().unwrap());
        rollouts.attach(&mut event);
        assert_eq!(event["proposed_plan"], true);
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
