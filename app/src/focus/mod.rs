//! Bringing the window an agent runs in forward — a terminal, a desktop app,
//! or an IDE.
//!
//! **The only action in this product.** Everything else is display: the app
//! never sends anything to an agent, never approves anything, never captures a
//! key. Clicking a lane raises a window, and that is the whole of it.
//!
//! Two documented Windows facilities and no window-title guessing:
//!
//! - **The hook reports its own Windows ancestry**, each ancestor as a pid and
//!   the exe name that pid had at event time; the nearest ancestor whose pid
//!   still resolves to its recorded name and owns a real window is the host.
//!   This is what a WSL agent could never have before: its process id means
//!   nothing to Windows, but our hook runs Windows-side through interop, so
//!   the chain it reports is real —
//!   `powershell.exe → wsl.exe → wsl.exe → WindowsTerminal.exe`.
//! - **UI Automation** to select a tab, because a terminal window's title is
//!   whichever tab is in front, so three agents sharing one window are
//!   indistinguishable to every window-level API Windows offers. The same
//!   tabs also choose the *window*: Windows Terminal hosts every window in one
//!   process (that is what lets a tab be dragged out into its own window), so
//!   identity finds the process, and the window holding the tab is the one
//!   raised.
//!
//! What each attempt actually achieved is reported rather than assumed. "The
//! window came forward showing the wrong agent" is a different outcome from
//! "the right tab is in front", and the user can see which they got.

#[cfg(windows)]
mod uia_tabs;
#[cfg(windows)]
mod window;

/// How well a focus request went, in the user's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub raised: bool,
    pub detail: String,
}

impl Report {
    fn new(raised: bool, detail: impl Into<String>) -> Self {
        Self {
            raised,
            detail: detail.into(),
        }
    }
}

/// Raises the host window that ran `ancestors`, and — when the host is a
/// terminal — puts `tab_names` in front of it if one of them is a tab there.
///
/// `tab_names` are tried in order — the lane's name first, since that is the
/// one the user chose and the one they can make match. Nothing here guesses
/// from a window title: a name that matches nothing leaves the window raised
/// and says the tab was not selected.
#[cfg(windows)]
pub fn raise(ancestors: &[crate::event::Ancestor], tab_names: &[String]) -> Report {
    window::raise(ancestors, tab_names)
}

#[cfg(not(windows))]
pub fn raise(_ancestors: &[crate::event::Ancestor], _tab_names: &[String]) -> Report {
    Report::new(false, "focus is a Windows facility")
}
