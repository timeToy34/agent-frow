//! Building the request body, and the field allowlist that is this binary's
//! reason to exist.

use serde_json::{Map, Value};

/// The only payload fields that ever leave the agent's process tree.
///
/// Everything else is dropped here, unread. This matters most for
/// `tool_response`: a `Read` of a 2 MB file produces a 2 MB hook payload, and
/// forwarding it would put the user's source through another process, onto a
/// socket, and into a capture file thousands of times a day.
///
/// These are *names*, not meanings. This binary never decides what a hook
/// implies about an agent — that is the app's job, and keeping it there is what
/// lets the state machine change without the hook command string changing, which
/// is what stops Codex demanding to be re-trusted.
const ALLOWED: [&str; 13] = [
    "hook_event_name",
    "session_id",
    "cwd",
    "prompt_id",
    "turn_id",
    "tool_name",
    "agent_id",
    "agent_type",
    "source",
    "notification_type",
    "reason",
    "end_reason",
    "permission_mode",
];

/// The tag Codex wraps a plan in when it ends a plan-mode turn by proposing
/// one; its TUI and desktop app recognise exactly this to offer "implement
/// the plan?".
const PROPOSED_PLAN_TAG: &str = "<proposed_plan>";

/// Projects the agent's payload onto the allowlist and adds what only this
/// process can know: which of the four flavors ran it, and where it is running.
///
/// Ancestry is a parameter, not a call, so the wire shape is testable on a
/// machine where the real walk is empty — and so nothing in the payload can
/// ever masquerade as it.
pub fn project(
    payload: &Value,
    source: &str,
    wt_session: Option<String>,
    ancestors: &[crate::ancestry::Ancestor],
    now_ms: u128,
) -> Value {
    let mut out = Map::new();
    out.insert("t".to_owned(), Value::from(now_ms as u64));
    out.insert("src".to_owned(), Value::String(source.to_owned()));

    if let Some(object) = payload.as_object() {
        for key in ALLOWED {
            match object.get(key) {
                // Only scalars pass. A field that arrives as an object or an
                // array is a shape we did not expect, and copying it would be
                // exactly the leak the allowlist exists to prevent.
                Some(value) if value.is_string() || value.is_number() || value.is_boolean() => {
                    out.insert(key.to_owned(), value.clone());
                }
                _ => {}
            }
        }
    }

    // One fact about a field that itself never leaves: whether Codex's `Stop`
    // carries a proposed plan. Codex has no dialog for "approve this plan?" —
    // its UI draws that prompt when a turn *ends* with the plan wrapped in
    // this tag, so the tag is the only evidence there is. Still a name and a
    // shape, not a meaning: this reports that the tag is present; what that
    // implies for a lane is decided in the app, as with every other field.
    if payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains(PROPOSED_PLAN_TAG))
    {
        out.insert("proposed_plan".to_owned(), Value::Bool(true));
    }

    if let Some(wt) = wt_session {
        out.insert("wt_session".to_owned(), Value::String(wt));
    }
    if !ancestors.is_empty() {
        out.insert(
            "ancestors".to_owned(),
            Value::Array(ancestors.iter().map(|a| Value::from(a.pid)).collect()),
        );
        // Parallel to `ancestors`, index for index. A separate array rather
        // than an array of objects so an app that predates it still parses the
        // pids exactly as before.
        out.insert(
            "ancestor_names".to_owned(),
            Value::Array(
                ancestors
                    .iter()
                    .map(|a| Value::String(a.exe.clone()))
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn projected(payload: serde_json::Value) -> Map<String, Value> {
        match project(&payload, "claude-wsl", None, &[], 1_700_000_000_000) {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[test]
    fn tool_output_never_survives_projection() {
        let out = projected(json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s1",
            "tool_name": "Read",
            "tool_input": { "file_path": "/home/me/secret.rs" },
            "tool_response": { "file": { "content": "SECRET CONTENTS" } },
            "transcript_path": "/home/me/.claude/projects/x/transcript.jsonl",
            "last_assistant_message": "I have read the file",
            "prompt": "do the thing",
        }));

        assert_eq!(out.get("hook_event_name"), Some(&json!("PostToolUse")));
        assert_eq!(out.get("tool_name"), Some(&json!("Read")));
        for leaky in [
            "tool_input",
            "tool_response",
            "transcript_path",
            "last_assistant_message",
            "prompt",
        ] {
            assert!(!out.contains_key(leaky), "{leaky} must not be forwarded");
        }
    }

    #[test]
    fn a_proposed_plan_is_reported_as_a_flag_and_never_as_text() {
        // Codex ends a plan-mode turn with the plan in the message, and its UI
        // asks "implement?" from that. The flag is the evidence; the plan
        // itself is the user's content and stays where it is.
        let out = projected(json!({
            "hook_event_name": "Stop",
            "session_id": "s1",
            "last_assistant_message": "<proposed_plan>\n# Rebuild the deck\n</proposed_plan>",
        }));
        assert_eq!(out.get("proposed_plan"), Some(&json!(true)));
        assert!(!out.contains_key("last_assistant_message"));

        // An ordinary final message sets nothing — absence, not `false`, so an
        // app that predates the flag sees the exact record it always did.
        let out = projected(json!({
            "hook_event_name": "Stop",
            "last_assistant_message": "Done. The deck validates at 35 slides.",
        }));
        assert!(!out.contains_key("proposed_plan"));

        // And the payload cannot assert it by name.
        let out = projected(json!({ "hook_event_name": "Stop", "proposed_plan": true }));
        assert!(!out.contains_key("proposed_plan"));
    }

    #[test]
    fn non_scalar_values_of_allowed_keys_are_dropped() {
        // A future agent version could turn one of these into an object. The
        // allowlist is by name, so the shape check is what keeps it honest.
        let out = projected(json!({
            "hook_event_name": "PostToolUse",
            "cwd": { "path": "/home/me/project" },
            "reason": ["clear"],
        }));
        assert!(!out.contains_key("cwd"));
        assert!(!out.contains_key("reason"));
    }

    #[test]
    fn ancestors_and_their_names_travel_as_parallel_arrays() {
        use crate::ancestry::Ancestor;
        let chain = [
            Ancestor {
                pid: 100,
                exe: "wsl.exe".to_owned(),
            },
            Ancestor {
                pid: 200,
                exe: "WindowsTerminal.exe".to_owned(),
            },
        ];
        let out = match project(&json!({}), "claude-wsl", None, &chain, 0) {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        };
        assert_eq!(out.get("ancestors"), Some(&json!([100, 200])));
        assert_eq!(
            out.get("ancestor_names"),
            Some(&json!(["wsl.exe", "WindowsTerminal.exe"])),
            "index-aligned with the pids, so the app can pair them"
        );
    }

    #[test]
    fn the_payload_cannot_forge_an_ancestry() {
        // Both keys come only from our own process walk. A payload naming them
        // must not smuggle its own values through the projection.
        let out = projected(json!({
            "hook_event_name": "Stop",
            "ancestors": [12345],
            "ancestor_names": ["icue.exe"],
        }));
        assert!(!out.contains_key("ancestors"));
        assert!(!out.contains_key("ancestor_names"));
    }

    #[test]
    fn a_payload_that_is_not_an_object_still_produces_a_record() {
        // Never fail the agent over a payload shape. The app can see that an
        // event arrived with nothing in it, which is more useful than silence.
        let out = projected(json!("nonsense"));
        assert_eq!(out.get("src"), Some(&json!("claude-wsl")));
        assert!(!out.contains_key("hook_event_name"));
    }
}
