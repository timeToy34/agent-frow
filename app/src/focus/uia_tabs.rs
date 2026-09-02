//! Reading and selecting Windows Terminal tabs through UI Automation.
//!
//! **The only file in this project that touches the accessibility tree or COM,
//! and nothing depends on it working.** Every entry point degrades to empty or
//! `false`, and the caller falls back to having raised a window, which is what
//! it could do before this module existed.
//!
//! It exists because a terminal window's title is whichever tab is in front, so
//! three agents sharing one window are invisible to every window-level API
//! Windows offers. The tab, however, is a real UI element with a name and a
//! `SelectionItemPattern` — an interface whose entire purpose is "select this
//! one". Reading it is not reverse engineering; it is the documented way an
//! application tells the desktop what it contains.
//!
//! No synthetic input: nothing here fabricates a click or a keystroke. This
//! file only reads and selects. The one keystroke the product can send lives
//! in `window::type_key`, behind the question this file answers — where the
//! keyboard focus is.

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationSelectionItemPattern,
    IUIAutomationVirtualizedItemPattern, TreeScope_Descendants, UIA_ControlTypePropertyId,
    UIA_SelectionItemPatternId, UIA_TabItemControlTypeId, UIA_VirtualizedItemPatternId,
};

/// The window class Windows Terminal hosts its tabs in. Checked before any UIA
/// call so an ordinary application window costs nothing: this whole module is
/// about one program's tabs and should not go walking anyone else's tree.
pub const TERMINAL_WINDOW_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";

/// Holds COM initialized for one call and puts it back exactly as it was.
///
/// Every use is a single synchronous burst on one thread — the runtime calls
/// `resolve` and `activate` without awaiting inside them, so the work cannot
/// migrate mid-flight. Initializing per call rather than keeping an apartment
/// alive costs about a millisecond on an action a person takes a few times a
/// minute, and buys not having to reason about which thread owns what.
struct Apartment {
    uninitialize: bool,
}

impl Apartment {
    /// Enters COM, or joins whatever this thread is already in.
    ///
    /// `RPC_E_CHANGED_MODE` means COM is live on this thread in the other
    /// apartment model — which is not a refusal, it is "you already have one".
    /// UI Automation is happy in either, so the right move is to use it and to
    /// leave it exactly as found: initialized by somebody else, uninitialized
    /// by nobody.
    ///
    /// An earlier version gave up here, which is subtle enough to be worth the
    /// paragraph: it meant tab selection silently did nothing whenever this ran
    /// on a thread that had already been made single-threaded — such as a GUI
    /// thread, which is the one thread a window application is sure to have.
    fn enter() -> Option<Self> {
        // SAFETY: FFI. Balanced by `Drop`, and the one HRESULT that must not be
        // balanced is the one that is not treated as success.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Some(Self {
            uninitialize: hr.is_ok() && hr != RPC_E_CHANGED_MODE,
        })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: paired with the CoInitializeEx that returned success.
            unsafe { CoUninitialize() };
        }
    }
}

/// One tab of a terminal window: what it is called, and whether it is the one
/// in front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub name: String,
    pub selected: bool,
}

/// Every tab in `hwnd`, in the order the window presents them.
///
/// One walk of the window's tree answers both "what tabs are there" and "which
/// is in front" — the walk (`FindAll` across processes) is the expensive part,
/// and a caller choosing between several windows asks both of every one.
///
/// Empty for anything that is not a terminal window, and empty rather than an
/// error whenever UIA declines to answer — a caller that cannot see tabs is in
/// exactly the position it was in before this module existed. Empty is also
/// what a window that has not been drawn can answer, so it means "unknown",
/// never "no tabs".
pub fn tabs(hwnd: HWND) -> Vec<Tab> {
    let Some(_apartment) = Apartment::enter() else {
        return Vec::new();
    };
    // SAFETY: all FFI, and every fallible call is matched rather than
    // unwrapped: a window that closed between enumerating and asking about it
    // is ordinary, not exceptional.
    unsafe {
        let Ok(automation) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let Ok(window) = automation.ElementFromHandle(hwnd) else {
            return Vec::new();
        };
        tab_elements(&automation, &window)
            .iter()
            .inspect(|tab| realize(tab))
            .filter_map(|tab| {
                let name = tab.CurrentName().ok()?.to_string();
                // A tab that does not offer the selection pattern is not the
                // selected one; it is also not worth dropping from the list.
                let selected = tab
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .and_then(|select| select.CurrentIsSelected())
                    .map(|selected| selected.as_bool())
                    .unwrap_or(false);
                Some(Tab { name, selected })
            })
            .collect()
    }
}

/// Selects the tab named `name` in `hwnd`. Returns whether it was selected.
///
/// The window still has to be raised separately; selecting a tab in a window
/// nobody can see changes what is in front of nothing.
pub fn select_tab(hwnd: HWND, name: &str) -> bool {
    let Some(_apartment) = Apartment::enter() else {
        return false;
    };
    // SAFETY: as above.
    unsafe {
        let Ok(automation) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_ALL)
        else {
            return false;
        };
        let Ok(window) = automation.ElementFromHandle(hwnd) else {
            return false;
        };
        for tab in tab_elements(&automation, &window) {
            realize(&tab);
            let Ok(tab_name) = tab.CurrentName() else {
                continue;
            };
            // BSTR compares against &str directly; going through String
            // would allocate a copy of every tab name just to throw it away.
            if tab_name != name {
                continue;
            }
            let Ok(select) = tab.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                UIA_SelectionItemPatternId,
            ) else {
                continue;
            };
            if select.Select().is_err() {
                return false;
            }
            // Report whether the tab *is* selected, not whether asking went
            // through. A terminal restores its own active tab while it handles
            // being activated, so a Select that lands mid-activation returns
            // success and is then quietly undone — and a caller told "done" has
            // no reason to try again. Asking the tab is what settles it.
            return select
                .CurrentIsSelected()
                .map(|selected| selected.as_bool())
                .unwrap_or(false);
        }
        false
    }
}

/// Whether keyboard focus in a terminal window is on its tab strip — on a
/// tab, where the arrow keys move between tabs — rather than in the
/// terminal itself. `None` when UI Automation cannot say, which a caller
/// about to send a keystroke must treat as "do not".
pub fn focus_on_tab_strip(hwnd: HWND) -> Option<bool> {
    let _apartment = Apartment::enter()?;
    let _ = hwnd;
    // SAFETY: all FFI, every fallible call matched.
    unsafe {
        let automation =
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_ALL).ok()?;
        let focused = automation.GetFocusedElement().ok()?;
        let control = focused.CurrentControlType().ok()?;
        Some(control == UIA_TabItemControlTypeId)
    }
}

/// Turns a placeholder into a real element where the tab strip virtualizes.
///
/// A tab scrolled out of view — or in a window that has not been drawn since it
/// went to the back — can sit in the tree as a placeholder that answers with no
/// name and offers no pattern to invoke. That is exactly the state a terminal
/// is in when the focus key is pressed at it, which is the only time any of
/// this runs. Harmless where nothing was virtualized: the pattern is simply
/// unsupported and there is nothing to do.
///
/// SAFETY: caller holds an apartment.
unsafe fn realize(tab: &IUIAutomationElement) {
    unsafe {
        if let Ok(virtualized) = tab.GetCurrentPatternAs::<IUIAutomationVirtualizedItemPattern>(
            UIA_VirtualizedItemPatternId,
        ) {
            let _ = virtualized.Realize();
        }
    }
}

/// The `TabItem` descendants of an element.
///
/// SAFETY: caller holds an apartment for the duration.
unsafe fn tab_elements(
    automation: &IUIAutomation,
    window: &IUIAutomationElement,
) -> Vec<IUIAutomationElement> {
    unsafe {
        let Ok(condition) = automation.CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_TabItemControlTypeId.0),
        ) else {
            return Vec::new();
        };
        let Ok(found) = window.FindAll(TreeScope_Descendants, &condition) else {
            return Vec::new();
        };
        let Ok(count) = found.Length() else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|index| found.GetElement(index).ok())
            .collect()
    }
}
