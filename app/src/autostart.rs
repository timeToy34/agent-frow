//! Starting with Windows, as a per-user Run entry.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` — the plain, documented
//! mechanism: no service, no task scheduler, no elevation, and the user can see
//! and disable it themselves in Task Manager's Startup tab.
//!
//! The registered command is always the **installed** executable under
//! `%LOCALAPPDATA%`, never whichever build happens to be running — the same
//! rule as the hook command, for the same reason: a build directory path stops
//! existing the moment somebody runs `cargo clean`.

/// Where the entry lives and what it is called.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Agent F-Row";

/// Whether the app is currently registered to start with Windows.
#[cfg(windows)]
pub fn enabled() -> bool {
    registry::read().is_some()
}

/// Registers the installed executable to start at logon.
///
/// Refuses when nothing is installed: registering a path that does not exist
/// would look enabled and silently do nothing at every boot.
#[cfg(windows)]
pub fn enable() -> Result<(), String> {
    let exe = crate::paths::install_dir()
        .ok_or("no install directory")?
        .join("agent-frow.exe");
    if !exe.exists() {
        return Err("install first — the startup entry points at the installed copy".to_owned());
    }
    registry::write(&exe)
}

#[cfg(windows)]
pub fn disable() -> Result<(), String> {
    registry::delete()
}

#[cfg(not(windows))]
pub fn enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn enable() -> Result<(), String> {
    Err("start-with-Windows is a Windows facility".to_owned())
}

#[cfg(not(windows))]
pub fn disable() -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
mod registry {
    use super::{RUN_KEY, VALUE_NAME};
    use std::ffi::c_void;
    use std::path::Path;

    // `RegSetKeyValueW` and friends do open/set/close in one documented call,
    // which keeps this module free of handle bookkeeping.
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegSetKeyValueW(
            hkey: isize,
            sub_key: *const u16,
            value_name: *const u16,
            kind: u32,
            data: *const c_void,
            data_len: u32,
        ) -> i32;
        fn RegDeleteKeyValueW(hkey: isize, sub_key: *const u16, value_name: *const u16) -> i32;
        fn RegGetValueW(
            hkey: isize,
            sub_key: *const u16,
            value_name: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            data_len: *mut u32,
        ) -> i32;
    }

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const REG_SZ: u32 = 1;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_SUCCESS: i32 = 0;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read() -> Option<Vec<u16>> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut len: u32 = 0;
        // SAFETY: FFI; a null data pointer asks only for the length.
        let probed = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if probed != ERROR_SUCCESS || len == 0 {
            return None;
        }
        let mut data = vec![0u16; len as usize / 2 + 1];
        let mut byte_len = (data.len() * 2) as u32;
        // SAFETY: the buffer is exactly the length just declared for it.
        let read = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut byte_len,
            )
        };
        (read == ERROR_SUCCESS).then_some(data)
    }

    pub fn write(exe: &Path) -> Result<(), String> {
        // Quoted, because the path contains spaces on most machines and the
        // Run key parses its commands like a shell would.
        let command = format!("\"{}\"", exe.display());
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let data = wide(&command);
        // SAFETY: FFI with null-terminated wide strings; the byte length
        // includes the terminator, as REG_SZ requires.
        let code = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(format!("could not write the startup entry (error {code})"));
        }
        Ok(())
    }

    pub fn delete() -> Result<(), String> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        // SAFETY: FFI with null-terminated wide strings.
        let code = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(format!("could not remove the startup entry (error {code})"));
        }
        Ok(())
    }
}
