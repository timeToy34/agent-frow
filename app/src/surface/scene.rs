//! When a frame is due, and what goes in it.
//!
//! Every lighting thread asks the same questions each tick: has anything
//! changed, is anything moving, what are the lanes showing right now — and
//! each is also the application's clock, sweeping stale sessions and retiring
//! an expired preview so lanes stay honest while the window is hidden in the
//! tray. That sequencing lives here once, so two surfaces cannot drift apart
//! on it. The surfaces themselves only connect, map key indices to LEDs, and
//! write.

use std::sync::Mutex;
use std::time::Instant;

use super::palette;
use crate::settings::Settings;
use crate::state::State;
use crate::tracker::Tracker;

/// The tracker's lock was poisoned: some other thread died holding it. There
/// is nothing left to paint from.
#[derive(Debug)]
pub struct Poisoned;

/// One frame's worth of input: what every lane is showing, under which
/// settings, at which moment of the animation.
pub struct Frame<'a> {
    pub states: &'a [Option<State>],
    pub settings: &'a Settings,
    /// Monotonic since the scene began — the animation's clock.
    pub elapsed_ms: u64,
}

/// The change detection behind a surface's loop. A motionless, unchanged board
/// gets no frame: the difference between a resting app and one that allocates
/// and writes a keyboard thirty times a second to say nothing.
pub struct Scene {
    start: Instant,
    /// What the keys were last told, so a still board is written once.
    last_states: Vec<Option<State>>,
    /// Kept so it is re-made only when the settings actually change — it
    /// carries every lane-name String.
    settings: Option<Settings>,
    dirty: bool,
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            last_states: Vec::new(),
            settings: None,
            dirty: true,
        }
    }

    /// Forces the next tick to produce a frame — for a keyboard that just
    /// (re)connected and knows nothing yet.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// What the board is showing right now, without counting it as painted.
    ///
    /// For a surface that paints on its own terms — one whose keys carry an
    /// elapsed time, say, which changes while the lane's state does not — and
    /// so cannot take "nothing changed" from [`Self::tick`] as "nothing to
    /// do". `None` before the first tick, when there is nothing to show yet.
    pub fn current(&self) -> Option<Frame<'_>> {
        let settings = self.settings.as_ref()?;
        Some(Frame {
            states: &self.last_states,
            settings,
            elapsed_ms: self.start.elapsed().as_millis() as u64,
        })
    }

    /// Runs the clock and says whether a frame is due.
    ///
    /// Always sweeps and retires an expired preview, whether or not anything
    /// is connected — that is the clock's job. Returns `None` when the board
    /// is unchanged and nothing on it moves; otherwise the frame, and the
    /// scene counts it as painted.
    pub fn tick(
        &mut self,
        tracker: &Mutex<Tracker>,
        now: u64,
    ) -> Result<Option<Frame<'_>>, Poisoned> {
        let states = {
            let mut tracker = tracker.lock().map_err(|_| Poisoned)?;
            tracker.sweep(now);
            // A preview overrides every lane while it lasts, and this is what
            // retires it — the window may be closed, and a preview that
            // outlives anyone looking at it should still die.
            if tracker
                .preview
                .is_some_and(|preview| preview.expires_at <= now)
            {
                tracker.preview = None;
            }
            let lanes = tracker.settings.lane_count;
            let states: Vec<Option<State>> = match tracker.preview {
                Some(preview) => vec![Some(preview.state); lanes],
                None => (0..lanes)
                    .map(|lane| {
                        tracker
                            .on_lane(lane)
                            .map(|session| session.effective_state())
                    })
                    .collect(),
            };
            if self
                .settings
                .as_ref()
                .is_none_or(|cached| *cached != tracker.settings)
            {
                self.settings = Some(tracker.settings.clone());
                self.dirty = true;
            }
            states
        };
        if states != self.last_states {
            self.last_states = states;
            self.dirty = true;
        }
        if !self.dirty && !palette::animated(&self.last_states) {
            return Ok(None);
        }
        let Some(settings) = self.settings.as_ref() else {
            return Ok(None);
        };
        self.dirty = false;
        Ok(Some(Frame {
            states: &self.last_states,
            settings,
            elapsed_ms: self.start.elapsed().as_millis() as u64,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::tracker::{Preview, Session};

    fn session(state: State, lane: usize) -> Session {
        Session {
            source: "claude-win".to_owned(),
            session_id: format!("s{lane}"),
            agent: None,
            cwd: None,
            state,
            since: 0,
            first_seen: 0,
            last_event: 0,
            events: 1,
            note: String::new(),
            subagents: Default::default(),
            lane: Some(lane),
            wt_session: None,
            ancestors: Vec::new(),
            gauges: Default::default(),
            failure: None,
        }
    }

    #[test]
    fn a_still_unchanged_board_is_painted_once() {
        let tracker = Mutex::new(Tracker::default());
        let mut scene = Scene::new();
        assert!(scene.tick(&tracker, 0).unwrap().is_some(), "first frame");
        assert!(
            scene.tick(&tracker, 1).unwrap().is_none(),
            "nothing changed"
        );
        scene.invalidate();
        assert!(
            scene.tick(&tracker, 2).unwrap().is_some(),
            "a reconnect repaints"
        );
    }

    #[test]
    fn a_change_of_state_or_settings_is_a_frame() {
        let tracker = Mutex::new(Tracker::default());
        let mut scene = Scene::new();
        scene.tick(&tracker, 0).unwrap();
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Done, 1));
        let frame = scene.tick(&tracker, 1).unwrap().expect("a lane appeared");
        assert_eq!(frame.states[1], Some(State::Done));
        assert!(scene.tick(&tracker, 2).unwrap().is_none());
        tracker.lock().unwrap().settings.tuning.brightness = 0.5;
        let frame = scene.tick(&tracker, 3).unwrap().expect("settings changed");
        assert_eq!(frame.settings.tuning.brightness, 0.5);
    }

    #[test]
    fn motion_is_a_frame_every_tick() {
        let tracker = Mutex::new(Tracker::default());
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Running, 0));
        let mut scene = Scene::new();
        for now in 0..5 {
            assert!(scene.tick(&tracker, now).unwrap().is_some(), "tick {now}");
        }
    }

    #[test]
    fn current_reports_the_board_without_counting_as_painted() {
        let tracker = Mutex::new(Tracker::default());
        let mut scene = Scene::new();
        assert!(scene.current().is_none(), "nothing before the first tick");
        scene.tick(&tracker, 0).unwrap();
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Waiting, 2));
        // The change is visible to the clock but a look is not a paint.
        assert!(scene.tick(&tracker, 1).unwrap().is_some());
        assert_eq!(scene.current().unwrap().states[2], Some(State::Waiting));
        scene.invalidate();
        assert_eq!(scene.current().unwrap().states[2], Some(State::Waiting));
        assert!(
            scene.tick(&tracker, 2).unwrap().is_some(),
            "current() left the frame due"
        );
    }

    #[test]
    fn an_expired_preview_is_retired_by_the_clock() {
        let tracker = Mutex::new(Tracker::default());
        tracker.lock().unwrap().preview = Some(Preview {
            state: State::Error,
            expires_at: 100,
        });
        let mut scene = Scene::new();
        let frame = scene.tick(&tracker, 50).unwrap().expect("preview plays");
        assert!(frame.states.iter().all(|s| *s == Some(State::Error)));
        let frame = scene.tick(&tracker, 100).unwrap().expect("preview ended");
        assert!(frame.states.iter().all(Option::is_none));
        assert!(tracker.lock().unwrap().preview.is_none());
    }
}
