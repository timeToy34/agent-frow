//! Regression sequences observed in the ownership journal: completion-only
//! lanes, children with independent ids, and lost parent focus ancestry.
#![allow(clippy::unwrap_used)]

use agent_frow::{event::Event, state::State, tracker::Tracker};
use serde_json::{Value, json};

fn send(tracker: &mut Tracker, sid: &str, kind: &str, at: u64, extra: Value) {
    let mut value = json!({"src":"codex-wsl", "session_id":sid,
        "hook_event_name":kind, "cwd":"/project"});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    tracker.accept(Event::parse(&value, at), at);
}

#[test]
fn completion_events_never_create_foreground_lanes() {
    for kind in ["SubagentStop", "SessionEnd"] {
        for extra in [
            json!({}),
            json!({"agent_id":"child"}),
            json!({"agent_type":"Explore"}),
        ] {
            let mut tracker = Tracker::default();
            send(&mut tracker, "unknown", kind, 10, extra);
            assert!(tracker.sessions.is_empty(), "{kind}");
        }
    }
}

#[test]
fn nested_children_arriving_first_wait_for_one_root_lane() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "grandchild",
        "SessionStart",
        10,
        json!({"parent_session_id":"child"}),
    );
    send(
        &mut tracker,
        "child",
        "SessionStart",
        20,
        json!({"parent_session_id":"root"}),
    );
    assert!(tracker.sessions.is_empty());
    send(&mut tracker, "root", "SessionStart", 30, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.sessions[0].subagents.len(), 2);
    // Parentage survives an event that omits metadata.
    send(&mut tracker, "grandchild", "SessionEnd", 40, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.sessions[0].subagents.len(), 1);
    send(&mut tracker, "child", "SessionEnd", 50, json!({}));
    assert!(tracker.sessions[0].subagents.is_empty());
    // An older child heartbeat arriving late cannot undo completion.
    send(&mut tracker, "child", "PostToolUse", 45, json!({}));
    assert!(tracker.sessions[0].subagents.is_empty());
}

#[test]
fn child_activity_cannot_replace_parent_focus_or_attention() {
    let mut tracker = Tracker::default();
    send(
        &mut tracker,
        "root",
        "PermissionRequest",
        10,
        json!({
            "ancestors":[100,200], "ancestor_names":["wsl.exe","WindowsTerminal.exe"],
            "wt_session":"root-tab", "gauges":{"ctx":20}
        }),
    );
    let focus = tracker.summon_target(0).unwrap();
    let note = tracker.sessions[0].note.clone();
    send(
        &mut tracker,
        "root",
        "PostToolUseFailure",
        20,
        json!({
            "agent_id":"child", "cwd":"/project/frontend", "ancestors":[300],
            "ancestor_names":["helper.exe"], "wt_session":"wrong", "gauges":{"ctx":90}
        }),
    );
    assert_eq!(tracker.summon_target(0).unwrap(), focus);
    assert_eq!(tracker.sessions[0].state, State::Waiting);
    assert_eq!(tracker.sessions[0].note, note);
    assert_eq!(tracker.sessions[0].gauges.context_used, Some(20));
    send(
        &mut tracker,
        "root",
        "PostToolUse",
        30,
        json!({
            "ancestors":[400], "ancestor_names":["detached.exe"]
        }),
    );
    assert_eq!(tracker.summon_target(0).unwrap().ancestors, focus.ancestors);
}

#[test]
fn ended_and_dismissed_roots_ignore_background_activity_until_a_new_prompt() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "root", "SessionStart", 10, json!({}));
    send(&mut tracker, "root", "SessionEnd", 20, json!({}));
    send(&mut tracker, "root", "PermissionRequest", 30, json!({}));
    send(
        &mut tracker,
        "root",
        "SubagentStop",
        40,
        json!({"agent_id":"child"}),
    );
    assert!(tracker.sessions.is_empty());
    send(&mut tracker, "root", "UserPromptSubmit", 50, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
    assert!(tracker.sessions[0].subagents.is_empty());
    send(&mut tracker, "root", "SessionEnd", 20, json!({}));
    assert_eq!(
        tracker.sessions.len(),
        1,
        "an old ending cannot close the reopened session"
    );
    tracker.dismiss(0);
    let now = agent_frow::now_ms() + 1;
    send(&mut tracker, "root", "PostToolUse", now, json!({}));
    assert!(tracker.sessions.is_empty());
    send(&mut tracker, "root", "UserPromptSubmit", now + 1, json!({}));
    assert_eq!(tracker.sessions.len(), 1);
}

#[test]
fn late_child_identification_removes_a_provisional_duplicate() {
    let mut tracker = Tracker::default();
    send(&mut tracker, "root", "SessionStart", 10, json!({}));
    send(&mut tracker, "child", "SessionStart", 20, json!({}));
    assert_eq!(tracker.sessions.len(), 2);
    send(
        &mut tracker,
        "child",
        "PostToolUse",
        30,
        json!({"parent_session_id":"root"}),
    );
    assert_eq!(tracker.sessions.len(), 1);
    assert!(tracker.sessions[0].subagents.contains_key("child"));
}

#[test]
fn independent_agents_in_the_same_folder_keep_separate_tabs_and_lanes() {
    let mut tracker = Tracker::default();
    for (sid, tab) in [("one", "tab-a"), ("two", "tab-b")] {
        send(
            &mut tracker,
            sid,
            "SessionStart",
            10,
            json!({"codex_cli":true,"wt_session":tab}),
        );
    }
    assert_eq!(tracker.sessions.len(), 2);
    assert_ne!(
        tracker.summon_target(0).unwrap().terminal_id,
        tracker.summon_target(1).unwrap().terminal_id
    );
}
