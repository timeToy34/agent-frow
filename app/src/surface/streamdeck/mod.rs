//! An Elgato Stream Deck: one row per lane, the row lit like the F-row with
//! the lane's name on its first key and its state on its last, every key a
//! summon — and, while the lane is Waiting, the three keys between as Up,
//! Down and Enter.
//!
//! The one surface that is taken whole rather than shared. A keyboard has
//! keys of its own beside ours; a deck has only the keys, and the Elgato app
//! repaints every one from its own profile and reads every press, so the
//! two cannot coexist. The surface opens the deck only while that app is not
//! running, hands it back — to its logo — the moment it is, and on the way
//! out.

pub mod canvas;
pub mod device;
pub mod surface;

pub use surface::{Surface, restore_now, start};

/// What `doctor` says: every deck on the bus, and whether its own software
/// has it. Read-only — nothing is opened — so it is safe while the app runs.
pub fn probe() -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    if device::elgato_app_running() {
        lines.push("the Stream Deck app is running; the deck is its while it is".to_owned());
    }
    let found = device::find()?;
    if found.is_empty() {
        lines.push("no Stream Deck on USB".to_owned());
    }
    lines.extend(found.iter().map(device::Found::describe));
    Ok(lines)
}
