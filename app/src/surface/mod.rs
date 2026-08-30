//! Where a scene becomes light.
//!
//! Each surface is a module here and nothing anywhere else: everything above
//! this line deals in lanes and states, never in LEDs or pixels. What the
//! surfaces share sits beside them — the [`palette`] that says what the
//! twelve keys look like, and the [`scene`] that decides when a frame is due.

pub mod corsair;
pub mod keychron;
pub mod monitor;
pub mod palette;
pub mod scene;
pub mod streamdeck;
