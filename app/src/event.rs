//! One hook event, as it reaches us.
//!
//! The hook already dropped everything that is not on its allowlist, so this is
//! only naming what survived. It classifies and it never interprets: what an
//! event *means* for a lane lives in [`crate::state`], so the meaning can change
//! without the wire format or the registered command string changing with it.

use std::path::PathBuf;

use serde_json::Value;

/// The hook events we register. Anything else is recorded as unrecognised
/// rather than guessed at — a new agent release should show up as a number in
/// the window, not as behaviour nobody can explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SessionStart,
    UserPromptSubmit,
    /// Registered for Codex only, and only for its question tool — see
    /// `install::QUESTION_TOOL_MATCHER`.
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionDenied,
    Notification,
    SubagentStart,
    SubagentStop,
    Stop,
    StopFailure,
    SessionEnd,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PostToolUseFailure" => Self::PostToolUseFailure,
            "PermissionRequest" => Self::PermissionRequest,
            "PermissionDenied" => Self::PermissionDenied,
            "Notification" => Self::Notification,
            "SubagentStart" => Self::SubagentStart,
            "SubagentStop" => Self::SubagentStop,
            "Stop" => Self::Stop,
            "StopFailure" => Self::StopFailure,
            "SessionEnd" => Self::SessionEnd,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::Notification => "Notification",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::SessionEnd => "SessionEnd",
        }
    }
}

/// One process above the agent, as the hook recorded it: the pid, and — when
/// the hook is new enough to send `ancestor_names` — the executable basename
/// that pid had at the moment of the event. The name is the recycling check:
/// at summon time a window only counts if its pid still resolves to this
/// executable. `None` (an old hook, or a process the snapshot had no row for)
/// falls back to the stricter terminal-class rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: u32,
    pub exe: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Event {
    /// When it happened, on the same machine's clock — the hook runs Windows-side
    /// even for a WSL agent, so there is only ever one clock involved.
    pub at: u64,
    /// Which flavor: `claude-win`, `codex-wsl`, and so on.
    pub source: String,
    pub kind: Kind,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub tool_name: Option<String>,
    /// `SessionStart`'s own `source` field: `startup`, `resume`, `clear`, `compact`.
    pub start_source: Option<String>,
    pub notification: Option<String>,
    /// A `Stop` whose final message carried Codex's `<proposed_plan>` tag —
    /// the hook reports the tag, this names it. Codex has no dialog for
    /// approving a plan: its UI asks "implement?" when a turn ends like this.
    pub proposed_plan: bool,
    /// The subagent this event belongs to, when it belongs to one — subagents
    /// share their parent's `session_id` and are told apart by this. Their
    /// events never change the lane's state (see [`crate::state::step`]); the
    /// id is what lets the tracker keep a roster of which ones are still busy.
    pub agent: Option<String>,
    /// This event belongs to a subagent (an `agent_id` or `agent_type` is
    /// present). Either field is enough — a guard that needs both fails open.
    pub subagent: bool,
    /// Milestone 4 carries this; nothing reads it yet.
    pub wt_session: Option<String>,
    /// The processes above the agent, nearest first — what summon walks to
    /// find the window the agent is sitting in.
    pub ancestors: Vec<Ancestor>,
}

/// What arrived, once the shape is known.
#[derive(Debug, Clone)]
pub enum Parsed {
    Event(Box<Event>),
    /// A hook event we do not register, or one with no name at all. Carries
    /// whatever it called itself, so the window can show the name rather than a
    /// bare count.
    Unrecognised {
        source: String,
        name: String,
    },
}

impl Event {
    /// Reads one projected payload. `received_ms` is the fallback clock for a
    /// payload with no timestamp of its own.
    pub fn parse(value: &Value, received_ms: u64) -> Parsed {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|found| !found.is_empty())
                .map(str::to_owned)
        };
        let source = text("src").unwrap_or_else(|| "unknown".to_owned());
        let name = text("hook_event_name");
        let Some(kind) = name.as_deref().and_then(Kind::parse) else {
            return Parsed::Unrecognised {
                source,
                name: name.unwrap_or_else(|| "(no hook_event_name)".to_owned()),
            };
        };

        // The hook's clock, clamped: a timestamp from the future would make a
        // lane's "for how long" count backwards.
        let at = value
            .get("t")
            .and_then(Value::as_u64)
            .filter(|stamp| *stamp <= received_ms)
            .unwrap_or(received_ms);

        let session_id = text("session_id").unwrap_or_default();
        // `ancestor_names` is index-aligned with `ancestors`, sent only by
        // newer hooks. Pairing per index means a missing, short, or malformed
        // names array degrades that one ancestor to "identity unknown" — it
        // can never poison the pid list.
        let names = value.get("ancestor_names").and_then(Value::as_array);
        let ancestors = value
            .get("ancestors")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .enumerate()
                    .filter_map(|(index, pid)| {
                        let pid = pid.as_u64()? as u32;
                        let exe = names
                            .and_then(|list| list.get(index))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned);
                        Some(Ancestor { pid, exe })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Parsed::Event(Box::new(Event {
            at,
            source,
            kind,
            session_id,
            cwd: text("cwd").map(PathBuf::from),
            tool_name: text("tool_name"),
            start_source: text("source"),
            notification: text("notification_type"),
            proposed_plan: value
                .get("proposed_plan")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            subagent: text("agent_id").is_some() || text("agent_type").is_some(),
            agent: text("agent_id"),
            wt_session: text("wt_session"),
            ancestors,
        }))
    }

    /// The short line a lane shows: what just happened on it.
    pub fn note(&self) -> String {
        let detail = self
            .tool_name
            .as_deref()
            .or(self.notification.as_deref())
            .or(self.start_source.as_deref())
            .or(self.proposed_plan.then_some("proposed plan"));
        match detail {
            Some(detail) => format!("{} {detail}", self.kind.label()),
            None => self.kind.label().to_owned(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(value: serde_json::Value, now: u64) -> Event {
        match Event::parse(&value, now) {
            Parsed::Event(event) => *event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn a_subagent_event_is_marked_as_one() {
        let parsed = event(
            json!({
                "src": "claude-win",
                "hook_event_name": "PostToolUse",
                "session_id": "s1",
                "tool_name": "Bash",
                "agent_id": "sub-7",
            }),
            10,
        );
        assert!(parsed.subagent);
        assert_eq!(parsed.note(), "PostToolUse Bash");
    }

    #[test]
    fn a_question_about_to_be_asked_is_named_as_such() {
        let parsed = event(
            json!({
                "src": "codex-wsl",
                "hook_event_name": "PreToolUse",
                "session_id": "s1",
                "tool_name": "request_user_input",
            }),
            10,
        );
        assert_eq!(parsed.kind, Kind::PreToolUse);
        assert!(!parsed.subagent);
        assert_eq!(parsed.note(), "PreToolUse request_user_input");
    }

    #[test]
    fn a_stop_that_proposes_a_plan_says_so() {
        let parsed = event(
            json!({
                "src": "codex-wsl",
                "hook_event_name": "Stop",
                "session_id": "s1",
                "proposed_plan": true,
            }),
            10,
        );
        assert!(parsed.proposed_plan);
        assert_eq!(parsed.note(), "Stop proposed plan");
        // Absent — the record every hook sent before the flag existed.
        let plain = event(
            json!({ "src": "codex-wsl", "hook_event_name": "Stop", "session_id": "s1" }),
            10,
        );
        assert!(!plain.proposed_plan);
        assert_eq!(plain.note(), "Stop");
    }

    #[test]
    fn ancestors_pair_with_their_names_by_index() {
        let parsed = event(
            json!({
                "src": "claude-win",
                "hook_event_name": "Stop",
                "session_id": "s1",
                "ancestors": [100, 200],
                "ancestor_names": ["claude.exe", "explorer.exe"],
            }),
            10,
        );
        assert_eq!(
            parsed.ancestors,
            vec![
                Ancestor {
                    pid: 100,
                    exe: Some("claude.exe".to_owned())
                },
                Ancestor {
                    pid: 200,
                    exe: Some("explorer.exe".to_owned())
                },
            ]
        );
    }

    #[test]
    fn an_old_hook_without_names_degrades_to_identity_unknown() {
        let parsed = event(
            json!({
                "src": "claude-win",
                "hook_event_name": "Stop",
                "session_id": "s1",
                "ancestors": [100, 200],
            }),
            10,
        );
        assert_eq!(
            parsed.ancestors,
            vec![
                Ancestor {
                    pid: 100,
                    exe: None
                },
                Ancestor {
                    pid: 200,
                    exe: None
                },
            ]
        );
    }

    #[test]
    fn a_malformed_names_array_never_poisons_the_pids() {
        // Shorter than the pids, with a non-string and an empty entry: each
        // index degrades alone.
        let parsed = event(
            json!({
                "src": "claude-win",
                "hook_event_name": "Stop",
                "session_id": "s1",
                "ancestors": [100, 200, 300],
                "ancestor_names": [42, "  "],
            }),
            10,
        );
        assert_eq!(
            parsed.ancestors,
            vec![
                Ancestor {
                    pid: 100,
                    exe: None
                },
                Ancestor {
                    pid: 200,
                    exe: None
                },
                Ancestor {
                    pid: 300,
                    exe: None
                },
            ]
        );
    }

    #[test]
    fn an_event_we_do_not_register_is_named_rather_than_dropped() {
        match Event::parse(
            &json!({ "src": "codex-wsl", "hook_event_name": "TeammateIdle" }),
            10,
        ) {
            Parsed::Unrecognised { source, name } => {
                assert_eq!(source, "codex-wsl");
                assert_eq!(name, "TeammateIdle");
            }
            other => panic!("expected unrecognised, got {other:?}"),
        }
    }

    #[test]
    fn a_timestamp_from_the_future_does_not_win() {
        // Otherwise a lane reports how long it has been in a state as a number
        // that counts backwards.
        let parsed = event(
            json!({ "src": "claude-win", "hook_event_name": "Stop", "t": 9_999 }),
            500,
        );
        assert_eq!(parsed.at, 500);
    }
}
