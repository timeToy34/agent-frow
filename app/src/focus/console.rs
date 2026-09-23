//! Read a console title in a disposable process. Attaching a console changes
//! process-wide state, so it must never happen in the GUI or the hook.

use std::io::{Read, Write};
use std::os::windows::{io::FromRawHandle, process::CommandExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::event::Ancestor;

pub(super) fn title(ancestors: &[Ancestor]) -> Option<String> {
    let candidates: Vec<_> = ancestors
        .iter()
        .take(24)
        .filter_map(|a| {
            a.exe
                .as_ref()
                .map(|exe| serde_json::json!({"pid": a.pid, "exe": exe}))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let mut child = Command::new(std::env::current_exe().ok()?)
        .arg("probe-console")
        .arg(serde_json::to_string(&candidates).ok()?)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut bytes = Vec::new();
    child
        .stdout
        .take()?
        .take(8192)
        .read_to_end(&mut bytes)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

pub(super) fn command(args: &[&str]) -> Result<(), String> {
    use core::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn FreeConsole() -> i32;
        fn GetConsoleTitleW(buffer: *mut u16, size: u32) -> u32;
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetCurrentProcess() -> *mut c_void;
        fn DuplicateHandle(
            source_process: *mut c_void,
            source: *mut c_void,
            target_process: *mut c_void,
            target: *mut *mut c_void,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
    }
    let input: serde_json::Value =
        serde_json::from_str(args.first().ok_or("missing console candidates")?)
            .map_err(|_| "invalid console candidates")?;
    let candidates = input
        .as_array()
        .filter(|a| a.len() <= 24)
        .ok_or("invalid console candidates")?;
    // Save an independent handle to the parent's pipe before AttachConsole
    // replaces the standard handles. No bytes can reach an agent's terminal.
    let mut output = unsafe {
        let mut pipe = std::ptr::null_mut();
        let process = GetCurrentProcess();
        if DuplicateHandle(
            process,
            GetStdHandle(-11i32 as u32),
            process,
            &mut pipe,
            0,
            0,
            2,
        ) == 0
        {
            return Err("cannot duplicate probe output".to_owned());
        }
        std::fs::File::from_raw_handle(pipe)
    };
    let mut title = None;
    for candidate in candidates {
        let Some(pid) = candidate["pid"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
        else {
            continue;
        };
        let Some(exe) = candidate["exe"].as_str() else {
            continue;
        };
        if exe.eq_ignore_ascii_case("explorer.exe")
            || exe.eq_ignore_ascii_case("WindowsTerminal.exe")
        {
            continue;
        }
        if !super::window::exe_basename_of_pid(pid)
            .is_some_and(|current| current.eq_ignore_ascii_case(exe))
        {
            continue;
        }
        // SAFETY: this short-lived helper owns its console attachment. It
        // never reads/writes the console, changes its title, or allocates one.
        unsafe {
            FreeConsole();
            if AttachConsole(pid) == 0 {
                continue;
            }
            let mut buffer = [0u16; 512];
            let length = GetConsoleTitleW(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
            FreeConsole();
            if length > 0 && length < buffer.len() - 1 {
                title = Some(String::from_utf16_lossy(&buffer[..length]));
                break;
            }
        }
    }
    serde_json::to_writer(&mut output, &serde_json::json!({"title": title}))
        .map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())
}
