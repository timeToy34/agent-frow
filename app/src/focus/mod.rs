//! Bringing the window an agent runs in forward — a terminal, a desktop app,
//! or an IDE.
//!
//! **The product's two actions.** Everything else is display: the app never
//! answers a hook, never approves anything, never captures a key. Clicking a
//! lane raises a window; and the answer keys — a lane's three after its
//! first on the F-row, a row's middle three on a Stream Deck — while the
//! lane is Waiting, raise the window and then send one Up, Down or Enter —
//! only into a window that verifiably has the keyboard, never to gain it.
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
    /// The top-level window that came forward, when one did — what a
    /// keystroke may then be sent to, once it is verified to have the
    /// keyboard.
    pub window: Option<isize>,
    pub detail: String,
}

impl Report {
    fn failed(detail: impl Into<String>) -> Self {
        Self {
            raised: false,
            window: None,
            detail: detail.into(),
        }
    }

    #[cfg(windows)]
    fn raised(window: isize, detail: impl Into<String>) -> Self {
        Self {
            raised: true,
            window: Some(window),
            detail: detail.into(),
        }
    }
}

/// The one keystroke a surface may send: an answer to a question the agent
/// is asking, pressed by the user on a key that says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
}

impl Key {
    pub fn name(self) -> &'static str {
        match self {
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Enter => "Enter",
        }
    }
}

/// Whether a keystroke may go out, given what was verified: the target is
/// the foreground window, and — for a terminal with tabs — keyboard focus
/// is not on its tab strip, where arrows switch tabs instead of reaching
/// the agent. `None` for the tab strip is "could not tell", and a keystroke
/// whose destination cannot be told is not sent; the user is told instead.
pub fn ready_to_type(
    foreground_is_target: bool,
    on_tab_strip: Option<bool>,
) -> Result<(), &'static str> {
    if !foreground_is_target {
        return Err("brought forward, but the keyboard is elsewhere — press again");
    }
    match on_tab_strip {
        Some(false) => Ok(()),
        Some(true) => Err("focus is on the terminal's tab strip — click into the terminal"),
        None => Err("could not tell where the keyboard is — click into the terminal"),
    }
}

/// Sends `key` to `window`, which a [`raise`] just reported, after verifying
/// it has the keyboard. `Ok` says what went where; `Err` says why nothing
/// was sent, in words for the status bar.
#[cfg(windows)]
pub fn type_key(window: isize, key: Key) -> Result<String, String> {
    window::type_key(window, key)
}

#[cfg(not(windows))]
pub fn type_key(_window: isize, _key: Key) -> Result<String, String> {
    Err("typing is a Windows facility".to_owned())
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
    Report::failed("focus is a Windows facility")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_keystroke_needs_the_foreground_and_not_the_tab_strip() {
        assert_eq!(ready_to_type(true, Some(false)), Ok(()));
        assert!(
            ready_to_type(false, Some(false))
                .unwrap_err()
                .contains("press again")
        );
        assert!(
            ready_to_type(true, Some(true))
                .unwrap_err()
                .contains("tab strip")
        );
        assert!(
            ready_to_type(true, None)
                .unwrap_err()
                .contains("could not tell")
        );
        assert!(
            ready_to_type(false, None)
                .unwrap_err()
                .contains("press again"),
            "the foreground is the first question"
        );
    }
}
