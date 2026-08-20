//! The lighting thread: connects to iCUE, paints the F-row from the tracker,
//! and keeps checking that it is still allowed to.
//!
//! All FFI happens on this one thread. It is also the application's clock —
//! it evicts stale sessions every frame — so lanes stay honest while the
//! window is hidden in the tray, which is most of the time.

use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ffi::{self, *};
use super::palette;
use crate::state::State;
use crate::tracker::{KeyboardStatus, Tracker};

/// ~30 Hz. Fast enough that a 420 ms pulse looks like a pulse rather than a
/// stutter, slow enough to be free.
const FRAME: Duration = Duration::from_millis(33);

/// How long iCUE gets to accept a connection before we say so and try again.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to wait before trying again after a failure.
///
/// This is the fix for the bug that made the F-row go dark and stay dark: the
/// old surface checked the session once at startup and ignored every
/// set-colour result afterwards, so an iCUE restart, a sleep, or a device
/// unplugged left a thread painting into nothing, silently, until the
/// application was restarted.
const RETRY: Duration = Duration::from_secs(5);

static SESSION_STATE: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_state_changed(
    _context: *mut std::ffi::c_void,
    event: *const CorsairSessionStateChanged,
) {
    if !event.is_null() {
        // SAFETY: the SDK passes a valid pointer for the duration of the call.
        SESSION_STATE.store(unsafe { (*event).state }, Ordering::SeqCst);
    }
}

/// Handle to the running surface. Dropping it stops the thread and gives the
/// keyboard back.
pub struct Surface {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the lighting. Returns immediately; everything happens on the thread,
/// and a machine with no iCUE simply gets a sentence in the window.
pub fn start(tracker: Arc<Mutex<Tracker>>) -> Surface {
    let running = Arc::new(AtomicBool::new(true));
    let thread_running = Arc::clone(&running);
    let handle = std::thread::Builder::new()
        .name("agent-frow-keyboard".to_owned())
        .spawn(move || render(tracker, thread_running))
        .ok();
    Surface { running, handle }
}

fn report(tracker: &Mutex<Tracker>, status: KeyboardStatus) {
    if let Ok(mut tracker) = tracker.lock() {
        tracker.keyboard = status;
    }
}

/// A keyboard we are painting.
struct Device {
    id: [c_char; 128],
    /// Which of our twelve LEDs this keyboard actually has.
    available: Vec<u32>,
    model: String,
}

fn render(tracker: Arc<Mutex<Tracker>>, running: Arc<AtomicBool>) {
    let sdk = match ffi::load() {
        Ok(sdk) => sdk,
        Err(reason) => {
            // Not an error to shout about: plenty of machines have no Corsair
            // keyboard. But a silent dark row is indistinguishable from a
            // broken application, so it says which it is.
            report(
                &tracker,
                KeyboardStatus::unavailable(format!(
                    "{reason}. Install the app so {} sits beside it.",
                    ffi::DLL_NAME
                )),
            );
            return;
        }
    };

    let start = Instant::now();
    let mut device: Option<Device> = None;
    let mut next_attempt = Instant::now();
    // What the keys were last told, so a motionless board is written once,
    // not thirty times a second. `dirty` forces the first frame after any
    // (re)connect; the settings clone is kept so it is re-made only when the
    // settings actually change — it carries every lane-name String.
    let mut last_states: Vec<Option<State>> = Vec::new();
    let mut settings_cache: Option<crate::settings::Settings> = None;
    let mut dirty = true;

    while running.load(Ordering::SeqCst) {
        // The clock. Doing it here rather than only when the window draws is
        // what keeps a lane from lingering on the keyboard for as long as the
        // window happens to be closed.
        if let Ok(mut tracker) = tracker.lock() {
            tracker.sweep(crate::now_ms());
        }

        let Some(ready) = &device else {
            if Instant::now() < next_attempt {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            match connect(&sdk, &running) {
                Ok(ready) => {
                    report(&tracker, KeyboardStatus::connected(&ready));
                    device = Some(ready);
                    dirty = true;
                }
                Err(reason) => {
                    report(&tracker, KeyboardStatus::unavailable(reason));
                    let _ = sdk.disconnect();
                    next_attempt = Instant::now() + RETRY;
                }
            }
            continue;
        };

        // Both of these used to be ignored, which is the whole reason this
        // loop has a `device: Option` at all.
        if SESSION_STATE.load(Ordering::SeqCst) != CSS_CONNECTED {
            report(
                &tracker,
                KeyboardStatus::unavailable("iCUE dropped the connection; reconnecting".to_owned()),
            );
            let _ = sdk.disconnect();
            device = None;
            next_attempt = Instant::now() + RETRY;
            continue;
        }

        let states = match tracker.lock() {
            Ok(mut tracker) => {
                // A preview overrides every lane while it lasts, and this
                // thread is what retires it — the window may be closed, and a
                // preview that outlives anyone looking at it should still die.
                let now = crate::now_ms();
                if tracker
                    .preview
                    .is_some_and(|preview| preview.expires_at <= now)
                {
                    tracker.preview = None;
                }
                let states = match tracker.preview {
                    Some(preview) => vec![Some(preview.state); tracker.settings.lane_count],
                    None => (0..tracker.settings.lane_count)
                        .map(|lane| {
                            tracker
                                .on_lane(lane)
                                .map(|session| session.effective_state())
                        })
                        .collect::<Vec<Option<State>>>(),
                };
                if settings_cache
                    .as_ref()
                    .is_none_or(|cached| *cached != tracker.settings)
                {
                    settings_cache = Some(tracker.settings.clone());
                    dirty = true;
                }
                states
            }
            Err(_) => break,
        };
        if states != last_states {
            last_states = states.clone();
            dirty = true;
        }
        // A motionless, unchanged board needs no frame. The wake stays at
        // FRAME so the sweep above keeps being the application's clock.
        if !dirty && !palette::animated(&states) {
            std::thread::sleep(FRAME);
            continue;
        }
        let Some(settings) = settings_cache.as_ref() else {
            std::thread::sleep(FRAME);
            continue;
        };

        let elapsed = start.elapsed().as_millis() as u64;
        let colors: Vec<CorsairLedColor> =
            palette::frame(&states, settings, elapsed, &ready.available)
                .into_iter()
                .map(|(id, color)| CorsairLedColor {
                    id,
                    r: color.r,
                    g: color.g,
                    b: color.b,
                    a: 255,
                })
                .collect();
        dirty = false;
        if sdk.set_led_colors(&ready.id, &colors) != CE_SUCCESS {
            report(
                &tracker,
                KeyboardStatus::unavailable("the keyboard stopped accepting colours".to_owned()),
            );
            let _ = sdk.disconnect();
            device = None;
            next_attempt = Instant::now() + RETRY;
            continue;
        }

        std::thread::sleep(FRAME);
    }

    let _ = sdk.disconnect();
    report(&tracker, KeyboardStatus::default());
}

fn connect(sdk: &Sdk, running: &AtomicBool) -> Result<Device, String> {
    if sdk.connect(on_state_changed) != CE_SUCCESS {
        return Err("could not reach iCUE — is it running?".to_owned());
    }
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while SESSION_STATE.load(Ordering::SeqCst) != CSS_CONNECTED {
        if !running.load(Ordering::SeqCst) {
            return Err("stopping".to_owned());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "iCUE did not accept the connection within {}s — is third-party control enabled?",
                CONNECT_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let device = find_keyboard(sdk).ok_or("iCUE reports no keyboard")?;
    // Sit above the user's own lighting without taking it away from them.
    let _ = sdk.set_layer_priority(SDK_LAYER_PRIORITY);
    Ok(device)
}

fn find_keyboard(sdk: &Sdk) -> Option<Device> {
    let filter = CorsairDeviceFilter {
        device_type_mask: CDT_KEYBOARD,
    };
    // SAFETY: a plain C struct array with no invalid bit patterns; the SDK
    // fills as many entries as it reports.
    let mut devices: [CorsairDeviceInfo; 8] = unsafe { std::mem::zeroed() };
    let (code, count) = sdk.devices(&filter, &mut devices);
    if code != CE_SUCCESS || count == 0 {
        return None;
    }
    let info = &devices[0];

    let max = info.led_count.max(1) as usize;
    let mut positions = vec![
        CorsairLedPosition {
            id: 0,
            cx: 0.0,
            cy: 0.0
        };
        max
    ];
    let (code, found) = sdk.led_positions(&info.id, &mut positions);
    if code != CE_SUCCESS {
        return None;
    }
    positions.truncate(found.max(0) as usize);
    let reported: Vec<u32> = positions.iter().map(|position| position.id).collect();

    Some(Device {
        id: info.id,
        // Ours, intersected with what this keyboard has. Nothing outside this
        // list is ever written.
        available: palette::our_led_ids()
            .filter(|id| reported.contains(id))
            .collect(),
        model: c_string(&info.model),
    })
}

fn c_string(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl KeyboardStatus {
    fn connected(device: &Device) -> Self {
        let driven = device.available.len();
        Self {
            connected: true,
            driven,
            detail: if driven == palette::KEYS {
                format!("{}: driving the {driven} F-row keys", device.model)
            } else {
                format!(
                    "{}: only {driven} of the {} F-row keys exist here, so the lanes are incomplete",
                    device.model,
                    palette::KEYS
                )
            },
        }
    }

    fn unavailable(detail: String) -> Self {
        Self {
            connected: false,
            driven: 0,
            detail,
        }
    }
}
