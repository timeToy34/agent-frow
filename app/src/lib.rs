//! Agent F-Row — the pieces the binary and its tests both use.
//!
//! Milestone 1 is agent detection, hook installation, and receiving events;
//! milestone 2 is reading those events as state. The lighting (3) and focus (4)
//! join here as they are built.

pub mod agents;
pub mod autostart;
pub mod event;
pub mod focus;
pub mod gauges;
pub mod icon;
pub mod ingress;
pub mod install;
pub mod keys;
pub mod lastseen;
pub mod paths;
pub mod settings;
pub mod state;
pub mod surface;
pub mod tracker;
pub mod ui;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}
