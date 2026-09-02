//! The lighting thread for the V0 Ultra numpad: finds the board on the cable
//! or through its receiver, takes the four shape keys and M1–M5, paints them
//! from the scene and the tracker's selection, and hands the board back on
//! the way out.
//!
//! Unlike the F-row this surface paints on its own terms, the way the deck
//! does: [`Scene::tick`] is used as the clock only, and every frame is
//! composed from [`Scene::current`] plus one look at the tracker — selection,
//! lock and the foreground of a Waiting agent's terminal all change without
//! any lane changing state, and [`Board::paint`] already writes only the keys
//! that differ, so a still board costs nothing anyway.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths;
use crate::settings::{NUMPAD_LANES, Rgb, Settings};
use crate::state::State;
use crate::surface::keychron::hid::{self, Transport};
use crate::surface::keychron::session::{self, Board, Snapshot};
use crate::surface::palette;
use crate::surface::scene::Scene;
use crate::tracker::{KeyboardStatus, Tracker};

/// How this surface names itself in the window.
pub const SURFACE: &str = "V0 Ultra";

/// ~30 Hz, like the other Keychron: a frame is at most a few reports and the
/// board answers each in about a millisecond.
const FRAME: Duration = Duration::from_millis(33);

/// How long to wait before looking for the board again.
const RETRY: Duration = Duration::from_secs(10);

/// How long the quit path waits for the board to be handed back.
const RESTORE_GRACE: Duration = Duration::from_millis(1500);

/// How long a "is its terminal the foreground window?" verdict is trusted.
/// Only Waiting lanes ask, and a flip two seconds late is invisible next to
/// the human walking back to the desk.
const FOREGROUND_TTL: Duration = Duration::from_secs(2);

/// The keys by position: the four shape keys are the top line…
const TOP_KEYS: usize = 4;

/// …then one M key per lane.
const KEYS: usize = TOP_KEYS + NUMPAD_LANES;

/// The thread's run flag; a static so the quit path can lower it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the thread as its last act, after the board is restored.
static FINISHED: AtomicBool = AtomicBool::new(true);

/// Handle to the running surface. Dropping it restores the board and stops
/// the thread.
pub struct Surface {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the lighting. Returns immediately; a machine with no numpad simply
/// gets a sentence in the window.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Surface {
    RUNNING.store(true, Ordering::SeqCst);
    FINISHED.store(false, Ordering::SeqCst);
    let handle = std::thread::Builder::new()
        .name("agent-frow-numpad".to_owned())
        .spawn(move || {
            render(&tracker);
            FINISHED.store(true, Ordering::SeqCst);
        })
        .ok();
    if handle.is_none() {
        FINISHED.store(true, Ordering::SeqCst);
    }
    Surface { handle }
}

/// For the quit path, which exits the process without unwinding: asks the
/// thread to hand the board back and waits, briefly, for it to have done so.
pub fn restore_now() {
    RUNNING.store(false, Ordering::SeqCst);
    let deadline = Instant::now() + RESTORE_GRACE;
    while !FINISHED.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn report(tracker: &Mutex<Tracker>, status: KeyboardStatus) {
    if let Ok(mut tracker) = tracker.lock() {
        tracker.report_keyboard(status);
    }
}

fn unavailable(detail: impl Into<String>) -> KeyboardStatus {
    KeyboardStatus::unavailable(SURFACE, detail.into())
}

/// Where the snapshot waits for an app that died before restoring it. Its
/// own file, beside the Ultra's — the two boards must never restore from
/// each other's state.
fn state_file() -> Option<PathBuf> {
    paths::install_dir().map(|dir| dir.join("v0ultra-state.json"))
}

fn remember(snapshot: &Snapshot) {
    if let Some(path) = state_file() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, snapshot.to_json());
    }
}

fn forget() {
    if let Some(path) = state_file() {
        let _ = std::fs::remove_file(path);
    }
}

fn recall() -> Option<Snapshot> {
    let text = std::fs::read_to_string(state_file()?).ok()?;
    let snapshot = Snapshot::parse(&text).ok()?;
    // The cross-restore guard: whatever this file claims, it only restores a
    // board with the numpad's LED count.
    (snapshot.colours.len() == usize::from(session::V0_ULTRA.expect_leds.unwrap_or(0)))
        .then_some(snapshot)
}

/// A board the app has taken: the board, what it looked like before, how the
/// window should name it, and the claim that keeps the Ultra surface off
/// this interface.
type Taken = (Board<Box<dyn Transport>>, Snapshot, String, hid::Claim);

/// A board being painted, with what it looked like before.
struct Live {
    board: Board<Box<dyn Transport>>,
    snapshot: Snapshot,
    _claim: hid::Claim,
}

fn render(tracker: &Mutex<Tracker>) {
    if !cfg!(windows) {
        report(tracker, unavailable("the numpad is only driven on Windows"));
        return;
    }

    let mut scene = Scene::new();
    let mut live: Option<Live> = None;
    let mut remembered: Option<Snapshot> = recall();
    let mut next_attempt = Instant::now();
    let mut enabled = true;
    // The foreground verdicts, per session, each trusted for a moment.
    let mut foreground: HashMap<(String, String), (bool, Instant)> = HashMap::new();

    while RUNNING.load(Ordering::SeqCst) {
        // The clock, board or no board. What it says about change is not
        // this surface's to use: selection and lock live outside it.
        if scene.tick(tracker, crate::now_ms()).is_err() {
            break;
        }

        let wanted = crate::surface::enabled(tracker, SURFACE);
        if wanted != enabled {
            enabled = wanted;
            if enabled {
                next_attempt = Instant::now();
            } else {
                if let Some(mut ready) = live.take()
                    && ready.board.restore(&ready.snapshot).is_ok()
                {
                    forget();
                }
                report(tracker, KeyboardStatus::off(SURFACE));
            }
        }
        if !enabled {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        if live.is_none() {
            if Instant::now() < next_attempt {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            match connect(remembered.as_ref()) {
                Ok((board, snapshot, model, claim)) => {
                    remember(&snapshot);
                    remembered = Some(snapshot.clone());
                    report(
                        tracker,
                        KeyboardStatus::driving(
                            SURFACE,
                            format!("{model}: the four shape keys and M1–M5"),
                            KEYS,
                        ),
                    );
                    live = Some(Live {
                        board,
                        snapshot,
                        _claim: claim,
                    });
                    scene.invalidate();
                }
                Err(reason) => {
                    report(tracker, unavailable(reason));
                    next_attempt = Instant::now() + RETRY;
                }
            }
            continue;
        }
        let Some(ready) = live.as_mut() else {
            continue;
        };

        if let Some(frame) = scene.current() {
            let tuning = frame.settings.tuning(SURFACE);
            let composed = {
                let Ok(guard) = tracker.lock() else {
                    break;
                };
                let now = Instant::now();
                let cache = &mut foreground;
                let waiting_foreground = |lane: usize| -> bool {
                    let Some(session) = guard.on_lane(lane) else {
                        return false;
                    };
                    let key = (session.source.clone(), session.session_id.clone());
                    if let Some((verdict, at)) = cache.get(&key)
                        && now.duration_since(*at) < FOREGROUND_TTL
                    {
                        return *verdict;
                    }
                    let verdict = crate::focus::is_foreground(&session.ancestors);
                    cache.insert(key, (verdict, now));
                    verdict
                };
                compose(
                    guard.selected,
                    guard.locked,
                    frame.states,
                    frame.settings,
                    frame.elapsed_ms,
                    waiting_foreground,
                )
            };
            let colours: Vec<(usize, Rgb)> = composed
                .into_iter()
                .map(|(key, color)| (key, palette::tune(color, tuning)))
                .collect();
            if ready.board.paint(&colours).is_err() {
                // Unplugged, switched transports, asleep: the board keeps
                // whatever state it has and the next connect sorts out which.
                report(tracker, unavailable("disconnected — reconnecting"));
                live = None;
                next_attempt = Instant::now() + RETRY;
                continue;
            }
        }

        std::thread::sleep(FRAME);
    }

    if let Some(mut ready) = live
        && ready.board.restore(&ready.snapshot).is_ok()
    {
        forget();
    }
    report(tracker, KeyboardStatus::searching(SURFACE));
}

/// One numpad frame, untuned and by key position — pure, so the whole
/// vocabulary is a table a test can read. Keys 0..3 are the top line: the
/// classic four-key lane pattern of the displayed agent, dark when nothing
/// is displayed. Keys 4..8 are the M column: one agent per key, the selected
/// one lifted, the locked one fading colour-to-white, a lane past the shown
/// count dark.
fn compose(
    selected: Option<usize>,
    locked: bool,
    states: &[Option<State>],
    settings: &Settings,
    elapsed_ms: u64,
    mut waiting_foreground: impl FnMut(usize) -> bool,
) -> Vec<(usize, Rgb)> {
    let mut keys = Vec::with_capacity(KEYS);
    let shown = settings.lane_count.min(NUMPAD_LANES);
    let lane_color = |lane: usize| {
        settings
            .lanes
            .get(lane)
            .map(|lane| lane.color)
            .unwrap_or(palette::OFF)
    };

    let top_state = selected.and_then(|lane| states.get(lane).copied().flatten());
    let top_color = selected.map(&lane_color).unwrap_or(palette::OFF);
    for (index, color) in palette::lane_colors(top_state, top_color, TOP_KEYS, elapsed_ms)
        .into_iter()
        .enumerate()
    {
        keys.push((index, color));
    }

    for lane in 0..NUMPAD_LANES {
        let key = TOP_KEYS + lane;
        if lane >= shown {
            keys.push((key, palette::OFF));
            continue;
        }
        let state = states.get(lane).copied().flatten();
        let is_selected = selected == Some(lane);
        let color = if is_selected && locked && state.is_some() {
            palette::lock_blend(lane_color(lane), elapsed_ms)
        } else {
            let focused = state == Some(State::Waiting) && waiting_foreground(lane);
            palette::m_key(state, lane_color(lane), elapsed_ms, is_selected, focused)
        };
        keys.push((key, color));
    }
    keys
}

/// Finds a V0 Ultra, learns it, and takes the nine — remembering what was
/// there first, unless the board turns out to be one the app already set up.
fn connect(remembered: Option<&Snapshot>) -> Result<Taken, String> {
    let found = hid::find()?;
    if found.is_empty() {
        return Err("not detected".to_owned());
    }
    let mut last_error = String::new();
    for candidate in &found {
        // The Ultra surface shares this bus, and two threads on one
        // interface eat each other's echoes: claim first, skip what it holds
        // — somebody else's board is not a failure, just not ours.
        let Some(claim) = hid::claim(candidate) else {
            continue;
        };
        let transport = match hid::open(candidate) {
            Ok(transport) => transport,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let mut board = match Board::connect_with(transport, &session::V0_ULTRA) {
            Ok(board) => board,
            // The F-row's Ultra answers this handshake too: not ours.
            Err(ref error) if error.ends_with(session::DIFFERENT_BOARD) => continue,
            Err(error) => {
                last_error = format!("{}: {error}", candidate.product);
                continue;
            }
        };
        let snapshot = match (board.is_ours()?, remembered) {
            (true, Some(known)) => known.clone(),
            _ => board.snapshot()?,
        };
        board.take_over(&snapshot)?;
        let model = format!("{} over {}", candidate.product, candidate.link());
        return Ok((board, snapshot, model, claim));
    }
    // A candidate that errored is a board that may be ours and is not
    // answering; none at all — or only the F-row's — is simply absence.
    Err(if last_error.is_empty() {
        "not detected".to_owned()
    } else {
        "not responding — retrying".to_owned()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn settings(lane_count: usize) -> Settings {
        let mut settings = Settings::default();
        settings.set_lane_count(lane_count);
        settings
    }

    fn colors_of(frame: &[(usize, Rgb)]) -> Vec<Rgb> {
        let mut by_key = vec![palette::OFF; KEYS];
        for (key, color) in frame {
            by_key[*key] = *color;
        }
        by_key
    }

    #[test]
    fn every_frame_names_exactly_the_nine_keys_once() {
        let states = [
            Some(State::Running),
            None,
            Some(State::Waiting),
            None,
            None,
            None,
        ];
        let frame = compose(Some(0), false, &states, &settings(6), 123, |_| false);
        let mut named: Vec<usize> = frame.iter().map(|(key, _)| *key).collect();
        named.sort_unstable();
        assert_eq!(named, (0..KEYS).collect::<Vec<usize>>());
    }

    #[test]
    fn the_top_line_is_the_displayed_agents_classic_lane() {
        let states = [
            Some(State::Waiting),
            Some(State::Running),
            None,
            None,
            None,
            None,
        ];
        let settings = settings(6);
        let lane_color = settings.lanes[0].color;
        let frame = compose(Some(0), false, &states, &settings, 0, |_| false);
        let expected = palette::lane_colors(Some(State::Waiting), lane_color, TOP_KEYS, 0);
        assert_eq!(&colors_of(&frame)[..TOP_KEYS], &expected[..]);
        // Nothing displayed: the top line goes dark, the column stays lit.
        let dark = compose(None, false, &states, &settings, 0, |_| false);
        assert!(
            colors_of(&dark)[..TOP_KEYS]
                .iter()
                .all(|c| *c == palette::OFF)
        );
        assert_ne!(
            colors_of(&dark)[TOP_KEYS],
            palette::OFF,
            "M1 still shows its agent"
        );
    }

    #[test]
    fn the_m_column_is_one_agent_per_key_and_dark_past_the_shown_lanes() {
        let states = [
            Some(State::Connected),
            None,
            Some(State::Error),
            None,
            None,
            None,
        ];
        let settings = settings(3);
        let frame = compose(None, false, &states, &settings, 7, |_| false);
        let keys = colors_of(&frame);
        assert_eq!(keys[TOP_KEYS], palette::base(settings.lanes[0].color));
        assert_eq!(keys[TOP_KEYS + 1], palette::OFF, "an empty lane is dark");
        assert_eq!(keys[TOP_KEYS + 2], palette::DARK_RED);
        assert_eq!(
            keys[TOP_KEYS + 3],
            palette::OFF,
            "past the three lanes shown"
        );
        assert_eq!(keys[TOP_KEYS + 4], palette::OFF);
    }

    #[test]
    fn the_locked_selection_fades_toward_white_on_its_m_key_only() {
        let states = [
            Some(State::Running),
            Some(State::Running),
            None,
            None,
            None,
            None,
        ];
        let settings = settings(6);
        // Sample at the fade's peak so the locked key is unmistakably white.
        let elapsed = 1200;
        let unlocked = colors_of(&compose(
            Some(0),
            false,
            &states,
            &settings,
            elapsed,
            |_| false,
        ));
        let locked = colors_of(&compose(Some(0), true, &states, &settings, elapsed, |_| {
            false
        }));
        assert_eq!(
            locked[TOP_KEYS],
            palette::lock_blend(settings.lanes[0].color, elapsed)
        );
        assert_ne!(locked[TOP_KEYS], unlocked[TOP_KEYS], "the lock is visible");
        assert_eq!(
            locked[TOP_KEYS + 1],
            unlocked[TOP_KEYS + 1],
            "the neighbour is untouched"
        );
        assert_eq!(
            &locked[..TOP_KEYS],
            &unlocked[..TOP_KEYS],
            "the top line stays a faithful state render"
        );
        // A locked selection whose lane is empty has nothing to fade.
        let empty = [None, None, None, None, None, None];
        let idle = colors_of(&compose(Some(0), true, &empty, &settings, elapsed, |_| {
            false
        }));
        assert_eq!(idle[TOP_KEYS], palette::OFF);
    }

    #[test]
    fn a_waiting_agents_m_key_asks_about_its_terminal_and_only_then() {
        let states = [
            Some(State::Waiting),
            Some(State::Running),
            None,
            None,
            None,
            None,
        ];
        let settings = settings(6);
        let mut asked: Vec<usize> = Vec::new();
        let frame = compose(None, false, &states, &settings, 300, |lane| {
            asked.push(lane);
            true
        });
        assert_eq!(asked, vec![0], "only the Waiting lane asks");
        // Foreground: steady full — the top line does the pulsing.
        assert_eq!(colors_of(&frame)[TOP_KEYS], settings.lanes[0].color);
        // Not foreground: the double pulse, which at this instant is not full.
        let away = compose(None, false, &states, &settings, 300, |_| false);
        assert_ne!(colors_of(&away)[TOP_KEYS], settings.lanes[0].color);
    }
}
