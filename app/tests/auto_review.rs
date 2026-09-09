//! Automatic approval must neither request a human answer nor dismiss one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent_frow::event::{Event, Parsed};
use agent_frow::state::{self, State, Step, WaitingReason};
use agent_frow::tracker::Tracker;
use serde_json::{Value, json};

fn event(source: &str, kind: &str, at: u64, extra: Value) -> Event {
    let mut payload = json!({
        "src": source, "hook_event_name": kind, "session_id": "s",
        "turn_id": "t", "cwd": "/project", "tool_name": "Bash",
        // This is the post-enrichment event shape. Ingress stripping and
        // transcript verification are covered by the rollout tests.
        "codex_approvals_reviewer": "auto_review"
    });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    match Event::parse(&payload, at) {
        Parsed::Event(event) => *event,
        _ => panic!("expected an event"),
    }
}

fn send(tracker: &mut Tracker, source: &str, kind: &str, at: u64, extra: Value) {
    tracker.accept(Parsed::Event(Box::new(event(source, kind, at, extra))), at);
}

#[test]
fn automatic_shell_and_patch_review_have_no_human_wait_or_completion_dependency() {
    for source in ["codex-win", "codex-wsl"] {
        for tool in ["Bash", "apply_patch"] {
            let mut tracker = Tracker::default();
            send(&mut tracker, source, "UserPromptSubmit", 10, json!({}));
            send(
                &mut tracker,
                source,
                "PermissionRequest",
                20,
                json!({"tool_name":tool}),
            );
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
            assert_eq!(tracker.on_lane(0).unwrap().waiting_reason, None);
            assert!(!tracker.answerable(0));
            tracker.sweep(5 * 60 * 1000);
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
            assert!(!tracker.answerable(0));
            send(
                &mut tracker,
                source,
                "PostToolUse",
                6 * 60 * 1000,
                json!({"tool_name":tool}),
            );
            send(&mut tracker, source, "Stop", 6 * 60 * 1000 + 1, json!({}));
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Done);
        }
    }
}

#[test]
fn automatic_review_preserves_all_existing_wait_reasons_and_prompt_notes() {
    for source in ["codex-win", "codex-wsl"] {
        for (kind, extra, reason) in [
            (
                "PreToolUse",
                json!({"tool_name":"request_user_input"}),
                WaitingReason::Question,
            ),
            (
                "Stop",
                json!({"proposed_plan":true, "tool_name":null}),
                WaitingReason::Plan,
            ),
            (
                "PermissionRequest",
                json!({"codex_approvals_reviewer":"user"}),
                WaitingReason::Permission,
            ),
            (
                "Notification",
                json!({"notification_type":"agent_needs_input"}),
                WaitingReason::Other,
            ),
        ] {
            let mut tracker = Tracker::default();
            send(&mut tracker, source, "UserPromptSubmit", 10, json!({}));
            send(&mut tracker, source, kind, 20, extra);
            let previous = tracker.on_lane(0).unwrap().clone();
            assert_eq!(previous.state, State::Waiting);
            assert_eq!(previous.waiting_reason, Some(reason));
            send(
                &mut tracker,
                source,
                "PermissionRequest",
                30,
                json!({"tool_name":"apply_patch", "cwd":"/project/subfolder"}),
            );
            let session = tracker.on_lane(0).unwrap();
            assert_eq!(session.state, State::Waiting);
            assert_eq!(session.waiting_reason, Some(reason));
            assert_eq!(session.note, previous.note);
            assert_eq!(session.since, previous.since);
            assert_eq!(session.cwd, previous.cwd);
            assert_eq!(session.lane, previous.lane);
            assert!(tracker.answerable(0));
            send(
                &mut tracker,
                source,
                "UserPromptSubmit",
                40,
                json!({"turn_id":"next"}),
            );
            assert_eq!(tracker.on_lane(0).unwrap().waiting_reason, None);
        }
    }
}

#[test]
fn manual_unknown_and_unclassified_requests_still_need_the_user() {
    for source in ["codex-win", "codex-wsl", "claude-win", "claude-wsl"] {
        for reviewer in [
            Value::Null,
            json!("user"),
            json!("future"),
            json!({"mode":"auto_review"}),
        ] {
            let request = event(
                source,
                "PermissionRequest",
                10,
                json!({"codex_approvals_reviewer":reviewer}),
            );
            assert_eq!(
                state::step(State::Running, &request),
                Step::Set(State::Waiting)
            );
            assert_eq!(state::adopt(&request), Some(State::Waiting));
        }
        for tool in [
            Value::Null,
            json!("mcp__server__call"),
            json!("computer_use"),
            json!("request_permissions"),
            json!("Edit"),
        ] {
            let request = event(source, "PermissionRequest", 10, json!({"tool_name":tool}));
            assert_eq!(
                state::step(State::Running, &request),
                Step::Set(State::Waiting)
            );
        }
    }
    for source in ["claude-win", "claude-wsl"] {
        let request = event(source, "PermissionRequest", 10, json!({}));
        assert_eq!(
            state::step(State::Running, &request),
            Step::Set(State::Waiting)
        );
    }
}

#[test]
fn a_review_can_adopt_or_wake_idle_but_cannot_resurrect_finished_states() {
    for source in ["codex-win", "codex-wsl"] {
        let request = event(source, "PermissionRequest", 10, json!({}));
        assert_eq!(state::adopt(&request), Some(State::Running));
        assert_eq!(
            state::step(State::Idle, &request),
            Step::Set(State::Running)
        );
        for current in [
            State::Connected,
            State::Running,
            State::Waiting,
            State::Done,
            State::Error,
        ] {
            assert_eq!(state::step(current, &request), Step::Stay);
        }
        let mut tracker = Tracker::default();
        send(&mut tracker, source, "PermissionRequest", 10, json!({}));
        assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
        assert!(!tracker.answerable(0));
    }
}

#[test]
fn subagents_and_cancelled_reviews_cannot_change_the_main_lane() {
    for source in ["codex-win", "codex-wsl"] {
        let mut tracker = Tracker::default();
        send(&mut tracker, source, "UserPromptSubmit", 10, json!({}));
        send(
            &mut tracker,
            source,
            "PreToolUse",
            20,
            json!({"tool_name":"request_user_input"}),
        );
        send(
            &mut tracker,
            source,
            "PermissionRequest",
            30,
            json!({"agent_id":"child"}),
        );
        assert_eq!(
            tracker.on_lane(0).unwrap().waiting_reason,
            Some(WaitingReason::Question)
        );
        send(&mut tracker, source, "Interrupt", 40, json!({}));
        assert_eq!(tracker.on_lane(0).unwrap().waiting_reason, None);
        send(
            &mut tracker,
            source,
            "UserPromptSubmit",
            50,
            json!({"turn_id":"retry"}),
        );
        send(
            &mut tracker,
            source,
            "PreToolUse",
            60,
            json!({"turn_id":"retry", "tool_name":"request_user_input"}),
        );
        let previous = tracker.on_lane(0).unwrap().clone();
        send(
            &mut tracker,
            source,
            "PermissionRequest",
            70,
            json!({"codex_approvals_reviewer":null}),
        );
        let session = tracker.on_lane(0).unwrap();
        assert_eq!(session.waiting_reason, Some(WaitingReason::Question));
        assert_eq!(session.note, previous.note);
        assert_eq!(session.events, previous.events);
    }
}

#[test]
fn a_changed_wait_cause_is_recorded_even_when_the_state_stays_waiting() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "codex-wsl",
        "PermissionRequest",
        10,
        json!({"codex_approvals_reviewer":"user"}),
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().waiting_reason,
        Some(WaitingReason::Permission)
    );
    send(
        &mut tracker,
        "codex-wsl",
        "PreToolUse",
        20,
        json!({"tool_name":"request_user_input"}),
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().waiting_reason,
        Some(WaitingReason::Question)
    );
    send(
        &mut tracker,
        "codex-wsl",
        "PostToolUse",
        30,
        json!({"tool_name":"request_user_input"}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
    assert_eq!(tracker.on_lane(0).unwrap().waiting_reason, None);
}
