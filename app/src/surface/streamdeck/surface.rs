//! The lighting thread for a Stream Deck: finds the deck on USB, takes it
//! while the Elgato app is not running, paints one row per lane from the
//! scene, and reads the keys back — as summons, and in Waiting as answers.
//!
//! A row is a lane the way the F-row is: its first key is the lane's name,
//! its last the lane's state, and the keys between are the lane's body, lit
//! from the same palette as the keyboards. Through [`Scene`] this thread is
//! also one of the application's clocks, like the keyboard ones. Unlike them
//! it paints on its own terms: a key carries how long the lane has been in
//! its state, which changes while the state does not, so the scene's
//! "nothing changed" is not this surface's. Instead every key is compared
//! with what it was last told, and only a key whose colour or label differs
//! is rasterised and written.
//!
//! One thread for both directions, by necessity: the device handle cannot be
//! shared, so the wait for a press is also the wait between frames.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::canvas::{self, Ink, Label};
use super::device::{self, Deck, Found};
use crate::focus::Key;
use crate::gauges::Gauges;
use crate::keys::Press;
use crate::settings::Rgb;
use crate::state::State;
use crate::surface::palette;
use crate::surface::scene::{Frame, Scene};
use crate::tracker::{self, KeyboardStatus, Tracker};

/// How this surface names itself in the window.
pub const SURFACE: &str = "Stream Deck";

/// ~10 Hz. A key is a JPEG in a few reports, and a lane in motion rewrites
/// its whole row every frame; ten a second is smooth on the LCD and easy on
/// the bus. The wait is spent listening for a press, so a key answers at
/// once whatever the rate.
const FRAME: Duration = Duration::from_millis(100);

/// How long to wait before looking for the deck again, and between checks
/// for the Elgato app while the deck is held.
const RETRY: Duration = Duration::from_secs(10);

/// How long the quit path waits for the deck to be handed back. A reset is
/// one feature report.
const RESTORE_GRACE: Duration = Duration::from_millis(500);

/// Why the deck is not taken while its own software is up.
const APP_RUNNING: &str =
    "the Stream Deck app is running — quit it to let Agent F-Row drive the deck";

/// A row needs this many keys to carry the three answers between its name
/// and its state.
const ANSWER_COLS: usize = 5;

/// The thread's run flag. A static rather than a field so the quit path — a
/// tray thread with no handle to anything — can lower it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Set by the thread as its last act, after the deck is handed back.
static FINISHED: AtomicBool = AtomicBool::new(true);

/// Handle to the running surface. Dropping it hands the deck back and stops
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

/// Starts the surface. Returns immediately; everything happens on the
/// thread, and a machine with no deck simply gets a sentence in the window.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Surface {
    RUNNING.store(true, Ordering::SeqCst);
    FINISHED.store(false, Ordering::SeqCst);
    let handle = std::thread::Builder::new()
        .name("agent-frow-streamdeck".to_owned())
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
/// thread to hand the deck back and waits, briefly, for it to have done so.
/// Without this a Quit would leave the keys showing the last frame.
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

/// What a key is told — the whole of it. Two equal faces are the same
/// pixels, which is what lets an unchanged key go unwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    pub colour: Rgb,
    pub label: Label,
    pub ink: Ink,
}

impl Face {
    /// A key past the lanes: dark, wordless.
    pub const BLANK: Self = Self {
        colour: palette::OFF,
        label: Label::None,
        ink: Ink::Quiet,
    };
}

/// What a key in a row is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The first key: the lane's name.
    Name,
    /// The keys between, counted from the left.
    Middle(usize),
    /// The last key: the lane's state.
    Status,
}

/// The role of column `col` in a row `cols` wide. On a row of one, the one
/// key is the name.
pub fn role(col: usize, cols: usize) -> Role {
    if col == 0 {
        Role::Name
    } else if col + 1 == cols {
        Role::Status
    } else {
        Role::Middle(col - 1)
    }
}

/// The colours of one row — the F-row's, with the last key steadied: the
/// state key holds the lane's resting glow through Waiting's pulse, Error's
/// red and Done's marker. Everything else is the palette's.
pub fn row_colors(state: Option<State>, colour: Rgb, cols: usize, elapsed_ms: u64) -> Vec<Rgb> {
    let mut row = palette::lane_colors(state, colour, cols, elapsed_ms);
    if cols >= 2
        && let Some(last) = row.last_mut()
        && let Some(State::Waiting | State::Error | State::Done) = state
    {
        *last = palette::base(colour);
    }
    row
}

/// How many lanes a deck of `rows` rows shows: a lane is a row, and the
/// lanes past the last row are simply not on the deck.
pub fn shown_lanes(rows: usize, lane_count: usize) -> usize {
    rows.min(lane_count)
}

/// The words for one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caption {
    pub name: String,
    /// `None` for a lane with nobody on it.
    pub state: Option<&'static str>,
    /// How long the lane has been as it is — a stopwatch, or in Waiting a
    /// count of minutes. Empty for a preview and for an empty lane.
    pub elapsed: String,
    /// The numbers, for a lane with a session on it; a preview has none.
    pub gauges: Option<Gauges>,
    /// Why the lane is in Error, when it is and when it is known.
    pub reason: Option<&'static str>,
}

/// What an empty lane's name key says when the user has not named it: the
/// lowest free lane is where the next agent lands, and says so in three
/// words; the others are simply free. "Lane 2" told nobody anything.
pub const NEXT_HERE: &str = "next agent here";
pub const FREE: &str = "free";

/// One caption per lane: what the lane is called, the state it shows, and
/// how long it has shown it. A preview names every lane with its state and
/// no time, since it is not a session; an empty lane says what it is for —
/// or its name, if the user gave it one, since a saved agent may be on its
/// way back to it.
pub fn captions(tracker: &Tracker, now: u64) -> Vec<Caption> {
    let settings = &tracker.settings;
    let next_free = (0..settings.lane_count).find(|lane| tracker.on_lane(*lane).is_none());
    (0..settings.lane_count)
        .map(|lane| {
            if let Some(preview) = tracker.preview {
                return Caption {
                    name: settings.display_name(lane, None),
                    state: Some(preview.state.label()),
                    elapsed: String::new(),
                    gauges: None,
                    reason: None,
                };
            }
            match tracker.on_lane(lane) {
                Some(session) => Caption {
                    name: settings.display_name(lane, session.project().as_deref()),
                    state: Some(session.effective_state().label()),
                    elapsed: tracker::clock(session.effective_state(), session.since, now),
                    gauges: Some(session.gauges),
                    reason: session.failure,
                },
                None => Caption {
                    name: if settings.named(lane) {
                        settings.display_name(lane, None)
                    } else if next_free == Some(lane) {
                        NEXT_HERE.to_owned()
                    } else {
                        FREE.to_owned()
                    },
                    state: None,
                    elapsed: String::new(),
                    gauges: None,
                    reason: None,
                },
            }
        })
        .collect()
}

/// Which keys went down between two readings. A key held is not pressed
/// again, and a key released is nothing.
pub fn pressed(before: &[bool], after: &[bool]) -> Vec<usize> {
    after
        .iter()
        .enumerate()
        .filter(|(index, down)| **down && !before.get(*index).copied().unwrap_or(false))
        .map(|(index, _)| index)
        .collect()
}

/// What every key should show, row-major: row `r` is lane `r`, its keys
/// coloured by [`row_colors`] and labelled by role — the name first, the
/// state over how long last (in Waiting, how long over the state), and
/// between them the lane's numbers: context used, the five-hour limit, the
/// seven-day limit — except in Waiting, where those three carry the answers
/// instead. Rows past the lanes are blank.
pub fn faces(frame: &Frame<'_>, captions: &[Caption], rows: usize, cols: usize) -> Vec<Face> {
    let settings = frame.settings;
    let shown = shown_lanes(rows, settings.lane_count);
    let mut faces = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        if row >= shown {
            faces.extend(std::iter::repeat_n(Face::BLANK, cols));
            continue;
        }
        let colour = settings
            .lanes
            .get(row)
            .map(|lane| lane.color)
            .unwrap_or(palette::OFF);
        let state = frame.states.get(row).copied().flatten();
        let caption = captions.get(row);
        let colours = row_colors(state, colour, cols, frame.elapsed_ms);
        for col in 0..cols {
            let key_colour = colours.get(col).copied().unwrap_or(palette::OFF);
            // A dark lane is labelled quietly; a lit one in the ink chosen
            // for its lane — steady, or fading with the key as the runner
            // crosses it, rather than vanishing into it.
            let ink = match state {
                None | Some(State::Idle) => Ink::Quiet,
                Some(_) => Ink::on(key_colour, colour),
            };
            let label = match role(col, cols) {
                Role::Name => Label::Name(caption.map(|c| c.name.clone()).unwrap_or_default()),
                Role::Status => match (state, caption.and_then(|c| c.state)) {
                    // Idle is one dim key and the rest off, words included.
                    (Some(State::Idle), _) | (_, None) => Label::None,
                    // In Error the second line is the reason, when there is
                    // one: "rate limit" says more than how long ago.
                    (Some(State::Error), Some(word))
                        if caption.and_then(|c| c.reason).is_some() =>
                    {
                        Label::Status {
                            state: word,
                            elapsed: caption
                                .and_then(|c| c.reason)
                                .unwrap_or_default()
                                .to_owned(),
                        }
                    }
                    // In Waiting how long is the news: the count as the
                    // headline, the word as its caption. A preview has no
                    // clock and keeps the word alone.
                    (Some(State::Waiting), Some(word))
                        if caption.is_some_and(|c| !c.elapsed.is_empty()) =>
                    {
                        Label::Wait {
                            state: word,
                            held: caption.map(|c| c.elapsed.clone()).unwrap_or_default(),
                        }
                    }
                    (_, Some(word)) => Label::Status {
                        state: word,
                        elapsed: caption.map(|c| c.elapsed.clone()).unwrap_or_default(),
                    },
                },
                Role::Middle(index) if state == Some(State::Waiting) && cols >= ANSWER_COLS => {
                    match index {
                        0 => Label::Up,
                        1 => Label::Down,
                        2 => Label::Enter,
                        _ => Label::None,
                    }
                }
                // The numbers, on the keys a narrow deck has: ctx alone on
                // three columns, all three on five or more. Idle shows
                // nothing but its name; a preview has no numbers to show.
                Role::Middle(index) => match (state, caption.and_then(|c| c.gauges), index) {
                    (Some(State::Idle), _, _) | (None, _, _) | (_, None, _) => Label::None,
                    (_, Some(gauges), 0) => Label::Gauge {
                        name: "ctx",
                        value: gauges.context_used,
                    },
                    (_, Some(gauges), 1) => Label::Gauge {
                        name: "5h",
                        value: gauges.five_hour,
                    },
                    (_, Some(gauges), 2) => Label::Gauge {
                        name: "7d",
                        value: gauges.seven_day,
                    },
                    _ => Label::None,
                },
            };
            faces.push(Face {
                colour: key_colour,
                label,
                ink,
            });
        }
    }
    faces
}

/// What pressing `key` on a deck `cols` wide means, with `shown` lanes on it
/// and the pressed row's lane `waiting` or not. Every key of a row summons
/// its lane; in Waiting the three after the name answer instead. A key past
/// the lanes is nothing. The same [`Press`] the F-row's keys speak.
pub fn press_of(key: usize, cols: usize, shown: usize, waiting: bool) -> Option<Press> {
    let cols = cols.max(1);
    let row = key / cols;
    let col = key % cols;
    if row >= shown {
        return None;
    }
    if waiting && cols >= ANSWER_COLS {
        match col {
            1 => return Some(Press::Answer(row, Key::Up)),
            2 => return Some(Press::Answer(row, Key::Down)),
            3 => return Some(Press::Answer(row, Key::Enter)),
            _ => {}
        }
    }
    Some(Press::Summon(row))
}

/// What a deck shows, for the window: the model, the lanes on it and the
/// keys each has — and, when the lane count runs past the rows, the lanes
/// that are not on it. The serial is `doctor`'s business, not the window's.
pub fn layout_sentence(found: &Found, lane_count: usize) -> String {
    fn lanes(from: usize, to: usize) -> String {
        if from == to {
            format!("lane {from}")
        } else {
            format!("lanes {from}–{to}")
        }
    }
    let shown = shown_lanes(found.rows, lane_count);
    let mut sentence = format!(
        "{}: {shown} lane{} of {} keys",
        found.model,
        if shown == 1 { "" } else { "s" },
        found.cols
    );
    if lane_count > shown {
        let verb = if lane_count - shown == 1 { "is" } else { "are" };
        sentence.push_str(&format!(
            ", {} {verb} not on it",
            lanes(shown + 1, lane_count)
        ));
    }
    sentence
}

/// Writes every key whose face differs from what it was last told, and
/// flushes if any did. `shown` is what the deck has, `None` for a key it has
/// never been told about; it is brought up to date as it goes. Returns how
/// many keys were written.
pub fn paint_changed(
    deck: &mut dyn Deck,
    shown: &mut [Option<Face>],
    wanted: &[Face],
) -> Result<usize, String> {
    let size = deck.key_size();
    let mut written = 0;
    for (key, face) in wanted.iter().enumerate() {
        if shown.get(key).is_some_and(|had| had.as_ref() == Some(face)) {
            continue;
        }
        let canvas = canvas::render_key(size, face.colour, &face.label, face.ink);
        deck.paint(key as u8, &canvas)?;
        if let Some(slot) = shown.get_mut(key) {
            *slot = Some(face.clone());
        }
        written += 1;
    }
    if written > 0 {
        deck.flush()?;
    }
    Ok(written)
}

/// A deck being painted.
struct Live {
    deck: Box<dyn Deck>,
    /// The deck as found: rows, columns, model — the geometry of the rows.
    layout: Found,
    /// What each key was last told; `None` until it has been.
    shown: Vec<Option<Face>>,
    /// Every key's last reading, so a press is an edge and not a level.
    buttons: Vec<bool>,
    /// The backlight last sent, so the slider is written only when moved.
    brightness: Option<u8>,
    /// When to look for the Elgato app next.
    next_check: Instant,
    /// The lane count the window was last told about — the sentence depends
    /// on nothing else that can change; `usize::MAX` until it has been told.
    reported: usize,
}

impl Live {
    fn new(deck: Box<dyn Deck>, layout: Found) -> Self {
        let keys = deck.keys();
        Self {
            deck,
            layout,
            shown: vec![None; keys],
            buttons: vec![false; keys],
            brightness: None,
            next_check: Instant::now() + RETRY,
            reported: usize::MAX,
        }
    }
}

fn render(tracker: &Arc<Mutex<Tracker>>) {
    if !cfg!(windows) {
        report(tracker, unavailable("the deck is only driven on Windows"));
        return;
    }

    let mut scene = Scene::new();
    let mut live: Option<Live> = None;
    let mut next_attempt = Instant::now();
    let mut enabled = true;

    while RUNNING.load(Ordering::SeqCst) {
        // The clock, deck or no deck. What it says about change is not
        // this surface's to use: the keys are compared one by one below.
        if scene.tick(tracker, crate::now_ms()).is_err() {
            break;
        }

        // Unticked in the window: hand the deck back to its logo and leave
        // it alone until ticked again, when it is looked for at once.
        let wanted = crate::surface::enabled(tracker, SURFACE);
        if wanted != enabled {
            enabled = wanted;
            if enabled {
                next_attempt = Instant::now();
            } else {
                if let Some(mut ready) = live.take() {
                    let _ = ready.deck.reset();
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
            match connect() {
                Ok((deck, found)) => live = Some(Live::new(deck, found)),
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

        if let Err(reason) = step(tracker, &scene, ready) {
            // Unplugged, asleep, or the Elgato app came up: the next connect
            // sorts out which.
            report(tracker, unavailable(reason));
            live = None;
            next_attempt = Instant::now() + RETRY;
        }
    }

    if let Some(mut ready) = live {
        let _ = ready.deck.reset();
    }
    report(tracker, KeyboardStatus::searching(SURFACE));
}

/// One frame and one wait: the app check, the backlight, every key that
/// changed, then up to [`FRAME`] listening for a press. `Err` is a deck that
/// is no longer ours.
fn step(tracker: &Arc<Mutex<Tracker>>, scene: &Scene, ready: &mut Live) -> Result<(), String> {
    if Instant::now() >= ready.next_check {
        ready.next_check = Instant::now() + RETRY;
        if device::elgato_app_running() {
            // Hand it back rather than fight over it.
            let _ = ready.deck.reset();
            return Err(APP_RUNNING.to_owned());
        }
    }

    let cols = ready.layout.cols.max(1);
    if let Some(frame) = scene.current() {
        let percent =
            (frame.settings.tuning(SURFACE).brightness.clamp(0.0, 1.0) * 100.0).round() as u8;
        if ready.brightness != Some(percent) {
            ready.deck.set_brightness(percent)?;
            ready.brightness = Some(percent);
        }
        let lane_count = frame.settings.lane_count;
        if lane_count != ready.reported {
            let shown = shown_lanes(ready.layout.rows, lane_count);
            report(
                tracker,
                KeyboardStatus::driving(
                    SURFACE,
                    layout_sentence(&ready.layout, lane_count),
                    shown * cols,
                ),
            );
            ready.reported = lane_count;
        }
        let now = crate::now_ms();
        let captions = {
            let tracker = tracker
                .lock()
                .map_err(|_| "the tracker's lock is poisoned".to_owned())?;
            captions(&tracker, now)
        };
        let wanted = faces(&frame, &captions, ready.layout.rows, cols);
        paint_changed(ready.deck.as_mut(), &mut ready.shown, &wanted)
            .map_err(|reason| format!("{reason}; reconnecting"))?;
    }

    if let Some(buttons) = ready
        .deck
        .poll(FRAME)
        .map_err(|reason| format!("{reason}; reconnecting"))?
    {
        let edges = pressed(&ready.buttons, &buttons);
        ready.buttons = buttons;
        if !edges.is_empty() {
            let presses: Vec<Press> = {
                let Ok(tracker) = tracker.lock() else {
                    return Ok(());
                };
                let shown = shown_lanes(ready.layout.rows, tracker.settings.lane_count);
                edges
                    .iter()
                    .filter_map(|&key| {
                        let row = key / cols;
                        press_of(key, cols, shown, tracker.answerable(row))
                    })
                    .collect()
            };
            for press in presses {
                match press {
                    Press::Summon(lane) => act(tracker, lane, None),
                    Press::Answer(lane, key) => act(tracker, lane, Some(key)),
                }
            }
        }
    }
    Ok(())
}

/// A press, on a thread of its own: a raise can spend a quarter of a second
/// on the terminal's tab strip, and the frame must not wait for it.
fn act(tracker: &Arc<Mutex<Tracker>>, lane: usize, key: Option<Key>) {
    let tracker = Arc::clone(tracker);
    let name = if key.is_some() {
        "agent-frow-deck-answer"
    } else {
        "agent-frow-deck-summon"
    };
    let _ = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || match key {
            Some(key) => crate::keys::answer_lane(&tracker, lane, key),
            None => crate::keys::summon_lane(&tracker, lane),
        });
}

/// Finds a deck and takes it — unless the Elgato app has it.
fn connect() -> Result<(Box<dyn Deck>, Found), String> {
    if device::elgato_app_running() {
        return Err(APP_RUNNING.to_owned());
    }
    let found = device::find()?;
    let Some(first) = found.first() else {
        return Err("no Stream Deck on USB".to_owned());
    };
    let deck = device::open(first)?;
    Ok((deck, first.clone()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::surface::streamdeck::device::fake::Recorder;
    use crate::tracker::{Preview, Session};
    use std::path::PathBuf;

    const ROWS: usize = 3;
    const COLS: usize = 5;

    fn session(state: State, lane: usize, since: u64) -> Session {
        Session {
            source: "claude-win".to_owned(),
            session_id: format!("s{lane}"),
            agent: None,
            cwd: Some(PathBuf::from("C:\\dev\\agent-frow")),
            state,
            since,
            first_seen: since,
            last_event: since,
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

    fn tracker(lane_count: usize) -> Tracker {
        let mut settings = Settings::default();
        settings.set_lane_count(lane_count);
        Tracker::new(settings, Default::default())
    }

    fn found(rows: usize, cols: usize) -> Found {
        Found {
            product_id: 0x006D,
            serial: "AL49".to_owned(),
            model: "Stream Deck OriginalV2".to_owned(),
            keys: rows * cols,
            rows,
            cols,
            size: (72, 72),
        }
    }

    fn faces_of(
        tracker: &Mutex<Tracker>,
        scene: &mut Scene,
        now: u64,
        rows: usize,
        cols: usize,
    ) -> Vec<Face> {
        scene.tick(tracker, now).unwrap();
        let frame = scene.current().expect("ticked");
        let captions = captions(&tracker.lock().unwrap(), now);
        faces(&frame, &captions, rows, cols)
    }

    fn lane_colour(tracker: &Mutex<Tracker>, lane: usize) -> Rgb {
        tracker.lock().unwrap().settings.lanes[lane].color
    }

    #[test]
    fn captions_name_the_lane_and_say_how_long() {
        let mut tracker = tracker(4);
        tracker.sessions.push(session(State::Waiting, 1, 0));
        let captions = captions(&tracker, 80_000);
        assert_eq!(captions.len(), 4);
        assert_eq!(captions[1].name, "agent-frow");
        assert_eq!(captions[1].state, Some("Waiting"));
        assert_eq!(captions[1].elapsed, "1m", "Waiting counts in minutes");
        assert_eq!(captions[0].name, NEXT_HERE, "the lowest free lane");
        assert_eq!(captions[0].state, None);
        assert_eq!(captions[0].elapsed, "");
        assert_eq!(captions[2].name, FREE);
        assert_eq!(captions[3].name, FREE);
    }

    #[test]
    fn a_named_empty_lane_keeps_its_name() {
        let mut tracker = tracker(3);
        tracker.settings.lanes[0].name = "Backend".to_owned();
        let captions = captions(&tracker, 0);
        assert_eq!(captions[0].name, "Backend");
        // A name does not reserve a lane — only a saved agent's preference
        // does — so the next agent still lands on lane 1, and the others
        // are merely free.
        assert_eq!(captions[1].name, FREE);
        assert_eq!(captions[2].name, FREE);
    }

    #[test]
    fn a_preview_captions_every_lane_with_its_state() {
        let mut tracker = tracker(3);
        tracker.sessions.push(session(State::Running, 0, 0));
        tracker.preview = Some(Preview {
            state: State::Error,
            expires_at: u64::MAX,
        });
        let captions = captions(&tracker, 5_000);
        assert!(captions.iter().all(|c| c.state == Some("Error")));
        assert!(captions.iter().all(|c| c.elapsed.is_empty()));
    }

    #[test]
    fn a_press_is_a_rising_edge_only() {
        let up = vec![false; 15];
        let mut down = up.clone();
        down[2] = true;
        assert_eq!(pressed(&up, &down), vec![2]);
        assert_eq!(pressed(&down, &down), Vec::<usize>::new(), "held");
        assert_eq!(pressed(&down, &up), Vec::<usize>::new(), "released");
        assert_eq!(pressed(&[], &down), vec![2], "first reading");
    }

    #[test]
    fn a_row_is_a_lane_and_the_last_key_is_status() {
        let tracker = Mutex::new(tracker(3));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Done, 2, 0));
        let colour = lane_colour(&tracker, 2);
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 80_000, ROWS, COLS);
        assert_eq!(faces.len(), 15);
        let row = &faces[10..15];
        assert_eq!(row[0].colour, colour, "the name key is lit in full");
        assert_eq!(row[0].label, Label::Name("agent-frow".to_owned()));
        for key in &row[1..4] {
            assert_eq!(key.colour, palette::base(colour), "the body rests");
            assert!(
                matches!(key.label, Label::Gauge { value: None, .. }),
                "unknown numbers, dashes: {:?}",
                key.label
            );
        }
        assert_eq!(
            row[4].colour,
            palette::base(colour),
            "the status key rests, Done included"
        );
        assert_eq!(
            row[4].label,
            Label::Status {
                state: "Done",
                elapsed: "1m 20s".to_owned()
            }
        );
        assert!(row.iter().all(|key| key.ink != Ink::Quiet));
        assert_eq!(
            row[0].ink,
            Ink::on(colour, colour),
            "the name key's ink suits its colour"
        );
    }

    #[test]
    fn waiting_puts_up_down_enter_on_the_middle_keys() {
        let tracker = Mutex::new(tracker(3));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Waiting, 0, 0));
        let colour = lane_colour(&tracker, 0);
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 1_000, ROWS, COLS);
        assert_eq!(
            faces[0].colour, colour,
            "the name key is the marker, in full"
        );
        assert_eq!(faces[1].label, Label::Up);
        assert_eq!(faces[2].label, Label::Down);
        assert_eq!(faces[3].label, Label::Enter);
        assert_eq!(
            faces[4].colour,
            palette::base(colour),
            "the status key does not pulse"
        );
        assert!(
            matches!(
                faces[4].label,
                Label::Wait {
                    state: "Waiting",
                    ..
                }
            ),
            "the status key leads with how long"
        );

        let narrow = faces_of(&tracker, &mut scene, 1_100, 2, 3);
        assert!(
            matches!(narrow[1].label, Label::Gauge { name: "ctx", .. }),
            "no answers on a three-key row — the context instead: {:?}",
            narrow[1].label
        );
    }

    #[test]
    fn an_error_row_is_red_in_the_middle_only() {
        let tracker = Mutex::new(tracker(3));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Error, 1, 0));
        let colour = lane_colour(&tracker, 1);
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, ROWS, COLS);
        let row = &faces[5..10];
        assert_eq!(row[0].colour, colour);
        for key in &row[1..4] {
            assert_eq!(key.colour, palette::DARK_RED);
        }
        assert_eq!(
            row[4].colour,
            palette::base(colour),
            "the status key is never red"
        );
    }

    #[test]
    fn an_idle_row_is_one_dim_name_key() {
        let tracker = Mutex::new(tracker(3));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Idle, 0, 0));
        let colour = lane_colour(&tracker, 0);
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, ROWS, COLS);
        assert_eq!(faces[0].colour, palette::base(colour));
        assert_eq!(faces[0].ink, Ink::Quiet);
        for key in &faces[1..5] {
            assert_eq!(key.colour, palette::OFF);
            assert_eq!(key.label, Label::None, "the rest is off, words included");
        }
    }

    #[test]
    fn an_empty_lane_says_what_it_is_for_in_grey() {
        let tracker = Mutex::new(tracker(3));
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, ROWS, COLS);
        assert_eq!(faces[0].colour, palette::OFF);
        assert_eq!(faces[0].label, Label::Name(NEXT_HERE.to_owned()));
        assert_eq!(faces[0].ink, Ink::Quiet);
        assert!(faces[1..5].iter().all(|key| *key == Face::BLANK));
    }

    #[test]
    fn rows_past_the_lanes_are_blank() {
        let four = Mutex::new(tracker(4));
        let mut scene = Scene::new();
        let faces = faces_of(&four, &mut scene, 0, ROWS, COLS);
        assert_eq!(faces.len(), 15);
        assert!(
            faces
                .iter()
                .step_by(COLS)
                .all(|key| key.label != Label::None),
            "every row of a three-row deck is a lane when there are four"
        );

        let three = Mutex::new(tracker(3));
        let faces = faces_of(&three, &mut scene, 0, 4, 8);
        assert_eq!(faces.len(), 32);
        assert!(
            faces[24..].iter().all(|key| *key == Face::BLANK),
            "an XL's fourth row"
        );
        assert_eq!(faces[16].label, Label::Name(FREE.to_owned()));
    }

    #[test]
    fn a_press_maps_to_row_and_role() {
        assert_eq!(press_of(7, 5, 3, true), Some(Press::Answer(1, Key::Down)));
        assert_eq!(press_of(6, 5, 3, true), Some(Press::Answer(1, Key::Up)));
        assert_eq!(press_of(8, 5, 3, true), Some(Press::Answer(1, Key::Enter)));
        assert_eq!(
            press_of(5, 5, 3, true),
            Some(Press::Summon(1)),
            "the name key"
        );
        assert_eq!(
            press_of(9, 5, 3, true),
            Some(Press::Summon(1)),
            "the status key"
        );
        assert_eq!(
            press_of(7, 5, 3, false),
            Some(Press::Summon(1)),
            "not waiting"
        );
        assert_eq!(press_of(12, 5, 2, true), None, "a row past the lanes");
        assert_eq!(
            press_of(1, 3, 1, true),
            Some(Press::Summon(0)),
            "a narrow deck"
        );
    }

    #[test]
    fn the_status_sentence_counts_the_lanes_on_the_deck() {
        let three = layout_sentence(&found(3, 5), 3);
        assert!(three.ends_with(": 3 lanes of 5 keys"), "{three}");
        assert!(!three.contains("serial"), "{three}");
        let six = layout_sentence(&found(3, 5), 6);
        assert!(
            six.ends_with("3 lanes of 5 keys, lanes 4–6 are not on it"),
            "{six}"
        );
        let four = layout_sentence(&found(3, 5), 4);
        assert!(four.ends_with(", lane 4 is not on it"), "{four}");
        let xl = layout_sentence(&found(4, 8), 3);
        assert!(xl.ends_with(": 3 lanes of 8 keys"), "{xl}");
    }

    #[test]
    fn paint_changed_writes_only_changed_keys() {
        let tracker = Mutex::new(tracker(4));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Done, 2, 0));
        let mut scene = Scene::new();
        let mut deck = Recorder::new(15);
        let mut shown = vec![None; 15];

        let wanted = faces_of(&tracker, &mut scene, 500, ROWS, COLS);
        let written = paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert_eq!(written, 15, "a deck just taken is painted whole");
        assert_eq!(deck.flushes, 1);

        let wanted = faces_of(&tracker, &mut scene, 900, ROWS, COLS);
        let written = paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert_eq!(written, 0, "still the same second, nothing moves");
        assert_eq!(deck.flushes, 1, "and nothing is flushed");

        let wanted = faces_of(&tracker, &mut scene, 1_500, ROWS, COLS);
        let written = paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert_eq!(written, 1, "the elapsed time on lane 3 ticked over");
        assert_eq!(
            deck.writes.last().unwrap().0,
            14,
            "and only its status key was written"
        );
        assert_eq!(deck.flushes, 2);
    }

    #[test]
    fn a_waiting_row_leads_with_how_long() {
        let tracker = Mutex::new(tracker(3));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Waiting, 0, 0));
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 80_000, ROWS, COLS);
        assert_eq!(
            faces[4].label,
            Label::Wait {
                state: "Waiting",
                held: "1m".to_owned()
            }
        );
        assert_eq!(faces[1].label, Label::Up, "the answer keys stay");
        assert_eq!(faces[2].label, Label::Down);
        assert_eq!(faces[3].label, Label::Enter);
    }

    #[test]
    fn a_waiting_status_key_is_written_once_a_minute() {
        let tracker = Mutex::new(tracker(4));
        tracker
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Waiting, 2, 0));
        let mut scene = Scene::new();
        let mut deck = Recorder::new(15);
        let mut shown = vec![None; 15];

        let wanted = faces_of(&tracker, &mut scene, 61_000, ROWS, COLS);
        paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert_eq!(
            shown[14].as_ref().unwrap().label,
            Label::Wait {
                state: "Waiting",
                held: "1m".to_owned()
            }
        );

        let before = deck.writes.len();
        let wanted = faces_of(&tracker, &mut scene, 61_900, ROWS, COLS);
        paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert!(
            deck.writes[before..].iter().all(|(key, _)| *key != 14),
            "inside the minute the status key holds; the pulse may move the others"
        );

        let wanted = faces_of(&tracker, &mut scene, 120_000, ROWS, COLS);
        paint_changed(&mut deck, &mut shown, &wanted).unwrap();
        assert!(
            deck.writes[before..].iter().any(|(key, _)| *key == 14),
            "and it turns over at the minute"
        );
        assert_eq!(
            shown[14].as_ref().unwrap().label,
            Label::Wait {
                state: "Waiting",
                held: "2m".to_owned()
            }
        );
    }

    #[test]
    fn the_layout_is_re_reported_when_the_lane_count_changes() {
        let tracker = Arc::new(Mutex::new(tracker(3)));
        let mut scene = Scene::new();
        scene.tick(&tracker, 0).unwrap();
        let mut live = Live::new(Box::new(Recorder::new(15)), found(3, 5));
        step(&tracker, &scene, &mut live).unwrap();
        let first = tracker.lock().unwrap().keyboards[0].clone();
        assert!(first.connected);
        assert_eq!(first.driven, 15);
        assert!(
            first.detail.ends_with("3 lanes of 5 keys"),
            "{}",
            first.detail
        );

        tracker.lock().unwrap().settings.set_lane_count(6);
        scene.tick(&tracker, 1).unwrap();
        step(&tracker, &scene, &mut live).unwrap();
        let second = tracker.lock().unwrap().keyboards[0].clone();
        assert!(
            second.detail.ends_with("lanes 4–6 are not on it"),
            "{}",
            second.detail
        );
        assert_eq!(second.driven, 15, "the deck still drives every key it has");
    }

    #[test]
    fn a_lane_with_numbers_shows_them_on_the_middle_keys() {
        let tracker = Mutex::new(tracker(3));
        {
            let mut session = session(State::Running, 0, 0);
            session.gauges = Gauges {
                context_used: Some(42),
                five_hour: Some(10),
                seven_day: None,
            };
            tracker.lock().unwrap().sessions.push(session);
        }
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, ROWS, COLS);
        assert_eq!(
            faces[1].label,
            Label::Gauge {
                name: "ctx",
                value: Some(42)
            }
        );
        assert_eq!(
            faces[2].label,
            Label::Gauge {
                name: "5h",
                value: Some(10)
            }
        );
        assert_eq!(
            faces[3].label,
            Label::Gauge {
                name: "7d",
                value: None
            }
        );
        assert!(matches!(
            faces[4].label,
            Label::Status {
                state: "Running",
                ..
            }
        ));
    }

    #[test]
    fn a_preview_row_has_no_gauges() {
        let tracker = Mutex::new(tracker(3));
        tracker.lock().unwrap().preview = Some(Preview {
            state: State::Done,
            expires_at: u64::MAX,
        });
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, ROWS, COLS);
        assert!(faces[1..4].iter().all(|key| key.label == Label::None));
        assert!(matches!(
            faces[4].label,
            Label::Status { state: "Done", .. }
        ));
    }

    #[test]
    fn an_error_row_says_why_on_the_status_key() {
        let limited = Mutex::new(tracker(3));
        {
            let mut session = session(State::Error, 0, 0);
            session.failure = Some("rate limit");
            limited.lock().unwrap().sessions.push(session);
        }
        let mut scene = Scene::new();
        let faces = faces_of(&limited, &mut scene, 80_000, ROWS, COLS);
        assert_eq!(
            faces[4].label,
            Label::Status {
                state: "Error",
                elapsed: "rate limit".to_owned()
            }
        );
        // Without a reason, the clock as usual.
        let plain = Mutex::new(tracker(3));
        plain
            .lock()
            .unwrap()
            .sessions
            .push(session(State::Error, 0, 0));
        let faces = faces_of(&plain, &mut scene, 80_000, ROWS, COLS);
        assert_eq!(
            faces[4].label,
            Label::Status {
                state: "Error",
                elapsed: "1m 20s".to_owned()
            }
        );
    }

    #[test]
    fn a_narrow_deck_shows_what_fits() {
        let tracker = Mutex::new(tracker(3));
        {
            let mut session = session(State::Done, 0, 0);
            session.gauges = Gauges {
                context_used: Some(42),
                five_hour: Some(10),
                seven_day: Some(3),
            };
            tracker.lock().unwrap().sessions.push(session);
        }
        let mut scene = Scene::new();
        let faces = faces_of(&tracker, &mut scene, 0, 2, 3);
        assert_eq!(
            faces[1].label,
            Label::Gauge {
                name: "ctx",
                value: Some(42)
            },
            "three columns: the context alone"
        );
        let faces = faces_of(&tracker, &mut scene, 0, 4, 8);
        assert!(matches!(faces[3].label, Label::Gauge { name: "7d", .. }));
        assert_eq!(faces[4].label, Label::None, "and nothing after the third");
    }
}
