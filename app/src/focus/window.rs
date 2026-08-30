//! Finding the terminal that ran an agent, and putting it in front.

use core::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VK_DOWN, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, FlashWindow, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, IsWindowVisible,
    PostMessageW, SC_RESTORE, SW_RESTORE, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow,
    SetWindowPos, ShowWindow, ShowWindowAsync, SwitchToThisWindow, WM_SYSCOMMAND, WS_EX_TOOLWINDOW,
    WindowFromPoint,
};
use windows::core::BOOL;

use crate::event::Ancestor;

use super::uia_tabs::{self, TERMINAL_WINDOW_CLASS};
use super::{Key, Report};

struct Candidate {
    hwnd: isize,
    process_id: u32,
    title: String,
    /// Kept so a terminal can be recognised without a UIA call per window.
    class_name: String,
    /// `WS_EX_TOOLWINDOW`: splash screens and floating palettes. A host app's
    /// real window is never one of these.
    tool_window: bool,
}

/// SAFETY: `EnumWindows` calls this for each top-level window; `lparam` carries
/// a `&mut Vec<Candidate>` set up before the call.
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    const CONTINUE: BOOL = BOOL(1);
    let collected = unsafe { &mut *(lparam.0 as *mut Vec<Candidate>) };

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return CONTINUE;
    }
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return CONTINUE;
    }
    let mut buffer = vec![0u16; (length + 1) as usize];
    let read = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if read <= 0 {
        return CONTINUE;
    }
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let mut class_buffer = [0u16; 256];
    let class_length = unsafe { GetClassNameW(hwnd, &mut class_buffer) };
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    collected.push(Candidate {
        hwnd: hwnd.0 as isize,
        process_id,
        title: String::from_utf16_lossy(&buffer[..read as usize]),
        class_name: String::from_utf16_lossy(&class_buffer[..class_length.max(0) as usize]),
        tool_window: ex_style & WS_EX_TOOLWINDOW.0 != 0,
    });
    CONTINUE
}

/// Whether a window class is a terminal.
///
/// `CASCADIA_HOSTING_WINDOW_CLASS` is Windows Terminal (WSL agents, and Windows
/// agents run there); `ConsoleWindowClass` is the classic console host, conhost
/// (a Windows agent in cmd or a bare console). No longer the gate on what can
/// be summoned — identity does that now — but still two jobs: preferring the
/// real terminal over a transient helper (`PopupHost`) inside Windows
/// Terminal's own pid, and the fallback rule for an ancestor whose exe name an
/// older hook did not record. A Terminal pid can own several such windows (one
/// process hosts them all); a console pid owns exactly one.
fn is_terminal_class(class_name: &str) -> bool {
    class_name == TERMINAL_WINDOW_CLASS || class_name == "ConsoleWindowClass"
}

/// This process's id, so focus never raises the app's own window even if some
/// ancestor pid has been recycled onto it.
fn own_process_id() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
    }
    // SAFETY: no arguments, returns this process's id.
    unsafe { GetCurrentProcessId() }
}

/// The executable basename `pid` currently resolves to, or `None` when the
/// process is gone or unreadable. This is the recycling check: the hook
/// recorded what each ancestor pid was *named* at event time, and a pid that
/// no longer resolves to that name belongs to some bystander now.
fn exe_basename_of_pid(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    // SAFETY: FFI; the handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: `buffer` and `length` describe the same live buffer.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: the handle came from OpenProcess above and is not used again.
    let _ = unsafe { CloseHandle(handle) };
    queried.ok()?;
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_owned)
}

fn visible_windows() -> Vec<Candidate> {
    let mut collected: Vec<Candidate> = Vec::new();
    // SAFETY: `enum_proc` receives a live pointer to `collected` for the
    // duration of the call, which returns before `collected` is used again.
    let _ = unsafe {
        EnumWindows(
            Some(enum_proc),
            LPARAM(&mut collected as *mut Vec<Candidate> as isize),
        )
    };
    collected
}

pub fn raise(ancestors: &[Ancestor], tab_names: &[String]) -> Report {
    if ancestors.is_empty() {
        // Older events carry no ancestry; the next one from that session will.
        return Report::failed("this session has not reported where it is running");
    }
    let windows = visible_windows();
    // What we raise is the agent's *host window* — the nearest ancestor that
    // still is what it was when the event was recorded and owns a real window.
    // Matching any window an ancestor owned went wrong three ways — it raised
    // a transient Terminal helper (`PopupHost`); it raised an unrelated app
    // whose pid a dead ancestor's had been recycled into (a summon once
    // brought iCUE forward); and it could raise this app's own window, which
    // is why a summon "did nothing" precisely when the app was focused.
    //
    // The first gate against all that was "terminal classes only", which also
    // ruled out every agent living in a desktop app or an IDE. The gate now is
    // identity: the hook records each ancestor's exe name at event time, and a
    // window only counts while its pid still resolves to that name. Within a
    // matching pid, a terminal-class window is preferred (keeps `PopupHost`
    // out of Windows Terminal's own pid), else the topmost window that is not
    // a tool window (keeps Electron splash screens and palettes out).
    // Ancestors are walked nearest first, so an agent in VS Code's terminal
    // raises VS Code, not whatever launched VS Code. `explorer.exe` is the one
    // deliberate exception: it sits above nearly everything ever launched from
    // the shell, and its windows are never the host.
    //
    // Identity finds the *process*, and Windows Terminal hosts every one of
    // its windows in a single process — that is what lets a tab be dragged out
    // into a window of its own. So a matching pid can own several terminal
    // windows, and which of them holds the agent is not something any
    // window-level API can say: their titles are whatever tab each has in
    // front. Taking the topmost raised the wrong window whenever the tab had
    // been torn out. All of the pid's terminal windows are kept, and the tab
    // is what chooses between them, below.
    let own_pid = own_process_id();
    let mut found: Option<(Vec<&Candidate>, Option<String>)> = None;
    for ancestor in ancestors {
        let of_this_pid =
            |window: &&Candidate| window.process_id == ancestor.pid && window.process_id != own_pid;
        let identity = match &ancestor.exe {
            Some(recorded) => {
                if recorded.eq_ignore_ascii_case("explorer.exe") {
                    continue;
                }
                let Some(current) = exe_basename_of_pid(ancestor.pid) else {
                    continue; // the process is gone
                };
                if !current.eq_ignore_ascii_case(recorded) {
                    continue; // the pid was recycled onto something else
                }
                Some(current)
            }
            // An old hook recorded no name, so there is no identity to check:
            // exactly the old rule, terminal classes only.
            None => None,
        };
        let hosts = hosts_of(windows.iter().filter(of_this_pid), identity.is_some());
        if !hosts.is_empty() {
            found = Some((hosts, identity));
            break;
        }
    }
    let Some((hosts, found_exe)) = found else {
        return Report::failed(
            "no window found for that agent — it may have closed, or its ancestry \
             is stale (restart the agent to refresh it)",
        );
    };

    // Several windows of one Terminal process: read each one's tabs, once, and
    // let the tab decide which window is the agent's. One window — a console,
    // a desktop app, a Terminal with nothing torn out — costs no UIA call here.
    let snapshot: Vec<TabbedWindow> = if hosts.len() > 1 {
        hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let tabs = uia_tabs::tabs(HWND(host.hwnd as *mut c_void));
                TabbedWindow {
                    index,
                    selected: tabs
                        .iter()
                        .find(|tab| tab.selected)
                        .map(|tab| tab.name.clone()),
                    tabs: tabs.into_iter().map(|tab| tab.name).collect(),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let found = hosts[choose(&snapshot, tab_names)];

    let hwnd = HWND(found.hwnd as *mut c_void);
    // Only Windows Terminal has tabs to select; a console host is one session
    // in one window, so raising it is the whole job.
    let tabbed = found.class_name == TERMINAL_WINDOW_CLASS;
    // Name the host by the tab we are after: a terminal's title is whatever tab
    // is in front, so its current title says nothing useful. A console keeps
    // its own title.
    let what = if tabbed {
        tab_names
            .first()
            .cloned()
            .unwrap_or_else(|| found.title.clone())
    } else {
        found.title.clone()
    };
    if !bring_forward(hwnd) {
        // Genuinely refused: flash the taskbar so the window can still be
        // found, and say so rather than claiming it came forward.
        // SAFETY: FFI with a handle from the enumeration above.
        let _ = unsafe { FlashWindow(hwnd, true) };
        let reason = if unsafe { IsIconic(hwnd) }.as_bool() {
            format!("{what} would not restore from minimized — flashed it in the taskbar")
        } else {
            format!("Windows refused to bring {what} forward")
        };
        return Report::failed(reason);
    }

    // A console host has one session and no tabs; raising it is everything.
    // A desktop-app host (Claude, Codex, an IDE) likewise — there is no tab
    // concept, so say what was raised, exe and all, and be done.
    if !tabbed {
        let detail = match &found_exe {
            Some(exe) if !is_terminal_class(&found.class_name) => {
                format!("raised {what} ({exe})")
            }
            _ => format!("raised {what}"),
        };
        return Report::raised(found.hwnd, detail);
    }
    // Never skip the window raise. `GetForegroundWindow` can name a terminal
    // that is still visibly behind another window, which made the old
    // "already in front" shortcut a silent no-op. Only the tab re-selection is
    // safe to skip: selecting an already-selected tab moves keyboard focus to
    // the tab strip and steals the terminal's arrow keys.
    if let Some(selected) = uia_tabs::selected_tab(hwnd)
        && tab_names.contains(&selected)
    {
        return Report::raised(
            found.hwnd,
            format!("raised, already showing the {selected} tab"),
        );
    }
    // Order matters: selecting a tab in a window nobody can see changes what is
    // in front of nothing.
    if let Some(name) = tab_names.iter().find(|name| settle_tab(hwnd, name)) {
        return Report::raised(found.hwnd, format!("raised, showing the {name} tab"));
    }
    // Half of what was asked for, and worth saying: the user is looking at the
    // right terminal showing the wrong agent. Naming the lane after its tab is
    // what fixes it, which is why a lane has a name — so the message names the
    // tabs that are actually there rather than leaving it to be guessed. With
    // several windows the snapshot already holds every tab of every one of
    // them; with one, read it now.
    if snapshot.is_empty() {
        let present = uia_tabs::tab_names(hwnd);
        if present.is_empty() {
            return Report::raised(
                found.hwnd,
                "raised the terminal; its tabs could not be read",
            );
        }
        return Report::raised(
            found.hwnd,
            format!(
                "raised the terminal, but no tab is called {}. Its tabs: {}",
                tab_names.join(" or "),
                present.join(", ")
            ),
        );
    }
    Report::raised(found.hwnd, no_such_tab(tab_names, &snapshot))
}

/// The windows of one ancestor worth raising, best first.
///
/// Every Windows Terminal window of the pid, in Z-order, because Terminal
/// hosts all of its windows in one process and any of them may hold the tab.
/// Otherwise the one window the older rule picked: a terminal-class window
/// (keeps `PopupHost` out of Terminal's own pid — and a console host owns
/// exactly one window), else, only once identity has been checked, the topmost
/// window that is not a tool window (a desktop app or an IDE).
fn hosts_of<'a>(
    windows: impl Iterator<Item = &'a Candidate> + Clone,
    identified: bool,
) -> Vec<&'a Candidate> {
    let terminals: Vec<&Candidate> = windows
        .clone()
        .filter(|window| window.class_name == TERMINAL_WINDOW_CLASS)
        .collect();
    if !terminals.is_empty() {
        return terminals;
    }
    windows
        .clone()
        .find(|window| is_terminal_class(&window.class_name))
        .or_else(|| {
            if identified {
                windows.clone().find(|window| !window.tool_window)
            } else {
                None
            }
        })
        .into_iter()
        .collect()
}

/// One terminal window's tabs, read once, for choosing between the several
/// windows of a Terminal process.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TabbedWindow {
    /// Position in the host list, so choosing needs no window handle.
    index: usize,
    /// The tab in front, when it could be read.
    selected: Option<String>,
    /// Every tab, in the window's order. Empty means *unread*, not tabless: a
    /// window that has not been drawn since it went to the back can answer
    /// with nothing, and so can a minimized one.
    tabs: Vec<String>,
}

/// Which of several terminal windows to raise for `tab_names`: its index.
///
/// Names are tried in order — the lane's first, since that is the one the user
/// controls — and for each, a window already showing it beats one merely
/// containing it, so the raise lands on a tab that needs no re-selecting. When
/// nothing matches, the first window: it is the topmost, which is exactly the
/// rule for a process that owns one window, and the report then says which
/// tabs were there. An empty list — nothing read from any window — gives the
/// same, for the same reason.
fn choose(windows: &[TabbedWindow], tab_names: &[String]) -> usize {
    for name in tab_names {
        let showing = windows
            .iter()
            .find(|window| window.selected.as_deref() == Some(name.as_str()));
        let holding = || windows.iter().find(|window| window.tabs.contains(name));
        if let Some(window) = showing.or_else(holding) {
            return window.index;
        }
    }
    windows.first().map_or(0, |window| window.index)
}

/// The report for a raise that found no tab called any of `tab_names` in any
/// of several windows: every tab of every window, so the user can see what is
/// there — grouped by window, in the order they were stacked.
fn no_such_tab(tab_names: &[String], windows: &[TabbedWindow]) -> String {
    let readable: Vec<String> = windows
        .iter()
        .filter(|window| !window.tabs.is_empty())
        .map(|window| window.tabs.join(", "))
        .collect();
    if readable.is_empty() {
        return "raised the terminal; its tabs could not be read".to_owned();
    }
    let mut detail = format!(
        "raised the terminal, but no tab is called {} in any of its {} windows. Their tabs: {}",
        tab_names.join(" or "),
        windows.len(),
        readable.join("; ")
    );
    let unreadable = windows.len() - readable.len();
    if unreadable > 0 {
        let plural = if unreadable == 1 { "" } else { "s" };
        detail.push_str(&format!("; {unreadable} window{plural} unreadable"));
    }
    detail
}

/// How often a restore is re-checked. Restoring is animated and runs on the
/// *target's* thread, so the only honest check is polling `IsIconic` until it
/// changes.
const RESTORE_STEP: std::time::Duration = std::time::Duration::from_millis(40);

/// Whether `hwnd` stops being minimized within `attempts` polls.
fn settled(hwnd: HWND, attempts: u32) -> bool {
    for _ in 0..attempts {
        // SAFETY: FFI with a handle from the enumeration.
        if !unsafe { IsIconic(hwnd) }.as_bool() {
            return true;
        }
        std::thread::sleep(RESTORE_STEP);
    }
    !unsafe { IsIconic(hwnd) }.as_bool()
}

/// Restores a minimized window, escalating until the window agrees.
///
/// The polite call is not enough on its own, and that is where this used to
/// lie: `ShowWindow(SW_RESTORE)` from a process without foreground rights is
/// demoted to a taskbar flash, and every call involved still reports success.
/// This used to be the summon key's situation when its low-level hook swallowed
/// the press before Windows delivered it to any process. Registered hotkeys now
/// deliver the input to this process, but the restore path remains defensive
/// because Windows can still refuse activation across input queues.
///
/// So after asking politely, ask the *target's own thread* to do it —
/// `ShowWindowAsync` and `SC_RESTORE` are both carried out by the thread that
/// owns the window, which needs no permission to restore itself — and if even
/// that is refused, take the task switcher's path: `SwitchToThisWindow` is how
/// Alt+Tab restores and raises a window from the outside.
///
/// `IsIconic` is believed over every return value, at every step.
fn restore(hwnd: HWND) -> bool {
    // SAFETY: all FFI with a handle from the enumeration; a stale handle makes
    // every one of these a no-op and the verdict stays "still minimized".
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    if settled(hwnd, 3) {
        return true;
    }
    let _ = unsafe { ShowWindowAsync(hwnd, SW_RESTORE) };
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_SYSCOMMAND,
            WPARAM(SC_RESTORE as usize),
            LPARAM(0),
        )
    };
    if settled(hwnd, 8) {
        return true;
    }
    unsafe { SwitchToThisWindow(hwnd, true) };
    settled(hwnd, 8)
}

/// Whether `hwnd` is visibly on top at its centre and stays there.
///
/// `GetForegroundWindow` is not an honest visual check: Windows can set that
/// flag while leaving the window behind another one in the Z-order. Looking up
/// the root window at the terminal's centre measures what the user can actually
/// see. The delayed second check catches a raise that only flashes on top.
fn visually_front_stable(hwnd: HWND) -> bool {
    fn visually_front(hwnd: HWND) -> bool {
        let mut rect = RECT::default();
        // SAFETY: `rect` is writable for the duration of the call and `hwnd`
        // came from the top-level-window enumeration.
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err()
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return false;
        }
        let centre = POINT {
            x: rect.left + (rect.right - rect.left) / 2,
            y: rect.top + (rect.bottom - rect.top) / 2,
        };
        // `WindowFromPoint` may identify a child, so compare its root with the
        // terminal's top-level handle.
        let visible = unsafe { WindowFromPoint(centre) };
        !visible.0.is_null() && unsafe { GetAncestor(visible, GA_ROOT) } == hwnd
    }

    let mut arrived = false;
    for _ in 0..10 {
        if visually_front(hwnd) {
            arrived = true;
            break;
        }
        std::thread::sleep(RESTORE_STEP);
    }
    if !arrived {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    visually_front(hwnd)
}

fn bring_forward(hwnd: HWND) -> bool {
    // A minimized window cannot be brought forward by SetForegroundWindow
    // alone; it has to actually be restored first, and `restore` reports
    // honestly whether it was.
    if unsafe { IsIconic(hwnd) }.as_bool() && !restore(hwnd) {
        return false;
    }
    // Always the attach-based path, and never trusting the call's own return —
    // the whole failure was a summon that reported success while the window
    // never actually became visible on top.
    force_foreground(hwnd);
    if visually_front_stable(hwnd) {
        return true;
    }
    // Still not there: the task switcher's path, which switches for real.
    unsafe { SwitchToThisWindow(hwnd, true) };
    force_foreground(hwnd);
    visually_front_stable(hwnd)
}

/// Makes `hwnd` the foreground window, by sharing input state with both ends of
/// the handoff.
///
/// Windows only lets the process that owns the foreground change it. Two cases
/// break the naive call, and the summon key hits both:
///
/// - **Another window is in front.** We are not the foreground process, so
///   `SetForegroundWindow` is refused outright.
/// - **This app's own window is in front.** We *are* the foreground process, so
///   the call is accepted — and then does not stick: the app's window
///   deactivates, the terminal flickers forward, and focus falls through to
///   whatever was behind (a browser, in the case that finally reproduced it).
///
/// For keyboard activation, attach our thread's input queue to the
/// *outgoing* foreground thread **and** to the *incoming* target's thread, so
/// for the length of the call all three share one input state and the
/// activation has the best chance to land. Attaching to the target — not only
/// to the old foreground — is what the previous version missed.
///
/// Activation and visual Z-order are separate on Windows. A topmost/not-topmost
/// `SetWindowPos` pair moves the terminal visibly above ordinary windows without
/// needing foreground permission; `SetForegroundWindow` still requests the
/// keyboard activation. No synthetic input is used to gain the foreground;
/// [`type_key`] sends one key only after the foreground is verified.
///
/// Every attach is detached on the way out: an input queue left attached to a
/// window that later closes changes how this process receives input afterwards,
/// and this process also runs a low-level keyboard hook.
fn force_foreground(hwnd: HWND) {
    // SAFETY: all FFI. A thread id of 0 means "do not attach", handled below.
    unsafe {
        let our_thread = GetCurrentThreadId();
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let target_thread = GetWindowThreadProcessId(hwnd, None);

        let attach_fg = foreground_thread != 0
            && foreground_thread != our_thread
            && AttachThreadInput(our_thread, foreground_thread, true).as_bool();
        let attach_target = target_thread != 0
            && target_thread != our_thread
            && target_thread != foreground_thread
            && AttachThreadInput(our_thread, target_thread, true).as_bool();

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        // Force the actual Z-order, not just the foreground flag. Making the
        // terminal topmost and immediately demoting it leaves it at the top of
        // the ordinary window stack without leaving an always-on-top terminal.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );

        if attach_target {
            let _ = AttachThreadInput(our_thread, target_thread, false);
        }
        if attach_fg {
            let _ = AttachThreadInput(our_thread, foreground_thread, false);
        }
    }
}

/// How long to keep trying to make a tab selection stick, and how often.
///
/// A quarter of a second, spent only when the first attempt does not take. The
/// common case — a terminal already in front — succeeds immediately and sleeps
/// not at all.
const TAB_ATTEMPTS: u32 = 10;
const TAB_RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(25);

/// Selects a tab and keeps at it until the window agrees.
///
/// Raising a window is asynchronous: `SetForegroundWindow` returns once the
/// change is *queued*, not once it has happened, and a terminal restores its own
/// active tab as it processes being activated. Selecting into that gap loses
/// twice over — the tab strip of a window that has not been drawn may still be
/// virtualized, and a selection that does land gets undone a moment later by the
/// activation finishing.
///
/// Which is exactly what focusing a backgrounded terminal did: the window came
/// forward showing the wrong tab, and a second attempt — with no activation left
/// to race — worked. So wait for the window to actually be in front, then
/// select, then believe the tab rather than the call, and try again if it
/// disagrees.
///
/// This blocks for as long as it runs, which is deliberate and bounded: focus
/// is something a person does a few times a minute, and the alternative is
/// reporting a success they can see is not one.
fn settle_tab(hwnd: HWND, tab: &str) -> bool {
    for attempt in 0..TAB_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TAB_RETRY_STEP);
        }
        // SAFETY: FFI, no arguments to get wrong.
        let in_front = unsafe { GetForegroundWindow() } == hwnd;
        // Give the activation the whole budget to land, but never leave without
        // having tried: a window that will not come forward at all still has a
        // tab worth selecting for the next time it does.
        if !in_front && attempt + 1 < TAB_ATTEMPTS {
            continue;
        }
        if uia_tabs::select_tab(hwnd, tab) {
            return true;
        }
    }
    false
}

/// The class name of a top-level window, or empty.
fn class_of(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: FFI with a writable buffer of the length passed.
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

/// A window's title, or empty.
fn title_of(hwnd: HWND) -> String {
    // SAFETY: FFI; the buffer is sized from the length the window reports.
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; (length + 1) as usize];
    let read = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..read.max(0) as usize])
}

/// Sends one key to `window`, which [`raise`] just brought forward — after
/// verifying that it has the keyboard.
///
/// A raise proves the window is on top; it does not prove the keystrokes go
/// there. Windows may have refused the activation (a press on a deck is not
/// input to this process), or, in Windows Terminal, a tab selection may have
/// left focus on the tab strip, where an arrow switches tabs. So: wait
/// briefly for the foreground to be the window, ask UI Automation where the
/// focus is when the window has tabs, and only then send. The activation is
/// asynchronous, hence the wait; the budget is the tab selection's.
pub fn type_key(window: isize, key: Key) -> Result<String, String> {
    let hwnd = HWND(window as *mut c_void);
    let mut in_front = false;
    for attempt in 0..TAB_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TAB_RETRY_STEP);
        }
        // SAFETY: FFI, no arguments to get wrong.
        if unsafe { GetForegroundWindow() } == hwnd {
            in_front = true;
            break;
        }
    }
    let on_tab_strip = if in_front && class_of(hwnd) == TERMINAL_WINDOW_CLASS {
        uia_tabs::focus_on_tab_strip(hwnd)
    } else {
        Some(false)
    };
    super::ready_to_type(in_front, on_tab_strip).map_err(str::to_owned)?;
    send_key(key)?;
    let title = title_of(hwnd);
    let what = if title.is_empty() {
        "the terminal".to_owned()
    } else {
        title
    };
    Ok(format!("sent {} to {what}", key.name()))
}

/// One press and release of `key`, as the keyboard would send it — scan code
/// included, and the arrows flagged extended, which is what they are.
fn send_key(key: Key) -> Result<(), String> {
    let (vk, flags) = match key {
        Key::Up => (VK_UP, KEYEVENTF_EXTENDEDKEY),
        Key::Down => (VK_DOWN, KEYEVENTF_EXTENDEDKEY),
        Key::Enter => (VK_RETURN, KEYBD_EVENT_FLAGS(0)),
    };
    // SAFETY: FFI, pure lookup.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) } as u16;
    let stroke = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [stroke(flags), stroke(flags | KEYEVENTF_KEYUP)];
    // SAFETY: FFI with a slice of fully initialised structures of the size
    // stated.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent < inputs.len() as u32 {
        return Err(
            "Windows refused the keystroke — an elevated terminal cannot be typed into from here"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TabbedWindow, choose, no_such_tab};

    fn window(index: usize, tabs: &[&str], selected: Option<&str>) -> TabbedWindow {
        TabbedWindow {
            index,
            selected: selected.map(str::to_owned),
            tabs: tabs.iter().map(|tab| (*tab).to_owned()).collect(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn one_window_is_the_window() {
        let windows = [window(0, &["other"], Some("other"))];
        assert_eq!(choose(&windows, &names(&["keeb"])), 0);
    }

    #[test]
    fn nothing_read_falls_back_to_the_topmost() {
        assert_eq!(choose(&[], &names(&["keeb"])), 0);
        let windows = [window(0, &[], None), window(1, &[], None)];
        assert_eq!(choose(&windows, &names(&["keeb"])), 0);
    }

    #[test]
    fn no_match_falls_back_to_the_topmost() {
        let windows = [window(0, &["a"], Some("a")), window(1, &["b"], Some("b"))];
        assert_eq!(choose(&windows, &names(&["keeb"])), 0);
        assert_eq!(choose(&windows, &[]), 0);
    }

    #[test]
    fn a_window_holding_the_tab_beats_the_topmost() {
        let windows = [
            window(0, &["a"], Some("a")),
            window(1, &["b", "keeb"], Some("b")),
        ];
        assert_eq!(choose(&windows, &names(&["keeb"])), 1);
    }

    #[test]
    fn a_window_showing_the_tab_beats_one_merely_holding_it() {
        // Two lanes on one project, unnamed, share a tab title: the one in
        // front needs no re-selecting, so it wins.
        let windows = [
            window(0, &["keeb", "x"], Some("x")),
            window(1, &["keeb"], Some("keeb")),
        ];
        assert_eq!(choose(&windows, &names(&["keeb"])), 1);
    }

    #[test]
    fn the_lane_name_beats_the_project_name_in_another_window() {
        // Window 0 shows the project; window 1 merely holds the lane's name.
        // The lane's name is the one the user chose, so it wins anyway.
        let windows = [
            window(0, &["ai-agent-keeb"], Some("ai-agent-keeb")),
            window(1, &["x", "keeb"], Some("x")),
        ];
        assert_eq!(choose(&windows, &names(&["keeb", "ai-agent-keeb"])), 1);
    }

    #[test]
    fn a_later_window_with_the_first_name_beats_an_earlier_one_with_the_second() {
        let windows = [
            window(0, &["proj"], Some("proj")),
            window(1, &["lane"], None),
        ];
        assert_eq!(choose(&windows, &names(&["lane", "proj"])), 1);
    }

    #[test]
    fn report_lists_every_window_grouped() {
        let windows = [
            window(0, &["a", "b"], Some("a")),
            window(1, &["c"], Some("c")),
        ];
        assert_eq!(
            no_such_tab(&names(&["keeb", "proj"]), &windows),
            "raised the terminal, but no tab is called keeb or proj in any of its 2 windows. \
             Their tabs: a, b; c"
        );
    }

    #[test]
    fn report_counts_windows_it_could_not_read() {
        let windows = [
            window(0, &["a"], Some("a")),
            window(1, &[], None),
            window(2, &[], None),
        ];
        assert_eq!(
            no_such_tab(&names(&["keeb"]), &windows),
            "raised the terminal, but no tab is called keeb in any of its 3 windows. \
             Their tabs: a; 2 windows unreadable"
        );
        let one = [window(0, &["a"], Some("a")), window(1, &[], None)];
        assert!(no_such_tab(&names(&["keeb"]), &one).ends_with("; 1 window unreadable"));
    }

    #[test]
    fn report_says_when_nothing_could_be_read() {
        let windows = [window(0, &[], None), window(1, &[], None)];
        assert_eq!(
            no_such_tab(&names(&["keeb"]), &windows),
            "raised the terminal; its tabs could not be read"
        );
    }
}
