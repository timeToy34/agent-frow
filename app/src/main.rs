//! `agent-frow` — shows what your coding agents are doing, on the keyboard.
//!
//! Finds the agents installed on this machine, registers our hook with them,
//! and reads the events they send as lane state. The lighting (milestone 3) and
//! focus (milestone 4) join here as they are built.

use std::sync::{Arc, Mutex};

use agent_frow::now_ms;
use agent_frow::{
    agents, autostart, event, ingress, install, keys, lastseen, paths, settings, surface, tracker,
    ui,
};

use agents::Found;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flags: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match flags.first().copied() {
        Some("doctor") => doctor(),
        Some("install") => install_command(&flags),
        Some("remove") => remove_command(&flags),
        // Explicit `run` never self-installs — the developer's way to run a
        // build in place. A bare launch (a double-click on the zip's exe) is
        // the user's way, and takes the first-run path.
        Some("run") => run(false, None),
        None => launch(),
        Some(other) => {
            eprintln!("agent-frow: unknown command `{other}`");
            usage();
            std::process::exit(2);
        }
    };
    if let Err(reason) = result {
        eprintln!("agent-frow: {reason}");
        std::process::exit(1);
    }
}

fn usage() {
    println!(
        "agent-frow — coding-agent status on the F-row\n\
         \n\
           run                 receive events in place, never self-install\n\
           doctor              what is installed, and whether it is working\n\
           install [--dry-run] register the hook with every agent found\n\
           remove              remove our hook from every agent\n\
         \n\
         `install` and `remove` accept --source NAME to act on one flavor,\n\
         for example --source codex-wsl.\n"
    );
}

fn flag_value<'a>(flags: &[&'a str], name: &str) -> Option<&'a str> {
    let index = flags.iter().position(|flag| *flag == name)?;
    flags.get(index + 1).copied()
}

/// The flavors to act on: all of them, or the one named by `--source`.
fn selected(flags: &[&str]) -> Vec<Found> {
    let found = agents::detect();
    match flag_value(flags, "--source") {
        Some(wanted) => found
            .into_iter()
            .filter(|entry| entry.flavor.source() == wanted)
            .collect(),
        None => found,
    }
}

fn doctor() -> Result<(), String> {
    let install_dir = paths::install_dir().ok_or("no home directory")?;
    let hook = install_dir.join("agent-frow-hook.exe");
    println!("install directory  {}", install_dir.display());
    println!(
        "  agent-frow-hook.exe  {}\n",
        if hook.exists() {
            "present"
        } else {
            "MISSING — run `agent-frow install`"
        }
    );

    println!(
        "start with Windows  {}\n",
        if autostart::enabled() { "yes" } else { "no" }
    );

    let seen = paths::last_seen_file()
        .map(|path| lastseen::load(&path))
        .unwrap_or_default();
    let now = now_ms();
    let found = agents::detect();
    if found.is_empty() {
        println!("no agents found");
        println!("  looked for .claude and .codex in the Windows profile and in every WSL home");
        return Ok(());
    }

    println!("agents");
    for entry in &found {
        let source = entry.flavor.source();
        println!("\n  {}   [{source}]", entry.flavor.describe());
        println!("    directory   {}", entry.home.display());
        println!(
            "    config      {}{}",
            entry.config.display(),
            if entry.config.exists() {
                ""
            } else {
                "  (not written yet — installing creates it)"
            }
        );
        println!(
            "    in use      {}",
            if entry.looks_used() {
                format!("yes ({})", entry.evidence.join(", "))
            } else {
                "no sign of use — this may be a leftover directory".to_owned()
            }
        );

        let missing = install::missing_events(entry);
        let installed = install::installed_events(entry);
        println!(
            "    hooks       {}",
            if installed.is_empty() {
                "not registered".to_owned()
            } else if missing.is_empty() {
                format!("{} registered", installed.len())
            } else {
                // A configuration that merely mentions us is not one that works.
                format!(
                    "{} registered, MISSING {}",
                    installed.len(),
                    missing.join(", ")
                )
            }
        );

        let stale = install::stale_events(entry);
        if !stale.is_empty() {
            println!(
                "    stale       {} — registered by an older build, install removes them",
                stale.join(", ")
            );
        }

        let last = lastseen::describe(seen.get(&source).copied(), now);
        println!("    last event  {last}");
        if !installed.is_empty() && !seen.contains_key(&source) {
            // The single most confusing state there is, so name it exactly.
            let hint = match entry.flavor.agent {
                agents::Agent::Codex => {
                    "registered but never seen — open Codex, run /hooks, and trust the entry"
                }
                agents::Agent::Claude => {
                    "registered but never seen — restart the agent so it reads its hooks"
                }
            };
            println!("                {hint}");
        }
    }
    Ok(())
}

fn install_command(flags: &[&str]) -> Result<(), String> {
    let dry_run = flags.contains(&"--dry-run");
    let install_dir = paths::install_dir().ok_or("no home directory")?;
    let found = selected(flags);
    if found.is_empty() {
        return Err("no agents found to install into".to_owned());
    }
    if dry_run {
        println!(
            "would install both executables to {}",
            install_dir.display()
        );
    } else {
        // The binaries go first: a configuration naming an executable that is
        // not there yet is a hook that silently does nothing.
        let copied = install::install_binaries(&install_dir)
            .map_err(|error| format!("installing executables: {error}"))?;
        println!("installed to {}", install_dir.display());
        for path in copied {
            println!("  {}", path.display());
        }
    }
    for entry in &found {
        let plan = install::plan_install(entry, &install_dir)
            .map_err(|error| format!("{}: {error}", entry.flavor.source()))?;
        report(entry, &plan, dry_run);
        if !dry_run && !plan.is_noop() {
            install::apply(&plan).map_err(|error| format!("{}: {error}", entry.flavor.source()))?;
        }
    }
    if !dry_run {
        println!("\nRestart each agent so it reads its hooks.");
        if found.iter().any(|e| e.flavor.agent == agents::Agent::Codex) {
            println!(
                "For Codex, also run /hooks inside it and trust the entry, or it will never run."
            );
        }
    }
    Ok(())
}

fn remove_command(flags: &[&str]) -> Result<(), String> {
    let found = selected(flags);
    for entry in &found {
        let plan = install::plan_remove(entry)
            .map_err(|error| format!("{}: {error}", entry.flavor.source()))?;
        report(entry, &plan, false);
        if !plan.is_noop() {
            install::apply(&plan).map_err(|error| format!("{}: {error}", entry.flavor.source()))?;
        }
    }
    Ok(())
}

fn report(entry: &Found, plan: &install::Plan, dry_run: bool) {
    println!("\n{}  [{}]", entry.flavor.describe(), entry.flavor.source());
    println!("  {}", plan.path.display());
    if plan.is_noop() {
        println!("  already as it should be — nothing to write");
        return;
    }
    if !plan.events_added.is_empty() {
        println!("  registering  {}", plan.events_added.join(", "));
    }
    if !plan.events_removed.is_empty() {
        println!("  removing     {}", plan.events_removed.join(", "));
    }
    if dry_run {
        println!("  --dry-run: not written. Proposed file:\n");
        for line in plan.after.lines() {
            println!("    {line}");
        }
    }
}

/// Hides the console window, but only when Windows made one just for us.
///
/// This binary is both the tray application and its own command line, so it has
/// to stay a console-subsystem executable or `doctor` and `install` would have
/// nowhere to print. The cost is a black window appearing behind the UI when it
/// is launched from Explorer or the tray. If we are the only process attached
/// to the console then it was created for us and nobody is reading it; if
/// somebody started us from their own terminal, the count is higher and we
/// leave theirs alone.
#[cfg(windows)]
fn hide_own_console() {
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    let mut attached = [0u32; 2];
    // SAFETY: the buffer is exactly the length declared to the call.
    let count = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), 2) };
    if count == 1 {
        // SAFETY: no arguments; detaching a console this process alone holds.
        unsafe { FreeConsole() };
    }
}

#[cfg(not(windows))]
fn hide_own_console() {}

/// Diagnostic — logs the identity fields of every hook event so the true
/// reported `cwd` can be seen (the folder-binding investigation). Off unless
/// the app was launched with `AGENT_FROW_DEBUG` set: this is scaffolding for
/// reading what an agent reports, not something to leave running.
fn log_event(value: &serde_json::Value) {
    if std::env::var_os("AGENT_FROW_DEBUG").is_none() {
        return;
    }
    let Some(dir) = paths::root() else {
        return;
    };
    let field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
            .to_owned()
    };
    let line = format!(
        "{} src={} evt={} sid={} cwd={:?} agent_id={} agent_type={} ntype={} reason={} end={}\n",
        now_ms(),
        field("src"),
        field("hook_event_name"),
        field("session_id"),
        field("cwd"),
        field("agent_id"),
        field("agent_type"),
        field("notification_type"),
        field("reason"),
        field("end_reason"),
    );
    let path = dir.join("events.log");
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 256 * 1024 {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
}

/// A bare launch: decide whether this copy should run, or install itself and
/// hand over to the installed one. Unzip anywhere, run once — the app moves
/// itself to `%LOCALAPPDATA%\agent-frow`, registers hooks with every agent it
/// finds, and continues from there; the unzipped folder can be deleted. The
/// same gesture with a newer zip is an upgrade.
fn launch() -> Result<(), String> {
    // First, so a double-click never flashes a console while installing.
    hide_own_console();
    if cfg!(debug_assertions) {
        // A debug build is a developer running from target/ — never hijack it.
        return run(true, None);
    }
    let Some(install_dir) = paths::install_dir() else {
        return run(true, None);
    };
    let Ok(own) = std::env::current_exe().and_then(|path| path.canonicalize()) else {
        return run(true, None);
    };
    let from_install_dir = install_dir
        .canonicalize()
        .is_ok_and(|dir| own.parent() == Some(dir.as_path()));
    let installed_exe = install_dir.join("agent-frow.exe");
    let decision = install::launch_decision(
        from_install_dir,
        installed_exe.exists(),
        install::installed_version(&install_dir).as_deref(),
        env!("CARGO_PKG_VERSION"),
    );
    if decision == install::Launch::Run {
        return run(true, None);
    }
    match bootstrap(&install_dir, &installed_exe) {
        // The installed copy is now running; this one's job is done.
        Ok(()) => Ok(()),
        // Never die silently: run in place and say why in the window.
        Err(reason) => run(true, Some(format!("could not install itself: {reason}"))),
    }
}

/// Installs everything and starts the installed copy: binaries, then hooks for
/// every agent found, then the handover.
#[cfg(windows)]
fn bootstrap(install_dir: &std::path::Path, installed_exe: &std::path::Path) -> Result<(), String> {
    // The old instance holds the ingress port, and its tray X only hides. It
    // has to go before the relaunch can bind — and old versions have no way
    // to be asked, so this is a targeted termination: only processes running
    // the installed executable itself.
    retire_installed_instance(installed_exe);
    install::install_binaries(install_dir)
        .map_err(|error| format!("installing executables: {error}"))?;
    // Hook registration failures are per-agent and not fatal: the app is
    // about to run and its agent cards show exactly what is and is not
    // registered.
    for entry in &agents::detect() {
        if let Ok(plan) = install::plan_install(entry, install_dir)
            && !plan.is_noop()
        {
            let _ = install::apply(&plan);
        }
    }
    std::process::Command::new(installed_exe)
        .spawn()
        .map_err(|error| format!("relaunching the installed copy: {error}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn bootstrap(
    _install_dir: &std::path::Path,
    _installed_exe: &std::path::Path,
) -> Result<(), String> {
    Err("self-install is a Windows facility".to_owned())
}

/// Ends every process running the installed executable, and waits for it.
///
/// Targeted three ways: by image name first (cheap), then by full canonical
/// path (the identity check), and never this process itself.
#[cfg(windows)]
fn retire_installed_instance(installed_exe: &std::path::Path) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject,
    };
    use windows::core::PWSTR;

    let Ok(installed) = installed_exe.canonicalize() else {
        return; // nothing installed, nothing to retire
    };
    // SAFETY: FFI; the handle is closed below on every path.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return;
    };
    let mut pids = Vec::new();
    // SAFETY: `dw_size` is set as the API requires before first use.
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry` is a live, correctly sized structure.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    let own_pid = unsafe { GetCurrentProcessId() };
    while ok {
        let len = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if entry.th32ProcessID != own_pid && name.eq_ignore_ascii_case("agent-frow.exe") {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: the handle came from CreateToolhelp32Snapshot and is done with.
    let _ = unsafe { CloseHandle(snapshot) };

    for pid in pids {
        // SAFETY: FFI; the handle is closed below on every path.
        let Ok(handle) = (unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }) else {
            continue;
        };
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
        let matches = queried.is_ok()
            && std::path::Path::new(&String::from_utf16_lossy(&buffer[..length as usize]))
                .canonicalize()
                .is_ok_and(|path| path == installed);
        if matches {
            // SAFETY: handle has PROCESS_TERMINATE; exit code 0 by intent.
            let _ = unsafe { TerminateProcess(handle, 0) };
            // SAFETY: handle has PROCESS_SYNCHRONIZE; bounded wait.
            let _ = unsafe { WaitForSingleObject(handle, 3_000) };
        }
        // SAFETY: handle came from OpenProcess and is not used again.
        let _ = unsafe { CloseHandle(handle) };
    }
}

/// "Already running" where a double-click can actually see it — the console
/// was freed long before the port check, so stderr goes nowhere.
#[cfg(windows)]
fn already_running_dialog() {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    use windows::core::w;
    // SAFETY: constant wide strings, no owner window.
    unsafe {
        MessageBoxW(
            None,
            w!("Agent F-Row is already running — look for it in the system tray."),
            w!("Agent F-Row"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(not(windows))]
fn already_running_dialog() {}

fn run(dialog_on_busy: bool, notice: Option<String>) -> Result<(), String> {
    hide_own_console();
    // This process has no console most of the time, so a panicking thread dies
    // without a trace — and a quietly dead worker thread is how a feature
    // "just stops working". Every panic lands in a file instead.
    std::panic::set_hook(Box::new(|info| {
        if let Some(dir) = paths::root() {
            let _ = std::fs::create_dir_all(&dir);
            let line = format!("{} {info}\n", now_ms());
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
            {
                use std::io::Write;
                let _ = file.write_all(line.as_bytes());
            }
        }
    }));
    let token = paths::read_or_create_token()?;
    let last_seen_path = paths::last_seen_file().ok_or("no home directory")?;
    let install_dir = paths::install_dir().ok_or("no home directory")?;
    let settings_path = paths::settings_file();

    // Binding the port is also the single-instance lock: the thing that must
    // not exist twice is the listener, so let it be the lock rather than
    // inventing a second mechanism that can disagree with it.
    let listener = match ingress::bind() {
        Ok(listener) => listener,
        // Not an error: the bind is the single-instance lock. A double-click
        // has no console (freed above), so a silent exit here reads as "the
        // app is broken" — say it where the user can see it.
        Err(ingress::BindError::Busy) if dialog_on_busy => {
            already_running_dialog();
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };

    // A settings file we cannot read is reported and left exactly as it is.
    // Starting from defaults is recoverable; overwriting colours somebody
    // hand-edited, because one character was wrong, is not.
    let (saved, settings_error) = match settings_path.as_deref() {
        Some(path) if path.exists() => match settings::load(path) {
            Ok(saved) => (saved, None),
            Err(error) => {
                eprintln!(
                    "agent-frow: ignoring {} ({error}); starting with defaults and leaving the file alone",
                    path.display()
                );
                (settings::Settings::default(), Some(error))
            }
        },
        _ => (settings::Settings::default(), None),
    };

    let mut started = tracker::Tracker::new(saved, lastseen::load(&last_seen_path));
    started.settings_error = settings_error;
    let tracker = Arc::new(Mutex::new(started));

    let ingress_tracker = Arc::clone(&tracker);
    // The accept loop answers one connection at a time, so everything it does
    // inline is time a waiting hook spends against its timeout — and what it
    // used to do inline was a disk write plus a wait on the tracker mutex,
    // which the lighting thread takes every frame. A busy instant lost real
    // events ("post failed (timed out)" was the whole of hook.log). So the
    // accept path now only validates and enqueues, stamped with its arrival
    // time, and this worker does the disk write and the state machine at its
    // own pace. Bounded: a wedged worker shows up as dropped events, never as
    // unbounded memory.
    let (ingest_send, ingest_recv) = std::sync::mpsc::sync_channel::<(serde_json::Value, u64)>(256);
    std::thread::spawn(move || {
        for (value, now) in ingest_recv {
            // Diagnostic (only when AGENT_FROW_DEBUG is set): raw cwd/agent
            // per event, to see what an agent actually reports for its
            // working directory.
            log_event(&value);
            let parsed = event::Event::parse(&value, now);
            // Recorded on disk as well as in memory, so `doctor` can answer
            // "is it actually working?" with the app not running.
            if let Some(source) = value.get("src").and_then(serde_json::Value::as_str) {
                lastseen::record(&last_seen_path, source, now);
            }
            if let Ok(mut tracker) = ingress_tracker.lock() {
                tracker.accept(parsed, now);
            }
        }
    });
    std::thread::spawn(move || {
        ingress::serve(&listener, &token, |request| {
            if request.path != "/hook" {
                return;
            }
            let Ok(text) = std::str::from_utf8(&request.body) else {
                return;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                return;
            };
            let _ = ingest_send.try_send((value, now_ms()));
        });
    });

    // The keyboard. Started before the window, because it is the point: the
    // F-row should be right whether or not anybody opens the window, and it is
    // also what evicts stale sessions while the window is hidden in the tray.
    let _keyboard = surface::corsair::start(Arc::clone(&tracker));

    // The summon keys: F13–F24, which the user's iCUE profile maps the F-row
    // to. Kept alive for the life of the app; dropping it unregisters them.
    let _keys = keys::start(Arc::clone(&tracker));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([760.0, 720.0])
            .with_min_inner_size([420.0, 300.0])
            .with_title("Agent F-Row")
            .with_icon(eframe::egui::IconData {
                rgba: agent_frow::icon::rgba(64),
                width: 64,
                height: 64,
            }),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Agent F-Row",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(ui::App::new(
                tracker,
                install_dir,
                settings_path,
                notice,
            )))
        }),
    )
    .map_err(|error| format!("could not open the window: {error}"))
}
