//! The chain of processes above this one, so the app can find the window an
//! agent is sitting in without matching window titles.
//!
//! A Windows executable invoked from inside WSL runs on the Windows side, so
//! its ancestry is Windows ancestry even when the agent is Linux. Measured on a
//! real machine:
//!
//! ```text
//! powershell.exe -> wsl.exe -> wsl.exe -> WindowsTerminal.exe -> explorer.exe
//! ```
//!
//! Each ancestor carries the executable basename the process table reported at
//! the moment of the event. Pids get recycled; the name is what lets the app
//! check, at summon time, that a pid still belongs to the process that was
//! actually above the agent — a stale pid once matched a bystander's window.

/// One process above us: its id, and its executable basename as the snapshot
/// reported it (e.g. `WindowsTerminal.exe`). Empty when the snapshot had no
/// row for it, which the app treats as "identity unknown".
pub struct Ancestor {
    pub pid: u32,
    pub exe: String,
}

/// Processes from our parent upward, nearest first.
///
/// Empty off Windows, which is the honest answer: a Linux pid means nothing to
/// `GetWindowThreadProcessId`, and sending one would invite the app to match a
/// Windows window against a number from another kernel.
#[cfg(not(windows))]
pub fn ancestors() -> Vec<Ancestor> {
    Vec::new()
}

#[cfg(windows)]
pub fn ancestors() -> Vec<Ancestor> {
    /// Guards against a corrupt snapshot forming a cycle; no real tree is
    /// anywhere near this deep.
    const MAX_DEPTH: usize = 16;

    let snapshot = win::Snapshot::take();
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut chain: Vec<Ancestor> = Vec::new();
    let mut pid = win::current_pid();
    for _ in 0..MAX_DEPTH {
        let Some(parent) = snapshot.parent_of(pid) else {
            break;
        };
        // pid 0 is the idle process; reaching it means we walked off the top.
        if parent == 0 || chain.iter().any(|ancestor| ancestor.pid == parent) {
            break;
        }
        chain.push(Ancestor {
            pid: parent,
            exe: snapshot.name_of(parent).unwrap_or_default(),
        });
        pid = parent;
    }
    chain
}

#[cfg(windows)]
mod win {
    use std::os::raw::{c_ulong, c_void};

    const TH32CS_SNAPPROCESS: c_ulong = 0x0000_0002;
    const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

    // The W variant: `sz_exe_file` in UTF-16, so the name survives any
    // codepage. The app compares it against `QueryFullProcessImageNameW`
    // output, which is Unicode too.
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: c_ulong,
        cnt_usage: c_ulong,
        th32_process_id: c_ulong,
        th32_default_heap_id: usize,
        th32_module_id: c_ulong,
        cnt_threads: c_ulong,
        th32_parent_process_id: c_ulong,
        pc_pri_class_base: i32,
        dw_flags: c_ulong,
        sz_exe_file: [u16; 260],
    }

    // Hand-written rather than pulling in the `windows` crate: this binary is
    // spawned thousands of times a day and its whole virtue is being small.
    unsafe extern "system" {
        fn GetCurrentProcessId() -> c_ulong;
        fn CreateToolhelp32Snapshot(flags: c_ulong, process_id: c_ulong) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    pub fn current_pid() -> u32 {
        // SAFETY: no arguments, cannot fail.
        unsafe { GetCurrentProcessId() }
    }

    struct Row {
        pid: u32,
        parent: u32,
        exe: String,
    }

    pub struct Snapshot {
        rows: Vec<Row>,
    }

    impl Snapshot {
        /// One pass over the process table, kept as (pid, parent, exe) rows.
        /// Taking the snapshot once and walking it in memory avoids
        /// re-enumerating for every step of the chain.
        pub fn take() -> Option<Self> {
            // SAFETY: FFI. The handle is closed on every path below.
            let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut rows = Vec::new();
            // SAFETY: `dw_size` is set as the API requires before first use.
            let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
            entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as c_ulong;
            // SAFETY: `entry` is a live, correctly sized structure.
            let mut ok = unsafe { Process32FirstW(handle, &mut entry) } != 0;
            while ok {
                let len = entry
                    .sz_exe_file
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(entry.sz_exe_file.len());
                rows.push(Row {
                    pid: entry.th32_process_id,
                    parent: entry.th32_parent_process_id,
                    exe: String::from_utf16_lossy(&entry.sz_exe_file[..len]),
                });
                // SAFETY: as above.
                ok = unsafe { Process32NextW(handle, &mut entry) } != 0;
            }
            // SAFETY: handle came from CreateToolhelp32Snapshot and is not used again.
            unsafe { CloseHandle(handle) };
            Some(Self { rows })
        }

        pub fn parent_of(&self, pid: u32) -> Option<u32> {
            self.rows
                .iter()
                .find(|row| row.pid == pid)
                .map(|row| row.parent)
        }

        pub fn name_of(&self, pid: u32) -> Option<String> {
            self.rows
                .iter()
                .find(|row| row.pid == pid)
                .map(|row| row.exe.clone())
        }
    }
}
