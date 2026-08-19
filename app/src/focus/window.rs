//! Finding the terminal that ran an agent, and putting it in front.

use core::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
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

use super::Report;
use super::uia_tabs::{self, TERMINAL_WINDOW_CLASS};

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
/// older hook did not record.
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
        return Report::new(false, "this session has not reported where it is running");
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
    let own_pid = own_process_id();
    let mut found: Option<(&Candidate, Option<String>)> = None;
    for ancestor in ancestors {
        let of_this_pid =
            |window: &&Candidate| window.process_id == ancestor.pid && window.process_id != own_pid;
        match &ancestor.exe {
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
                if let Some(window) = windows
                    .iter()
                    .filter(of_this_pid)
                    .find(|window| is_terminal_class(&window.class_name))
                    .or_else(|| {
                        windows
                            .iter()
                            .filter(of_this_pid)
                            .find(|window| !window.tool_window)
                    })
                {
                    found = Some((window, Some(current)));
                    break;
                }
            }
            // An old hook recorded no name, so there is no identity to check:
            // exactly the old rule, terminal classes only.
            None => {
                if let Some(window) = windows
                    .iter()
                    .filter(of_this_pid)
                    .find(|window| is_terminal_class(&window.class_name))
                {
                    found = Some((window, None));
                    break;
                }
            }
        }
    }
    let Some((found, found_exe)) = found else {
        return Report::new(
            false,
            "no window found for that agent — it may have closed, or its ancestry \
             is stale (restart the agent to refresh it)",
        );
    };

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
        return Report::new(false, reason);
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
        return Report::new(true, detail);
    }
    // Never skip the window raise. `GetForegroundWindow` can name a terminal
    // that is still visibly behind another window, which made the old
    // "already in front" shortcut a silent no-op. Only the tab re-selection is
    // safe to skip: selecting an already-selected tab moves keyboard focus to
    // the tab strip and steals the terminal's arrow keys.
    if let Some(selected) = uia_tabs::selected_tab(hwnd)
        && tab_names.contains(&selected)
    {
        return Report::new(true, format!("raised, already showing the {selected} tab"));
    }
    // Order matters: selecting a tab in a window nobody can see changes what is
    // in front of nothing.
    if let Some(name) = tab_names.iter().find(|name| settle_tab(hwnd, name)) {
        return Report::new(true, format!("raised, showing the {name} tab"));
    }
    // Half of what was asked for, and worth saying: the user is looking at the
    // right terminal showing the wrong agent. Naming the lane after its tab is
    // what fixes it, which is why a lane has a name — so the message names the
    // tabs that are actually there rather than leaving it to be guessed.
    let present = uia_tabs::tab_names(hwnd);
    if present.is_empty() {
        return Report::new(true, "raised the terminal; its tabs could not be read");
    }
    Report::new(
        true,
        format!(
            "raised the terminal, but no tab is called {}. Its tabs: {}",
            tab_names.join(" or "),
            present.join(", ")
        ),
    )
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
/// keyboard activation. No synthetic input is needed.
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
