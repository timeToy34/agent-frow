//! The F-row's keys: three lanes of four. Any key of a lane brings its agent
//! forward; while the lane is Waiting, the three after the first answer it
//! instead — Up, Down, Enter — the rule a Stream Deck row follows, on the
//! keyboard.
//!
//! The app listens for **F13–F24**, not F1–F12. The F-row's ordinary meanings
//! belong to every other application on the machine, so the user remaps the
//! keyboard's F-row to F13–F24 in the keyboard's own software — an iCUE
//! profile, the Keychron Launcher keymap — a one-time setup, and from then on
//! those twelve codes mean nothing to anything but us. Registering them
//! as global hotkeys keeps them out of whichever application has keyboard focus
//! and delivers a `WM_HOTKEY` message to this process instead.
//!
//! `RegisterHotKey` is load-bearing here. The old `WH_KEYBOARD_LL` hook ate the
//! press before Windows delivered input to any process, so Windows denied the
//! subsequent attempt to activate a terminal whenever this app was focused.
//! `WM_HOTKEY` is the physical input delivered to us, giving the summon the
//! same foreground permission that clicking the Focus button gets. The OS also
//! suppresses the registered F13–F24 keystrokes globally, and `MOD_NOREPEAT`
//! replaces the key-up bookkeeping the hook used to need — and makes a held
//! answer key one answer, not a stream of them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::event::Ancestor;
use crate::focus::Key;
use crate::settings::{KEYBOARD_LANES, KEYS, KEYS_PER_LANE};
use crate::tracker::Tracker;

/// Stops the hotkey pump immediately, for the quit path.
///
/// Quit exits the process without unwinding, so `Drop` never runs. Windows will
/// release process-owned hotkeys at exit, but asking the owning thread to stop
/// first is cleaner and makes the registrations available immediately.
pub fn unhook_now() {
    #[cfg(windows)]
    windows_impl::unhook_now();
}

/// Stage counters through the delivery chain, so when a press goes missing
/// the panel can say exactly which stage dropped it rather than "not working".
static RECEIVED: AtomicU64 = AtomicU64::new(0);
static QUEUED: AtomicU64 = AtomicU64::new(0);
static HANDLED: AtomicU64 = AtomicU64::new(0);

/// (F-row and V0 hotkeys received, queued to the worker, handled).
pub fn stages() -> (u64, u64, u64) {
    (
        RECEIVED.load(Ordering::Relaxed),
        QUEUED.load(Ordering::Relaxed),
        HANDLED.load(Ordering::Relaxed),
    )
}

/// F13, the first code of the range we own.
pub const VK_F13: u32 = 0x7C;

/// How many codes: F13 through F24, one per F-row key.
pub const SUMMON_KEYS: usize = KEYS;

/// Knob CCW/CW/press and M1–M5 use Ctrl+Shift+F13–F20.
const NUMPAD_CHORDS: usize = 8;

/// The top line uses single keys so answering cannot itself hold Ctrl/Shift.
/// These Windows virtual keys were checked on the US layout (00000409):
/// Launcher KC_INT1 / KC_INT5 / KC_INT6 / KC_KP_COMMA, left to right.
/// Bare F13–F24 belong to the F-row; the old top-row chords are not registered.
const NUMPAD_TOP_ROW: [(u32, &str); 4] = [
    (0xC1, "Intl1"),
    (0xEB, "Intl5"),
    (0xEA, "Intl6"),
    (0xC2, "Keypad Comma"),
];

/// The numpad's controls: the knob's three, M1–M5, then the top line's four.
pub const NUMPAD_KEYS: usize = NUMPAD_CHORDS + NUMPAD_TOP_ROW.len();

/// The label a key index carries in the window: 0 → "F13"; the numpad's
/// controls follow, 12 → "Ctrl+Shift+F13", 20 → "Intl1".
pub fn key_label(index: usize) -> String {
    if index < SUMMON_KEYS {
        format!("F{}", 13 + index)
    } else if index < SUMMON_KEYS + NUMPAD_CHORDS {
        format!("Ctrl+Shift+F{}", 13 + index - SUMMON_KEYS)
    } else {
        NUMPAD_TOP_ROW
            .get(index - SUMMON_KEYS - NUMPAD_CHORDS)
            .map_or("Unknown key", |(_, label)| label)
            .to_owned()
    }
}

/// The keys a lane has on the keyboard, "F13–F16" — or `None` for a lane
/// past the three the keyboard carries.
pub fn lane_keys_label(lane: usize) -> Option<String> {
    (lane < KEYBOARD_LANES).then(|| {
        let first = lane * KEYS_PER_LANE;
        format!(
            "{}–{}",
            key_label(first),
            key_label(first + KEYS_PER_LANE - 1)
        )
    })
}

/// What a press on any surface does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// Bring the lane's agent forward.
    Summon(usize),
    /// Bring it forward and answer it with one key.
    Answer(usize, Key),
    /// Move the numpad's selection this many steps — the knob turning.
    Select(isize),
    /// Pin the numpad's selection, or unpin it — the knob pressed.
    ToggleLock,
}

/// What pressing key `offset` (0..4) of a four-key lane means: every key
/// summons the lane; while the lane is answerable the three after the first
/// are Up, Down and Enter. This is the rule the lane pattern draws — the
/// steady marker first, the beating answer keys after it — so the F-row and
/// the numpad's top line both bind through it and cannot drift apart.
fn lane_press(lane: usize, offset: usize, answerable: bool) -> Press {
    match (answerable, offset) {
        (true, 1) => Press::Answer(lane, Key::Up),
        (true, 2) => Press::Answer(lane, Key::Down),
        (true, 3) => Press::Answer(lane, Key::Enter),
        _ => Press::Summon(lane),
    }
}

/// What pressing F-row key `index` (0 → F13) means with `lane_count` lanes,
/// the key's lane `answerable` or not. The lane is the key's group of four;
/// a key past the lanes the keyboard has is nothing.
pub fn press_of(index: usize, lane_count: usize, answerable: bool) -> Option<Press> {
    let lane = index / KEYS_PER_LANE;
    if lane >= lane_count.min(KEYBOARD_LANES) {
        return None;
    }
    Some(lane_press(lane, index % KEYS_PER_LANE, answerable))
}

/// What pressing numpad control `index` (0 → Ctrl+Shift+F13) means: the knob's
/// three, then M1–M5 summoning their lanes, then the top line's four keys
/// acting on `selected` — the agent it is showing — as one F-row lane: any of
/// them summons it, and while it is answerable the three after the first are
/// ⏶⏷Enter. With nothing shown the top line is nothing: no stray keystrokes.
pub fn numpad_press_of(
    index: usize,
    lane_count: usize,
    selected: Option<usize>,
    answerable: bool,
) -> Option<Press> {
    let shown = lane_count.min(crate::settings::NUMPAD_LANES);
    match index {
        0 => Some(Press::Select(-1)),
        1 => Some(Press::Select(1)),
        2 => Some(Press::ToggleLock),
        3..=7 => {
            let lane = index - 3;
            (lane < shown).then_some(Press::Summon(lane))
        }
        8..=11 => selected.map(|lane| lane_press(lane, index - 8, answerable)),
        _ => None,
    }
}

/// Handle to the running capture. Dropping it unregisters the hotkeys — which
/// is the only thing the held value is *for*, hence the underscore name.
pub struct Keys {
    #[cfg(windows)]
    _inner: windows_impl::Hotkeys,
}

/// Starts global capture of F13–F24 and the V0 controls. `None` when the F-row
/// hotkeys cannot be registered (or off Windows). V0 registration failures are
/// reported individually without disabling the working keys.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Option<Keys> {
    #[cfg(windows)]
    {
        let outcome = windows_impl::install(Arc::clone(&tracker));
        // A capture that silently failed to register looks exactly like a remap
        // nobody has done yet, so the outcome is recorded either way — in its
        // own field, where nothing later overwrites it.
        if let Ok(mut tracker) = tracker.lock() {
            tracker.keys_error = match &outcome {
                Ok((_, warnings)) if !warnings.is_empty() => Some(warnings.join("; ")),
                Err(error) => Some(error.clone()),
                _ => None,
            };
        }
        outcome.ok().map(|(inner, _)| Keys { _inner: inner })
    }
    #[cfg(not(windows))]
    {
        let _ = tracker;
        None
    }
}

/// One press, handled off the hotkey pump: record it, then do what it
/// means. An answer blocks on the raise and on verifying where the keyboard
/// is, which is why this runs on the worker and not the pump.
fn handle_press(tracker: &Arc<Mutex<Tracker>>, index: usize) {
    HANDLED.fetch_add(1, Ordering::Relaxed);
    if index >= SUMMON_KEYS {
        return handle_numpad(tracker, index - SUMMON_KEYS);
    }
    let press = {
        let Ok(mut tracker) = tracker.lock() else {
            return;
        };
        tracker.last_key = Some((index, crate::now_ms()));
        let lane = index / KEYS_PER_LANE;
        press_of(index, tracker.settings.lane_count, tracker.answerable(lane))
    };
    match press {
        Some(Press::Summon(lane)) => summon_lane(tracker, lane),
        Some(Press::Answer(lane, key)) => answer_lane(tracker, lane, key),
        // A key past the lanes: swallowed and recorded, nothing to do.
        _ => {}
    }
}

/// One numpad control, handled like [`handle_press`]: selection and lock are a
/// mutation under the lock and done; a summon or answer runs unlocked on this
/// same worker. An M key and a top-line key select what they summon — the user's
/// hand moves the cursor, always, locked or not.
fn handle_numpad(tracker: &Arc<Mutex<Tracker>>, index: usize) {
    let press = {
        let Ok(mut tracker) = tracker.lock() else {
            return;
        };
        tracker.last_key = Some((SUMMON_KEYS + index, crate::now_ms()));
        let answerable = tracker
            .selected
            .is_some_and(|lane| tracker.answerable(lane));
        match numpad_press_of(
            index,
            tracker.settings.lane_count,
            tracker.selected,
            answerable,
        ) {
            Some(Press::Select(delta)) => {
                tracker.select_delta(delta);
                None
            }
            Some(Press::ToggleLock) => {
                tracker.toggle_lock();
                None
            }
            Some(Press::Summon(lane)) => {
                tracker.select(lane);
                Some(Press::Summon(lane))
            }
            other => other,
        }
    };
    match press {
        Some(Press::Summon(lane)) => summon_lane(tracker, lane),
        Some(Press::Answer(lane, key)) => answer_lane(tracker, lane, key),
        _ => {}
    }
}

/// Finds where a lane's agent is, does `act` to it, and records how that
/// went for the status bar. The lock is held only to look the lane up: the
/// act itself — a raise can spend a quarter of a second on the terminal's
/// tab strip — runs with nothing held, which is also why callers with a
/// frame to keep call this on a thread of its own.
fn act_on_lane(
    tracker: &Arc<Mutex<Tracker>>,
    lane: usize,
    act: impl FnOnce(&[Ancestor], &[String]) -> String,
) {
    let target = {
        let Ok(tracker) = tracker.lock() else {
            return;
        };
        tracker.summon_target(lane)
    };
    let report = match target {
        Ok((ancestors, names)) => act(&ancestors, &names),
        Err(reason) => reason,
    };
    if let Ok(mut tracker) = tracker.lock() {
        tracker.summon = Some(report);
    }
}

/// Brings a lane's agent forward. The product's first action, shared by every
/// surface with a button: the F-row's lane keys and a Stream Deck's row keys
/// arrive here alike.
pub fn summon_lane(tracker: &Arc<Mutex<Tracker>>, lane: usize) {
    act_on_lane(tracker, lane, |ancestors, names| {
        crate::focus::raise(ancestors, names).detail
    });
}

/// Brings a lane's agent forward and answers it with one key — the product's
/// second action, from the F-row's or a Stream Deck's answer keys while the
/// lane is Waiting.
/// The key is sent only if the window that came forward verifiably has the
/// keyboard; otherwise the press has focused the lane, and the status bar
/// says what to do next.
pub fn answer_lane(tracker: &Arc<Mutex<Tracker>>, lane: usize, key: crate::focus::Key) {
    act_on_lane(tracker, lane, |ancestors, names| {
        let raise = crate::focus::raise(ancestors, names);
        match raise.window {
            Some(window) => match crate::focus::type_key(window, key) {
                Ok(typed) => format!("{} — {typed}", raise.detail),
                Err(why) => format!("{} — {why}", raise.detail),
            },
            None => raise.detail,
        }
    });
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::sync::{Arc, Mutex};

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
    };

    use super::{NUMPAD_CHORDS, NUMPAD_KEYS, NUMPAD_TOP_ROW, SUMMON_KEYS, VK_F13};
    use crate::tracker::Tracker;

    /// Every id this module may hold: the F-row's twelve, then the numpad's.
    const ALL_KEYS: usize = SUMMON_KEYS + NUMPAD_KEYS;

    /// The thread that owns the hotkeys, for [`unhook_now`]. Zero before it is
    /// ready and after it has unregistered them.
    static PUMP_THREAD: AtomicU32 = AtomicU32::new(0);

    const FIRST_HOTKEY_ID: i32 = 1;

    pub fn unhook_now() {
        let thread_id = PUMP_THREAD.load(Ordering::SeqCst);
        if thread_id != 0 {
            // SAFETY: posting only WM_QUIT to the thread id this module owns.
            let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }

    pub struct Hotkeys {
        thread_id: u32,
        pump: Option<std::thread::JoinHandle<()>>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Hotkeys {
        fn drop(&mut self) {
            // Break the message loop; the pump unregisters every hotkey on its
            // way out.
            // SAFETY: posting to the thread this struct started.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
            // The sender belonged to the pump, so joining it closes the
            // worker's receive loop.
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    struct Ready {
        thread_id: u32,
        warnings: Vec<String>,
    }

    pub fn install(tracker: Arc<Mutex<Tracker>>) -> Result<(Hotkeys, Vec<String>), String> {
        let (sender, receiver) = sync_channel::<usize>(64);

        // The install is reported back so a failure is a reason the caller can
        // show, not a thread that died where nobody was looking.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Ready, String>>();
        let pump = std::thread::Builder::new()
            .name("agent-frow-keys".to_owned())
            .spawn(move || pump_thread(&ready_tx, sender))
            .map_err(|error| format!("could not start the hotkey thread: {error}"))?;

        let Ready {
            thread_id,
            warnings,
        } = match ready_rx.recv() {
            Ok(Ok(ready)) => ready,
            outcome => {
                let _ = pump.join();
                return Err(match outcome {
                    Ok(Err(reason)) => reason,
                    _ => "the hotkey thread died before reporting".to_owned(),
                });
            }
        };

        let worker = match std::thread::Builder::new()
            .name("agent-frow-summon".to_owned())
            .spawn(move || {
                // Ends when the pump drops its sender during `Hotkeys::drop`.
                // A panic in
                // one summon must not kill the thread — a dead worker makes
                // every later press vanish without a trace, which is the worst
                // failure this feature can have — so each press is caught, and
                // a crash is reported where the user will see it.
                while let Ok(index) = receiver.recv() {
                    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        super::handle_press(&tracker, index);
                    }));
                    if caught.is_err()
                        && let Ok(mut tracker) = tracker.lock()
                    {
                        tracker.keys_error =
                            Some("a summon crashed — see ~/.agent-frow/panic.log".to_owned());
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                // SAFETY: asking the pump we just started to unregister and
                // exit before returning the startup failure.
                let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
                let _ = pump.join();
                return Err(format!("could not start the summon thread: {error}"));
            }
        };

        Ok((
            Hotkeys {
                thread_id,
                pump: Some(pump),
                worker: Some(worker),
            },
            warnings,
        ))
    }

    fn unregister_hotkeys(count: usize) {
        for index in 0..count {
            // SAFETY: unregistering ids owned by this calling thread.
            let _ = unsafe { UnregisterHotKey(None, FIRST_HOTKEY_ID + index as i32) };
        }
    }

    /// Registration order is the control index sent to the worker. There are
    /// exactly twelve F-row and twelve V0 bindings, with no legacy aliases.
    fn bindings() -> impl Iterator<Item = (HOT_KEY_MODIFIERS, u32)> {
        (0..SUMMON_KEYS)
            .map(|index| (MOD_NOREPEAT, VK_F13 + index as u32))
            .chain((0..NUMPAD_CHORDS).map(|index| {
                (
                    MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
                    VK_F13 + index as u32,
                )
            }))
            .chain(NUMPAD_TOP_ROW.map(|(vk, _)| (MOD_NOREPEAT, vk)))
    }

    fn register_hotkeys(
        mut register: impl FnMut(i32, HOT_KEY_MODIFIERS, u32) -> Result<(), String>,
        mut unregister: impl FnMut(i32),
    ) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        for (index, (modifiers, vk)) in bindings().enumerate() {
            let id = FIRST_HOTKEY_ID + index as i32;
            if let Err(error) = register(id, modifiers, vk) {
                let reason = format!("could not register {}: {error}", super::key_label(index));
                if index < SUMMON_KEYS {
                    for previous in 0..index {
                        unregister(FIRST_HOTKEY_ID + previous as i32);
                    }
                    return Err(reason);
                }
                // A V0 collision must not take down the F-row or other V0
                // controls, but it must be visible in the Keyboard panel.
                warnings.push(reason);
            }
        }
        Ok(warnings)
    }

    fn pump_thread(
        ready: &std::sync::mpsc::Sender<Result<Ready, String>>,
        sender: SyncSender<usize>,
    ) {
        let warnings = match register_hotkeys(
            // SAFETY: thread-owned registrations, each with a unique id.
            |id, modifiers, vk| {
                unsafe { RegisterHotKey(None, id, modifiers, vk) }
                    .map_err(|error| error.to_string())
            },
            |id| {
                // SAFETY: unregistering an id owned by this calling thread.
                let _ = unsafe { UnregisterHotKey(None, id) };
            },
        ) {
            Ok(warnings) => warnings,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };

        // SAFETY: this thread's own id.
        let thread_id = unsafe { GetCurrentThreadId() };
        PUMP_THREAD.store(thread_id, Ordering::SeqCst);
        if ready
            .send(Ok(Ready {
                thread_id,
                warnings,
            }))
            .is_err()
        {
            unregister_hotkeys(ALL_KEYS);
            PUMP_THREAD.store(0, Ordering::SeqCst);
            return;
        }

        // Thread-owned hotkeys arrive as WM_HOTKEY messages. Forward only the
        // small key index; the worker does every potentially blocking action.
        let mut message = MSG::default();
        // SAFETY: a standard message loop; exits when WM_QUIT arrives.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            if message.message == WM_HOTKEY {
                let id = message.wParam.0 as i32;
                if (FIRST_HOTKEY_ID..FIRST_HOTKEY_ID + ALL_KEYS as i32).contains(&id) {
                    let index = (id - FIRST_HOTKEY_ID) as usize;
                    super::RECEIVED.fetch_add(1, Ordering::Relaxed);
                    if sender.try_send(index).is_ok() {
                        super::QUEUED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        unregister_hotkeys(ALL_KEYS);
        let _ = PUMP_THREAD.compare_exchange(thread_id, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    mod tests {
        use super::*;

        #[test]
        fn launcher_keys_register_once_without_the_old_top_row_chords() {
            let keymap: serde_json::Value = serde_json::from_str(include_str!(
                "../../firmware/keychron-ultra/keymaps/keychron_v0_ultra_ansi.json"
            ))
            .unwrap();
            let bindings: Vec<_> = bindings().collect();
            assert_eq!(bindings.len(), 24);
            let unique: std::collections::BTreeSet<_> = bindings
                .iter()
                .map(|(modifiers, vk)| (modifiers.0, *vk))
                .collect();
            assert_eq!(unique.len(), bindings.len());
            for index in 0..SUMMON_KEYS {
                assert_eq!(bindings[index], (MOD_NOREPEAT, VK_F13 + index as u32));
            }
            let chord_modifiers = MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT;
            for index in 0..8 {
                assert_eq!(
                    bindings[SUMMON_KEYS + index],
                    (chord_modifiers, VK_F13 + index as u32)
                );
            }
            for old_vk in 0x84..=0x87 {
                assert!(!bindings.contains(&(chord_modifiers, old_vk)));
            }

            // Independent HID -> Windows mapping, checked on US layout 00000409.
            // Read the shipped Launcher file so an app/firmware mismatch fails.
            let hid_to_vk = [(135, 0xC1), (139, 0xEB), (140, 0xEA), (133, 0xC2)];
            for (offset, &(qmk, vk)) in hid_to_vk.iter().enumerate() {
                let key = keymap["keymap"][0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|key| key["row"] == 0 && key["col"] == offset + 1)
                    .unwrap();
                assert_eq!(key["val"], qmk);
                assert_eq!(bindings[20 + offset], (MOD_NOREPEAT, vk));
            }
        }

        #[test]
        fn a_v0_registration_failure_is_named_and_keeps_other_controls() {
            for failed_index in SUMMON_KEYS..ALL_KEYS {
                let mut registered = Vec::new();
                let mut unregistered = Vec::new();
                let warnings = register_hotkeys(
                    |id, _, _| {
                        if id == FIRST_HOTKEY_ID + failed_index as i32 {
                            Err("already registered".to_owned())
                        } else {
                            registered.push(id);
                            Ok(())
                        }
                    },
                    |id| unregistered.push(id),
                )
                .unwrap();
                assert_eq!(
                    warnings,
                    [format!(
                        "could not register {}: already registered",
                        crate::keys::key_label(failed_index)
                    )]
                );
                assert_eq!(registered.len(), ALL_KEYS - 1);
                for index in 0..SUMMON_KEYS {
                    assert!(registered.contains(&(FIRST_HOTKEY_ID + index as i32)));
                }
                assert!(unregistered.is_empty(), "working controls must remain live");
            }
        }

        #[test]
        fn an_f_row_registration_failure_cleans_up_before_reporting() {
            for failed_index in 0..SUMMON_KEYS {
                let mut registered = Vec::new();
                let mut unregistered = Vec::new();
                let error = register_hotkeys(
                    |id, _, _| {
                        if id == FIRST_HOTKEY_ID + failed_index as i32 {
                            Err("already registered".to_owned())
                        } else {
                            registered.push(id);
                            Ok(())
                        }
                    },
                    |id| unregistered.push(id),
                )
                .unwrap_err();
                assert_eq!(
                    error,
                    format!(
                        "could not register F{}: already registered",
                        13 + failed_index
                    )
                );
                assert_eq!(registered.len(), failed_index);
                assert_eq!(unregistered, registered);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_key_of_a_lane_summons_it_and_none_reaches_a_lane_off_the_keyboard() {
        // Whatever the lane count, the twelve keys are lanes 1–3, four each.
        for lane_count in crate::settings::LANE_COUNTS {
            for index in 0..SUMMON_KEYS {
                assert_eq!(
                    press_of(index, lane_count, false),
                    Some(Press::Summon(index / KEYS_PER_LANE)),
                    "{lane_count} lanes, key {index}"
                );
            }
        }
        assert_eq!(press_of(SUMMON_KEYS, 6, false), None, "past the F-row");
    }

    #[test]
    fn while_a_lane_answers_its_three_keys_after_the_first_are_up_down_enter() {
        assert_eq!(
            press_of(0, 3, true),
            Some(Press::Summon(0)),
            "the first key still summons"
        );
        assert_eq!(press_of(1, 3, true), Some(Press::Answer(0, Key::Up)));
        assert_eq!(press_of(2, 3, true), Some(Press::Answer(0, Key::Down)));
        assert_eq!(press_of(3, 3, true), Some(Press::Answer(0, Key::Enter)));
        assert_eq!(press_of(5, 3, true), Some(Press::Answer(1, Key::Up)));
        assert_eq!(press_of(11, 3, true), Some(Press::Answer(2, Key::Enter)));
        assert_eq!(
            press_of(7, 3, false),
            Some(Press::Summon(1)),
            "not answerable: a summon like any other key"
        );
    }

    #[test]
    fn keys_are_named_as_the_user_remapped_them() {
        assert_eq!(key_label(0), "F13");
        assert_eq!(key_label(11), "F24");
        assert_eq!(key_label(12), "Ctrl+Shift+F13", "the numpad's first chord");
        assert_eq!(key_label(19), "Ctrl+Shift+F20", "the numpad's last chord");
        assert_eq!(key_label(20), "Intl1");
        assert_eq!(key_label(21), "Intl5");
        assert_eq!(key_label(22), "Intl6");
        assert_eq!(key_label(23), "Keypad Comma");
        assert_eq!(key_label(24), "Unknown key");
        assert_eq!(lane_keys_label(0).as_deref(), Some("F13–F16"));
        assert_eq!(lane_keys_label(2).as_deref(), Some("F21–F24"));
        assert_eq!(lane_keys_label(3), None, "no keys past the keyboard");
    }

    #[test]
    fn the_numpad_controls_mean_the_owners_table() {
        assert_eq!(
            numpad_press_of(0, 5, Some(2), false),
            Some(Press::Select(-1))
        );
        assert_eq!(numpad_press_of(1, 5, None, false), Some(Press::Select(1)));
        assert_eq!(numpad_press_of(2, 5, None, false), Some(Press::ToggleLock));
        assert_eq!(
            numpad_press_of(3, 5, None, false),
            Some(Press::Summon(0)),
            "M1"
        );
        assert_eq!(
            numpad_press_of(7, 5, None, false),
            Some(Press::Summon(4)),
            "M5"
        );
        assert_eq!(
            numpad_press_of(7, 4, None, false),
            None,
            "M5 past the lanes"
        );
        assert_eq!(
            numpad_press_of(7, 6, None, false),
            Some(Press::Summon(4)),
            "six lanes still cap at five M keys"
        );
        assert_eq!(
            numpad_press_of(8, 5, Some(2), false),
            Some(Press::Summon(2)),
            "the top line's first key summons the shown agent"
        );
        assert_eq!(
            numpad_press_of(8, 5, Some(2), true),
            Some(Press::Summon(2)),
            "answerable or not"
        );
        assert_eq!(
            numpad_press_of(9, 5, Some(2), true),
            Some(Press::Answer(2, Key::Up))
        );
        assert_eq!(
            numpad_press_of(10, 5, Some(2), true),
            Some(Press::Answer(2, Key::Down))
        );
        assert_eq!(
            numpad_press_of(11, 5, Some(2), true),
            Some(Press::Answer(2, Key::Enter))
        );
        assert_eq!(
            numpad_press_of(10, 5, Some(2), false),
            Some(Press::Summon(2)),
            "not answerable: the answer keys summon, like the F-row's"
        );
        assert_eq!(
            numpad_press_of(11, 5, None, false),
            None,
            "nothing displayed"
        );
        assert_eq!(
            numpad_press_of(8, 5, None, true),
            None,
            "nothing displayed: the first key too"
        );
        assert_eq!(
            numpad_press_of(12, 5, Some(0), true),
            None,
            "past the controls"
        );
    }

    #[test]
    fn the_top_line_is_an_f_row_lane_for_the_shown_agent() {
        // Controls 8..12 on the shown lane mean exactly what F-row keys 4..8
        // mean on lane 1, answerable or not — one rule, two keyboards.
        for answerable in [false, true] {
            for offset in 0..KEYS_PER_LANE {
                assert_eq!(
                    numpad_press_of(8 + offset, 5, Some(1), answerable),
                    press_of(KEYS_PER_LANE + offset, 3, answerable),
                    "offset {offset}, answerable {answerable}"
                );
            }
        }
    }
}
