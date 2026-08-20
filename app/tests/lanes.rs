//! Sessions arriving, taking a lane, and going away again.
//!
//! Lane position is identity: a display you glance at teaches you nothing if
//! lane 2 moves while you are looking at it. Most of what is asserted here is
//! that nothing moves.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use agent_frow::event::Event;
use agent_frow::settings::{Bind, BindAgent, Settings};
use agent_frow::state::State;
use agent_frow::tracker::{ACTIVE_IDLE_MS, RESTING_IDLE_MS, Tracker};
use serde_json::{Value, json};

fn send(tracker: &mut Tracker, source: &str, session: &str, name: &str, cwd: &str, at: u64) {
    let payload: Value = json!({
        "src": source,
        "hook_event_name": name,
        "session_id": session,
        "cwd": cwd,
    });
    tracker.accept(Event::parse(&payload, at), at);
}

fn tracker(lane_count: usize) -> Tracker {
    let mut settings = Settings::default();
    settings.set_lane_count(lane_count);
    Tracker::new(settings, Default::default())
}

fn lane_project(tracker: &Tracker, lane: usize) -> Option<String> {
    tracker.on_lane(lane).and_then(|session| session.project())
}

#[test]
fn the_first_session_takes_the_first_lane_and_keeps_it() {
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    send(
        &mut tracker,
        "codex-wsl",
        "b",
        "PostToolUse",
        "/home/j/beta",
        20,
    );

    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("beta"));

    // More events from either one must not shuffle them.
    send(&mut tracker, "codex-wsl", "b", "Stop", "/home/j/beta", 30);
    send(
        &mut tracker,
        "claude-win",
        "a",
        "PermissionRequest",
        r"C:\dev\alpha",
        40,
    );
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("beta"));
    assert_eq!(tracker.on_lane(0).map(|s| s.state), Some(State::Waiting));
    assert_eq!(tracker.on_lane(1).map(|s| s.state), Some(State::Done));
}

#[test]
fn a_bound_lane_claims_its_project() {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.lanes[2].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from(r"C:\dev\beta"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-win",
        "b",
        "UserPromptSubmit",
        r"C:\dev\beta",
        20,
    );

    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 2).as_deref(), Some("beta"));
    assert!(tracker.on_lane(1).is_none());
}

#[test]
fn a_binding_can_name_the_agent_as_well_as_the_folder() {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.lanes[1].bind = Some(Bind {
        agent: BindAgent::Codex,
        folder: PathBuf::from(r"C:\dev\alpha"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    // Same folder, wrong agent: the binding does not apply, so it takes the
    // first free lane like anything else.
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    assert_eq!(
        tracker.on_lane(0).map(|s| s.source.clone()).as_deref(),
        Some("claude-win")
    );

    send(
        &mut tracker,
        "codex-win",
        "b",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        20,
    );
    assert_eq!(
        tracker.on_lane(1).map(|s| s.source.clone()).as_deref(),
        Some("codex-win")
    );
}

#[test]
fn nothing_takes_a_lane_away_from_a_session_that_already_has_one() {
    // The borrowing session got there first, under scarcity. Moving it now
    // would be the display rearranging itself under somebody who is looking
    // at it, which is worse than a binding not applying today.
    let mut settings = Settings::default();
    settings.set_lane_count(3);
    settings.lanes[0].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from(r"C:\dev\beta"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    // Strangers fill the unbound lanes first…
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-win",
        "g",
        "UserPromptSubmit",
        r"C:\dev\gamma",
        11,
    );
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 2).as_deref(), Some("gamma"));

    // …and only scarcity lets one borrow the bound lane.
    send(
        &mut tracker,
        "claude-win",
        "d",
        "UserPromptSubmit",
        r"C:\dev\delta",
        12,
    );
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("delta"));

    // The lane's own project arriving does not evict the borrower.
    send(
        &mut tracker,
        "claude-win",
        "b",
        "UserPromptSubmit",
        r"C:\dev\beta",
        20,
    );
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("delta"));
    assert_eq!(tracker.overflow().len(), 1);
}

#[test]
fn a_session_without_a_cwd_never_takes_a_bound_lane() {
    // Hooks post concurrently; a session adopted from a subagent's event has
    // no cwd and so cannot prove a bind match. Granting it a reserved lane is
    // how two agents once came up reversed after an app restart, summoning
    // each other's windows.
    let mut settings = Settings::default();
    settings.set_lane_count(3);
    settings.lanes[0].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/beta"),
    });
    settings.lanes[1].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/gamma"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    // Adopted from a subagent event: the cwd is deliberately not believed.
    let subagent: Value = json!({
        "src": "claude-wsl",
        "hook_event_name": "SubagentStart",
        "session_id": "b",
        "cwd": "/home/j/beta/frontend",
        "agent_id": "sub-1",
    });
    tracker.accept(Event::parse(&subagent, 10), 10);

    // Both bound lanes are free, but only the unbound lane 2 is claimable.
    let session = tracker.on_lane(2);
    assert!(session.is_some(), "an unbound lane is fine without a cwd");
    assert!(tracker.on_lane(0).is_none());
    assert!(tracker.on_lane(1).is_none());
}

#[test]
fn a_late_cwd_claims_the_bound_lane_it_could_not_prove_before() {
    let mut settings = Settings::default();
    settings.set_lane_count(3);
    settings.lanes[0].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/beta"),
    });
    settings.lanes[1].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/gamma"),
    });
    settings.lanes[2].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/delta"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    // Every lane is bound, so a cwd-less adoption waits off the keyboard…
    let subagent: Value = json!({
        "src": "claude-wsl",
        "hook_event_name": "SubagentStart",
        "session_id": "b",
        "cwd": "/home/j/beta/frontend",
        "agent_id": "sub-1",
    });
    tracker.accept(Event::parse(&subagent, 10), 10);
    assert_eq!(tracker.overflow().len(), 1);

    // …until the main agent's first event proves where it lives.
    send(
        &mut tracker,
        "claude-wsl",
        "b",
        "PostToolUse",
        "/home/j/beta",
        20,
    );
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("beta"));
    assert!(tracker.overflow().is_empty());
}

#[test]
fn bindings_survive_layouts_where_their_lane_does_not_exist() {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.lanes[3].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from("/home/j/beta"),
    });
    let mut tracker = Tracker::new(settings, Default::default());

    tracker.set_lane_count(3);
    tracker.set_lane_count(4);
    assert!(
        tracker.settings.lanes[3].bind.is_some(),
        "a binding outlives layouts that hide its lane"
    );
    send(
        &mut tracker,
        "claude-wsl",
        "b",
        "UserPromptSubmit",
        "/home/j/beta",
        10,
    );
    assert_eq!(lane_project(&tracker, 3).as_deref(), Some("beta"));
}

#[test]
fn sessions_beyond_the_lane_count_are_listed_and_promoted_when_one_frees_up() {
    let mut tracker = tracker(3);
    for (index, name) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
        send(
            &mut tracker,
            "claude-win",
            name,
            "UserPromptSubmit",
            &format!(r"C:\dev\{name}"),
            10 + index as u64,
        );
    }
    assert_eq!(tracker.overflow().len(), 1);
    assert_eq!(tracker.overflow()[0].project().as_deref(), Some("delta"));

    // The middle lane ends. Everything that had a lane keeps the one it had,
    // and the waiting session takes the hole.
    send(
        &mut tracker,
        "claude-win",
        "beta",
        "SessionEnd",
        r"C:\dev\beta",
        50,
    );
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("delta"));
    assert_eq!(lane_project(&tracker, 2).as_deref(), Some("gamma"));
    assert!(tracker.overflow().is_empty());
}

#[test]
fn shrinking_the_layout_moves_only_the_lanes_that_stopped_existing() {
    let mut tracker = tracker(6);
    for (index, name) in ["a", "b", "c", "d"].iter().enumerate() {
        send(
            &mut tracker,
            "claude-win",
            name,
            "UserPromptSubmit",
            &format!(r"C:\dev\{name}"),
            10 + index as u64,
        );
    }
    tracker.set_lane_count(3);
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("a"));
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("b"));
    assert_eq!(lane_project(&tracker, 2).as_deref(), Some("c"));
    assert_eq!(tracker.overflow().len(), 1);
}

#[test]
fn a_quiet_session_goes_idle_and_keeps_its_lane() {
    // Silence is reported, never punished: the user who stepped away comes
    // back to the board they left. Only SessionEnd and ✕ remove a session.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "resting",
        "Stop",
        r"C:\dev\alpha",
        1_000,
    );
    send(
        &mut tracker,
        "claude-win",
        "asking",
        "PermissionRequest",
        r"C:\dev\beta",
        1_000,
    );

    tracker.sweep(1_000 + RESTING_IDLE_MS - 1);
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Done);

    tracker.sweep(1_000 + RESTING_IDLE_MS + 1);
    assert_eq!(tracker.sessions.len(), 2, "nothing is ever evicted");
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Idle);
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("alpha"));

    // A Waiting lane holds through every threshold — it is the one fact this
    // product exists to show.
    tracker.sweep(1_000 + ACTIVE_IDLE_MS * 10);
    assert_eq!(tracker.on_lane(1).unwrap().state, State::Waiting);
    assert_eq!(tracker.sessions.len(), 2);
}

#[test]
fn a_silent_running_lane_dims_to_idle_after_the_long_allowance() {
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        1_000,
    );
    tracker.sweep(1_000 + ACTIVE_IDLE_MS - 1);
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
    tracker.sweep(1_000 + ACTIVE_IDLE_MS + 1);
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Idle);

    // The agent speaking again revives the lane where it stood.
    send(
        &mut tracker,
        "claude-win",
        "a",
        "PostToolUse",
        r"C:\dev\alpha",
        1_000 + ACTIVE_IDLE_MS + 60_000,
    );
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Running);
}

#[test]
fn an_outcome_is_never_dimmed_by_time() {
    // An Error is what the user stepped away from; elapsed time does not
    // change how a turn ended.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "failed",
        "StopFailure",
        r"C:\dev\alpha",
        1_000,
    );
    tracker.sweep(1_000 + ACTIVE_IDLE_MS * 100);
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Error);
}

#[test]
fn the_same_session_id_from_two_flavors_is_two_sessions() {
    // Nothing promises these ids are unique across agents, and two lanes
    // collapsing into one would be invisible: it looks exactly like one agent
    // behaving oddly.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "same",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    send(
        &mut tracker,
        "codex-wsl",
        "same",
        "UserPromptSubmit",
        "/home/j/beta",
        20,
    );
    assert_eq!(tracker.sessions.len(), 2);
}

#[test]
fn an_unknown_notification_is_counted_rather_than_acted_on() {
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    let payload = json!({
        "src": "claude-win",
        "hook_event_name": "Notification",
        "session_id": "a",
        "notification_type": "brand_new_thing",
    });
    tracker.accept(Event::parse(&payload, 20), 20);

    assert_eq!(tracker.on_lane(0).map(|s| s.state), Some(State::Running));
    assert_eq!(
        tracker
            .unknown_notifications
            .get("brand_new_thing")
            .copied(),
        Some(1)
    );
}

#[test]
fn moving_a_lane_takes_its_session_name_colour_and_binding_along() {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.lanes[0].name = "First".to_owned();
    settings.lanes[1].bind = Some(Bind {
        agent: BindAgent::Any,
        folder: PathBuf::from(r"C:\dev\beta"),
    });
    let mut tracker = Tracker::new(settings, Default::default());
    send(
        &mut tracker,
        "claude-win",
        "a",
        "UserPromptSubmit",
        r"C:\dev\alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-win",
        "b",
        "UserPromptSubmit",
        r"C:\dev\beta",
        20,
    );

    tracker.move_lane(0, 1);

    // The whole lane moved: the session, the name, and the binding — and the
    // lane it swapped with moved back the other way, intact.
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("alpha"));
    assert_eq!(lane_project(&tracker, 0).as_deref(), Some("beta"));
    assert_eq!(tracker.settings.lanes[1].name, "First");
    assert!(tracker.settings.lanes[0].bind.is_some());

    // Out-of-range and self moves change nothing.
    tracker.move_lane(1, 9);
    tracker.move_lane(1, 1);
    assert_eq!(lane_project(&tracker, 1).as_deref(), Some("alpha"));
}

#[test]
fn dismissing_a_lane_drops_its_session_and_the_next_event_brings_it_back() {
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    assert!(tracker.on_lane(0).is_some());

    // The agent died without a SessionEnd; the user removes it by hand.
    tracker.dismiss(0);
    assert!(tracker.on_lane(0).is_none());
    assert!(tracker.sessions.is_empty());

    // Dismissing is display-side only: an agent that was actually alive is
    // re-adopted from its very next event, in the right state.
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "PostToolUse",
        "/home/j/api",
        20,
    );
    assert_eq!(tracker.on_lane(0).map(|s| s.state), Some(State::Running));

    // An empty lane dismisses to nothing, quietly.
    tracker.dismiss(2);
}

#[test]
fn a_subagents_folder_never_becomes_the_lanes_project() {
    // The agent was launched in the project root; a subagent works in a
    // subfolder. The lane's project must be the root, never the subfolder —
    // this is the "bound to the wrong folder" report.
    let mut tracker = tracker(4);

    // A subagent tool event arrives first, from a deeper folder.
    let subagent = json!({
        "src": "claude-wsl",
        "hook_event_name": "PostToolUse",
        "session_id": "s1",
        "cwd": "/home/j/ai-brand-dna/frontend",
        "tool_name": "Edit",
        "agent_id": "sub-1",
        "agent_type": "Explore",
    });
    tracker.accept(Event::parse(&subagent, 10), 10);
    // It created the lane (liveness) but did not claim its folder.
    assert!(tracker.on_lane(0).is_some());
    assert!(tracker.on_lane(0).unwrap().cwd.is_none());

    // The main agent's own event pins the project to the root.
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "UserPromptSubmit",
        "/home/j/ai-brand-dna",
        20,
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().project().as_deref(),
        Some("ai-brand-dna")
    );

    // A later subagent event from the subfolder must not move it.
    tracker.accept(Event::parse(&subagent, 30), 30);
    assert_eq!(
        tracker.on_lane(0).unwrap().project().as_deref(),
        Some("ai-brand-dna")
    );
}

#[test]
fn session_start_pins_the_launch_directory_over_a_fallback() {
    // Adopted mid-turn from a tool event in a subfolder, then SessionStart —
    // which carries the real launch directory — corrects it.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "PostToolUse",
        "/home/j/ai-brand-dna/frontend",
        10,
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().project().as_deref(),
        Some("frontend")
    );

    let start = json!({
        "src": "claude-wsl",
        "hook_event_name": "SessionStart",
        "session_id": "s1",
        "cwd": "/home/j/ai-brand-dna",
        "source": "resume",
    });
    tracker.accept(Event::parse(&start, 20), 20);
    assert_eq!(
        tracker.on_lane(0).unwrap().project().as_deref(),
        Some("ai-brand-dna")
    );
}

#[test]
fn a_lane_reads_busy_until_the_last_background_subagent_stops() {
    let mut tracker = tracker(4);
    let sub = |tracker: &mut Tracker, name: &str, agent_id: &str, at: u64| {
        let payload = json!({
            "src": "claude-wsl",
            "hook_event_name": name,
            "session_id": "s1",
            "cwd": "/home/j/api",
            "agent_id": agent_id,
            "agent_type": "Explore",
        });
        tracker.accept(Event::parse(&payload, at), at);
    };

    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    sub(&mut tracker, "SubagentStart", "sub-a", 20);
    sub(&mut tracker, "SubagentStart", "sub-b", 21);

    // The main turn finishes while both subagents are still at work: the state
    // is honestly Done, and the lane honestly still looks busy.
    send(&mut tracker, "claude-wsl", "s1", "Stop", "/home/j/api", 30);
    let session = tracker.on_lane(0).unwrap();
    assert_eq!(session.state, State::Done);
    assert_eq!(session.subagents.len(), 2);
    assert_eq!(session.effective_state(), State::Running);

    // A subagent's tool events are its heartbeat, and never touch the state.
    sub(&mut tracker, "PostToolUse", "sub-a", 40);
    assert_eq!(tracker.on_lane(0).unwrap().state, State::Done);

    sub(&mut tracker, "SubagentStop", "sub-a", 50);
    assert_eq!(tracker.on_lane(0).unwrap().subagents.len(), 1);
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Running
    );

    sub(&mut tracker, "SubagentStop", "sub-b", 60);
    let session = tracker.on_lane(0).unwrap();
    assert!(session.subagents.is_empty());
    assert_eq!(session.effective_state(), State::Done);
}

#[test]
fn a_waiting_lane_outranks_its_busy_subagents() {
    // Waiting is the state that needs the user; a scanner because a subagent
    // is grepping somewhere must never dress it up as mere work.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    let payload = json!({
        "src": "claude-wsl", "hook_event_name": "SubagentStart",
        "session_id": "s1", "agent_id": "sub-a",
    });
    tracker.accept(Event::parse(&payload, 20), 20);
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "PermissionRequest",
        "/home/j/api",
        30,
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Waiting
    );
}

#[test]
fn a_silent_subagent_falls_off_the_roster_eventually() {
    use agent_frow::tracker::SUBAGENT_IDLE_MS;
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "Stop",
        "/home/j/api",
        1_000,
    );
    let payload = json!({
        "src": "claude-wsl", "hook_event_name": "SubagentStart",
        "session_id": "s1", "agent_id": "sub-a",
    });
    tracker.accept(Event::parse(&payload, 1_000), 1_000);
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Running
    );

    // The main agent stays in touch, so the session itself is not evicted…
    send(
        &mut tracker,
        "claude-wsl",
        "s1",
        "PostToolUse",
        "/home/j/api",
        1_000 + SUBAGENT_IDLE_MS / 2,
    );
    // …but the subagent died without a SubagentStop; silence retires it alone.
    tracker.sweep(1_000 + SUBAGENT_IDLE_MS + 1);
    assert_eq!(tracker.on_lane(0).unwrap().effective_state(), State::Done);
}

#[test]
fn a_summon_target_carries_the_lane_name_then_the_project() {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.lanes[0].name = "Backend".to_owned();
    let mut tracker = Tracker::new(settings, Default::default());
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );

    let (ancestors, names) = tracker.summon_target(0).unwrap();
    // This synthetic event reported no ancestry; a real hook's would be here.
    assert!(ancestors.is_empty());
    // The name the user typed first — it is the one they can make match a
    // terminal tab — then the project folder as the fallback.
    assert_eq!(names, vec!["Backend".to_owned(), "api".to_owned()]);
}

#[test]
fn a_session_keeps_ancestor_identities_and_replaces_them_wholesale() {
    use agent_frow::event::Ancestor;
    let mut tracker = tracker(4);
    let named: Value = json!({
        "src": "claude-win",
        "hook_event_name": "UserPromptSubmit",
        "session_id": "a",
        "cwd": "C:/dev/api",
        "ancestors": [100, 200],
        "ancestor_names": ["claude.exe", "explorer.exe"],
    });
    tracker.accept(Event::parse(&named, 10), 10);
    let (ancestors, _) = tracker.summon_target(0).unwrap();
    assert_eq!(
        ancestors,
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

    // A later event from an older hook carries pids alone: the chain is
    // replaced wholesale — pids and names travel together, never half-mixed.
    let bare: Value = json!({
        "src": "claude-win",
        "hook_event_name": "PostToolUse",
        "session_id": "a",
        "ancestors": [300],
    });
    tracker.accept(Event::parse(&bare, 20), 20);
    let (ancestors, _) = tracker.summon_target(0).unwrap();
    assert_eq!(
        ancestors,
        vec![Ancestor {
            pid: 300,
            exe: None
        }]
    );
}

#[test]
fn an_interrupted_turn_goes_idle_once_claude_idles() {
    // No hook fires at the moment of an interrupt; Claude only reveals one by
    // idling with the turn still open, about a minute later — and an
    // interrupt is something the user did, so the lane goes quietly Idle
    // rather than raising an alarm.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    let idle: Value = json!({
        "src": "claude-wsl",
        "hook_event_name": "Notification",
        "session_id": "a",
        "cwd": "/home/j/api",
        "notification_type": "idle_prompt",
    });
    tracker.accept(Event::parse(&idle, 20), 20);
    assert_eq!(tracker.on_lane(0).unwrap().effective_state(), State::Idle);
}

#[test]
fn an_interrupted_prompt_becomes_running_then_idle() {
    // The owner's exact scenario: a pending permission prompt rejected by an
    // interrupt. PermissionDenied clears Waiting on the spot; the idle
    // notification a minute later is what says the turn died with it.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "PermissionRequest",
        "/home/j/api",
        20,
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Waiting
    );

    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "PermissionDenied",
        "/home/j/api",
        30,
    );
    assert_eq!(
        tracker.on_lane(0).unwrap().effective_state(),
        State::Running
    );

    let idle: Value = json!({
        "src": "claude-wsl",
        "hook_event_name": "Notification",
        "session_id": "a",
        "cwd": "/home/j/api",
        "notification_type": "idle_prompt",
    });
    tracker.accept(Event::parse(&idle, 40), 40);
    assert_eq!(tracker.on_lane(0).unwrap().effective_state(), State::Idle);
}

#[test]
fn a_denial_after_the_turn_ended_changes_nothing() {
    // Hook processes post concurrently, so a denial can arrive after the Stop
    // that ended its turn. It must not resurrect it.
    let mut tracker = tracker(4);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/api",
        10,
    );
    send(&mut tracker, "claude-wsl", "a", "Stop", "/home/j/api", 20);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "PermissionDenied",
        "/home/j/api",
        30,
    );
    assert_eq!(tracker.on_lane(0).unwrap().effective_state(), State::Done);
}

#[test]
fn promoting_an_off_keyboard_session_swaps_with_the_bottom_lane() {
    let mut tracker = tracker(3);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "b",
        "UserPromptSubmit",
        "/home/j/beta",
        11,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "c",
        "UserPromptSubmit",
        "/home/j/gamma",
        12,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "d",
        "UserPromptSubmit",
        "/home/j/delta",
        13,
    );

    tracker.promote("claude-wsl", "d");
    assert_eq!(lane_project(&tracker, 0), Some("alpha".to_owned()));
    assert_eq!(lane_project(&tracker, 1), Some("beta".to_owned()));
    assert_eq!(lane_project(&tracker, 2), Some("delta".to_owned()));
    let overflow = tracker.overflow();
    assert_eq!(overflow.len(), 1, "the incumbent steps off the keyboard");
    assert_eq!(overflow[0].project(), Some("gamma".to_owned()));

    // Promoting a session that already holds a lane is a no-op.
    tracker.promote("claude-wsl", "a");
    assert_eq!(lane_project(&tracker, 0), Some("alpha".to_owned()));
    assert_eq!(lane_project(&tracker, 2), Some("delta".to_owned()));
}

#[test]
fn dismissing_an_off_keyboard_session_touches_no_lane() {
    let mut tracker = tracker(3);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "b",
        "UserPromptSubmit",
        "/home/j/beta",
        11,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "c",
        "UserPromptSubmit",
        "/home/j/gamma",
        12,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "d",
        "UserPromptSubmit",
        "/home/j/delta",
        13,
    );

    tracker.dismiss_session("claude-wsl", "d");
    assert!(tracker.overflow().is_empty());
    assert_eq!(lane_project(&tracker, 0), Some("alpha".to_owned()));
    assert_eq!(lane_project(&tracker, 1), Some("beta".to_owned()));
    assert_eq!(lane_project(&tracker, 2), Some("gamma".to_owned()));
}

#[test]
fn growing_the_layout_promotes_the_oldest_waiters_first() {
    let mut tracker = tracker(3);
    send(
        &mut tracker,
        "claude-wsl",
        "a",
        "UserPromptSubmit",
        "/home/j/alpha",
        10,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "b",
        "UserPromptSubmit",
        "/home/j/beta",
        11,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "c",
        "UserPromptSubmit",
        "/home/j/gamma",
        12,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "d",
        "UserPromptSubmit",
        "/home/j/delta",
        13,
    );
    send(
        &mut tracker,
        "claude-wsl",
        "e",
        "UserPromptSubmit",
        "/home/j/echo",
        14,
    );
    assert_eq!(tracker.overflow().len(), 2);

    tracker.set_lane_count(4);
    assert_eq!(lane_project(&tracker, 3), Some("delta".to_owned()));
    assert_eq!(tracker.overflow().len(), 1);

    tracker.set_lane_count(6);
    assert_eq!(lane_project(&tracker, 4), Some("echo".to_owned()));
    assert!(tracker.overflow().is_empty());
}

#[test]
fn summoning_an_off_keyboard_session_uses_its_project_name() {
    let mut tracker = tracker(3);
    for (id, folder) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
        send(
            &mut tracker,
            "claude-wsl",
            id,
            "UserPromptSubmit",
            &format!("/home/j/{folder}"),
            10,
        );
    }
    let with_ancestry: Value = json!({
        "src": "claude-wsl",
        "hook_event_name": "UserPromptSubmit",
        "session_id": "d",
        "cwd": "/home/j/delta",
        "ancestors": [700],
        "ancestor_names": ["WindowsTerminal.exe"],
    });
    tracker.accept(Event::parse(&with_ancestry, 20), 20);

    let (ancestors, names) = tracker.summon_session("claude-wsl", "d").unwrap();
    assert_eq!(ancestors.len(), 1);
    assert_eq!(names, vec!["delta".to_owned()], "no lane, so no lane name");

    let reason = tracker.summon_session("claude-wsl", "zz").unwrap_err();
    assert!(reason.contains("gone"), "{reason}");
}

#[test]
fn summoning_an_empty_lane_says_so_in_words() {
    let tracker = tracker(4);
    let reason = tracker.summon_target(2).unwrap_err();
    assert!(reason.contains("lane 3"), "{reason}");
    assert!(reason.contains("empty"), "{reason}");
}

#[test]
fn a_hook_event_we_do_not_register_is_named_and_counted() {
    let mut tracker = tracker(4);
    let payload = json!({ "src": "claude-win", "hook_event_name": "TeammateIdle" });
    tracker.accept(Event::parse(&payload, 10), 10);
    assert_eq!(
        tracker.unrecognised_events.get("TeammateIdle").copied(),
        Some(1)
    );
    assert!(tracker.sessions.is_empty());
    // It still proves that flavor is alive.
    assert_eq!(tracker.last_seen.get("claude-win").copied(), Some(10));
}
