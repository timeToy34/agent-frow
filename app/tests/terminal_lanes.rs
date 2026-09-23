//! A terminal's lane survives conversation changes, never borrowing identity
//! from a folder or allowing an inactive conversation to reclaim the lane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent_frow::event::Event;
use agent_frow::lifecycle::Action;
use agent_frow::state::State;
use agent_frow::tracker::Tracker;
use serde_json::{Value, json};

fn send(tracker: &mut Tracker, session: &str, kind: &str, at: u64, extra: Value) {
    let mut value = json!({
        "src": "codex-wsl", "session_id": session, "hook_event_name": kind,
        "wt_session": "tab-one", "codex_cli": true, "cwd": "/project", "project_dir":"/project"
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    tracker.accept(Event::parse(&value, at), at);
}

#[test]
fn a_new_conversation_keeps_the_lane_but_resets_conversation_data() {
    let mut tracker = Tracker::default();
    tracker.settings.lanes[0].name = "My Codex".to_owned();
    send(
        &mut tracker,
        "old",
        "SessionStart",
        10,
        json!({
            "gauges": {"ctx": 90, "d7": 65}, "ancestors": [10], "ancestor_names": ["wsl.exe"]
        }),
    );
    send(
        &mut tracker,
        "old",
        "SubagentStart",
        20,
        json!({"agent_id": "child"}),
    );
    send(&mut tracker, "old", "PermissionRequest", 30, json!({}));
    tracker.sessions[0].failure = Some("rate limit");
    let settings = tracker.settings.clone();
    tracker.selected = Some(0);
    tracker.locked = true;

    send(
        &mut tracker,
        "new",
        "UserPromptSubmit",
        40,
        json!({"cwd": "/new-project", "project_dir":"/new-project"}),
    );
    assert_eq!(tracker.sessions.len(), 1);
    let session = tracker.on_lane(0).unwrap();
    assert_eq!(session.session_id, "new");
    assert_eq!(session.state, State::Running);
    assert_eq!(session.project().as_deref(), Some("new-project"));
    assert_eq!(
        (
            session.first_seen,
            session.since,
            session.last_event,
            session.events
        ),
        (10, 40, 40, 1)
    );
    assert!(session.subagents.is_empty());
    assert!(session.gauges.is_empty());
    assert!(session.failure.is_none());
    assert_eq!(session.ancestors[0].pid, 10);
    assert_eq!(tracker.settings, settings);
    assert_eq!((tracker.selected, tracker.locked), (Some(0), true));
}

#[test]
fn interrupting_and_retrying_a_task_does_not_add_a_lane() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "same",
        "UserPromptSubmit",
        10,
        json!({"turn_id": "first"}),
    );
    send(
        &mut tracker,
        "same",
        "Interrupt",
        20,
        json!({"turn_id": "first", "cwd": "/project/frontend"}),
    );
    let session = tracker.on_lane(0).unwrap();
    assert_eq!(session.state, State::Connected);
    assert_eq!(session.note, "Interrupted");
    assert_eq!(session.project().as_deref(), Some("project"));
    assert_eq!(session.wt_session.as_deref(), Some("tab-one"));
    assert!(!tracker.answerable(0));
    send(
        &mut tracker,
        "same",
        "UserPromptSubmit",
        30,
        json!({"turn_id": "retry"}),
    );
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "same");
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
    // A cancelled prompt must not advance terminal routing's timestamp: the
    // current turn's subsequently delivered event can have an earlier clock.
    send(
        &mut tracker,
        "same",
        "UserPromptSubmit",
        50,
        json!({"turn_id": "first"}),
    );
    send(
        &mut tracker,
        "same",
        "PermissionRequest",
        60,
        json!({"turn_id": "retry", "t": 40}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Waiting);
}

#[test]
fn inactive_conversations_cannot_reappear_or_change_the_current_lane() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "old", "SessionStart", 10, json!({}));
    send(&mut tracker, "new", "SessionStart", 20, json!({}));
    send(&mut tracker, "new", "PermissionRequest", 30, json!({}));
    for kind in [
        "PostToolUse",
        "PermissionRequest",
        "Stop",
        "SessionEnd",
        "SubagentStart",
    ] {
        // Metadata and even WT_SESSION may be missing on a delayed event.
        send(
            &mut tracker,
            "old",
            kind,
            40,
            json!({"wt_session": null, "codex_cli": false}),
        );
        assert_eq!(tracker.sessions.len(), 1, "{kind}");
        assert_eq!(tracker.on_lane(0).unwrap().session_id, "new");
        assert_eq!(tracker.on_lane(0).unwrap().state, State::Waiting, "{kind}");
        assert_eq!(tracker.on_lane(0).unwrap().last_event, 30);
    }
    for kind in ["UserPromptSubmit", "SessionStart", "SubagentStart"] {
        send(&mut tracker, "old", kind, 50, json!({"agent_id": "child"}));
        send(
            &mut tracker,
            "unseen",
            kind,
            60,
            json!({"agent_id": "child"}),
        );
        assert_eq!(tracker.sessions.len(), 1, "subagent {kind}");
        assert_eq!(tracker.on_lane(0).unwrap().session_id, "new");
    }
}

#[test]
fn an_old_interrupt_timestamp_still_identifies_cancelled_results() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "same",
        "UserPromptSubmit",
        10,
        json!({"turn_id": "first"}),
    );
    send(
        &mut tracker,
        "same",
        "UserPromptSubmit",
        30,
        json!({"turn_id": "retry"}),
    );
    send(
        &mut tracker,
        "same",
        "PermissionRequest",
        40,
        json!({"turn_id": "retry"}),
    );
    send(
        &mut tracker,
        "same",
        "Interrupt",
        50,
        json!({"turn_id": "first", "t": 20}),
    );
    send(
        &mut tracker,
        "same",
        "PostToolUse",
        60,
        json!({"turn_id": "first"}),
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Waiting);
    assert_eq!(tracker.on_lane(0).unwrap().last_event, 40);
}

#[test]
fn only_a_fresh_foreground_lifecycle_event_switches_conversations() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "old", "SessionStart", 10, json!({}));
    send(&mut tracker, "new", "PostToolUse", 20, json!({}));
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "old");
    send(&mut tracker, "new", "UserPromptSubmit", 30, json!({}));
    for id in ["old", "new"] {
        for kind in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
            send(&mut tracker, id, kind, 100, json!({"t": 15}));
            assert_eq!(tracker.on_lane(0).unwrap().session_id, "new");
            assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
        }
    }
    // Explicitly revisiting the old conversation switches the same lane back.
    send(&mut tracker, "old", "UserPromptSubmit", 110, json!({}));
    send(&mut tracker, "new", "SessionEnd", 120, json!({}));
    send(&mut tracker, "new", "PostToolUse", 130, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "old");
    send(&mut tracker, "old", "SessionEnd", 140, json!({}));
    assert!(tracker.sessions.is_empty());
}

#[test]
fn identical_folders_in_different_tabs_or_sources_keep_separate_lanes() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "a", "SessionStart", 10, json!({}));
    send(
        &mut tracker,
        "b",
        "SessionStart",
        20,
        json!({"wt_session": "tab-two"}),
    );
    send(
        &mut tracker,
        "c",
        "SessionStart",
        30,
        json!({"src": "codex-win"}),
    );
    send(
        &mut tracker,
        "d",
        "SessionStart",
        40,
        json!({"src": "claude-wsl"}),
    );
    send(&mut tracker, "a-new", "UserPromptSubmit", 50, json!({}));
    assert_eq!(tracker.sessions.len(), 4);
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "a-new");
    assert_eq!(tracker.on_lane(1).unwrap().session_id, "b");
    assert_eq!(tracker.on_lane(2).unwrap().session_id, "c");
}

#[test]
fn missing_or_desktop_identity_never_merges_conversations() {
    for extra in [
        json!({"codex_cli": false}),
        json!({"wt_session": null}),
        json!({"wt_session": " "}),
        json!({"src": "claude-wsl"}),
    ] {
        let mut tracker = Tracker::default();
        send(&mut tracker, "a", "SessionStart", 10, extra.clone());
        send(&mut tracker, "b", "UserPromptSubmit", 20, extra);
        assert_eq!(tracker.sessions.len(), 2);
    }
}

#[test]
fn learning_a_terminal_identity_removes_a_previously_unclassified_duplicate() {
    for duplicate_first in [false, true] {
        let mut tracker = Tracker::default();
        if duplicate_first {
            send(
                &mut tracker,
                "new",
                "PostToolUse",
                5,
                json!({"codex_cli": false}),
            );
        }
        send(&mut tracker, "old", "SessionStart", 10, json!({}));
        let original_lane = tracker
            .sessions
            .iter()
            .find(|s| s.session_id == "old")
            .unwrap()
            .lane;
        if !duplicate_first {
            send(
                &mut tracker,
                "new",
                "PostToolUse",
                15,
                json!({"codex_cli": false}),
            );
        }
        assert_eq!(tracker.sessions.len(), 2);
        send(&mut tracker, "new", "UserPromptSubmit", 20, json!({}));
        assert_eq!(tracker.sessions.len(), 1);
        assert_eq!(tracker.sessions[0].session_id, "new");
        assert_eq!(tracker.sessions[0].lane, original_lane);
    }
}

#[test]
fn an_off_keyboard_terminal_keeps_one_place_in_the_queue_across_conversations() {
    let mut tracker = Tracker::default();
    for i in 0..tracker.settings.lane_count {
        send(
            &mut tracker,
            &format!("other-{i}"),
            "SessionStart",
            10 + i as u64,
            json!({"wt_session": format!("other-{i}")}),
        );
    }
    send(&mut tracker, "old", "SessionStart", 30, json!({}));
    assert!(tracker.sessions.last().unwrap().lane.is_none());
    let first_seen = tracker.sessions.last().unwrap().first_seen;
    send(&mut tracker, "new", "UserPromptSubmit", 40, json!({}));
    assert_eq!(tracker.sessions.len(), tracker.settings.lane_count + 1);
    assert_eq!(tracker.sessions.last().unwrap().session_id, "new");
    assert_eq!(tracker.sessions.last().unwrap().first_seen, first_seen);
    assert!(tracker.sessions.last().unwrap().lane.is_none());
}

#[test]
fn the_journal_records_handoffs_and_removals_without_every_tool_event() {
    let mut tracker = Tracker::default();
    let (sender, receiver) = std::sync::mpsc::sync_channel(16);
    tracker.lifecycle = Some(sender);
    send(&mut tracker, "old", "SessionStart", 10, json!({}));
    send(&mut tracker, "old", "PostToolUse", 20, json!({}));
    send(&mut tracker, "new", "UserPromptSubmit", 30, json!({}));
    send(&mut tracker, "old", "SessionEnd", 40, json!({}));
    send(&mut tracker, "new", "SessionEnd", 50, json!({}));
    let records = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(
        records.iter().map(|r| r.action).collect::<Vec<_>>(),
        [Action::Created, Action::Replaced, Action::Removed]
    );
    assert_eq!(records[1].previous_session_id.as_deref(), Some("old"));
    assert_eq!(records[1].session_id, "new");
    assert_eq!(records[1].terminal_id.as_deref(), Some("tab-one"));
    assert_eq!(records[1].lane, Some(0));
    assert_eq!(records[1].at, 30);
    send(&mut tracker, "new", "SessionStart", 60, json!({}));
    tracker.dismiss(0);
    assert_eq!(receiver.try_iter().last().unwrap().reason, "dismissed");
}

#[test]
fn a_full_journal_queue_does_not_block_a_handoff() {
    let mut tracker = Tracker::default();
    let (sender, _receiver) = std::sync::mpsc::sync_channel(0);
    tracker.lifecycle = Some(sender);
    send(&mut tracker, "a", "SessionStart", 10, json!({}));
    send(&mut tracker, "b", "UserPromptSubmit", 20, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "b");
}
