//! The summon keys: press the leftmost key of a lane and its agent comes
//! forward.
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
//! replaces the key-up bookkeeping the hook used to need.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

/// (F13–F24 hotkeys received, queued to the worker, handled).
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
pub const SUMMON_KEYS: usize = 12;

/// The label a key index carries in the window: 0 → "F13".
pub fn key_label(index: usize) -> String {
    format!("F{}", 13 + index)
}

/// Which lane a key index summons under `keys_per_lane`, if it is a lane's
/// leftmost key — the marker key, the one lit at 100%.
pub fn lane_of(index: usize, keys_per_lane: usize, lane_count: usize) -> Option<usize> {
    let kpl = keys_per_lane.max(1);
    if !index.is_multiple_of(kpl) {
        return None;
    }
    let lane = index / kpl;
    (lane < lane_count).then_some(lane)
}

/// Handle to the running capture. Dropping it unregisters the hotkeys — which
/// is the only thing the held value is *for*, hence the underscore name.
pub struct Keys {
    #[cfg(windows)]
    _inner: windows_impl::Hotkeys,
}

/// Starts global capture of F13–F24. `None` when the hotkeys cannot be registered
/// (or off Windows) — the buttons in the window still work, and the Keyboard
/// panel says the keys do not.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Option<Keys> {
    #[cfg(windows)]
    {
        let outcome = windows_impl::install(Arc::clone(&tracker));
        // A capture that silently failed to register looks exactly like a remap
        // nobody has done yet, so the outcome is recorded either way — in its
        // own field, where nothing later overwrites it.
        if let Ok(mut tracker) = tracker.lock() {
            tracker.keys_error = outcome.as_ref().err().cloned();
        }
        outcome.ok().map(|inner| Keys { _inner: inner })
    }
    #[cfg(not(windows))]
    {
        let _ = tracker;
        None
    }
}

/// One press, handled off the hotkey pump: record it, and if it is a lane's
/// marker key, summon that lane's agent.
fn handle_press(tracker: &Arc<Mutex<Tracker>>, index: usize) {
    HANDLED.fetch_add(1, Ordering::Relaxed);
    let target = {
        let Ok(mut tracker) = tracker.lock() else {
            return;
        };
        let now = crate::now_ms();
        tracker.last_key = Some((index, now));
        let settings = &tracker.settings;
        let Some(lane) = lane_of(index, settings.keys_per_lane(), settings.lane_count) else {
            // Not a marker key. Swallowed and recorded, nothing to do.
            return;
        };
        tracker.summon_target(lane)
    };
    let report = match target {
        Ok((ancestors, names)) => crate::focus::raise(&ancestors, &names).detail,
        Err(reason) => reason,
    };
    if let Ok(mut tracker) = tracker.lock() {
        tracker.summon = Some(report);
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::sync::{Arc, Mutex};

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
    };

    use super::{SUMMON_KEYS, VK_F13};
    use crate::tracker::Tracker;

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

    pub fn install(tracker: Arc<Mutex<Tracker>>) -> Result<Hotkeys, String> {
        let (sender, receiver) = sync_channel::<usize>(64);

        // The install is reported back so a failure is a reason the caller can
        // show, not a thread that died where nobody was looking.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let pump = std::thread::Builder::new()
            .name("agent-frow-keys".to_owned())
            .spawn(move || pump_thread(&ready_tx, sender))
            .map_err(|error| format!("could not start the hotkey thread: {error}"))?;

        let thread_id = match ready_rx.recv() {
            Ok(Ok(thread_id)) => thread_id,
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

        Ok(Hotkeys {
            thread_id,
            pump: Some(pump),
            worker: Some(worker),
        })
    }

    fn unregister_hotkeys(count: usize) {
        for index in 0..count {
            // SAFETY: unregistering ids owned by this calling thread.
            let _ = unsafe { UnregisterHotKey(None, FIRST_HOTKEY_ID + index as i32) };
        }
    }

    fn pump_thread(
        ready: &std::sync::mpsc::Sender<Result<u32, String>>,
        sender: SyncSender<usize>,
    ) {
        for index in 0..SUMMON_KEYS {
            let id = FIRST_HOTKEY_ID + index as i32;
            let vk = VK_F13 + index as u32;
            // SAFETY: thread-owned registration with a unique id and one
            // modifier-free virtual key from F13 through F24.
            if let Err(error) = unsafe { RegisterHotKey(None, id, MOD_NOREPEAT, vk) } {
                unregister_hotkeys(index);
                let _ = ready.send(Err(format!(
                    "could not register F{} as a summon key: {error}",
                    13 + index
                )));
                return;
            }
        }
        // SAFETY: this thread's own id.
        let thread_id = unsafe { GetCurrentThreadId() };
        PUMP_THREAD.store(thread_id, Ordering::SeqCst);
        if ready.send(Ok(thread_id)).is_err() {
            unregister_hotkeys(SUMMON_KEYS);
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
                if (FIRST_HOTKEY_ID..FIRST_HOTKEY_ID + SUMMON_KEYS as i32).contains(&id) {
                    let index = (id - FIRST_HOTKEY_ID) as usize;
                    super::RECEIVED.fetch_add(1, Ordering::Relaxed);
                    if sender.try_send(index).is_ok() {
                        super::QUEUED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        unregister_hotkeys(SUMMON_KEYS);
        let _ = PUMP_THREAD.compare_exchange(thread_id, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lanes_leftmost_key_summons_it_in_every_layout() {
        // (keys_per_lane, lane_count) for the three layouts.
        for (kpl, lanes) in [(4, 3), (3, 4), (2, 6)] {
            for index in 0..SUMMON_KEYS {
                let expected = if index % kpl == 0 && index / kpl < lanes {
                    Some(index / kpl)
                } else {
                    None
                };
                assert_eq!(
                    lane_of(index, kpl, lanes),
                    expected,
                    "{kpl}x{lanes} key {index}"
                );
            }
        }
    }

    #[test]
    fn keys_are_named_as_the_user_remapped_them() {
        assert_eq!(key_label(0), "F13");
        assert_eq!(key_label(11), "F24");
    }
}
