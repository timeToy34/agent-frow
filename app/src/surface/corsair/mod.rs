//! Corsair keyboards, through the iCUE SDK.

pub mod ffi;
pub mod palette;
pub mod surface;

pub use surface::{Surface, start};
