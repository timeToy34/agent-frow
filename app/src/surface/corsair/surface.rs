//! The lighting thread: connects to iCUE, paints the F-row from the scene,
//! and keeps checking that it is still allowed to.
//!
//! All FFI happens on this one thread. Through [`Scene`] it is also one of the
//! application's clocks — it evicts stale sessions every frame — so lanes stay
//! honest while the window is hidden in the tray, which is most of the time.

use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ffi::{self, *};
use crate::settings::KEYS;
use crate::surface::palette;
use crate::surface::scene::Scene;
use crate::tracker::{KeyboardStatus, Tracker};

/// How this surface names itself in the window.
const SURFACE: &str = "Corsair";

/// The F-row LEDs, `CLK_F1` = 2 through `CLK_F12` = 13, contiguous.
const F_ROW_FIRST_LUID: u32 = 2;

/// The SDK's id for the nth key of the F-row — the one place the palette's
/// key index becomes something Corsair-shaped.
fn luid(key: usize) -> u32 {
    F_ROW_FIRST_LUID + key as u32
}

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
        tracker.report_keyboard(status);
    }
}

fn unavailable(detail: impl Into<String>) -> KeyboardStatus {
    KeyboardStatus::unavailable(SURFACE, detail.into())
}

/// A keyboard we are painting.
struct Device {
    id: [c_char; 128],
    /// Which of our twelve keys this keyboard actually has, by F-row position.
    available: Vec<usize>,
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
                unavailable(format!(
                    "{reason}. Install the app so {} sits beside it.",
                    ffi::DLL_NAME
                )),
            );
            return;
        }
    };

    let mut scene = Scene::new();
    let mut device: Option<Device> = None;
    let mut next_attempt = Instant::now();

    while running.load(Ordering::SeqCst) {
        // The clock, keyboard or no keyboard. Doing it here rather than only
        // when the window draws is what keeps a lane from lingering on the
        // keyboard for as long as the window happens to be closed.
        let Ok(frame) = scene.tick(&tracker, crate::now_ms()) else {
            break;
        };

        let Some(ready) = &device else {
            if Instant::now() < next_attempt {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            match connect(&sdk, &running) {
                Ok(ready) => {
                    report(
                        &tracker,
                        KeyboardStatus::connected(SURFACE, &ready.model, ready.available.len()),
                    );
                    device = Some(ready);
                    scene.invalidate();
                }
                Err(reason) => {
                    report(&tracker, unavailable(reason));
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
                unavailable("iCUE dropped the connection; reconnecting"),
            );
            let _ = sdk.disconnect();
            device = None;
            next_attempt = Instant::now() + RETRY;
            continue;
        }

        // A motionless, unchanged board needs no frame. The wake stays at
        // FRAME so the tick above keeps being the application's clock.
        let Some(frame) = frame else {
            std::thread::sleep(FRAME);
            continue;
        };

        let colors: Vec<CorsairLedColor> = palette::frame(
            frame.states,
            frame.settings,
            frame.elapsed_ms,
            &ready.available,
        )
        .into_iter()
        .map(|(key, color)| CorsairLedColor {
            id: luid(key),
            r: color.r,
            g: color.g,
            b: color.b,
            a: 255,
        })
        .collect();
        if sdk.set_led_colors(&ready.id, &colors) != CE_SUCCESS {
            report(
                &tracker,
                unavailable("the keyboard stopped accepting colours"),
            );
            let _ = sdk.disconnect();
            device = None;
            next_attempt = Instant::now() + RETRY;
            continue;
        }

        std::thread::sleep(FRAME);
    }

    let _ = sdk.disconnect();
    report(&tracker, KeyboardStatus::searching(SURFACE));
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
        available: (0..KEYS)
            .filter(|key| reported.contains(&luid(*key)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_index_maps_onto_the_contiguous_f_row_luids() {
        assert_eq!(luid(0), 2, "CLK_F1");
        assert_eq!(luid(KEYS - 1), 13, "CLK_F12");
    }
}
