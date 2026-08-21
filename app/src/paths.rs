//! Where this application keeps its own files.
//!
//! One directory, so there is one place to look and one place to clear.

use std::path::PathBuf;

pub fn home() -> Option<PathBuf> {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var(first)
        .ok()
        .or_else(|| std::env::var(second).ok())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn root() -> Option<PathBuf> {
    Some(home()?.join(".agent-frow"))
}

pub fn token_file() -> Option<PathBuf> {
    std::env::var("AGENT_FROW_TOKEN_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(root()?.join("token")))
}

/// Where both executables are installed, and the only path an agent's
/// configuration ever names.
///
/// `%LOCALAPPDATA%` rather than the build directory: a hook registered against
/// `target/debug` stops working the moment anyone runs `cargo clean`, and for
/// Codex a moved path also costs the user a fresh `/hooks` trust.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|local| PathBuf::from(local).join("agent-frow"))
        .or_else(|| Some(root()?.join("bin")))
}

/// When each flavor last reached us, so a diagnostic can answer the only
/// question that matters after an install: is it actually working?
pub fn last_seen_file() -> Option<PathBuf> {
    Some(root()?.join("last-seen.json"))
}

/// Lane names, colours, saved agents, the lane count — the window's settings.
///
/// Beside the installed executables rather than in the profile directory: it is
/// this application's own configuration, and it should go wherever removing
/// this application would take it.
pub fn settings_file() -> Option<PathBuf> {
    Some(install_dir()?.join("settings.json"))
}

/// Reads the shared token, creating one on first use.
///
/// Generated from the OS random source. This is not protecting much — anything
/// running as this user could read the file — but it is what stops a web page
/// in a browser from posting to our loopback port, since a custom header forces
/// a preflight the page cannot satisfy.
pub fn read_or_create_token() -> Result<String, String> {
    let path = token_file().ok_or("no home directory to keep a token in")?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_owned();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let token = random_token()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&path, &token).map_err(|error| format!("{}: {error}", path.display()))?;
    restrict(&path);
    Ok(token)
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {
    // The user profile directory already carries the right ACL on Windows.
}

fn random_token() -> Result<String, String> {
    let bytes = os_random(16)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn os_random(len: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|error| format!("/dev/urandom: {error}"))?;
    let mut buffer = vec![0u8; len];
    file.read_exact(&mut buffer)
        .map_err(|error| format!("/dev/urandom: {error}"))?;
    Ok(buffer)
}

#[cfg(windows)]
fn os_random(len: usize) -> Result<Vec<u8>, String> {
    // `RtlGenRandom` is documented under that name but exported from
    // advapi32 as `SystemFunction036`, so it has to be linked by the real
    // symbol or the binary fails to link at all.
    #[link(name = "advapi32")]
    unsafe extern "system" {
        #[link_name = "SystemFunction036"]
        fn RtlGenRandom(buffer: *mut u8, length: u32) -> u8;
    }
    let mut buffer = vec![0u8; len];
    // SAFETY: writes exactly `len` bytes into a buffer of that length.
    let ok = unsafe { RtlGenRandom(buffer.as_mut_ptr(), len as u32) };
    if ok == 0 {
        return Err("the system random source refused".to_owned());
    }
    Ok(buffer)
}
