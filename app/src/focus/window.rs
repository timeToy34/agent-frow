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

use super::FocusTarget;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
pub(super) fn exe_basename_of_pid(pid: u32) -> Option<String> {
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

#[derive(Clone, Debug)]
struct LocatedTab {
    window: isize,
    pid: u32,
    born: Option<u64>,
    tab: uia_tabs::Tab,
}

#[derive(Clone)]
struct Binding {
    window: isize,
    pid: u32,
    born: u64,
    id: Vec<i32>,
}

static FOCUS: Mutex<()> = Mutex::new(());
static BINDINGS: OnceLock<Mutex<BTreeMap<(String, String), Binding>>> = OnceLock::new();

fn process_birth(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: a query-only handle, closed on every path.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut born = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result = GetProcessTimes(process, &mut born, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        result.ok()?;
        Some((u64::from(born.dwHighDateTime) << 32) | u64::from(born.dwLowDateTime))
    }
}

/// A duplicate is ambiguous even if one matching tab is already selected.
fn unique<'a>(mut matches: impl Iterator<Item = &'a LocatedTab>) -> Option<&'a LocatedTab> {
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn named<'a>(tabs: &'a [LocatedTab], name: Option<&str>) -> Option<&'a LocatedTab> {
    let name = name.filter(|s| !s.trim().is_empty())?;
    unique(tabs.iter().filter(|found| found.tab.name == name))
}

fn cached<'a>(tabs: &'a [LocatedTab], binding: &Binding) -> Option<&'a LocatedTab> {
    unique(tabs.iter().filter(|found| {
        found.window == binding.window
            && found.pid == binding.pid
            && found.born == Some(binding.born)
            && found.tab.id == binding.id
    }))
}

fn activate(found: &LocatedTab, method: &'static str) -> Report {
    let hwnd = HWND(found.window as *mut c_void);
    if !bring_forward(hwnd) {
        // SAFETY: HWND came from the live enumeration.
        let _ = unsafe { FlashWindow(hwnd, true) };
        return Report::failed("Windows refused to bring the agent terminal forward");
    }
    if !settle_tab(hwnd, &found.tab.id) {
        let mut report = Report::raised(
            found.window,
            "raised the terminal, but the agent tab could not be selected",
        );
        report.target_selected = false;
        return report;
    }
    let mut report = Report::raised(
        found.window,
        format!("raised, showing the {} tab ({method})", found.tab.name),
    );
    report.tab_id = Some(found.tab.id.clone());
    report.method = method;
    report
}

pub fn raise(target: &FocusTarget) -> Report {
    // A scan temporarily selects tabs. Serialize focus requests from all
    // surfaces so they cannot scan, restore, or cache each other's selection.
    let Ok(_focus) = FOCUS.lock() else {
        return Report::failed("focus worker unavailable");
    };
    let windows = visible_windows();
    let own_pid = own_process_id();
    let mut hosts = Vec::new();
    for ancestor in &target.ancestors {
        let identified = match &ancestor.exe {
            Some(recorded) => {
                if recorded.eq_ignore_ascii_case("explorer.exe") {
                    continue;
                }
                if !exe_basename_of_pid(ancestor.pid)
                    .is_some_and(|exe| exe.eq_ignore_ascii_case(recorded))
                {
                    continue;
                }
                true
            }
            None => false,
        };
        hosts = hosts_of(
            windows
                .iter()
                .filter(|w| w.process_id == ancestor.pid && w.process_id != own_pid),
            identified,
        );
        if !hosts.is_empty() {
            break;
        }
    }
    if hosts.is_empty() {
        // Recover incomplete/stale WSL ancestry using unique evidence only.
        // Never select a bystander's application or the first terminal window.
        hosts = windows
            .iter()
            .filter(|w| {
                w.class_name == TERMINAL_WINDOW_CLASS
                    && exe_basename_of_pid(w.process_id)
                        .is_some_and(|exe| exe.eq_ignore_ascii_case("WindowsTerminal.exe"))
            })
            .collect();
    }
    if hosts.is_empty() {
        return Report::failed("no live terminal window found for this agent");
    }
    if hosts[0].class_name != TERMINAL_WINDOW_CLASS {
        let host = hosts[0];
        return if bring_forward(HWND(host.hwnd as *mut c_void)) {
            Report::raised(host.hwnd, format!("raised {}", host.title))
        } else {
            Report::failed("Windows refused to bring the agent window forward")
        };
    }
    let key = (
        target.source.clone(),
        target
            .terminal_id
            .as_ref()
            .unwrap_or(&target.session_id)
            .clone(),
    );
    let cache = BINDINGS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let collect = || -> Vec<LocatedTab> {
        hosts
            .iter()
            .flat_map(|host| {
                let born = process_birth(host.process_id);
                uia_tabs::tabs(HWND(host.hwnd as *mut c_void))
                    .into_iter()
                    .map(move |tab| LocatedTab {
                        window: host.hwnd,
                        pid: host.process_id,
                        born,
                        tab,
                    })
            })
            .collect()
    };
    let tabs = collect();
    let remember = |found: &LocatedTab, method| {
        let report = activate(found, method);
        if report.target_selected
            && let Some(born) = found.born
            && let Ok(mut cache) = cache.lock()
        {
            // Runtime ids and HWNDs never go on disk. A different Terminal
            // process lifetime cannot inherit a previous association.
            if cache.len() >= 256 {
                cache.clear();
            }
            cache.insert(
                key.clone(),
                Binding {
                    window: found.window,
                    pid: found.pid,
                    born,
                    id: found.tab.id.clone(),
                },
            );
        }
        report
    };
    if let Some(found) = named(&tabs, target.custom_name.as_deref()) {
        return remember(found, "custom name");
    }
    let binding = cache.lock().ok().and_then(|cache| cache.get(&key).cloned());
    if let Some(found) = binding.as_ref().and_then(|binding| cached(&tabs, binding)) {
        return remember(found, "remembered tab");
    }
    if let Ok(mut cache) = cache.lock() {
        cache.remove(&key);
    }
    let title = super::console::title(&target.ancestors);
    if let Some(found) = named(&tabs, title.as_deref()) {
        return remember(found, "console title");
    }

    // A custom Terminal tab label can hide the running console's title.
    // Inspect the TermControl HelpText of each tab, not its text buffer. This
    // happens only on an explicit summon, never on background hook traffic.
    let original_foreground = unsafe { GetForegroundWindow() };
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut originals = Vec::new();
    let mut inspected = Vec::new();
    let mut matches = Vec::new();
    let mut complete = true;
    let mut count = 0;
    for host in &hosts {
        let hwnd = HWND(host.hwnd as *mut c_void);
        let before = uia_tabs::tabs(hwnd).into_iter().find(|tab| tab.selected);
        if !bring_forward(hwnd) {
            complete = false;
            continue;
        }
        let snapshot = settled_tabs(hwnd, None);
        let original = before.or_else(|| snapshot.iter().find(|tab| tab.selected).cloned());
        if let Some(original) = original {
            originals.push((hwnd, original.id));
        }
        if snapshot.is_empty() {
            complete = false;
        }
        for tab in snapshot {
            let found = LocatedTab {
                window: host.hwnd,
                pid: host.process_id,
                born: process_birth(host.process_id),
                tab,
            };
            inspected.push(found.clone());
            if let Some(title) = title.as_deref() {
                if count >= 24 || Instant::now() >= deadline {
                    complete = false;
                    continue;
                }
                count += 1;
                if !settle_tab(hwnd, &found.tab.id) {
                    complete = false;
                    continue;
                }
                // A fresh selection can retain the old accessibility tree for
                // a frame. Require a stable selected id and title observation.
                std::thread::sleep(TAB_RETRY_STEP);
                let titles = uia_tabs::console_titles(hwnd);
                if titles.len() == 1 && titles[0] == title {
                    matches.push(found.clone());
                }
            }
        }
    }
    for (hwnd, id) in &originals {
        let _ = uia_tabs::select_tab(*hwnd, id);
    }
    // Recheck a dynamic console title before trusting the completed scan.
    let title_stable = title.is_some() && super::console::title(&target.ancestors) == title;
    if complete
        && title_stable
        && let Some(found) = unique(matches.iter())
    {
        return remember(found, "live console");
    }
    // Minimized windows may only have exposed their tab labels during the
    // scan. A custom name still wins once the full snapshot is available.
    if let Some(found) = named(&inspected, target.custom_name.as_deref()) {
        return remember(found, "custom name");
    }
    if matches.is_empty()
        && let Some(found) = named(&inspected, target.project.as_deref())
    {
        return remember(found, "project");
    }
    if !original_foreground.0.is_null() {
        let _ = bring_forward(original_foreground);
    }
    Report::failed(if matches.len() > 1 {
        "multiple terminal tabs match this agent; no tab selected"
    } else if !complete {
        "terminal discovery was incomplete; press Focus again"
    } else {
        "the agent terminal could not be identified from its live console or project"
    })
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

/// Reads a terminal's tabs after it has been raised, waiting through the same
/// bounded activation budget as selection. A background or newly restored
/// window can expose an empty or partial, virtualized tab tree for its first
/// few frames, so keep the fullest picture and wait for the first-choice name
/// specifically. Empty after the budget still means "unreadable", never "has
/// no tabs"; a readable picture without the first choice permits the fallback.
fn settled_tabs(hwnd: HWND, first_choice: Option<&str>) -> Vec<uia_tabs::Tab> {
    let mut fullest = Vec::new();
    for attempt in 0..TAB_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TAB_RETRY_STEP);
        }
        let tabs = uia_tabs::tabs(hwnd);
        if !tabs.is_empty()
            && first_choice.is_none_or(|wanted| tabs.iter().any(|tab| tab.name == wanted))
        {
            return tabs;
        }
        if tabs.len() > fullest.len() {
            fullest = tabs;
        }
    }
    fullest
}

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
fn settle_tab(hwnd: HWND, tab: &[i32]) -> bool {
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
        // Selecting an already selected tab can move keyboard focus onto
        // the tab strip, making the next answer key operate on tabs.
        if uia_tabs::tabs(hwnd)
            .iter()
            .any(|found| found.selected && found.id == tab)
        {
            return true;
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
    super::input::send_key(key)?;
    let title = title_of(hwnd);
    let what = if title.is_empty() {
        "the terminal".to_owned()
    } else {
        title
    };
    Ok(format!("sent {} to {what}", key.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(window: isize, name: &str, id: i32) -> LocatedTab {
        LocatedTab {
            window,
            pid: 100,
            born: Some(10),
            tab: uia_tabs::Tab {
                name: name.to_owned(),
                selected: false,
                id: vec![id],
            },
        }
    }

    #[test]
    fn duplicate_titles_are_ambiguous_even_in_different_windows() {
        let mut tabs = vec![tab(1, "project", 1), tab(2, "project", 2)];
        tabs[0].tab.selected = true;
        assert!(named(&tabs, Some("project")).is_none());
        assert!(named(&tabs, Some("missing")).is_none());
    }

    #[test]
    fn live_title_does_not_require_a_lane_name() {
        let tabs = vec![tab(1, "Claude: task | repo", 1), tab(2, "repo", 2)];
        assert!(named(&tabs, None).is_none());
        assert_eq!(
            named(&tabs, Some("Claude: task | repo")).map(|t| t.tab.id.clone()),
            Some(vec![1])
        );
    }

    #[test]
    fn cached_identity_survives_rename_but_not_recycled_process_or_closed_tab() {
        let tabs = vec![tab(1, "new title", 7)];
        let mut binding = Binding {
            window: 1,
            pid: 100,
            born: 10,
            id: vec![7],
        };
        assert!(cached(&tabs, &binding).is_some());
        binding.born = 9;
        assert!(cached(&tabs, &binding).is_none());
        binding.born = 10;
        binding.id = vec![8];
        assert!(cached(&tabs, &binding).is_none());
        binding.id = vec![7];
        binding.window = 2;
        assert!(cached(&tabs, &binding).is_none());
    }
}
