//! A bounded journal of lane ownership, separate from optional hook tracing.
//! The tracker only enqueues identities. A worker writes them outside its lock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};

use serde_json::json;

const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Created,
    Replaced,
    Removed,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Change {
    pub at: u64,
    pub action: Action,
    pub source: String,
    pub terminal_id: Option<String>,
    pub session_id: String,
    pub previous_session_id: Option<String>,
    /// Zero-based in memory; one-based in the journal, like the window.
    pub lane: Option<usize>,
    pub project_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub reason: &'static str,
}

pub fn start(path: PathBuf) -> SyncSender<Change> {
    let (send, receive) = sync_channel::<Change>(256);
    std::thread::spawn(move || {
        for change in receive {
            let _ = append(&path, &change);
        }
    });
    send
}

fn append(path: &Path, change: &Change) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(&json!({
        "t": change.at,
        "action": change.action.label(),
        "source": change.source,
        "terminal_id": change.terminal_id,
        "session_id": change.session_id,
        "previous_session_id": change.previous_session_id,
        "lane": change.lane.map(|lane| lane + 1),
        "project_dir": change.project_dir,
        "cwd": change.cwd,
        "reason": change.reason,
    }))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_BYTES {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let truncate = std::fs::metadata(path)
        .map(|metadata| metadata.len().saturating_add(bytes.len() as u64) > MAX_BYTES)
        .unwrap_or(false);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate)
        .open(path)?;
    file.write_all(&bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn journal_records_are_bounded_and_identifiers_cannot_inject_lines() {
        let directory =
            std::env::temp_dir().join(format!("agent-frow-journal-{}", std::process::id()));
        let path = directory.join("lane-events.log");
        let mut change = Change {
            at: 123,
            action: Action::Replaced,
            source: "codex-wsl".to_owned(),
            terminal_id: Some("tab".to_owned()),
            session_id: "new\nconversation".to_owned(),
            previous_session_id: Some("old".to_owned()),
            lane: Some(2),
            project_dir: Some(PathBuf::from("/project")),
            cwd: Some(PathBuf::from("/project/backend")),
            reason: "UserPromptSubmit",
        };
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, " ".repeat(MAX_BYTES as usize)).unwrap();
        append(&path, &change).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            json!({"t":123,"action":"replaced","source":"codex-wsl",
            "terminal_id":"tab","session_id":"new\nconversation","previous_session_id":"old",
            "lane":3,"reason":"UserPromptSubmit", "project_dir":"/project", "cwd":"/project/backend"})
        );
        change.session_id = "x".repeat(MAX_BYTES as usize);
        append(&path, &change).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
