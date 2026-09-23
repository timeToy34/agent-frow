//! Saved-agent placeholders reserve slots without inventing live sessions.
#![allow(clippy::unwrap_used)]

use std::sync::Mutex;

use agent_frow::{
    event::Event,
    settings::{self, AgentFilter, SavedAgent, Settings},
    state::State,
    surface::{monitor, scene::Scene, streamdeck::surface::captions},
    tracker::Tracker,
};
use serde_json::{Value, json};

fn tracker() -> Tracker {
    let mut settings = Settings::default();
    settings.set_lane_count(3);
    settings.saved = vec![
        SavedAgent {
            agent: AgentFilter::Any,
            folder: "/project".into(),
            lane: Some(1),
            reserved_lane: None,
        },
        SavedAgent {
            agent: AgentFilter::Codex,
            folder: "/other".into(),
            lane: None,
            reserved_lane: None,
        },
    ];
    Tracker::new(settings, Default::default())
}

fn send(tracker: &mut Tracker, id: &str, kind: &str, at: u64, extra: Value) {
    let mut event = json!({"src":"claude-wsl", "session_id":id,
        "hook_event_name":kind, "cwd":"/project/backend", "project_dir":"/project"});
    event
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    tracker.accept(Event::parse(&event, at), at);
}

#[test]
fn an_idle_reservation_appears_on_all_surfaces_without_a_fake_session() {
    let mut tracker = tracker();
    assert_eq!(tracker.reserve_saved(0).unwrap(), 1);
    assert!(tracker.sessions.is_empty());
    assert_eq!(tracker.events, 0);
    assert!(tracker.last_seen.is_empty());
    assert!(!tracker.running(&tracker.settings.saved[0]));
    assert_eq!(tracker.lane_state(1), Some(State::Idle));
    assert_eq!(tracker.selectable_lanes(), vec![1]);
    assert!(!tracker.answerable(1));
    assert!(
        tracker
            .summon_target(1)
            .unwrap_err()
            .contains("has not started")
    );
    let rows = monitor::rows(&tracker, 100, 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, State::Idle);
    assert_eq!(rows[0].name, "project");
    let words = captions(&tracker, 100);
    assert_eq!(words[1].name, "project");
    assert_eq!(words[1].state, Some("Idle"));
    assert!(words[1].elapsed.is_empty());
    assert!(words[1].gauges.is_none());
    let tracker = Mutex::new(tracker);
    let mut scene = Scene::new();
    let frame = scene.tick(&tracker, 100).unwrap().unwrap();
    assert_eq!(frame.states, &[None, Some(State::Idle), None]);
}

#[test]
fn unrelated_agents_cannot_take_the_reservation_and_matching_agent_replaces_idle() {
    let mut tracker = tracker();
    tracker.reserve_saved(0).unwrap();
    for (n, id) in ["one", "two", "overflow"].into_iter().enumerate() {
        send(
            &mut tracker,
            id,
            "SessionStart",
            n as u64 + 1,
            json!({"cwd":"/unrelated", "project_dir":"/unrelated"}),
        );
    }
    assert!(tracker.on_lane(1).is_none());
    assert_eq!(tracker.overflow().len(), 1);
    send(
        &mut tracker,
        "main",
        "UserPromptSubmit",
        10,
        json!({"wt_session":"tab"}),
    );
    assert_eq!(tracker.on_lane(1).unwrap().session_id, "main");
    assert_eq!(tracker.lane_state(1), Some(State::Running));
    assert_eq!(
        tracker.summon_target(1).unwrap().terminal_id.as_deref(),
        Some("tab")
    );
    send(&mut tracker, "main", "SessionEnd", 20, json!({}));
    assert!(tracker.on_lane(1).is_none());
    assert_eq!(tracker.lane_state(1), Some(State::Idle));
    // Releasing the idle slot fills it from the queue, without dismissing
    // the session that is promoted into it.
    assert!(tracker.dismiss(1));
    assert_eq!(tracker.on_lane(1).unwrap().session_id, "overflow");
}

#[test]
fn reservation_and_any_lane_survive_restart_move_and_hidden_lanes() {
    let mut tracker = tracker();
    assert_eq!(tracker.reserve_saved(1).unwrap(), 0);
    assert_eq!(tracker.settings.saved[1].lane, None);
    tracker.move_lane(0, 2);
    assert_eq!(tracker.settings.saved[1].reserved_lane, Some(2));
    assert_eq!(tracker.settings.saved[1].lane, None);
    let settings = settings::parse(&settings::to_json(&tracker.settings)).unwrap();
    let mut restarted = Tracker::new(settings, Default::default());
    assert_eq!(restarted.reservation(2).unwrap().project(), "other");
    restarted.set_lane_count(6);
    restarted.move_lane(2, 5);
    restarted.set_lane_count(3);
    assert!(restarted.reservation(5).is_none());
    assert_eq!(restarted.settings.saved[1].reserved_lane, Some(5));
    restarted.set_lane_count(6);
    assert_eq!(restarted.lane_state(5), Some(State::Idle));
}

#[test]
fn full_lanes_and_repeated_clicks_never_displace_an_occupant() {
    let mut tracker = tracker();
    tracker.reserve_saved(0).unwrap();
    assert!(tracker.reserve_saved(0).is_err());
    tracker.reserve_saved(1).unwrap();
    send(
        &mut tracker,
        "live",
        "SessionStart",
        1,
        json!({"project_dir":"/third", "cwd":"/third"}),
    );
    tracker.settings.saved.push(SavedAgent {
        agent: AgentFilter::Any,
        folder: "/fourth".into(),
        lane: None,
        reserved_lane: None,
    });
    let settings = tracker.settings.clone();
    assert!(tracker.reserve_saved(2).is_err());
    assert_eq!(tracker.settings, settings);
    assert_eq!(tracker.on_lane(2).unwrap().session_id, "live");
}

#[test]
fn late_launch_metadata_claims_the_reserved_slot_and_child_metadata_cannot() {
    let mut tracker = tracker();
    tracker.reserve_saved(0).unwrap();
    send(
        &mut tracker,
        "main",
        "UserPromptSubmit",
        10,
        json!({"project_dir":null}),
    );
    assert_eq!(tracker.sessions[0].lane, Some(0));
    send(
        &mut tracker,
        "main",
        "StatusLine",
        20,
        json!({"agent_id":"child"}),
    );
    assert_eq!(tracker.sessions[0].lane, Some(0));
    send(&mut tracker, "main", "StatusLine", 30, json!({}));
    assert_eq!(tracker.sessions[0].lane, Some(1));
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.sessions[0].last_event, 10);
    assert_eq!(tracker.settings.saved[0].reserved_lane, Some(1));
}

#[test]
fn a_reservation_matches_agent_filter_and_only_one_live_session() {
    let mut tracker = tracker();
    tracker.reserve_saved(1).unwrap();
    send(
        &mut tracker,
        "wrong-agent",
        "SessionStart",
        10,
        json!({"project_dir":"/other"}),
    );
    assert_ne!(tracker.sessions[0].lane, Some(0));
    for (i, id) in ["codex-one", "codex-two"].into_iter().enumerate() {
        send(
            &mut tracker,
            id,
            "SessionStart",
            20 + i as u64,
            json!({"src":"codex-wsl", "project_dir":"/other"}),
        );
    }
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "codex-one");
    assert_eq!(tracker.sessions.len(), 3);
    assert_eq!(
        tracker
            .sessions
            .iter()
            .filter(|session| session.lane == Some(0))
            .count(),
        1
    );
    assert!(tracker.release_reservation(0));
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "codex-one");
}

#[test]
fn any_lane_still_matches_saved_identity_without_preferring_a_slot() {
    let mut tracker = tracker();
    send(
        &mut tracker,
        "main",
        "SessionStart",
        10,
        json!({"src":"codex-wsl", "project_dir":"/other"}),
    );
    assert!(tracker.running(&tracker.settings.saved[1]));
    assert_eq!(tracker.on_lane(0).unwrap().session_id, "main");
    assert_eq!(tracker.settings.saved[1].lane, None);
}

#[test]
fn old_settings_and_any_lane_formats_load_without_losing_saved_agents() {
    let parsed = settings::parse(
        r#"{"saved":[
        {"folder":"/old","lane":2},
        {"folder":"/any","lane":null,"reserved_lane":1},
        {"folder":"/missing"},
        {"folder":"/text","lane":"any","reserved_lane":1}
    ]}"#,
    )
    .unwrap();
    assert_eq!(parsed.saved.len(), 4);
    assert_eq!(parsed.saved[0].lane, Some(1));
    assert_eq!(parsed.saved[1].reserved_lane, Some(0));
    assert_eq!(parsed.saved[3].reserved_lane, None);
    assert!(parsed.saved[1..].iter().all(|saved| saved.lane.is_none()));
    assert_eq!(
        settings::parse(&settings::to_json(&parsed)).unwrap(),
        parsed
    );
}
