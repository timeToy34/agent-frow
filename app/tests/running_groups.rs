//! Active child counts flow from the existing roster into each lane display.
#![allow(clippy::unwrap_used)]

use std::sync::Mutex;

use agent_frow::{
    event::Event,
    settings::{AgentFilter, SavedAgent},
    state::State,
    surface::{monitor, palette, scene::Scene, streamdeck::surface as deck},
    tracker::{Preview, SUBAGENT_IDLE_MS, Tracker},
};
use serde_json::{Value, json};

fn send(tracker: &mut Tracker, id: &str, kind: &str, at: u64, extra: Value) {
    let mut event = json!({"src":"codex-wsl", "session_id":id,
        "hook_event_name":kind, "cwd":"/project"});
    event
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    tracker.accept(Event::parse(&event, at), at);
}

fn counts(tracker: &Mutex<Tracker>, scene: &mut Scene, now: u64) -> Vec<usize> {
    scene.tick(tracker, now).unwrap();
    scene.current().unwrap().agent_counts.to_vec()
}

#[test]
fn child_lifecycle_changes_the_existing_parent_group() {
    let mut tracker = Tracker::default();
    tracker.set_lane_count(3);
    send(&mut tracker, "root", "UserPromptSubmit", 10, json!({}));
    let tracker = Mutex::new(tracker);
    let mut scene = Scene::new();
    assert_eq!(counts(&tracker, &mut scene, 10), [1, 0, 0]);
    send(
        &mut tracker.lock().unwrap(),
        "root",
        "SubagentStart",
        20,
        json!({"agent_id":"child"}),
    );
    assert_eq!(counts(&tracker, &mut scene, 20), [2, 0, 0]);
    // Repeated activity from the same child must not count it twice.
    send(
        &mut tracker.lock().unwrap(),
        "root",
        "PostToolUse",
        30,
        json!({"agent_id":"child"}),
    );
    assert_eq!(counts(&tracker, &mut scene, 30), [2, 0, 0]);
    send(
        &mut tracker.lock().unwrap(),
        "root",
        "SubagentStart",
        40,
        json!({"agent_id":"second"}),
    );
    assert_eq!(counts(&tracker, &mut scene, 40), [3, 0, 0]);
    send(
        &mut tracker.lock().unwrap(),
        "root",
        "SubagentStop",
        50,
        json!({"agent_id":"child"}),
    );
    assert_eq!(counts(&tracker, &mut scene, 50), [2, 0, 0]);
    assert_eq!(
        counts(&tracker, &mut scene, 40 + SUBAGENT_IDLE_MS),
        [1, 0, 0]
    );
    assert_eq!(tracker.lock().unwrap().sessions.len(), 1);
    assert_eq!(scene.current().unwrap().states[0], Some(State::Running));
}

#[test]
fn nested_children_keep_one_lane_and_keep_a_finished_parent_running() {
    let mut tracker = Tracker::default();
    tracker.set_lane_count(3);
    send(&mut tracker, "root", "UserPromptSubmit", 10, json!({}));
    send(
        &mut tracker,
        "child",
        "SessionStart",
        20,
        json!({"parent_session_id":"root"}),
    );
    send(
        &mut tracker,
        "nested",
        "SessionStart",
        30,
        json!({"parent_session_id":"child"}),
    );
    send(&mut tracker, "root", "Stop", 40, json!({}));
    let tracker = Mutex::new(tracker);
    let mut scene = Scene::new();
    assert_eq!(counts(&tracker, &mut scene, 40), [3, 0, 0]);
    assert_eq!(scene.current().unwrap().states[0], Some(State::Running));
    assert_eq!(tracker.lock().unwrap().sessions[0].state, State::Done);
    assert_eq!(tracker.lock().unwrap().sessions.len(), 1);
    send(
        &mut tracker.lock().unwrap(),
        "nested",
        "SessionEnd",
        50,
        json!({}),
    );
    assert_eq!(counts(&tracker, &mut scene, 50), [2, 0, 0]);
    send(
        &mut tracker.lock().unwrap(),
        "child",
        "SessionEnd",
        60,
        json!({}),
    );
    assert_eq!(counts(&tracker, &mut scene, 60), [1, 0, 0]);
    assert_eq!(scene.current().unwrap().states[0], Some(State::Done));
}

#[test]
fn previews_show_one_agent_and_reservations_have_no_agent_count() {
    let mut tracker = Tracker::default();
    tracker.set_lane_count(3);
    tracker.settings.saved.push(SavedAgent {
        agent: AgentFilter::Any,
        folder: "/reserved".into(),
        lane: Some(1),
        reserved_lane: None,
    });
    tracker.reserve_saved(0).unwrap();
    send(&mut tracker, "root", "UserPromptSubmit", 10, json!({}));
    send(
        &mut tracker,
        "root",
        "SubagentStart",
        20,
        json!({"agent_id":"child"}),
    );
    let tracker = Mutex::new(tracker);
    let mut scene = Scene::new();
    assert_eq!(counts(&tracker, &mut scene, 20), [2, 0, 0]);
    tracker.lock().unwrap().preview = Some(Preview {
        state: State::Running,
        expires_at: 100,
    });
    assert_eq!(counts(&tracker, &mut scene, 30), [1, 1, 1]);
    let rows = monitor::rows(&tracker.lock().unwrap(), 30, 0);
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(
            row.keys
                .iter()
                .filter(|key| key.colour == row.colour)
                .count(),
            1
        );
    }
    assert_eq!(counts(&tracker, &mut scene, 100), [2, 0, 0]);
    assert_eq!(scene.current().unwrap().states[1], Some(State::Idle));
}

#[test]
fn deck_and_mini_rows_use_each_sessions_count_including_off_keyboard() {
    let mut tracker = Tracker::default();
    tracker.set_lane_count(3);
    for (id, count) in [("one", 1), ("two", 2), ("many", 8), ("overflow", 3)] {
        send(&mut tracker, id, "UserPromptSubmit", 10, json!({}));
        for child in 1..count {
            send(
                &mut tracker,
                id,
                "SubagentStart",
                20,
                json!({"agent_id":format!("{id}-{child}")}),
            );
        }
    }
    let tracker = Mutex::new(tracker);
    let mut scene = Scene::new();
    assert_eq!(counts(&tracker, &mut scene, 20), [1, 2, 8]);
    let mut frame = scene.current().unwrap();
    frame.elapsed_ms = 0;
    let guard = tracker.lock().unwrap();
    let captions = deck::captions(&guard, 20);
    // Narrow and wide decks cap at their actual physical column count.
    for cols in [1, 3, 5, 8] {
        let faces = deck::faces(&frame, &captions, 3, cols);
        for (lane, count) in [1, 2, 8].into_iter().enumerate() {
            let full = guard.settings.lanes[lane].color;
            let row = &faces[lane * cols..(lane + 1) * cols];
            assert_eq!(
                row.iter().filter(|key| key.colour == full).count(),
                count.min(cols)
            );
        }
    }
    for elapsed in [0, 140, 700, 1399] {
        frame.elapsed_ms = elapsed;
        let faces = deck::faces(&frame, &captions, 3, 5);
        let rows = monitor::rows(&guard, 20, elapsed);
        assert_eq!(rows.len(), 4);
        for (lane, row) in rows.iter().take(3).enumerate() {
            assert_eq!(
                row.keys.iter().map(|key| key.colour).collect::<Vec<_>>(),
                faces[lane * 5..(lane + 1) * 5]
                    .iter()
                    .map(|key| key.colour)
                    .collect::<Vec<_>>()
            );
        }
        let overflow = &rows[3];
        assert!(overflow.off_keyboard);
        assert_eq!(
            overflow
                .keys
                .iter()
                .map(|key| key.colour)
                .collect::<Vec<_>>(),
            palette::lane_colors(Some(State::Running), overflow.colour, 5, elapsed, 3)
        );
    }
}
