//! The transition table, and adoption.
//!
//! These are the bugs that made the previous application unusable, so they are
//! the ones worth pinning down: a lane stuck in Waiting, a lane cleared while
//! somebody was being asked a question, and a lane that dropped to Connected in
//! the middle of a turn.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent_frow::event::{Event, Parsed};
use agent_frow::state::{self, State, Step};
use serde_json::{Value, json};

fn event(name: &str, extra: Value) -> Event {
    let mut payload = json!({
        "src": "claude-win",
        "hook_event_name": name,
        "session_id": "s1",
        "cwd": "C:\\dev\\thing",
    });
    if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    match Event::parse(&payload, 1_000) {
        Parsed::Event(event) => *event,
        other => panic!("expected an event, got {other:?}"),
    }
}

#[test]
fn a_permission_request_asks_for_the_user_and_activity_clears_it() {
    // The whole design in one test. Nothing is correlated: the next tool that
    // finishes is what says somebody answered.
    assert_eq!(
        state::step(State::Running, &event("PermissionRequest", json!({}))),
        Step::Set(State::Waiting)
    );
    assert_eq!(
        state::step(
            State::Waiting,
            &event("PostToolUse", json!({ "tool_name": "Bash" }))
        ),
        Step::Set(State::Running)
    );
}

#[test]
fn activity_never_resurrects_a_finished_turn() {
    // Several hook processes post concurrently, so a PostToolUse emitted before
    // Stop can arrive after it. Promoting from anything but Waiting would put a
    // finished lane back to Running and leave it there until the next Stop.
    for finished in [State::Done, State::Error, State::Connected] {
        assert_eq!(
            state::step(
                finished,
                &event("PostToolUse", json!({ "tool_name": "Read" }))
            ),
            Step::Stay,
            "{finished:?} must not be promoted by a late tool event"
        );
    }
}

#[test]
fn an_idle_lane_is_revived_by_genuine_activity() {
    // The only-from-Waiting concurrency guard exists for events racing
    // seconds apart; anything arriving after half an hour of silence is real.
    assert_eq!(
        state::step(
            State::Idle,
            &event("PostToolUse", json!({ "tool_name": "Bash" }))
        ),
        Step::Set(State::Running)
    );
    assert_eq!(
        state::step(State::Idle, &event("PermissionDenied", json!({}))),
        Step::Set(State::Running)
    );
    assert_eq!(
        state::step(State::Idle, &event("UserPromptSubmit", json!({}))),
        Step::Set(State::Running)
    );
    // An idle notification on an already-idle lane says nothing new.
    assert_eq!(
        state::step(
            State::Idle,
            &event(
                "Notification",
                json!({ "notification_type": "idle_prompt" })
            )
        ),
        Step::Stay
    );
}

#[test]
fn mid_turn_compaction_does_not_demote_a_live_turn() {
    // Claude fires SessionStart when it compacts mid-turn. Without the guard a
    // running lane drops to Connected and stays there until Stop.
    assert_eq!(
        state::step(
            State::Running,
            &event("SessionStart", json!({ "source": "compact" }))
        ),
        Step::Stay
    );
    assert_eq!(
        state::step(
            State::Running,
            &event("SessionStart", json!({ "source": "resume" }))
        ),
        Step::Set(State::Connected)
    );
}

#[test]
fn an_idle_notification_never_clears_a_waiting_lane() {
    // "A prompt has sat unanswered" is equally true of a permission dialog
    // nobody has answered. Acting on it from Waiting would clear the lane at
    // exactly the moment nobody is at the desk.
    assert_eq!(
        state::step(
            State::Waiting,
            &event(
                "Notification",
                json!({ "notification_type": "idle_prompt" })
            )
        ),
        Step::Stay
    );
    assert_eq!(
        state::step(
            State::Running,
            &event(
                "Notification",
                json!({ "notification_type": "idle_prompt" })
            )
        ),
        Step::Set(State::Idle)
    );
}

#[test]
fn a_subagents_events_are_liveness_and_nothing_else() {
    // Subagent tool events carry the *parent's* session_id. Acting on them
    // would let one subagent finishing a tool clear a Waiting another one
    // raised — the stuck lane inverted, and worse, because Running is the
    // state you stop looking at.
    let subagent = event(
        "PostToolUse",
        json!({ "tool_name": "Bash", "agent_id": "sub-1", "agent_type": "general-purpose" }),
    );
    for current in [
        State::Waiting,
        State::Running,
        State::Connected,
        State::Done,
        State::Error,
        State::Idle,
    ] {
        assert_eq!(state::step(current, &subagent), Step::Stay);
    }
}

#[test]
fn a_notification_nobody_recognises_changes_nothing() {
    for current in [State::Running, State::Waiting, State::Connected] {
        assert_eq!(
            state::step(
                current,
                &event(
                    "Notification",
                    json!({ "notification_type": "something_new_in_1_9" })
                )
            ),
            Step::Stay
        );
    }
    // And the four known-but-uninteresting ones say nothing either.
    for known in [
        "agent_completed",
        "auth_success",
        "elicitation_complete",
        "elicitation_response",
    ] {
        assert_eq!(
            state::step(
                State::Running,
                &event("Notification", json!({ "notification_type": known }))
            ),
            Step::Stay,
            "{known}"
        );
    }
}

#[test]
fn every_way_of_being_asked_something_lands_on_waiting() {
    for kind in [
        "permission_prompt",
        "agent_needs_input",
        "elicitation_dialog",
        "elicitation_url_dialog",
    ] {
        assert_eq!(
            state::step(
                State::Running,
                &event("Notification", json!({ "notification_type": kind }))
            ),
            Step::Set(State::Waiting),
            "{kind}"
        );
    }
}

#[test]
fn subagent_lifecycle_events_never_move_the_main_state() {
    for name in ["SubagentStart", "SubagentStop"] {
        for current in [State::Running, State::Waiting, State::Done] {
            // With the id, the subagent guard catches it; without, its own arm.
            assert_eq!(
                state::step(current, &event(name, json!({ "agent_id": "sub-1" }))),
                Step::Stay
            );
            assert_eq!(state::step(current, &event(name, json!({}))), Step::Stay);
        }
    }
}

#[test]
fn a_subagents_event_adopts_its_unknown_session_as_connected() {
    // A background subagent outlives the turn that spawned it, so its events
    // prove the session is alive and nothing about what the main agent is
    // doing. The roster is what makes the lane read busy.
    let tool = event(
        "PostToolUse",
        json!({ "tool_name": "Grep", "agent_id": "sub-1" }),
    );
    assert_eq!(state::adopt(&tool), Some(State::Connected));
}

#[test]
fn stopping_ends_a_turn_and_session_end_ends_the_session() {
    assert_eq!(
        state::step(State::Running, &event("Stop", json!({}))),
        Step::Set(State::Done)
    );
    assert_eq!(
        state::step(State::Running, &event("StopFailure", json!({}))),
        Step::Set(State::Error)
    );
    assert_eq!(
        state::step(State::Waiting, &event("SessionEnd", json!({}))),
        Step::Release
    );
}

#[test]
fn a_session_we_have_never_seen_is_read_from_the_event_that_introduced_it() {
    // The headline requirement: an agent may be started before or after the
    // app, and the next event picks it up. A tool finishing means a turn is
    // open, whatever we did or did not see before it.
    let cases = [
        (
            "PostToolUse",
            json!({ "tool_name": "Bash" }),
            Some(State::Running),
        ),
        ("PostToolUseFailure", json!({}), Some(State::Running)),
        ("UserPromptSubmit", json!({}), Some(State::Running)),
        ("PermissionRequest", json!({}), Some(State::Waiting)),
        ("Stop", json!({}), Some(State::Done)),
        ("StopFailure", json!({}), Some(State::Error)),
        (
            "SessionStart",
            json!({ "source": "startup" }),
            Some(State::Connected),
        ),
        // Compaction mid-turn: what it is "unchanged" from is a turn in flight.
        (
            "SessionStart",
            json!({ "source": "compact" }),
            Some(State::Running),
        ),
        (
            "Notification",
            json!({ "notification_type": "permission_prompt" }),
            Some(State::Waiting),
        ),
        // Nothing to end that we ever saw start.
        ("SessionEnd", json!({}), None),
    ];
    for (name, extra, expected) in cases {
        assert_eq!(state::adopt(&event(name, extra)), expected, "{name}");
    }
}
