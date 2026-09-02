//! The Keychron V0 Ultra numpad: a second Keychron surface, on the same
//! protocol the Ultra speaks, with a different shape — the four top shape
//! keys are a **top line** showing one agent in the classic four-key lane
//! patterns, and M1–M5 are an **agent column**, one key per lane. The knob
//! and the keys arrive as Ctrl+Shift chords through [`crate::keys`]; this
//! module only paints.
//!
//! Everything below the surface thread is borrowed from [`super::keychron`]:
//! same protocol, same transport, same session logic under the V0 geometry.

pub mod surface;

pub use surface::{Surface, restore_now, start};

/// What `doctor` says about the numpad: every Launcher interface on the bus,
/// and whether it answers as a V0 Ultra. Read-only.
pub fn probe() -> Result<Vec<String>, String> {
    use crate::surface::keychron::{hid, session};
    let found = hid::find()?;
    if found.is_empty() {
        return Ok(vec![
            "no Keychron on the cable or a receiver (Bluetooth cannot carry the lighting)"
                .to_owned(),
        ]);
    }
    let mut lines = Vec::new();
    for candidate in &found {
        let transport = match hid::open(candidate) {
            Ok(transport) => transport,
            Err(error) => {
                lines.push(format!("{}: could not open: {error}", candidate.product));
                continue;
            }
        };
        match session::Board::connect_with(transport, &session::V0_ULTRA) {
            Ok(board) => lines.push(format!(
                "{} over {} (pid {:04X}): firmware {}, the nine at {:?}",
                candidate.product,
                candidate.link(),
                candidate.product_id,
                board.firmware,
                board.leds
            )),
            Err(error) => lines.push(format!("{}: not a V0 Ultra — {error}", candidate.product)),
        }
    }
    Ok(lines)
}
