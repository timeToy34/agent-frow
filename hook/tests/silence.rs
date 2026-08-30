//! The one invariant this binary must never break.
//!
//! We register on `PermissionRequest` for both agents, which is a *decision*
//! hook: Claude and Codex both read the hook's stdout and will act on a
//! well-formed decision there. A status mirror must not be able to approve or
//! deny a real tool call, so the guarantee is not "we try not to print" but
//! "nothing is printed, ever, for any input".
//!
//! These run the real binary rather than a function, because the thing being
//! asserted is a property of the process: a stray `println!`, a panic message,
//! or a library writing to stdout would all pass a unit test and fail here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::process::{Command, Stdio};

const HOOK: &[&str] = &["--source", "claude-wsl"];
const STATUS: &[&str] = &["--status", "--source", "claude-wsl"];
const TEE: &[&str] = &["--status", "--tee", "--source", "claude-wsl"];
const TEE_ALONE: &[&str] = &["--tee", "--source", "claude-wsl"];
const FLAGS_LAST: &[&str] = &["--source", "claude-wsl", "--tee", "--status"];

/// What Claude hands its status-line command, shape-wise.
const STATUS_JSON: &str = "{\"session_id\":\"s\",\"cwd\":\"/home/me\",\"model\":{\"id\":\"claude-opus-5\"},\"context_window\":{\"used_percentage\":42.5},\"rate_limits\":{\"five_hour\":{\"used_percentage\":10},\"seven_day\":{\"used_percentage\":3}}}\r\n";

fn run(payload: &[u8], args: &[&str]) -> (Vec<u8>, Vec<u8>, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-frow-hook"))
        .args(args)
        // Point the token and log somewhere that does not exist, so the binary
        // takes its failure paths — those are exactly the ones most likely to
        // want to say something.
        .env("AGENT_FROW_TOKEN_FILE", "/nonexistent/agent-frow/token")
        .env("AGENT_FROW_HOOK_LOG", "/nonexistent/agent-frow/hook.log")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the hook binary should start");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(payload)
        .expect("the hook should accept its payload");
    let output = child.wait_with_output().expect("the hook should exit");
    (
        output.stdout,
        output.stderr,
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn stdout_is_empty_and_exit_is_zero_for_every_shape() {
    let payloads = [
        // The dangerous one: a decision hook, where anything parseable on
        // stdout is read as allow or deny.
        r#"{"hook_event_name":"PermissionRequest","session_id":"s","prompt_id":"p","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#,
        r#"{"hook_event_name":"PreToolUse","session_id":"s","prompt_id":"p","tool_name":"Bash","tool_use_id":"t"}"#,
        r#"{"hook_event_name":"Notification","session_id":"s","notification_type":"permission_prompt"}"#,
        r#"{"hook_event_name":"Stop","session_id":"s","prompt_id":"p","last_assistant_message":"done"}"#,
        r#"{"hook_event_name":"SessionEnd","session_id":"s","reason":"clear"}"#,
        // Shapes that are not hooks at all.
        "",
        "not json at all",
        "null",
        "[1,2,3]",
        r#"{"hook_event_name":12345}"#,
    ];

    for payload in payloads {
        let (stdout, stderr, code) = run(payload.as_bytes(), HOOK);
        assert!(
            stdout.is_empty(),
            "stdout must be exactly zero bytes, got {:?} for payload {payload}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(
            stderr.is_empty(),
            "stderr must stay quiet too, got {:?} for payload {payload}",
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(code, 0, "exit code must be 0 for payload {payload}");
    }
}

#[test]
fn a_payload_larger_than_any_real_hook_is_still_silent() {
    // A `Read` of a large file produces a hook payload of the same size. It must
    // be dropped without complaint, not truncated into something printable.
    let big = "x".repeat(4 * 1024 * 1024);
    let payload = format!(
        r#"{{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Read","tool_response":{{"content":"{big}"}}}}"#
    );
    let (stdout, stderr, code) = run(payload.as_bytes(), HOOK);
    assert!(
        stdout.is_empty(),
        "stdout must be empty for a large payload"
    );
    assert!(
        stderr.is_empty(),
        "stderr must be empty for a large payload"
    );
    assert_eq!(code, 0);
}

#[test]
fn status_mode_is_silent_for_every_shape() {
    // Without `--tee` the status line is ours to report from and nobody's to
    // read: not a byte comes back, whatever came in.
    for payload in [STATUS_JSON, "", "not json at all", "null", "[1,2,3]"] {
        let (stdout, stderr, code) = run(payload.as_bytes(), STATUS);
        assert!(stdout.is_empty(), "stdout must be empty for {payload:?}");
        assert!(stderr.is_empty(), "stderr must be empty for {payload:?}");
        assert_eq!(code, 0);
    }
}

#[test]
fn tee_returns_stdin_byte_for_byte_and_nothing_else() {
    // With `--tee` the user's own status-line command is after the pipe and
    // must see exactly what Claude wrote — carriage returns, invalid UTF-8,
    // a megabyte — with the app down (the token path does not exist).
    let big = "x".repeat(1024 * 1024);
    let inputs: [&[u8]; 4] = [
        STATUS_JSON.as_bytes(),
        b"",
        b"\xff\xfe not text \x00",
        big.as_bytes(),
    ];
    for input in inputs {
        let (stdout, stderr, code) = run(input, TEE);
        assert_eq!(stdout, input, "stdout must be the input, exactly");
        assert!(stderr.is_empty(), "stderr must stay quiet");
        assert_eq!(code, 0);
    }
}

#[test]
fn tee_without_status_is_still_a_silent_hook() {
    // `--tee` means nothing outside status mode. A hook's reply is empty,
    // and a mis-written command string must not turn a decision hook into
    // an echo of its own input — which, parseable, would be read as a
    // decision.
    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","prompt_id":"p","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#;
    let (stdout, stderr, code) = run(payload.as_bytes(), TEE_ALONE);
    assert!(
        stdout.is_empty(),
        "a hook with --tee must still say nothing"
    );
    assert!(stderr.is_empty());
    assert_eq!(code, 0);
}

#[test]
fn the_flags_are_read_wherever_they_stand() {
    // The command string `install` writes puts the flags first; one edited
    // by hand may not, and the mode must not depend on it.
    let (stdout, stderr, code) = run(STATUS_JSON.as_bytes(), FLAGS_LAST);
    assert_eq!(stdout, STATUS_JSON.as_bytes());
    assert!(stderr.is_empty());
    assert_eq!(code, 0);
}
