//! Where a scene becomes light.
//!
//! Each surface is a module here and nothing anywhere else: everything above
//! this line deals in lanes and states, never in LEDs or pixels. What the
//! surfaces share sits beside them — the [`palette`] that says what the
//! twelve keys look like, and the [`scene`] that decides when a frame is due.

pub mod corsair;
pub mod keychron;
pub mod keychron_v0;
pub mod monitor;
pub mod palette;
pub mod scene;
pub mod streamdeck;

/// Whether the user is driving `surface` at all — a device unticked in the
/// window is left alone. Read by each device thread once a loop, so a tick
/// takes effect within a frame.
pub fn enabled(tracker: &std::sync::Mutex<crate::tracker::Tracker>, surface: &str) -> bool {
    tracker
        .lock()
        .map(|tracker| tracker.settings.device_enabled(surface))
        .unwrap_or(true)
}
