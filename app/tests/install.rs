//! The installer edits files that belong to the user and to their agents.
//!
//! Everything here is about *not* damaging them: their keys, their own hooks,
//! and their formatting survive; a file we cannot parse is refused rather than
//! rewritten; removing takes ours and only ours.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use agent_frow::agents::{Agent, Flavor, Found, Host};
use agent_frow::install;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-frow-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn found(agent: Agent, dir: &std::path::Path, contents: Option<&str>) -> Found {
    let config = dir.join(match agent {
        Agent::Claude => "settings.json",
        Agent::Codex => "hooks.json",
    });
    if let Some(text) = contents {
        std::fs::write(&config, text).unwrap();
    }
    Found {
        flavor: Flavor {
            agent,
            host: Host::Windows,
        },
        home: dir.to_path_buf(),
        config,
        evidence: Vec::new(),
    }
}

fn install_dir() -> PathBuf {
    PathBuf::from(r"C:\Users\someone\AppData\Local\agent-frow")
}

#[test]
fn the_users_own_settings_and_hooks_survive() {
    let dir = scratch("preserve");
    let original = r#"{
  "model": "opus",
  "permissions": { "defaultMode": "auto" },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "my-own-linter" }] }
    ],
    "Stop": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "notify-send done" }] }
    ]
  }
}
"#;
    let entry = found(Agent::Claude, &dir, Some(original));
    let plan = install::plan_install(&entry, &install_dir()).unwrap();
    install::apply(&plan).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&entry.config).unwrap()).unwrap();

    assert_eq!(written["model"], "opus");
    assert_eq!(written["permissions"]["defaultMode"], "auto");

    // Their PreToolUse linter must still be there even though we register no
    // PreToolUse hook of our own.
    let pre = written["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0]["hooks"][0]["command"], "my-own-linter");

    // Their Stop hook keeps its place; ours is added alongside it.
    let stop = written["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
    assert!(
        stop[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("agent-frow-hook")
    );
}

#[test]
fn removing_takes_ours_and_leaves_theirs() {
    let dir = scratch("remove");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(
            r#"{"hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"notify-send done"}]}]}}"#,
        ),
    );
    install::apply(&install::plan_install(&entry, &install_dir()).unwrap()).unwrap();
    install::apply(&install::plan_remove(&entry).unwrap()).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&entry.config).unwrap()).unwrap();
    let stop = written["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 1, "only their hook should remain");
    assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
    // Events where we were the only hook are removed entirely rather than left
    // behind as empty arrays.
    assert!(written["hooks"].get("SessionStart").is_none());
}

#[test]
fn a_configuration_we_cannot_parse_is_refused_untouched() {
    let dir = scratch("broken");
    let broken = "{ this is not json";
    let entry = found(Agent::Claude, &dir, Some(broken));

    let error = install::plan_install(&entry, &install_dir());
    assert!(error.is_err(), "a broken config must not produce a plan");
    assert_eq!(
        std::fs::read_to_string(&entry.config).unwrap(),
        broken,
        "their file must be exactly as they left it"
    );
}

#[test]
fn codex_hooks_are_never_async_and_claude_gets_a_matcher() {
    let dir = scratch("shape");
    let codex = found(Agent::Codex, &dir, None);
    install::apply(&install::plan_install(&codex, &install_dir()).unwrap()).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&codex.config).unwrap()).unwrap();

    // Codex 0.147 skips any hook carrying `async` outright — "async hooks are
    // not supported yet" — so writing it would silently disable five of the six
    // events on that version. Measured against a real install; the published
    // documentation says it is supported, and for 0.148 it is.
    for event in [
        "PermissionRequest",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ] {
        assert!(
            written["hooks"][event][0]["hooks"][0]
                .get("async")
                .is_none(),
            "{event} must not be async, or older Codex drops the hook entirely"
        );
    }
    // Codex clamps this one to 3s and warns about anything larger.
    assert_eq!(written["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
    assert_eq!(written["hooks"]["Stop"][0]["hooks"][0]["timeout"], 5);
    // Codex reads a matcher as a regex, where "*" is not a valid pattern — so
    // every event but one omits it, and that one uses a real pattern: Codex
    // asks questions through the `request_user_input` tool, and PreToolUse is
    // registered for that tool alone, never for the other 46% of traffic.
    for (event, entries) in written["hooks"].as_object().unwrap() {
        let matcher = entries[0].get("matcher");
        if event == "PreToolUse" {
            assert_eq!(matcher, Some(&serde_json::json!("^request_user_input$")));
        } else {
            assert!(matcher.is_none(), "{event} must have no matcher");
        }
    }

    let claude = found(Agent::Claude, &dir, None);
    install::apply(&install::plan_install(&claude, &install_dir()).unwrap()).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude.config).unwrap()).unwrap();
    assert_eq!(written["hooks"]["Stop"][0]["matcher"], "*");
    // The state Claude could never reach before, because it was never registered.
    assert!(written["hooks"].get("StopFailure").is_some());
    // 46% of all hook traffic, and no use to a lane: Claude's questions reach
    // us as a notification, so it gets no PreToolUse at all.
    assert!(written["hooks"].get("PreToolUse").is_none());
}

#[test]
fn a_bare_launch_knows_when_to_install_itself() {
    use install::Launch;
    // The installed copy itself always just runs.
    assert_eq!(
        install::launch_decision(true, true, Some("0.1.0"), "9.9.9"),
        Launch::Run
    );
    // Nothing installed yet: first run installs.
    assert_eq!(
        install::launch_decision(false, false, None, "0.3.0"),
        Launch::Install
    );
    // Installed before markers existed: reads as another version, one
    // reinstall heals it.
    assert_eq!(
        install::launch_decision(false, true, None, "0.3.0"),
        Launch::Install
    );
    // A newer zip run from anywhere is an upgrade.
    assert_eq!(
        install::launch_decision(false, true, Some("0.2.0"), "0.3.0"),
        Launch::Install
    );
    // The same version run from a foreign folder is deliberate: run in place.
    assert_eq!(
        install::launch_decision(false, true, Some("0.3.0"), "0.3.0"),
        Launch::Run
    );
}

#[test]
fn the_version_marker_round_trips() {
    let dir = scratch("version-marker");
    assert_eq!(install::installed_version(&dir), None);
    install::write_version_marker(&dir).unwrap();
    assert_eq!(
        install::installed_version(&dir).as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn a_windows_agent_gets_a_bash_safe_forward_slash_command() {
    let flavor = Flavor {
        agent: Agent::Claude,
        host: Host::Windows,
    };
    let command = install::command_for(&flavor, &install_dir());
    assert_eq!(
        command,
        "C:/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --source claude-win",
        "agents may run hooks through bash, which eats backslashes as escapes"
    );
}

#[test]
fn a_wsl_agent_is_pointed_at_the_windows_executable() {
    let flavor = Flavor {
        agent: Agent::Claude,
        host: Host::Wsl {
            distro: "Ubuntu".into(),
            user: "jerome".into(),
        },
    };
    let command = install::command_for(&flavor, &install_dir());
    assert_eq!(
        command,
        "/mnt/c/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --source claude-wsl",
        "a WSL agent runs the Windows exe through interop, so it reaches loopback"
    );
    assert!(!command.contains("target"), "never a build directory");
}

#[test]
fn installing_twice_changes_nothing_the_second_time() {
    // Codex hashes this command string for trust. A plan that churns would make
    // it demand re-approval for no reason.
    let dir = scratch("idempotent");
    let entry = found(Agent::Claude, &dir, None);
    install::apply(&install::plan_install(&entry, &install_dir()).unwrap()).unwrap();
    let again = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(again.is_noop(), "a second install must be a no-op");
}

#[test]
fn installing_clears_out_an_older_builds_registrations() {
    // The previous version of this app registered PreToolUse — 46% of all hook
    // traffic. Adding only what we now want would leave it firing forever.
    let dir = scratch("stale");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"C:\\old\\agent-frow-hook.exe --provider claude"}]}]}}"#,
        ),
    );
    assert_eq!(install::stale_events(&entry), vec!["PreToolUse".to_owned()]);

    let plan = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(plan.events_removed.contains(&"PreToolUse".to_owned()));
    install::apply(&plan).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&entry.config).unwrap()).unwrap();
    assert!(
        written["hooks"].get("PreToolUse").is_none(),
        "the old registration must be gone, not merely unused"
    );
    assert!(install::stale_events(&entry).is_empty());
}

fn written(entry: &Found) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(&entry.config).unwrap()).unwrap()
}

const OURS_PLAIN: &str =
    "C:/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --status --source claude-win";
const OURS_TEE: &str = "C:/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --status --tee --source claude-win";
const THEIRS: &str = "bash /home/me/.claude/statusline-command.sh";

#[test]
fn a_claude_install_registers_a_status_line_when_there_is_none() {
    let dir = scratch("status-new");
    let entry = found(Agent::Claude, &dir, None);
    assert_eq!(install::status_line_installed(&entry), Some(false));
    let plan = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(
        plan.notes
            .iter()
            .any(|note| note == "status line registered"),
        "{:?}",
        plan.notes
    );
    install::apply(&plan).unwrap();
    let document = written(&entry);
    assert_eq!(document["statusLine"]["type"], "command");
    assert_eq!(document["statusLine"]["command"], OURS_PLAIN);
    assert_eq!(install::status_line_installed(&entry), Some(true));
}

#[test]
fn an_existing_status_line_is_wrapped_and_its_other_keys_kept() {
    let dir = scratch("status-wrap");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(&format!(
            r#"{{"statusLine":{{"type":"command","command":"{THEIRS}","padding":0}}}}"#
        )),
    );
    let plan = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(
        plan.notes
            .iter()
            .any(|note| note.starts_with("status line wrapped")),
        "{:?}",
        plan.notes
    );
    install::apply(&plan).unwrap();
    let document = written(&entry);
    assert_eq!(
        document["statusLine"]["command"],
        format!("{OURS_TEE} | {THEIRS}")
    );
    assert_eq!(
        document["statusLine"]["padding"], 0,
        "their other keys survive"
    );
    assert_eq!(document["statusLine"]["type"], "command");
}

#[test]
fn a_wrapped_status_line_is_stable_across_installs() {
    let dir = scratch("status-idempotent");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(&format!(
            r#"{{"statusLine":{{"type":"command","command":"{THEIRS}"}}}}"#
        )),
    );
    install::apply(&install::plan_install(&entry, &install_dir()).unwrap()).unwrap();
    let again = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(
        again.is_noop(),
        "wrapping what is already wrapped must change nothing"
    );
    assert!(again.notes.is_empty());
}

#[test]
fn removing_unwraps_the_status_line() {
    let dir = scratch("status-unwrap");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(&format!(
            r#"{{"statusLine":{{"type":"command","command":"{THEIRS}","padding":1}}}}"#
        )),
    );
    install::apply(&install::plan_install(&entry, &install_dir()).unwrap()).unwrap();
    let plan = install::plan_remove(&entry).unwrap();
    assert!(
        plan.notes
            .iter()
            .any(|note| note == "status line unwrapped")
    );
    install::apply(&plan).unwrap();
    let document = written(&entry);
    assert_eq!(
        document["statusLine"]["command"], THEIRS,
        "theirs, exactly as it was"
    );
    assert_eq!(document["statusLine"]["padding"], 1);
    assert_eq!(install::status_line_installed(&entry), Some(false));
}

#[test]
fn removing_deletes_a_status_line_that_was_only_ours() {
    let dir = scratch("status-delete");
    let entry = found(Agent::Claude, &dir, None);
    install::apply(&install::plan_install(&entry, &install_dir()).unwrap()).unwrap();
    let plan = install::plan_remove(&entry).unwrap();
    assert!(plan.notes.iter().any(|note| note == "status line removed"));
    install::apply(&plan).unwrap();
    let document = written(&entry);
    assert!(document.get("statusLine").is_none(), "{document}");
    assert!(document.get("hooks").is_none(), "nothing of ours is left");
}

#[test]
fn a_status_line_that_is_not_ours_is_left_alone_on_remove() {
    let dir = scratch("status-theirs");
    let entry = found(
        Agent::Claude,
        &dir,
        Some(&format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "statusLine": { "type": "command", "command": THEIRS }
            }))
            .unwrap()
        )),
    );
    let plan = install::plan_remove(&entry).unwrap();
    assert!(plan.is_noop(), "nothing of ours there, nothing to do");
}

#[test]
fn codex_never_gets_a_status_line() {
    let dir = scratch("status-codex");
    let entry = found(Agent::Codex, &dir, None);
    assert_eq!(install::status_line_installed(&entry), None);
    let plan = install::plan_install(&entry, &install_dir()).unwrap();
    assert!(plan.notes.is_empty());
    install::apply(&plan).unwrap();
    assert!(written(&entry).get("statusLine").is_none());
}

#[test]
fn a_wsl_claude_status_command_names_the_windows_executable() {
    let flavor = Flavor {
        agent: Agent::Claude,
        host: Host::Wsl {
            distro: "Ubuntu".to_owned(),
            user: "me".to_owned(),
        },
    };
    assert_eq!(
        install::status_command_for(&flavor, &install_dir(), true),
        "/mnt/c/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --status --tee --source claude-wsl"
    );
    assert_eq!(
        install::status_command_for(&flavor, &install_dir(), false),
        "/mnt/c/Users/someone/AppData/Local/agent-frow/agent-frow-hook.exe --status --source claude-wsl"
    );
}
