//! The lighting thread for the V0 Ultra numpad: finds the board on the cable
//! or through its receiver, takes the four shape keys and M1–M5, paints them
//! from the scene and the tracker's selection, and hands the board back on
//! the way out.
//!
//! Unlike the F-row this surface paints on its own terms, the way the deck
//! does: [`Scene::tick`] is used as the clock only, and every frame is
//! composed from [`Scene::current`] plus one look at the tracker — selection
//! and lock change without any lane changing state, and [`Board::paint`]
//! already writes only the keys that differ, so a still board costs nothing
//! anyway.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths;
use crate::settings::{NUMPAD_LANES, Rgb, Settings};
use crate::state::State;
use crate::surface::keychron::hid::{self, Transport};
use crate::surface::keychron::journal::{Journal, Try};
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

/// How long after Windows reports a change to look. A keyboard arrives as
/// several interfaces in a burst, and each one pushes this back.
const SETTLE: Duration = Duration::from_secs(1);

/// How long the quit path waits for the board to be handed back.
const RESTORE_GRACE: Duration = Duration::from_millis(1500);

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
    claim: hid::Claim,
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
    let mut journal = Journal::new(SURFACE);
    let changes = hid::watch();

    while RUNNING.load(Ordering::SeqCst) {
        // The clock, board or no board. What it says about change is not
        // this surface's to use: selection and lock live outside it.
        if scene.tick(tracker, crate::now_ms()).is_err() {
            break;
        }

        let wanted = crate::surface::enabled(tracker, SURFACE);
        if wanted != enabled {
            enabled = wanted;
            journal.ticked(enabled);
            if enabled {
                next_attempt = Instant::now();
            } else {
                if let Some(mut ready) = live.take() {
                    let restored = ready.board.restore(&ready.snapshot);
                    journal.handed_back(&restored);
                    if restored.is_ok() {
                        forget();
                    }
                }
                report(tracker, KeyboardStatus::off(SURFACE));
            }
        }
        // Windows says a Keychron interface came or went: drop a board whose
        // interface left, check one that may have lost its keyboard to the
        // other link, and look soon rather than at the next retry.
        for change in changes.try_iter() {
            if !enabled {
                continue;
            }
            let held = live.as_ref().map(|ready| &ready.claim);
            let gone = match hid::respond(&change, held) {
                hid::Response::Drop => Some("Windows reported its interface removed".to_owned()),
                hid::Response::Check => live.as_mut().and_then(|ready| ready.board.answers().err()),
                hid::Response::Look => {
                    next_attempt = Instant::now() + SETTLE;
                    None
                }
                hid::Response::Ignore => None,
            };
            if let Some(error) = gone {
                journal.lost(&error);
                report(tracker, unavailable("disconnected — reconnecting"));
                live = None;
                next_attempt = Instant::now() + SETTLE;
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
            let mut tries = Vec::new();
            match connect(remembered.as_ref(), &mut tries) {
                Ok((board, snapshot, model, claim)) => {
                    journal.connected(&model, &board.firmware, board.led_count, &tries);
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
                        claim,
                    });
                    scene.invalidate();
                }
                Err(reason) => {
                    journal.failed(&reason, &tries);
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
                compose(
                    guard.selected,
                    guard.locked,
                    frame.states,
                    frame.agent_counts,
                    frame.settings,
                    frame.elapsed_ms,
                )
            };
            let colours: Vec<(usize, Rgb)> = composed
                .into_iter()
                .map(|(key, color)| (key, palette::tune(color, tuning)))
                .collect();
            if let Err(error) = ready.board.paint(&colours) {
                // Unplugged, switched transports, asleep: the board keeps
                // whatever state it has and the next connect sorts out which.
                journal.lost(&error);
                report(tracker, unavailable("disconnected — reconnecting"));
                live = None;
                next_attempt = Instant::now() + RETRY;
                continue;
            }
        }

        std::thread::sleep(FRAME);
    }

    if let Some(mut ready) = live {
        let restored = ready.board.restore(&ready.snapshot);
        journal.handed_back(&restored);
        if restored.is_ok() {
            forget();
        }
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
    agent_counts: &[usize],
    settings: &Settings,
    elapsed_ms: u64,
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
    let top_count = selected
        .and_then(|lane| agent_counts.get(lane))
        .copied()
        .unwrap_or(0);
    for (index, color) in
        palette::lane_colors(top_state, top_color, TOP_KEYS, elapsed_ms, top_count)
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
            palette::m_key(state, lane_color(lane), elapsed_ms, is_selected)
        };
        keys.push((key, color));
    }
    keys
}

/// Finds a V0 Ultra, learns it, and takes the nine — remembering what was
/// there first, unless the board turns out to be one the app already set up.
/// Every interface looked at goes into `tries`, with the step it stopped at.
fn connect(remembered: Option<&Snapshot>, tries: &mut Vec<Try>) -> Result<Taken, String> {
    let found = hid::find()?;
    if found.is_empty() {
        return Err("not detected".to_owned());
    }
    let mut last_error = String::new();
    for candidate in &found {
        let started = Instant::now();
        // The Ultra surface shares this bus, and two threads on one
        // interface eat each other's echoes: claim first, skip what it holds
        // — somebody else's board is not a failure, just not ours.
        let Some(claim) = hid::claim(candidate) else {
            tries.push(Try::new(candidate, "held", "", started));
            continue;
        };
        let transport = match hid::open(candidate) {
            Ok(transport) => transport,
            Err(error) => {
                tries.push(Try::new(candidate, "open", &error, started));
                last_error = error;
                continue;
            }
        };
        let mut board = match Board::connect_with(transport, &session::V0_ULTRA) {
            Ok(board) => board,
            // The F-row's Ultra answers this handshake too: not ours.
            Err(ref error) if error.ends_with(session::DIFFERENT_BOARD) => {
                tries.push(Try::new(candidate, "keyboard", "", started));
                continue;
            }
            Err(error) => {
                tries.push(Try::new(candidate, "handshake", &error, started));
                last_error = format!("{}: {error}", candidate.product);
                continue;
            }
        };
        let snapshot = match board.settle(remembered) {
            Ok(snapshot) => snapshot,
            Err((step, error)) => {
                tries.push(Try::new(candidate, step, &error, started));
                return Err(error);
            }
        };
        tries.push(Try::new(candidate, "taken", "", started));
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
        let frame = compose(Some(0), false, &states, &[1; 6], &settings(6), 123);
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
        let frame = compose(Some(0), false, &states, &[1; 6], &settings, 0);
        let expected = palette::lane_colors(Some(State::Waiting), lane_color, TOP_KEYS, 0, 1);
        assert_eq!(&colors_of(&frame)[..TOP_KEYS], &expected[..]);
        // Nothing displayed: the top line goes dark, the column stays lit.
        let dark = compose(None, false, &states, &[1; 6], &settings, 0);
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
    fn subagents_widen_only_the_selected_lanes_top_line() {
        let settings = settings(3);
        let states = [Some(State::Running); 3];
        let single = colors_of(&compose(Some(1), false, &states, &[1; 3], &settings, 0));
        let group = colors_of(&compose(Some(1), false, &states, &[1, 4, 1], &settings, 0));
        assert_eq!(
            &single[TOP_KEYS..],
            &group[TOP_KEYS..],
            "M column stays unchanged"
        );
        assert_eq!(&group[..TOP_KEYS], &[settings.lanes[1].color; TOP_KEYS]);
        assert_ne!(&single[..TOP_KEYS], &group[..TOP_KEYS]);
        let other = colors_of(&compose(Some(0), false, &states, &[1, 4, 1], &settings, 0));
        assert_eq!(
            &other[..TOP_KEYS],
            &palette::lane_colors(
                Some(State::Running),
                settings.lanes[0].color,
                TOP_KEYS,
                0,
                1
            )
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
        let frame = compose(None, false, &states, &[1; 6], &settings, 7);
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
            &[1; 6],
            &settings,
            elapsed,
        ));
        let locked = colors_of(&compose(
            Some(0),
            true,
            &states,
            &[1; 6],
            &settings,
            elapsed,
        ));
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
        let idle = colors_of(&compose(Some(0), true, &empty, &[0; 6], &settings, elapsed));
        assert_eq!(idle[TOP_KEYS], palette::OFF);
    }

    #[test]
    fn a_done_agents_m_key_holds_full_while_a_waiting_ones_beats() {
        let states = [
            Some(State::Done),
            Some(State::Waiting),
            None,
            None,
            None,
            None,
        ];
        let settings = settings(6);
        // Sampled off the beat: Done is still exactly its colour, Waiting is not.
        let frame = colors_of(&compose(None, false, &states, &[1; 6], &settings, 300));
        assert_eq!(frame[TOP_KEYS], settings.lanes[0].color);
        assert_ne!(frame[TOP_KEYS + 1], settings.lanes[1].color);
    }
}
