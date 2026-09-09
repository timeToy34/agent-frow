//! Effective reviewer settings from a verified Codex CLI transcript.
//!
//! A long turn can put its context megabytes before the tail used for gauges.
//! Read existing records once, then only appended records, on the ingest worker.
//! Keep settings and offsets, never transcript text. This is an internal Codex
//! format: any missing or uncertain evidence leaves approval handling unchanged.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

use crate::event::ApprovalReviewer;

const MAX_RECORD: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct ReviewerReader {
    identity: Option<(u64, u64)>,
    length: u64,
    modified: Option<SystemTime>,
    /// Start of the next record; an unfinished record is read again next time.
    offset: u64,
    session: Option<String>,
    turn: Option<String>,
    reviewer: Option<ApprovalReviewer>,
}

impl ReviewerReader {
    pub(crate) fn read(
        &mut self,
        path: &Path,
        session: &str,
        turn: &str,
    ) -> Option<ApprovalReviewer> {
        match self.refresh(path) {
            Ok(true)
                if self.session.as_deref() == Some(session)
                    && self.turn.as_deref() == Some(turn) =>
            {
                self.reviewer
            }
            Ok(_) => None,
            Err(_) => {
                // A failed refresh cannot leave stale auto-review evidence in
                // force. Recover by scanning again when the file is readable.
                *self = Self::default();
                None
            }
        }
    }

    fn refresh(&mut self, path: &Path) -> io::Result<bool> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let identity = file_identity(&file)?;
        let length = meta.len();
        let modified = meta.modified().ok();
        if self.identity != Some(identity)
            || length < self.length
            || (length == self.length && modified != self.modified)
        {
            *self = Self::default();
        }
        self.identity = Some(identity);
        self.length = length;
        self.modified = modified;
        file.seek(SeekFrom::Start(self.offset))?;
        // Snapshot the length: an active writer cannot keep this scan running
        // indefinitely. Partial final records never count as current settings.
        let mut reader = BufReader::new(file.take(length - self.offset));
        let mut record = Vec::new();
        let mut consumed = 0;
        let mut oversized = false;
        loop {
            let bytes = reader.fill_buf()?;
            if bytes.is_empty() {
                return Ok(consumed == 0);
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(bytes.len(), |index| index + 1);
            consumed += count as u64;
            if !oversized && record.len() + count <= MAX_RECORD {
                record.extend_from_slice(&bytes[..count]);
            } else {
                oversized = true;
                record.clear();
            }
            reader.consume(count);
            if newline.is_some() {
                if oversized {
                    // We cannot prove this wasn't a configuration update.
                    self.reviewer = None;
                } else {
                    self.observe(&record);
                }
                self.offset += consumed;
                consumed = 0;
                oversized = false;
                record.clear();
            }
        }
    }

    fn observe(&mut self, record: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(record) else {
            self.reviewer = None;
            return;
        };
        let payload = &value["payload"];
        if self.offset == 0 {
            if value["type"] == "session_meta" && payload["source"] == "cli" {
                self.session = text(&payload["id"]);
            }
            return;
        }
        if self.session.is_none() {
            return;
        }
        match value["type"].as_str() {
            Some("turn_context") => {
                self.turn = text(&payload["turn_id"]);
                self.reviewer = ApprovalReviewer::parse(&payload["approvals_reviewer"]);
            }
            Some("event_msg") => match payload["type"].as_str() {
                Some("task_started") => {
                    let turn = text(&payload["turn_id"]);
                    if self.turn != turn {
                        self.turn = turn;
                        self.reviewer = None;
                    }
                }
                Some("thread_settings_applied")
                    if payload["thread_id"].as_str() == self.session.as_deref() =>
                {
                    self.reviewer =
                        ApprovalReviewer::parse(&payload["thread_settings"]["approvals_reviewer"]);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the borrowed handle belongs to this open File and info is writable
    // for the duration of the call. No ownership of the handle is transferred.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;

    struct Transcript(PathBuf);

    impl Transcript {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "agent-frow-review-{name}-{}.jsonl",
                std::process::id()
            ));
            let this = Self(path);
            this.replace("auto_review");
            this
        }

        fn replace(&self, reviewer: &str) {
            let metadata = json!({"type":"session_meta", "payload":{"id":"s", "source":"cli"}});
            let context = context("t", json!(reviewer));
            std::fs::write(&self.0, format!("{metadata}\n{context}\n")).unwrap();
        }

        fn append(&self, value: Value) {
            self.bytes(format!("{value}\n").as_bytes());
        }

        fn bytes(&self, bytes: &[u8]) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.0)
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }

        fn read(&self, reader: &mut ReviewerReader) -> Option<ApprovalReviewer> {
            reader.read(&self.0, "s", "t")
        }
    }

    impl Drop for Transcript {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn context(turn: &str, reviewer: Value) -> Value {
        json!({"type":"turn_context", "payload":{
            "turn_id":turn, "approvals_reviewer":reviewer
        }})
    }

    fn settings(session: &str, reviewer: Value) -> Value {
        json!({"type":"event_msg", "payload":{
            "type":"thread_settings_applied", "thread_id":session,
            "thread_settings":{"approvals_reviewer":reviewer}
        }})
    }

    #[test]
    fn a_cold_long_turn_recovers_context_outside_the_gauge_tail() {
        let file = Transcript::new("long");
        for _ in 0..12 {
            file.append(json!({"type":"response_item", "payload":{
                "type":"message", "text":"x".repeat(64 * 1024)
            }}));
        }
        let mut reader = ReviewerReader::default();
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        assert_eq!(reader.offset, std::fs::metadata(&file.0).unwrap().len());
        let offset = reader.offset;
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        assert_eq!(reader.offset, offset);
        file.append(settings("s", json!("user")));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::User));
        assert!(reader.offset > offset);
        file.append(settings("s", json!("auto_review")));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
    }

    #[test]
    fn partial_settings_are_unknown_until_the_record_is_complete() {
        let file = Transcript::new("partial");
        let mut reader = ReviewerReader::default();
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        let before = reader.offset;
        let record = settings("s", json!("user")).to_string();
        let split = record.len() / 2;
        file.bytes(&record.as_bytes()[..split]);
        assert_eq!(file.read(&mut reader), None);
        assert_eq!(reader.offset, before);
        file.bytes(&record.as_bytes()[split..]);
        assert_eq!(
            file.read(&mut reader),
            None,
            "newline is the commit boundary"
        );
        file.bytes(b"\n");
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::User));
    }

    #[test]
    fn settings_cannot_be_borrowed_from_another_session_or_turn() {
        let file = Transcript::new("identity");
        let mut reader = ReviewerReader::default();
        assert_eq!(reader.read(&file.0, "wrong", "t"), None);
        assert_eq!(reader.read(&file.0, "s", "wrong"), None);
        file.append(settings("another-session", json!("user")));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        file.append(
            json!({"type":"event_msg", "payload":{"type":"task_started", "turn_id":"new"}}),
        );
        assert_eq!(file.read(&mut reader), None);
        assert_eq!(reader.read(&file.0, "s", "new"), None);
        file.append(context("new", json!("user")));
        assert_eq!(file.read(&mut reader), None);
        assert_eq!(
            reader.read(&file.0, "s", "new"),
            Some(ApprovalReviewer::User)
        );
    }

    #[test]
    fn malformed_missing_and_future_reviewers_invalidate_old_evidence() {
        let file = Transcript::new("unknown");
        let mut reader = ReviewerReader::default();
        for unknown in [
            Value::Null,
            json!(false),
            json!("future_reviewer"),
            json!({"mode":"auto_review"}),
        ] {
            file.append(context("t", json!("auto_review")));
            assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
            file.append(settings("s", unknown));
            assert_eq!(file.read(&mut reader), None);
        }
        file.append(context("t", json!("auto_review")));
        file.bytes(b"{broken record}\n");
        assert_eq!(file.read(&mut reader), None);
        file.append(context("t", json!("auto_review")));
        file.append(json!({"type":"turn_context", "payload":{"turn_id":"t"}}));
        assert_eq!(file.read(&mut reader), None);
    }

    #[test]
    fn truncation_replacement_and_read_failures_discard_cached_settings() {
        let file = Transcript::new("replacement");
        let mut reader = ReviewerReader::default();
        file.append(json!({"type":"response_item", "payload":{"text":"padding".repeat(100)}}));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        file.replace("user");
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::User));

        // A new file at the same name, larger than the old one, must rescan.
        let old = file.0.with_extension("old");
        std::fs::rename(&file.0, &old).unwrap();
        file.replace("auto_review");
        file.append(json!({"type":"response_item", "payload":{"text":"padding".repeat(1000)}}));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        std::fs::remove_file(old).unwrap();
        std::fs::remove_file(&file.0).unwrap();
        assert_eq!(file.read(&mut reader), None);
        assert_eq!(reader.offset, 0);
        file.replace("user");
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::User));
    }

    #[test]
    fn only_first_record_cli_metadata_can_establish_identity() {
        let file = Transcript::new("metadata");
        for metadata in [
            json!({"type":"session_meta", "payload":{"id":"s", "source":{"subagent":{"other":"guardian"}}}}),
            json!({"type":"response_item", "payload":{"id":"s", "source":"cli"}}),
            json!({"type":"session_meta", "payload":{"source":"cli"}}),
        ] {
            std::fs::write(&file.0, format!("{metadata}\n")).unwrap();
            file.append(json!({"type":"session_meta", "payload":{"id":"s", "source":"cli"}}));
            file.append(context("t", json!("auto_review")));
            assert_eq!(file.read(&mut ReviewerReader::default()), None);
        }
        std::fs::write(&file.0, b"{\"type\":\"session_meta\",").unwrap();
        let mut reader = ReviewerReader::default();
        assert_eq!(file.read(&mut reader), None);
        file.bytes(b"\"payload\":{\"id\":\"s\",\"source\":\"cli\"}}\n");
        file.append(context("t", json!("auto_review")));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
    }

    #[test]
    fn oversized_records_are_bounded_and_cannot_preserve_stale_auto_review() {
        let file = Transcript::new("oversized");
        let mut reader = ReviewerReader::default();
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
        file.append(json!({"type":"event_msg", "payload":{
            "type":"thread_settings_applied", "thread_id":"s", "thread_settings":{
                "approvals_reviewer":"user", "instructions":"x".repeat(MAX_RECORD)
            }
        }}));
        assert_eq!(file.read(&mut reader), None);
        file.append(context("t", json!("auto_review")));
        assert_eq!(file.read(&mut reader), Some(ApprovalReviewer::AutoReview));
    }
}
