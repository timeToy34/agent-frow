//! The lighting thread for a Keychron Ultra: finds the keyboard on the cable
//! or through its receiver, takes the F-row, paints it from the scene, and
//! hands it back on the way out.
//!
//! Through [`Scene`] this thread is also one of the application's clocks,
//! like the Corsair one. The two run side by side; whichever keyboard is
//! plugged in lights up, and both do if both are.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::hid::{self, Transport};
use super::session::{Board, Snapshot};
use crate::paths;
use crate::surface::palette;
use crate::surface::scene::Scene;
use crate::tracker::{KeyboardStatus, Tracker};

/// How this surface names itself in the window.
pub const SURFACE: &str = "Keychron";

/// ~30 Hz, like the Corsair surface: a frame is at most two reports and the
/// keyboard answers each in about a millisecond, on the cable or the receiver.
const FRAME: Duration = Duration::from_millis(33);

/// How long to wait before looking for the keyboard again. Looking is an
/// enumeration of the HID bus — cheap, but not free thirty times a second.
const RETRY: Duration = Duration::from_secs(10);

/// How long the quit path waits for the keyboard to be handed back. A restore
/// is a dozen reports; this is room for a slow receiver, not a stuck one.
const RESTORE_GRACE: Duration = Duration::from_millis(1500);

/// The thread's run flag. A static rather than a field so the quit path — a
/// tray thread with no handle to anything — can lower it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the thread as its last act, after the keyboard is restored.
static FINISHED: AtomicBool = AtomicBool::new(true);

/// Handle to the running surface. Dropping it restores the keyboard and
/// stops the thread.
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

/// Starts the lighting. Returns immediately; everything happens on the
/// thread, and a machine with no Keychron simply gets a sentence in the window.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Surface {
    RUNNING.store(true, Ordering::SeqCst);
    FINISHED.store(false, Ordering::SeqCst);
    let handle = std::thread::Builder::new()
        .name("agent-frow-keychron".to_owned())
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
/// thread to hand the keyboard back and waits, briefly, for it to have done
/// so. Without this a Quit would leave the F-row showing the last frame until
/// the keyboard is power-cycled.
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

/// Where the snapshot waits for the app that died before restoring it. On the
/// next start a keyboard found already in the app's mode is restored from
/// here rather than "snapshotted" — which would capture the app's own work.
fn state_file() -> Option<PathBuf> {
    paths::install_dir().map(|dir| dir.join("keychron-state.json"))
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
    Snapshot::parse(&text).ok()
}

/// A keyboard the app has taken: the board, what it looked like before, and
/// how the window should name it.
type Taken = (Board<Box<dyn Transport>>, Snapshot, String);

/// A keyboard being painted, with what it looked like before.
struct Live {
    board: Board<Box<dyn Transport>>,
    snapshot: Snapshot,
    available: Vec<usize>,
}

fn render(tracker: &Mutex<Tracker>) {
    if !cfg!(windows) {
        report(
            tracker,
            unavailable("the keyboard is only driven on Windows"),
        );
        return;
    }

    let mut scene = Scene::new();
    let mut live: Option<Live> = None;
    // What the keyboard looked like before the app, kept across reconnects:
    // a keyboard that re-enumerates after sleep is still in the app's mode,
    // and this is what it goes back to.
    let mut remembered: Option<Snapshot> = recall();
    let mut next_attempt = Instant::now();

    while RUNNING.load(Ordering::SeqCst) {
        // The clock, keyboard or no keyboard.
        let Ok(frame) = scene.tick(tracker, crate::now_ms()) else {
            break;
        };

        if live.is_none() {
            if Instant::now() < next_attempt {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            match connect(remembered.as_ref()) {
                Ok((board, snapshot, model)) => {
                    remember(&snapshot);
                    remembered = Some(snapshot.clone());
                    report(
                        tracker,
                        KeyboardStatus::connected(SURFACE, &model, board.available().len()),
                    );
                    live = Some(Live {
                        available: board.available(),
                        board,
                        snapshot,
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

        // A motionless, unchanged board needs no frame. The wake stays at
        // FRAME so the tick above keeps being the application's clock.
        let Some(frame) = frame else {
            std::thread::sleep(FRAME);
            continue;
        };

        let colours = palette::frame(
            frame.states,
            frame.settings,
            frame.elapsed_ms,
            &ready.available,
        );
        if let Err(reason) = ready.board.paint(&colours) {
            // Unplugged, switched to the receiver, asleep: the keyboard keeps
            // whatever state it has and the next connect sorts out which.
            report(tracker, unavailable(format!("{reason}; reconnecting")));
            live = None;
            next_attempt = Instant::now() + RETRY;
            continue;
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

/// Finds a keyboard, learns it, and takes the F-row — remembering what was
/// there first, unless the keyboard turns out to be one the app already set
/// up, in which case `remembered` is what it goes back to.
fn connect(remembered: Option<&Snapshot>) -> Result<Taken, String> {
    let found = hid::find()?;
    if found.is_empty() {
        return Err("no Keychron Ultra on the cable or the receiver".to_owned());
    }
    let mut last_error = String::new();
    for candidate in &found {
        let transport = match hid::open(candidate) {
            Ok(transport) => transport,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let mut board = match Board::connect(transport) {
            Ok(board) => board,
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
        return Ok((board, snapshot, model));
    }
    Err(last_error)
}
