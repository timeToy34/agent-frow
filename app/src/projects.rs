//! Stable launch folders, independent of the agent's changing working folder.
//! Runs on the ingestion worker, outside the tracker lock. Only identity
//! metadata is retained; recovering a folder never restores a live lane.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::event::{Event, Kind};

const HEADER_BYTES: u64 = 256 * 1024;
const STORE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;

#[derive(Default)]
pub struct Folders {
    known: BTreeMap<(String, String), (PathBuf, u64)>,
    path: Option<PathBuf>,
    distros: Option<Vec<String>>,
}

impl Folders {
    pub fn open(path: Option<PathBuf>) -> Self {
        let mut folders = Self {
            path,
            ..Self::default()
        };
        let loaded = folders.path.as_ref().and_then(|path| {
            let file = std::fs::File::open(path).ok()?;
            if file.metadata().ok()?.len() > STORE_BYTES {
                return None;
            }
            serde_json::from_reader::<_, Value>(file.take(STORE_BYTES)).ok()
        });
        if let Some(rows) = loaded.as_ref().and_then(Value::as_array) {
            for row in rows.iter().take(MAX_ENTRIES) {
                if let (Some(source), Some(id), Some(folder)) = (
                    row["source"].as_str(),
                    row["session_id"].as_str(),
                    directory(&row["project_dir"]),
                ) && valid_key(source, id)
                {
                    folders.known.insert(
                        (source.to_owned(), id.to_owned()),
                        (folder, row["learned_at"].as_u64().unwrap_or(0)),
                    );
                }
            }
        }
        folders
    }

    /// The first established launch folder wins for this session. Child
    /// events sharing their parent's id must never seed or replace it.
    pub fn attach(&mut self, event: &mut Event, transcript: Option<&str>) {
        if event.subagent || matches!(event.kind, Kind::SubagentStart | Kind::SubagentStop) {
            event.project_dir = None;
            return;
        }
        if !valid_key(&event.source, &event.session_id) {
            return;
        }
        let key = (event.source.clone(), event.session_id.clone());
        if let Some((folder, _)) = self.known.get(&key) {
            event.project_dir = Some(folder.clone());
            return;
        }
        let folder = event
            .project_dir
            .clone()
            .filter(|p| valid_directory(p))
            .or_else(|| {
                if !event.source.starts_with("claude-") {
                    return None;
                }
                let transcript = transcript?;
                let distros = if event.source.starts_with("claude-wsl") {
                    self.distros
                        .get_or_insert_with(crate::agents::wsl_distros)
                        .as_slice()
                } else {
                    &[]
                };
                crate::gauges::candidates(&event.source, transcript, distros)
                    .into_iter()
                    .find_map(|path| claude_launch(&path, &event.session_id))
            })
            .or_else(|| {
                event
                    .launch_dir()
                    .filter(|p| valid_directory(p))
                    .map(Path::to_path_buf)
            });
        event.project_dir = folder.clone();
        if let Some(folder) = folder {
            self.known.insert(key, (folder, event.at));
            while self.known.len() > MAX_ENTRIES {
                let oldest = self
                    .known
                    .iter()
                    .min_by_key(|(_, (_, at))| at)
                    .map(|(key, _)| key.clone());
                if let Some(key) = oldest {
                    self.known.remove(&key);
                }
            }
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let rows: Vec<Value> = self
            .known
            .iter()
            .map(|((source, id), (folder, at))| {
                json!({
                    "source":source, "session_id":id, "project_dir":folder, "learned_at":at,
                })
            })
            .collect();
        let Ok(bytes) = serde_json::to_vec(&rows) else {
            return;
        };
        if bytes.len() as u64 > STORE_BYTES {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(temp, path);
        }
    }
}

fn valid_key(source: &str, id: &str) -> bool {
    (source.starts_with("claude-") || source.starts_with("codex-"))
        && source.len() <= 64
        && !id.trim().is_empty()
        && id.len() <= 256
}

fn valid_directory(path: &Path) -> bool {
    path.has_root() && path.as_os_str().len() <= 4096
}

fn directory(value: &Value) -> Option<PathBuf> {
    let path = PathBuf::from(value.as_str()?.trim());
    valid_directory(&path).then_some(path)
}

/// Read complete records from a bounded prefix. Ignore child and unrelated
/// sessions (including copied fork history), and retain only the first main
/// record's folder. A partial prefix is retried on the next hook.
fn claude_launch(path: &Path, session_id: &str) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(HEADER_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 || line.last() != Some(&b'\n') {
            return None;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value["sessionId"] == session_id
            && value["isSidechain"] == false
            && value.get("agentId").is_none_or(Value::is_null)
            && let Some(folder) = directory(&value["cwd"])
        {
            return Some(folder);
        }
    }
}
