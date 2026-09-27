//! What the Keychron surfaces did with the bus, kept in
//! `~/.agent-frow/keychron-events.log`: each connect attempt, which interface
//! it looked at and the step that stopped it, each lost connection, each
//! untick and re-tick. The window shows only the latest sentence; this is the
//! history behind it, for a keyboard that stopped coming back overnight.
//!
//! A run of identical failures is written once, re-stated every ten minutes
//! or so, and summed up when it ends — a keyboard gone all night costs a
//! handful of lines, not one per retry. Nothing here reads lighting state or
//! keystrokes: product names, ids, OS paths, steps and error text only.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Instant;

use serde_json::{Value, json};

use super::hid::Found;

const MAX_BYTES: u64 = 256 * 1024;

/// A run of identical failures is re-stated after this many repeats — about
/// ten minutes at the surfaces' ten-second retry.
const RESTATE_EVERY: u32 = 60;

/// One interface a connect attempt looked at, and where it stopped.
#[derive(Debug, Clone)]
pub struct Try {
    product: String,
    product_id: u16,
    link: &'static str,
    path: String,
    /// The step it stopped at, or `taken`.
    step: &'static str,
    error: String,
    ms: u64,
}

impl Try {
    pub fn new(found: &Found, step: &'static str, error: &str, started: Instant) -> Self {
        Self {
            product: found.product.clone(),
            product_id: found.product_id,
            link: found.link(),
            path: found.path.clone(),
            step,
            error: error.to_owned(),
            ms: started.elapsed().as_millis() as u64,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "product": self.product, "pid": format!("{:04X}", self.product_id),
            "link": self.link, "path": self.path, "step": self.step,
            "error": (!self.error.is_empty()).then_some(&self.error), "ms": self.ms,
        })
    }

    /// What makes two attempts "the same": everything but the timing.
    fn identity(&self) -> String {
        format!(
            "{}|{:04X}|{}|{}|{}",
            self.product, self.product_id, self.path, self.step, self.error
        )
    }
}

struct Run {
    identity: String,
    first: u64,
    last: u64,
    count: u32,
}

/// One surface's journal. The surface thread owns it; records go to a
/// shared writer thread, so the lighting loop never waits for the disk.
pub struct Journal {
    surface: &'static str,
    failing: Option<Run>,
}

impl Journal {
    pub fn new(surface: &'static str) -> Self {
        let journal = Self {
            surface,
            failing: None,
        };
        send(journal.record(crate::now_ms(), "started", json!({})));
        journal
    }

    pub fn connected(&mut self, model: &str, firmware: &str, leds: u8, tries: &[Try]) {
        let fields = json!({
            "model": model, "firmware": firmware, "leds": leds, "tries": tries_json(tries),
        });
        self.write(|journal, now| journal.event(now, "connected", fields));
    }

    pub fn failed(&mut self, status: &str, tries: &[Try]) {
        self.write(|journal, now| journal.failure(now, status, tries));
    }

    pub fn lost(&mut self, error: &str) {
        self.write(|journal, now| journal.event(now, "lost", json!({ "error": error })));
    }

    pub fn ticked(&mut self, enabled: bool) {
        let event = if enabled { "ticked" } else { "unticked" };
        self.write(|journal, now| journal.event(now, event, json!({})));
    }

    pub fn handed_back(&mut self, result: &Result<(), String>) {
        let error = result.as_ref().err();
        self.write(|journal, now| journal.event(now, "handed_back", json!({ "error": error })));
    }

    fn write(&mut self, make: impl FnOnce(&mut Self, u64) -> Vec<Value>) {
        for record in make(self, crate::now_ms()) {
            send(record);
        }
    }

    /// Any event but a failure: ends a run of failures first.
    fn event(&mut self, now: u64, event: &str, fields: Value) -> Vec<Value> {
        let mut records = self.end_run(now);
        records.push(self.record(now, event, fields));
        records
    }

    fn failure(&mut self, now: u64, status: &str, tries: &[Try]) -> Vec<Value> {
        let identity = std::iter::once(status.to_owned())
            .chain(tries.iter().map(Try::identity))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(run) = self.failing.as_mut()
            && run.identity == identity
        {
            run.count += 1;
            run.last = now;
            if run.count % RESTATE_EVERY != 0 {
                return Vec::new();
            }
            let fields = json!({ "count": run.count, "since": run.first });
            return vec![self.record(now, "still_failing", fields)];
        }
        let mut records = self.end_run(now);
        let fields = json!({ "status": status, "tries": tries_json(tries) });
        records.push(self.record(now, "failed", fields));
        self.failing = Some(Run {
            identity,
            first: now,
            last: now,
            count: 1,
        });
        records
    }

    fn end_run(&mut self, now: u64) -> Vec<Value> {
        match self.failing.take() {
            Some(run) if run.count > 1 => {
                let fields = json!({ "count": run.count, "since": run.first, "until": run.last });
                vec![self.record(now, "failures_ended", fields)]
            }
            _ => Vec::new(),
        }
    }

    fn record(&self, now: u64, event: &str, fields: Value) -> Value {
        let mut record = json!({ "t": now, "surface": self.surface, "event": event });
        if let (Some(record), Value::Object(fields)) = (record.as_object_mut(), fields) {
            record.extend(fields);
        }
        record
    }
}

fn tries_json(tries: &[Try]) -> Value {
    Value::Array(tries.iter().map(Try::to_json).collect())
}

fn send(record: Value) {
    static WRITER: OnceLock<SyncSender<Value>> = OnceLock::new();
    let sender = WRITER.get_or_init(|| {
        let (send, receive) = sync_channel::<Value>(64);
        std::thread::spawn(move || {
            let Some(root) = crate::paths::root() else {
                return;
            };
            let path = root.join("keychron-events.log");
            for value in receive {
                let Ok(mut bytes) = serde_json::to_vec(&value) else {
                    continue;
                };
                bytes.push(b'\n');
                if bytes.len() > 8192 {
                    continue;
                }
                let _ = std::fs::create_dir_all(&root);
                let truncate = std::fs::metadata(&path)
                    .is_ok_and(|m| m.len() + bytes.len() as u64 > MAX_BYTES);
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(!truncate)
                    .truncate(truncate)
                    .open(&path)
                {
                    let _ = file.write_all(&bytes);
                }
            }
        });
        send
    });
    let _ = sender.try_send(record);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn journal() -> Journal {
        Journal {
            surface: "Keychron",
            failing: None,
        }
    }

    fn receiver(step: &'static str, error: &str, ms: u64) -> Try {
        Try {
            product: "Keychron Ultra-Link 8K".to_owned(),
            product_id: 0xD028,
            link: "2.4 GHz",
            path: "\\\\?\\hid#vid_3434&pid_d028&mi_02".to_owned(),
            step,
            error: error.to_owned(),
            ms,
        }
    }

    fn events(records: &[Value]) -> Vec<&str> {
        records
            .iter()
            .map(|record| record["event"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn a_night_of_the_same_failure_is_a_handful_of_lines() {
        let mut journal = journal();
        let status = "not responding — retrying";
        let first = journal.failure(0, status, &[receiver("handshake", "no answer", 250)]);
        assert_eq!(events(&first), ["failed"]);
        assert_eq!(first[0]["tries"][0]["step"], "handshake");
        assert_eq!(first[0]["tries"][0]["pid"], "D028");
        let mut written = first.len();
        // Eight hours at one try every ten seconds; the timing differs each
        // time and does not make a failure new.
        for attempt in 1..2880u64 {
            let tries = [receiver("handshake", "no answer", 250 + attempt % 3)];
            written += journal.failure(attempt * 10_000, status, &tries).len();
        }
        assert_eq!(written, 1 + 2880 / RESTATE_EVERY as usize);
        let back = journal.event(28_800_000, "connected", json!({ "model": "V3" }));
        assert_eq!(events(&back), ["failures_ended", "connected"]);
        assert_eq!(back[0]["count"], 2880);
        assert_eq!(back[0]["since"], 0);
        assert_eq!(back[0]["until"], 28_790_000);
        assert_eq!(back[1]["surface"], "Keychron");
        assert_eq!(back[1]["model"], "V3");
    }

    #[test]
    fn a_different_failure_is_written_and_a_single_one_needs_no_summary() {
        let mut journal = journal();
        let status = "not responding — retrying";
        journal.failure(0, status, &[receiver("handshake", "no answer", 250)]);
        let open = journal.failure(10_000, status, &[receiver("open", "access denied", 1)]);
        assert_eq!(events(&open), ["failed"], "one failure, nothing to sum up");
        assert_eq!(open[0]["tries"][0]["error"], "access denied");
        journal.failure(20_000, status, &[receiver("open", "access denied", 1)]);
        let gone = journal.failure(30_000, "no Ultra keyboard detected", &[]);
        assert_eq!(events(&gone), ["failures_ended", "failed"]);
        assert_eq!(gone[0]["count"], 2);
        assert_eq!(gone[1]["tries"], json!([]));
    }
}
