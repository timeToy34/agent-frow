//! Registering our hook in an agent's own configuration.
//!
//! The file belongs to the agent and to the user: it holds their model, their
//! permissions, their own hooks. So this edits it the way a careful person
//! would — read what is there, add only entries that are unmistakably ours,
//! leave every other key and every other hook alone, show the result before
//! writing it, and keep a backup. Removing takes out our entries and nothing
//! else. A file that does not parse is refused, never repaired.
//!
//! Claude and Codex disagree about the filename and about matchers, and agree
//! about everything else, which is why one installer serves both.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agents::{Agent, Flavor, Found, Host};

/// Our entries are the ones naming this executable, and nothing else in a
/// user's configuration will.
const BRIDGE_NAME: &str = "agent-frow-hook";

/// Seconds an agent may wait for us. We answer in well under a second and never
/// hold a decision open, so this only has to be long enough to survive a busy
/// machine. The old application registered 120 and then held hooks for 90 of
/// them; nothing here ever blocks an agent on a human.
const TIMEOUT_SECS: u64 = 5;

/// Codex clamps `SessionEnd` to three seconds and warns when asked for more.
/// Asking for exactly what it allows keeps its hooks screen clean, so a real
/// problem there is not buried among warnings we caused ourselves.
const CODEX_SESSION_END_TIMEOUT_SECS: u64 = 3;

/// The one tool Codex's `PreToolUse` is registered for. Codex reads a matcher
/// as a regular expression against the tool name, so this keeps the hook to
/// the calls that ask the user a question and none of the other 46%.
pub const QUESTION_TOOL_MATCHER: &str = "^request_user_input$";

/// The hooks each agent is asked for, and why.
///
/// `PreToolUse` is deliberately never registered *unfiltered*. It was 46% of
/// all hook traffic in a real 46-hour measurement and contributed nothing a
/// lane could show: a pre-tool hook fires for every tool call, and agents
/// auto-approve nearly all of them. Codex gets it for exactly one tool — see
/// [`QUESTION_TOOL_MATCHER`] — because for that tool, running it *is* asking
/// the user.
fn events(agent: Agent) -> &'static [&'static str] {
    match agent {
        // `StopFailure` is how a lane can ever show Error, and the previous
        // application never registered it, which is why that state had never
        // once been reachable. `PostToolUseFailure` matters for the same reason
        // in miniature: a tool that fails fires it *instead* of `PostToolUse`.
        // `SubagentStart`/`SubagentStop` are what lets a lane say "the turn is
        // done but subagents are still at work" — background subagents outlive
        // the turn that spawned them. `PermissionDenied` is the only thing
        // Claude sends when a pending prompt is answered no — including by an
        // interrupt — and without it a rejected prompt's lane sat on Waiting
        // forever.
        Agent::Claude => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "PermissionDenied",
            "Notification",
            "SubagentStart",
            "SubagentStop",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ],
        // Codex emits no `Notification` at all, and has neither an error nor an
        // interrupt event. It asks the user two ways: `PermissionRequest` for
        // an approval, and the `request_user_input` tool for a question — an
        // ordinary function tool whose handler shows the dialog and blocks on
        // the answer, so its `PreToolUse` is the moment the question appears
        // and its `PostToolUse` the moment it was answered.
        Agent::Codex => &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "SubagentStart",
            "SubagentStop",
            "Stop",
            "SessionEnd",
        ],
    }
}

#[derive(Debug)]
pub enum Error {
    Unreadable(String),
    Unparseable(String),
    NotAnObject,
    Write(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(f, "could not read the configuration: {detail}"),
            Self::Unparseable(detail) => write!(
                f,
                "the configuration is not valid JSON ({detail}) — refusing to touch it"
            ),
            Self::NotAnObject => write!(f, "the configuration is not a JSON object"),
            Self::Write(detail) => write!(f, "could not write the configuration: {detail}"),
        }
    }
}

/// A proposed change, complete enough to show someone before anything is written.
#[derive(Debug)]
pub struct Plan {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub events_added: Vec<String>,
    pub events_removed: Vec<String>,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.before == self.after
    }
}

/// The command an agent should run, for this flavor.
///
/// Always the installed executable under `%LOCALAPPDATA%`, never a build
/// directory: the old application registered `target/debug/...`, which any
/// rebuild or `cargo clean` silently broke. It must also never change once
/// written — Codex records trust against a hash of this exact string and stops
/// running a hook whose text has moved.
///
/// Forward slashes even on the Windows side: CreateProcess, cmd, and
/// PowerShell all accept them, and agents that run their hooks through bash
/// (Claude Code's desktop entrypoint does) eat backslashes as escapes —
/// `C:\Users\...` became `C:Users...: command not found`, a hook that
/// silently never ran.
pub fn command_for(flavor: &Flavor, install_dir: &Path) -> String {
    let exe = install_dir.join(format!("{BRIDGE_NAME}.exe"));
    let path = match &flavor.host {
        Host::Windows => exe.display().to_string().replace('\\', "/"),
        // A WSL agent runs the *Windows* executable through interop, so it
        // reaches loopback on the Windows side. That is what removes the need
        // for any component inside the distribution.
        Host::Wsl { .. } => windows_path_to_wsl(&exe),
    };
    format!("{} --source {}", shell_word(&path), flavor.source())
}

/// `C:\Users\me\AppData\Local\...` as WSL sees it.
fn windows_path_to_wsl(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => {
            format!("/mnt/{}/{rest}", drive.to_lowercase())
        }
        _ => text,
    }
}

/// Quotes a path only when it needs it.
///
/// Agents run these through a shell, so a home directory containing a space
/// would otherwise be read as a path plus an argument — a hook that silently
/// never runs, which is the hardest kind of failure to notice.
fn shell_word(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

pub fn plan_install(found: &Found, install_dir: &Path) -> Result<Plan, Error> {
    let command = command_for(&found.flavor, install_dir);
    rewrite(found, |hooks, agent| {
        // Strip every entry of ours first, everywhere — not just from the
        // events we are about to register. An older version of this app
        // registered `PreToolUse`, and adding only what we want now would leave
        // that behind: 46% of all hook traffic, still being paid for, with
        // nothing reading it.
        let mut removed = Vec::new();
        let existing: Vec<String> = hooks.keys().cloned().collect();
        for name in existing {
            let Some(list) = hooks.get_mut(&name).and_then(Value::as_array_mut) else {
                continue;
            };
            let before = list.len();
            list.retain(|entry| !is_ours(entry));
            if list.len() != before && !events(agent).contains(&name.as_str()) {
                removed.push(name.clone());
            }
            if list.is_empty() {
                hooks.remove(&name);
            }
        }

        let mut added = Vec::new();
        for event in events(agent) {
            let entries = hooks
                .entry((*event).to_owned())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Some(list) = entries.as_array_mut() else {
                continue;
            };
            list.push(entry_for(agent, event, &command));
            added.push((*event).to_owned());
        }
        (added, removed)
    })
}

pub fn plan_remove(found: &Found) -> Result<Plan, Error> {
    rewrite(found, |hooks, _| {
        let mut removed = Vec::new();
        let names: Vec<String> = hooks.keys().cloned().collect();
        for name in names {
            let Some(list) = hooks.get_mut(&name).and_then(Value::as_array_mut) else {
                continue;
            };
            let before = list.len();
            list.retain(|entry| !is_ours(entry));
            if list.len() != before {
                removed.push(name.clone());
            }
            // An event left with no hooks at all is our leftover, not theirs.
            if list.is_empty() {
                hooks.remove(&name);
            }
        }
        (Vec::new(), removed)
    })
}

fn rewrite(
    found: &Found,
    edit: impl FnOnce(&mut Map<String, Value>, Agent) -> (Vec<String>, Vec<String>),
) -> Result<Plan, Error> {
    let before = match std::fs::read_to_string(&found.config) {
        Ok(text) => text,
        // No configuration yet is ordinary: the agent writes one when it first
        // needs to. Starting from an empty object is correct, not an error.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => return Err(Error::Unreadable(error.to_string())),
    };
    let mut document: Value = serde_json::from_str(if before.trim().is_empty() {
        "{}"
    } else {
        &before
    })
    .map_err(|error| Error::Unparseable(error.to_string()))?;

    let root = document.as_object_mut().ok_or(Error::NotAnObject)?;
    let hooks = root
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(Error::NotAnObject)?;

    let (events_added, events_removed) = edit(hooks, found.flavor.agent);
    if hooks.is_empty() {
        root.remove("hooks");
    }

    let after = format!(
        "{}\n",
        serde_json::to_string_pretty(&document).map_err(|error| Error::Write(error.to_string()))?
    );
    Ok(Plan {
        path: found.config.clone(),
        before,
        after,
        events_added,
        events_removed,
    })
}

fn entry_for(agent: Agent, event: &str, command: &str) -> Value {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::String("command".to_owned()));
    hook.insert("command".to_owned(), Value::String(command.to_owned()));
    let timeout = if agent == Agent::Codex && event == "SessionEnd" {
        CODEX_SESSION_END_TIMEOUT_SECS
    } else {
        TIMEOUT_SECS
    };
    hook.insert("timeout".to_owned(), Value::from(timeout));

    // No `async: true`, deliberately, even though Codex documents it and newer
    // builds accept it. Codex 0.147 does not, and it does not merely ignore the
    // key — it *skips the whole hook*, reporting "async hooks are not supported
    // yet". Five of six events would silently never fire, which is the worst
    // failure this application can have: a surface that is confidently blind.
    //
    // Synchronous is fine regardless. Every Codex hook blocks, but ours connects
    // to loopback, writes a few hundred bytes and exits — single-digit
    // milliseconds, and an immediate refusal when the app is not running. What
    // made the previous version unusable with Codex was not the hook being
    // synchronous, it was the broker holding it open for ninety seconds waiting
    // for a decision. Nothing here ever waits for anything.

    let mut entry = Map::new();
    // Claude expects the literal `"*"`. Codex reads a matcher as a regular
    // expression, where `*` is not a valid pattern — omitting it is what means
    // "every occurrence" there, and a real pattern is how its `PreToolUse` is
    // narrowed to the question tool alone.
    match (agent, event) {
        (Agent::Claude, _) => {
            entry.insert("matcher".to_owned(), Value::String("*".to_owned()));
        }
        (Agent::Codex, "PreToolUse") => {
            entry.insert(
                "matcher".to_owned(),
                Value::String(QUESTION_TOOL_MATCHER.to_owned()),
            );
        }
        (Agent::Codex, _) => {}
    }
    entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));
    Value::Object(entry)
}

/// Whether a hook entry is one of ours.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(BRIDGE_NAME))
            })
        })
}

/// Writes the plan, keeping a backup, and never leaving a half-written file.
pub fn apply(plan: &Plan) -> Result<(), Error> {
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Write(error.to_string()))?;
    }
    if plan.path.exists() {
        let backup = plan.path.with_extension("json.agent-frow.bak");
        std::fs::copy(&plan.path, &backup).map_err(|error| Error::Write(error.to_string()))?;
    }
    // Temp file then rename, so an interrupted write cannot leave the agent
    // with a configuration it refuses to start with.
    let temp = plan.path.with_extension("json.agent-frow.tmp");
    std::fs::write(&temp, &plan.after).map_err(|error| Error::Write(error.to_string()))?;
    std::fs::rename(&temp, &plan.path).map_err(|error| Error::Write(error.to_string()))
}

/// Which of our events are currently registered in a configuration.
pub fn installed_events(found: &Found) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(&found.config) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    hooks
        .iter()
        .filter(|(_, entries)| {
            entries
                .as_array()
                .is_some_and(|list| list.iter().any(is_ours))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Events this build needs that are not registered.
///
/// A configuration that merely *mentions* us is not a configuration that works:
/// the previous application had Claude registered for seven events on Windows
/// and eight in WSL, and nothing anywhere reported the difference.
pub fn missing_events(found: &Found) -> Vec<String> {
    let installed = installed_events(found);
    events(found.flavor.agent)
        .iter()
        .filter(|event| !installed.iter().any(|have| have == *event))
        .map(|event| (*event).to_owned())
        .collect()
}

/// Events where we are registered but no longer want to be.
///
/// Left behind by an older version of this application. They cost a process
/// spawn on every occurrence and tell a lane nothing.
pub fn stale_events(found: &Found) -> Vec<String> {
    let wanted = events(found.flavor.agent);
    installed_events(found)
        .into_iter()
        .filter(|event| !wanted.contains(&event.as_str()))
        .collect()
}

/// Copies both executables to the directory the hooks name.
///
/// The registered command must point somewhere stable, so it points here and
/// never at `target/`. Copying on every install also means "update the app" is
/// just "run install again" — the hook and the app can never drift apart into
/// versions that disagree about the wire format.
pub fn install_binaries(install_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let current = std::env::current_exe().map_err(|error| Error::Write(error.to_string()))?;
    let from = current
        .parent()
        .ok_or_else(|| Error::Write("the running executable has no directory".to_owned()))?;
    std::fs::create_dir_all(install_dir).map_err(|error| Error::Write(error.to_string()))?;

    let mut copied = Vec::new();
    for name in [
        "agent-frow.exe",
        "agent-frow-hook.exe",
        // The iCUE SDK, if this build has a copy beside it. Optional: without
        // it the application runs and the keyboard simply stays dark, and the
        // window says which of those it is.
        crate::surface::corsair::ffi::DLL_NAME,
    ] {
        let source = from.join(name);
        if !source.exists() {
            if name == crate::surface::corsair::ffi::DLL_NAME {
                continue;
            }
            return Err(Error::Write(format!(
                "{} is not next to the running executable",
                source.display()
            )));
        }
        let target = install_dir.join(name);
        // Not a textual compare: the same file spelled two ways (case, an 8.3
        // short path) must count as the same file, or installing from the
        // installed copy renames the live executable aside and then fails to
        // copy a source that no longer exists.
        let same_file = same_file(&source, &target);
        // Copying over a running executable fails on Windows, which is exactly
        // what happens when you install from the installed copy. Renaming the
        // old one out of the way first is what Windows itself does.
        if target.exists() && !same_file {
            // `name.old`, not `with_extension`, which would turn
            // `iCUESDK.x64_2019.dll` into `iCUESDK.x64_2019.old` and lose which
            // file it had been.
            let aside = target.with_file_name(format!("{name}.old"));
            let _ = std::fs::remove_file(&aside);
            let _ = std::fs::rename(&target, &aside);
        }
        if !same_file {
            std::fs::copy(&source, &target).map_err(|error| {
                Error::Write(format!(
                    "{} -> {}: {error}",
                    source.display(),
                    target.display()
                ))
            })?;
        }
        copied.push(target);
    }
    // The marker is what lets a bare launch from a foreign folder decide
    // whether the installed copy is current — see `launch_decision`.
    write_version_marker(install_dir).map_err(|error| Error::Write(error.to_string()))?;
    Ok(copied)
}

/// Whether two paths name the same file on disk, however they spell it.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // A path that does not resolve is a file that is not there; it cannot
        // be the same file as anything.
        _ => false,
    }
}

/// Records which version the installed copy is, next to it.
pub fn write_version_marker(install_dir: &Path) -> std::io::Result<()> {
    std::fs::write(install_dir.join("version"), env!("CARGO_PKG_VERSION"))
}

/// The version the installed copy says it is, if it says.
pub fn installed_version(install_dir: &Path) -> Option<String> {
    std::fs::read_to_string(install_dir.join("version"))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// What a bare launch should do, given where it runs from and what is
/// installed. Pure, so the reasoning is testable without a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// This is the installed copy, or an equal twin of it: just run.
    Run,
    /// Foreign copy, and the install is absent or another version: install
    /// everything and relaunch from the installed location.
    Install,
}

pub fn launch_decision(
    running_from_install_dir: bool,
    installed_exe_present: bool,
    installed_version: Option<&str>,
    own_version: &str,
) -> Launch {
    if running_from_install_dir {
        return Launch::Run;
    }
    if !installed_exe_present {
        return Launch::Install;
    }
    match installed_version {
        Some(version) if version == own_version => Launch::Run,
        // No marker (an install from before markers existed) reads as "another
        // version": one reinstall heals it.
        _ => Launch::Install,
    }
}
