//! Keychron Ultra keyboards, through the raw-HID protocol their Launcher
//! already speaks — no firmware of ours on the keyboard, no driver on the
//! machine.
//!
//! Layers, bottom up: [`protocol`] is the bytes, pure; [`hid`] moves them;
//! [`session`] is one keyboard from handshake to hand-back, written against a
//! transport so it is tested against a scripted keyboard; [`surface`] is the
//! thread that runs it from the scene.
//!
//! One thing the protocol cannot do over Bluetooth: the firmware serves this
//! interface on the cable and through the 2.4 GHz receiver only. The summon
//! keys, being ordinary keycodes in the keyboard's own keymap, work on all
//! three.

pub mod hid;
pub mod protocol;
pub mod session;
pub mod surface;

pub use surface::{Surface, restore_now, start};

/// What `doctor` says about the Keychron side: every Launcher interface on
/// the bus, and what each answers. Read-only — the keyboard is left exactly
/// as found.
pub fn probe() -> Result<Vec<String>, String> {
    let found = hid::find()?;
    if found.is_empty() {
        return Ok(vec![
            "no Keychron Ultra on the cable or the receiver (Bluetooth cannot carry the lighting)"
                .to_owned(),
        ]);
    }
    let mut lines = Vec::new();
    for candidate in &found {
        lines.push(format!(
            "{} over {} (pid {:04X})",
            candidate.product,
            candidate.link(),
            candidate.product_id
        ));
        let transport = match hid::open(candidate) {
            Ok(transport) => transport,
            Err(error) => {
                lines.push(format!("  could not open: {error}"));
                continue;
            }
        };
        let mut board = match session::Board::connect(transport) {
            Ok(board) => board,
            Err(error) => {
                lines.push(format!("  no handshake: {error}"));
                continue;
            }
        };
        lines.push(format!(
            "  firmware {}, {} LEDs, F-row at {:?}",
            board.firmware, board.led_count, board.leds
        ));
        match board.snapshot() {
            Ok(snapshot) => lines.push(format!(
                "  effect {} (0 off, 23 per-key, 24 mixed), brightness {}, per-key type {}",
                snapshot.effect, snapshot.brightness, snapshot.per_key_type
            )),
            Err(error) => lines.push(format!("  could not read its state: {error}")),
        }
        if let Ok(true) = board.is_ours() {
            lines.push("  set up by Agent F-Row (not handed back yet)".to_owned());
        }
    }
    Ok(lines)
}
