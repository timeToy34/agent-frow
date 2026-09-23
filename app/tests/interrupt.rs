//! Real hook payload shapes, cancellation races, and retained session identity.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent_frow::event::Event;
use agent_frow::state::State;
use agent_frow::tracker::Tracker;
use serde_json::{Value, json};

fn send(tracker: &mut Tracker, source: &str, kind: &str, at: u64, extra: Value) {
    let mut payload = json!({
        "src": source, "hook_event_name": kind, "session_id": "session",
        "cwd": "/project", "project_dir": "/project", "turn_id": "first"
    });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    tracker.accept(Event::parse(&payload, at), at);
}

#[test]
fn interrupt_clears_running_and_waiting_without_changing_identity() {
    for source in ["codex-win", "codex-wsl"] {
        for pending in [false, true] {
            let mut tracker = Tracker::default();
            send(
                &mut tracker,
                source,
                "SessionStart",
                10,
                json!({"turn_id": null}),
            );
            send(&mut tracker, source, "UserPromptSubmit", 20, json!({}));
            if pending {
                send(&mut tracker, source, "PermissionRequest", 30, json!({}));
                assert!(tracker.answerable(0));
            }
            let previous = tracker.on_lane(0).unwrap().clone();
            send(
                &mut tracker,
                source,
                "Interrupt",
                40,
                json!({"cwd": "/project/frontend"}),
            );
            let session = tracker.on_lane(0).unwrap();
            assert_eq!(session.state, State::Connected);
            assert_eq!(session.effective_state(), State::Connected);
            assert_eq!(session.note, "Interrupted");
            assert_eq!(session.since, 40);
            assert_eq!(session.session_id, previous.session_id);
            assert_eq!(session.first_seen, previous.first_seen);
            assert_eq!(session.cwd, previous.cwd);
            assert!(!tracker.answerable(0));
        }
    }
}

#[test]
fn cancelled_results_cannot_replace_the_interrupt_or_disturb_a_retry() {
    for retry in [false, true] {
        let mut tracker = Tracker::default();
        send(&mut tracker, "codex-wsl", "UserPromptSubmit", 10, json!({}));
        send(&mut tracker, "codex-wsl", "Interrupt", 20, json!({}));
        if retry {
            send(
                &mut tracker,
                "codex-wsl",
                "UserPromptSubmit",
                30,
                json!({"turn_id": "retry"}),
            );
            send(
                &mut tracker,
                "codex-wsl",
                "PermissionRequest",
                40,
                json!({"turn_id": "retry"}),
            );
        }
        let previous = tracker.on_lane(0).unwrap().clone();
        for kind in [
            "Interrupt",
            "PostToolUse",
            "PreToolUse",
            "PermissionRequest",
            "Stop",
            "UserPromptSubmit",
        ] {
            send(
                &mut tracker,
                "codex-wsl",
                kind,
                50,
                json!({
                    "tool_name": "request_user_input", "proposed_plan": true,
                    "cwd": "/project/frontend", "gauges": {"ctx": 99}
                }),
            );
            let session = tracker.on_lane(0).unwrap();
            assert_eq!(session.state, previous.state, "{kind}, retry={retry}");
            assert_eq!(session.note, previous.note);
            assert_eq!(session.last_event, previous.last_event);
            assert_eq!(session.events, previous.events);
            assert_eq!(session.cwd, previous.cwd);
            assert_eq!(session.gauges, previous.gauges);
        }
        if retry {
            send(
                &mut tracker,
                "codex-wsl",
                "PostToolUse",
                60,
                json!({"turn_id": "retry"}),
            );
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
            send(
                &mut tracker,
                "codex-wsl",
                "Stop",
                70,
                json!({"turn_id": "retry"}),
            );
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Done);
        }
    }
}

#[test]
fn an_interrupt_delivered_after_a_new_prompt_only_cancels_its_own_turn() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "codex-wsl", "UserPromptSubmit", 10, json!({}));
    send(
        &mut tracker,
        "codex-wsl",
        "UserPromptSubmit",
        20,
        json!({"turn_id": "retry"}),
    );
    // A late result must not steal the turn identity from the newer prompt.
    send(&mut tracker, "codex-wsl", "PostToolUse", 30, json!({}));
    send(
        &mut tracker,
        "codex-wsl",
        "PermissionRequest",
        40,
        json!({"turn_id": "retry"}),
    );
    send(&mut tracker, "codex-wsl", "Interrupt", 50, json!({}));
    send(&mut tracker, "codex-wsl", "PostToolUse", 60, json!({}));
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Waiting);
    assert_eq!(tracker.on_lane(0).unwrap().last_event, 40);
    send(
        &mut tracker,
        "codex-wsl",
        "Interrupt",
        70,
        json!({"turn_id": "retry"}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Connected);
    send(
        &mut tracker,
        "codex-wsl",
        "UserPromptSubmit",
        80,
        json!({"turn_id": "third"}),
    );
    for id in ["first", "retry"] {
        send(
            &mut tracker,
            "codex-wsl",
            "PermissionRequest",
            90,
            json!({"turn_id": id}),
        );
    }
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
}

#[test]
fn subagents_keep_working_and_leave_the_interruption_note_visible() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "codex-wsl", "UserPromptSubmit", 10, json!({}));
    send(
        &mut tracker,
        "codex-wsl",
        "SubagentStart",
        20,
        json!({"agent_id": "child", "turn_id": "child-turn"}),
    );
    send(&mut tracker, "codex-wsl", "Interrupt", 30, json!({}));
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Running
    );
    send(
        &mut tracker,
        "codex-wsl",
        "PostToolUse",
        40,
        json!({"agent_id": "child", "cwd": "/project/child"}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().subagents["child"], 40);
    assert_eq!(tracker.on_lane(0).unwrap().note, "Interrupted");
    send(
        &mut tracker,
        "codex-wsl",
        "SubagentStop",
        50,
        json!({"agent_id": "child"}),
    );
    let session = tracker.on_lane(0).unwrap();
    assert!(session.subagents.is_empty());
    assert_eq!(session.effective_state(), State::Connected);
    assert_eq!(session.note, "Interrupted");
    assert_eq!(session.project().as_deref(), Some("project"));
    // SessionEnd is about the whole session, even if it includes the old id.
    send(&mut tracker, "codex-wsl", "SessionEnd", 60, json!({}));
    assert!(tracker.sessions.is_empty());
}

#[test]
fn an_interrupt_can_be_the_first_event_or_follow_mid_turn_adoption() {
    for introduction in [None, Some("PostToolUse"), Some("PermissionRequest")] {
        let mut tracker = Tracker::default();
        if let Some(kind) = introduction {
            send(&mut tracker, "codex-win", kind, 10, json!({}));
        }
        send(
            &mut tracker,
            "codex-win",
            "Interrupt",
            20,
            json!({"turn_id": " first "}),
        );
        assert_eq!(tracker.on_lane(0).unwrap().state, State::Connected);
        send(
            &mut tracker,
            "codex-win",
            "PermissionRequest",
            30,
            json!({}),
        );
        assert_eq!(tracker.on_lane(0).unwrap().last_event, 20);
        send(
            &mut tracker,
            "codex-win",
            "UserPromptSubmit",
            40,
            json!({"turn_id": "new"}),
        );
        assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
    }
}

#[test]
fn invalid_interrupts_do_not_adopt_or_refresh_sessions() {
    for extra in [
        json!({"turn_id": null}),
        json!({"turn_id": " "}),
        json!({"turn_id": 42}),
        json!({"agent_id": "child"}),
        json!({"agent_type": "worker"}),
        json!({"src": "claude-win"}),
    ] {
        let mut tracker = Tracker::default();
        send(&mut tracker, "codex-win", "Interrupt", 10, extra.clone());
        assert!(tracker.sessions.is_empty());
        send(
            &mut tracker,
            "codex-win",
            "PermissionRequest",
            20,
            json!({}),
        );
        send(&mut tracker, "codex-win", "Interrupt", 30, extra);
        assert_eq!(tracker.sessions.len(), 1);
        assert_eq!(tracker.on_lane(0).unwrap().state, State::Waiting);
        assert_eq!(tracker.on_lane(0).unwrap().last_event, 20);
        assert_eq!(tracker.events, 3);
        assert!(tracker.unrecognised_events.is_empty());
    }
}

#[test]
fn payloads_without_turn_ids_keep_their_existing_activity_behavior() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "codex-win",
        "PermissionRequest",
        10,
        json!({"turn_id": null}),
    );
    send(
        &mut tracker,
        "codex-win",
        "PostToolUse",
        20,
        json!({"turn_id": null}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
}
