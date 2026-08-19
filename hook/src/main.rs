//! `agent-frow-hook` — the shim every supported agent runs on each lifecycle
//! hook.
//!
//! It has three jobs: drop everything but an allowlist of field names, add the
//! process ancestry and terminal identity only it can see, and post the result
//! to the app on loopback. It has one prohibition, and it is absolute:
//!
//! **This binary never writes to standard output, and always exits 0.**
//!
//! That is not tidiness. We are registered on `PermissionRequest` for both
//! agents, which is a *decision* hook: anything on stdout that happens to parse
//! is read as an instruction to allow or deny a real tool call. A status mirror
//! must never be able to answer one, so the safe output is no output, and the
//! safe exit is success. `tests/silence.rs` runs this binary against real
//! payload shapes and asserts stdout is exactly zero bytes.
//!
//! Diagnostics therefore go to a file, never to a stream the agent reads.

mod ancestry;
mod post;
mod wire;

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed, so the command string we register never changes. Codex records trust
/// against a hash of that string and stops running a hook whose text moved, so a
/// port that could vary would mean re-trusting after every restart.
const PORT: u16 = 47115;

fn main() {
    // Whatever happens below, the agent sees success and silence.
    let outcome = run();
    if let Err(reason) = outcome {
        log(&reason);
    }
    std::process::exit(0);
}

fn run() -> Result<(), String> {
    let source = source_from_args().unwrap_or_else(|| "unknown".to_owned());

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|error| format!("{source}: stdin unreadable ({})", error.kind()))?;

    // A payload we cannot parse still gets reported: "an event arrived and made
    // no sense" is a fact worth having, and far better than the agent's hook
    // appearing to do nothing at all.
    let payload = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    if !payload.is_object() {
        log(&format!(
            "{source}: payload not a JSON object ({} bytes on stdin)",
            raw.len()
        ));
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let record = wire::project(
        &payload,
        &source,
        wt_session(),
        &ancestry::ancestors(),
        now_ms,
    );
    let body = serde_json::to_vec(&record)
        .map_err(|error| format!("{source}: record not serializable ({error})"))?;

    let token = read_token().ok_or_else(|| format!("{source}: no token file"))?;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);
    post::send(addr, &token, &body)
        .map_err(|error| format!("{source}: post failed ({})", error.kind()))
}

/// Which of the four flavors ran us, as the installer wrote it.
///
/// Passed in argv rather than sniffed, because the installer is the only thing
/// that actually knows: it wrote the entry into one specific settings file. It
/// is what lets diagnostics say "codex-wsl has never been seen" rather than
/// "some hook somewhere is not firing".
fn source_from_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--source" {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--source=") {
            return Some(value.to_owned());
        }
    }
    None
}

/// Windows Terminal's per-tab identifier.
///
/// Present for a Windows-native agent, and also for a WSL one: Windows Terminal
/// puts it in `WSLENV`, so it crosses into the distribution and back out again
/// when the agent runs this executable through `/mnt/c`. Verified on a real
/// machine — it is the only tab-level identity either side offers.
fn wt_session() -> Option<String> {
    std::env::var("WT_SESSION").ok().filter(|s| !s.is_empty())
}

fn home_dir() -> Option<PathBuf> {
    // USERPROFILE first on Windows, HOME first elsewhere: Git Bash and the WSL
    // interop layer both set the other one too, so order is what decides.
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

/// The shared token, read from a file rather than carried in argv.
///
/// Keeping it out of the command string is what lets the token be rotated
/// without Codex demanding the hook be re-trusted, and keeps it out of every
/// process listing on the machine.
fn read_token() -> Option<String> {
    let path = std::env::var("AGENT_FROW_TOKEN_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home_dir()?.join(".agent-frow").join("token")))?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// Appends one line to a bounded diagnostic file.
///
/// The agent hides this binary's stderr when it exits 0, which it always does,
/// so a file is the only place a failure can be seen. Never a payload value —
/// only which flavor failed and how.
fn log(message: &str) {
    use std::io::Write;

    let Some(path) = std::env::var("AGENT_FROW_HOOK_LOG")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home_dir()?.join(".agent-frow").join("hook.log")))
    else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate rather than rotate: this file is a place to look when something
    // is wrong, not a record to keep.
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 64 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{message}");
    }
}
