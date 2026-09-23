//! Launch identity must survive cwd changes, children, and application restart.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_frow::{
    event::{Event, Parsed},
    projects::Folders,
    settings::{AgentFilter, SavedAgent, Settings},
    tracker::Tracker,
};
use serde_json::{Value, json};

const ROOT: &str = "/home/jerome/dev/ai-brand-dna";
const BACKEND: &str = "/home/jerome/dev/ai-brand-dna/backend";

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "agent-frow-projects-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tracker() -> Tracker {
    let mut settings = Settings::default();
    settings.set_lane_count(4);
    settings.saved.push(SavedAgent {
        agent: AgentFilter::Any,
        folder: ROOT.into(),
        lane: Some(2),
        reserved_lane: None,
    });
    // A real, separately saved backend must not steal the root's match.
    settings.saved.push(SavedAgent {
        agent: AgentFilter::Any,
        folder: BACKEND.into(),
        lane: Some(1),
        reserved_lane: None,
    });
    Tracker::new(settings, Default::default())
}

fn send(tracker: &mut Tracker, folders: &mut Folders, kind: &str, at: u64, extra: Value) {
    let mut value = json!({"src":"claude-win", "session_id":"main",
        "hook_event_name":kind, "cwd":BACKEND, "wt_session":"main-tab"});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut parsed = Event::parse(&value, at);
    if let Parsed::Event(event) = &mut parsed {
        folders.attach(event, value["transcript_path"].as_str());
    }
    tracker.accept(parsed, at);
}

fn main_row(id: &str, cwd: &str) -> Value {
    json!({"type":"system", "sessionId":id, "isSidechain":false, "cwd":cwd})
}

fn transcript(path: &Path) {
    let rows = [
        json!({"type":"file-history-snapshot", "data":"PRIVATE"}),
        main_row("copied-history", "/unrelated"),
        json!({"sessionId":"main", "isSidechain":true, "cwd":BACKEND}),
        main_row("main", ROOT),
        main_row("main", BACKEND),
    ];
    std::fs::write(
        path,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
}

#[test]
fn restart_recovers_claude_launch_folder_before_saved_lane_and_focus_matching() {
    let scratch = Scratch::new();
    let path = scratch.0.join("transcript.jsonl");
    let cache = scratch.0.join("projects.json");
    transcript(&path);
    let mut folders = Folders::open(Some(cache.clone()));
    let mut tracker = tracker();
    // No SessionStart seen: the app was installed/restarted mid-session.
    send(
        &mut tracker,
        &mut folders,
        "UserPromptSubmit",
        10,
        json!({"transcript_path":path}),
    );
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.sessions[0].cwd.as_deref(), Some(Path::new(ROOT)));
    assert_eq!(
        tracker.sessions[0].current_cwd.as_deref(),
        Some(Path::new(BACKEND))
    );
    assert_eq!(tracker.sessions[0].lane, Some(2));
    assert!(tracker.running(&tracker.settings.saved[0]));
    assert!(!tracker.running(&tracker.settings.saved[1]));
    assert_eq!(
        tracker.summon_target(2).unwrap().project.as_deref(),
        Some("ai-brand-dna")
    );
    assert_eq!(
        tracker.summon_target(2).unwrap().terminal_id.as_deref(),
        Some("main-tab")
    );
    assert!(!std::fs::read_to_string(&cache).unwrap().contains("PRIVATE"));

    // Another restart, with the transcript unavailable: persisted identity
    // must still win over the first hook's current directory.
    std::fs::remove_file(path).unwrap();
    let mut folders = Folders::open(Some(cache));
    let mut restarted = Tracker::new(tracker.settings.clone(), Default::default());
    assert!(restarted.sessions.is_empty());
    send(&mut restarted, &mut folders, "PostToolUse", 20, json!({}));
    assert_eq!(restarted.sessions[0].lane, Some(2));
    assert_eq!(
        restarted.summon_target(2).unwrap().project.as_deref(),
        Some("ai-brand-dna")
    );
}

#[test]
fn main_cwd_changes_compaction_and_resume_keep_launch_lane_and_terminal() {
    for source in ["claude-win", "claude-wsl", "codex-win", "codex-wsl"] {
        let mut tracker = tracker();
        let mut folders = Folders::default();
        send(
            &mut tracker,
            &mut folders,
            "SessionStart",
            10,
            json!({"src":source,"cwd":ROOT,"source":"startup"}),
        );
        let original_focus = tracker.summon_target(2).unwrap();
        for (i, kind, extra) in [
            (20, "PostToolUse", json!({})),
            (30, "SessionStart", json!({"source":"compact"})),
            (40, "SessionStart", json!({"source":"resume"})),
            (50, "UserPromptSubmit", json!({})),
        ] {
            let mut extra = extra;
            extra["src"] = json!(source);
            send(&mut tracker, &mut folders, kind, i, extra);
            assert_eq!(tracker.summon_target(2).unwrap(), original_focus);
            assert_eq!(
                tracker.sessions[0].current_cwd.as_deref(),
                Some(Path::new(BACKEND))
            );
            assert!(tracker.running(&tracker.settings.saved[0]));
        }
    }
}

#[test]
fn child_folder_events_cannot_seed_or_replace_parent_identity() {
    let mut tracker = tracker();
    let mut folders = Folders::default();
    for (i, kind) in ["PostToolUse", "StatusLine", "SubagentStart"]
        .iter()
        .enumerate()
    {
        send(
            &mut tracker,
            &mut folders,
            kind,
            i as u64,
            json!({
                "agent_id":"child", "project_dir":BACKEND, "gauges":{"ctx":99}
            }),
        );
    }
    assert!(tracker.sessions.is_empty());
    send(
        &mut tracker,
        &mut folders,
        "SessionStart",
        10,
        json!({"cwd":ROOT,"source":"startup"}),
    );
    let focus = tracker.summon_target(2).unwrap();
    for kind in ["PostToolUse", "StatusLine", "SubagentStop"] {
        send(
            &mut tracker,
            &mut folders,
            kind,
            20,
            json!({
                "agent_type":"Explore", "project_dir":BACKEND,
                "wt_session":"wrong-tab", "gauges":{"ctx":99}
            }),
        );
    }
    assert_eq!(tracker.sessions.len(), 1);
    assert_eq!(tracker.summon_target(2).unwrap(), focus);
    assert_eq!(
        tracker.sessions[0].current_cwd.as_deref(),
        Some(Path::new(ROOT))
    );
    assert_eq!(tracker.sessions[0].gauges.context_used, None);
}

#[test]
fn status_before_first_hook_supplies_identity_without_resurrecting_a_lane() {
    let scratch = Scratch::new();
    let cache = scratch.0.join("projects.json");
    let mut tracker = tracker();
    let mut folders = Folders::open(Some(cache.clone()));
    send(
        &mut tracker,
        &mut folders,
        "StatusLine",
        10,
        json!({"project_dir":ROOT}),
    );
    assert!(tracker.sessions.is_empty());
    assert_eq!(tracker.events, 0);
    let mut folders = Folders::open(Some(cache));
    send(
        &mut tracker,
        &mut folders,
        "UserPromptSubmit",
        20,
        json!({}),
    );
    assert_eq!(tracker.sessions[0].lane, Some(2));
    let before = tracker.sessions[0].clone();
    send(
        &mut tracker,
        &mut folders,
        "StatusLine",
        30,
        json!({"project_dir":BACKEND,"gauges":{"ctx":42}}),
    );
    let after = &tracker.sessions[0];
    assert_eq!(after.cwd, before.cwd);
    assert_eq!(after.state, before.state);
    assert_eq!(after.last_event, before.last_event);
    assert_eq!(after.events, before.events);
    assert_eq!(after.gauges.context_used, Some(42));
}

#[test]
fn missing_metadata_does_not_promote_a_subfolder_to_project_identity() {
    let mut tracker = tracker();
    let mut folders = Folders::default();
    send(
        &mut tracker,
        &mut folders,
        "UserPromptSubmit",
        10,
        json!({}),
    );
    assert!(tracker.sessions[0].cwd.is_none());
    assert!(!tracker.running(&tracker.settings.saved[1]));
    assert!(
        tracker
            .summon_target(tracker.sessions[0].lane.unwrap())
            .unwrap()
            .project
            .is_none()
    );
    send(
        &mut tracker,
        &mut folders,
        "SessionStart",
        20,
        json!({"source":"compact"}),
    );
    assert!(tracker.sessions[0].cwd.is_none());
    send(
        &mut tracker,
        &mut folders,
        "SessionStart",
        30,
        json!({"source":"resume"}),
    );
    assert!(tracker.sessions[0].cwd.is_none());
    let before = tracker.sessions[0].clone();
    send(
        &mut tracker,
        &mut folders,
        "StatusLine",
        40,
        json!({"project_dir":ROOT}),
    );
    assert_eq!(tracker.sessions[0].cwd.as_deref(), Some(Path::new(ROOT)));
    assert!(tracker.running(&tracker.settings.saved[0]));
    assert_eq!(tracker.sessions[0].lane, before.lane);
    assert_eq!(tracker.sessions[0].last_event, before.last_event);
}

#[test]
fn partial_transcripts_are_retried_and_other_session_ids_do_not_match() {
    let scratch = Scratch::new();
    let path = scratch.0.join("transcript.jsonl");
    let mut folders = Folders::default();
    let mut tracker = tracker();
    std::fs::write(
        &path,
        format!(
            "{}\n{}",
            main_row("other", "/wrong"),
            main_row("main", ROOT)
        ),
    )
    .unwrap();
    send(
        &mut tracker,
        &mut folders,
        "PostToolUse",
        10,
        json!({"transcript_path":path}),
    );
    assert!(tracker.sessions[0].cwd.is_none());
    transcript(&path);
    send(
        &mut tracker,
        &mut folders,
        "PostToolUse",
        20,
        json!({"transcript_path":path}),
    );
    assert_eq!(tracker.sessions[0].cwd.as_deref(), Some(Path::new(ROOT)));
    // A different session in the same source has independent identity.
    send(
        &mut tracker,
        &mut folders,
        "UserPromptSubmit",
        30,
        json!({"session_id":"different", "transcript_path":path, "wt_session":"other-tab"}),
    );
    assert_eq!(tracker.sessions.len(), 2);
    assert!(tracker.sessions[1].cwd.is_none());
}
