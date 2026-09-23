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
mod console;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod journal;
#[cfg(windows)]
mod uia_tabs;
#[cfg(windows)]
mod window;

/// A foreground agent's identity, shared by every focus/answer entry point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusTarget {
    pub source: String,
    pub session_id: String,
    pub terminal_id: Option<String>,
    pub ancestors: Vec<crate::event::Ancestor>,
    pub custom_name: Option<String>,
    pub project: Option<String>,
}

/// How well a focus request went, in the user's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub raised: bool,
    /// The top-level window that came forward, when one did — what a
    /// keystroke may then be sent to, once it is verified to have the
    /// keyboard.
    pub window: Option<isize>,
    /// The selected tab's live UIA identity. None for a non-tabbed host.
    pub tab_id: Option<Vec<i32>>,
    /// A raised window alone is not permission to send an answer.
    pub target_selected: bool,
    pub method: &'static str,
    pub detail: String,
}

impl Report {
    fn failed(detail: impl Into<String>) -> Self {
        Self {
            raised: false,
            window: None,
            tab_id: None,
            target_selected: false,
            method: "unresolved",
            detail: detail.into(),
        }
    }

    #[cfg(windows)]
    fn raised(window: isize, detail: impl Into<String>) -> Self {
        Self {
            raised: true,
            window: Some(window),
            tab_id: None,
            target_selected: true,
            method: "host",
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

/// Raises the agent's host and resolves its exact terminal tab automatically.
/// A custom lane name is optional; a window-only result cannot receive keys.
#[cfg(windows)]
pub fn raise(target: &FocusTarget) -> Report {
    let report = window::raise(target);
    journal::record(target, &report);
    report
}

#[cfg(not(windows))]
pub fn raise(_target: &FocusTarget) -> Report {
    Report::failed("focus is a Windows facility")
}

/// Recheck the exact tab immediately before the existing input safeguards.
pub fn answer(report: &Report, key: Key) -> Result<String, String> {
    if !report.target_selected {
        return Err("the agent tab was not identified — no answer sent".to_owned());
    }
    let Some(window) = report.window else {
        return Err("no agent window selected".to_owned());
    };
    #[cfg(windows)]
    if let Some(id) = &report.tab_id {
        use windows::Win32::Foundation::HWND;
        if !uia_tabs::tabs(HWND(window as *mut core::ffi::c_void))
            .iter()
            .any(|tab| tab.selected && &tab.id == id)
        {
            return Err("the selected tab changed — no answer sent".to_owned());
        }
    }
    type_key(window, key)
}

/// Internal read-only helper; never called by an agent's hook registration.
pub fn probe_console_command(args: &[&str]) -> Result<(), String> {
    #[cfg(windows)]
    {
        console::command(args)
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        Err("console probing is a Windows facility".to_owned())
    }
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

    #[test]
    fn a_raised_window_without_an_identified_tab_cannot_receive_an_answer() {
        let report = Report {
            raised: true,
            window: Some(123),
            tab_id: None,
            target_selected: false,
            method: "unresolved",
            detail: String::new(),
        };
        for key in [Key::Up, Key::Down, Key::Enter] {
            assert!(answer(&report, key).unwrap_err().contains("no answer sent"));
        }
    }
}
