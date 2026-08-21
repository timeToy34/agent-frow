//! Lane configuration, saved agents, and keeping both on disk.
//!
//! The F-row has twelve keys. A lane is a group of them and is where a session
//! is shown. A saved agent is an `(agent, project folder)` the user asked the
//! app to remember, with the lane it would rather come back to.
//!
//! A file that does not parse is **refused and left untouched**. Starting from
//! defaults is recoverable; silently overwriting colours somebody hand-edited
//! is not.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Keys on the F-row. Fixed by the keyboard, not a preference.
pub const KEYS: usize = 12;

/// The lane counts that divide twelve keys evenly.
pub const LANE_COUNTS: [usize; 3] = [3, 4, 6];

/// Configuration is always kept for the largest layout, so switching to three
/// lanes and back does not lose the names and colours of the other three.
pub const MAX_LANES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    fn from_hex(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 {
            return None;
        }
        let byte = |at: usize| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok();
        Some(Self::new(byte(0)?, byte(2)?, byte(4)?))
    }
}

/// Which agent a saved entry accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFilter {
    Any,
    Claude,
    Codex,
}

impl AgentFilter {
    pub const ALL: [Self; 3] = [Self::Any, Self::Claude, Self::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Any => "any agent",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            _ => Self::Any,
        }
    }
}

/// "Remember this agent in this project" — and the lane it would rather have.
///
/// The lane is a *preference*, not a record: a session that matches takes it
/// when it is free and lands anywhere else when it is not, and landing
/// elsewhere never rewrites it. Only the user changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedAgent {
    pub agent: AgentFilter,
    pub folder: PathBuf,
    /// Zero-based, like everything in the code; the file and the window count
    /// from one.
    pub lane: usize,
}

impl SavedAgent {
    pub fn matches(&self, agent: crate::agents::Agent, cwd: Option<&Path>) -> bool {
        let agent_ok = match self.agent {
            AgentFilter::Any => true,
            AgentFilter::Claude => agent == crate::agents::Agent::Claude,
            AgentFilter::Codex => agent == crate::agents::Agent::Codex,
        };
        agent_ok && cwd.is_some_and(|cwd| same_folder(&self.folder, cwd))
    }

    /// The folder's own name — what a card calls the project.
    pub fn project(&self) -> String {
        self.folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.folder.display().to_string())
    }
}

/// Whether two paths name the same project directory.
///
/// Compared as normalised text, not by asking the filesystem: half of these
/// paths are WSL paths that mean nothing to Windows, and a `canonicalize` that
/// fails would silently stop every saved agent from matching.
pub fn same_folder(left: &Path, right: &Path) -> bool {
    fn normalise(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    }
    normalise(left) == normalise(right)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// Empty means "the project folder's name". Load-bearing: focus finds a
    /// terminal tab by this name.
    pub name: String,
    pub color: Rgb,
}

/// The six default lane colours, chosen to stay apart from each other and from
/// the state colours in [`crate::state`].
const DEFAULT_COLORS: [Rgb; MAX_LANES] = [
    Rgb::new(80, 170, 255),
    Rgb::new(250, 190, 60),
    Rgb::new(90, 210, 130),
    Rgb::new(225, 110, 200),
    Rgb::new(120, 220, 225),
    Rgb::new(245, 140, 70),
];

impl Lane {
    fn default_at(index: usize) -> Self {
        Self {
            name: String::new(),
            color: DEFAULT_COLORS[index % MAX_LANES],
        }
    }
}

/// How bright the keyboard is driven, as a fraction. Never zero: a slider at
/// the bottom of its travel would be indistinguishable from the app being
/// broken, and the way to turn the lighting off is to quit.
pub const MIN_BRIGHTNESS: f32 = 0.05;

/// How far a colour-gain channel may go. Above 1.0 clips on already-full
/// channels, so the range leaves headroom without pretending it is free.
pub const COLOR_GAIN_RANGE: (f32, f32) = (0.25, 2.0);

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub lane_count: usize,
    /// Always [`MAX_LANES`] long; `lane_count` says how many are live.
    pub lanes: Vec<Lane>,
    /// The roster, in the order the user saved them. Earlier entries win when
    /// two would claim the same lane at once.
    pub saved: Vec<SavedAgent>,
    pub brightness: f32,
    /// Per-channel gain (R, G, B) multiplied into what the keys are sent —
    /// calibration for a keyboard whose LEDs do not match the screen. Unity
    /// is untouched; the window is never corrected.
    pub color_gain: [f32; 3],
    /// Whether the Settings section of the window is unfolded. Remembered so
    /// the window opens the way it was left.
    pub settings_open: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lane_count: 4,
            lanes: (0..MAX_LANES).map(Lane::default_at).collect(),
            saved: Vec::new(),
            brightness: 0.8,
            color_gain: [1.0, 1.0, 1.0],
            settings_open: false,
        }
    }
}

impl Settings {
    /// How many keys each lane gets on the keyboard.
    pub fn keys_per_lane(&self) -> usize {
        KEYS / self.lane_count.max(1)
    }

    pub fn set_lane_count(&mut self, count: usize) {
        if LANE_COUNTS.contains(&count) {
            self.lane_count = count;
        }
    }

    /// What a lane is called: what the user typed, or the project on it.
    pub fn display_name(&self, index: usize, project: Option<&str>) -> String {
        let named = self
            .lanes
            .get(index)
            .map(|lane| lane.name.trim())
            .filter(|name| !name.is_empty());
        match (named, project) {
            (Some(name), _) => name.to_owned(),
            (None, Some(project)) => project.to_owned(),
            (None, None) => format!("Lane {}", index + 1),
        }
    }

    /// The first saved entry this session is, if any. A session whose agent
    /// or folder is unknown is nobody's save.
    pub fn saved_matching(
        &self,
        agent: Option<crate::agents::Agent>,
        cwd: Option<&Path>,
    ) -> Option<usize> {
        let agent = agent?;
        self.saved
            .iter()
            .position(|entry| entry.matches(agent, cwd))
    }

    /// Whether any saved agent would rather have this lane.
    pub fn prefers(&self, lane: usize) -> bool {
        self.saved.iter().any(|entry| entry.lane == lane)
    }

    /// The saved agents that would rather have this lane, by roster index.
    pub fn preferring(&self, lane: usize) -> impl Iterator<Item = &SavedAgent> {
        self.saved.iter().filter(move |entry| entry.lane == lane)
    }

    /// Adds an entry unless an equal `(agent, folder)` is already there.
    pub fn remember(&mut self, entry: SavedAgent) -> bool {
        let duplicate = self
            .saved
            .iter()
            .any(|known| known.agent == entry.agent && same_folder(&known.folder, &entry.folder));
        if duplicate {
            return false;
        }
        self.saved.push(entry);
        true
    }
}

pub fn load(path: &Path) -> Result<Settings, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("{error}"))?;
    parse(&text)
}

/// Parsed leniently in the small and strictly in the large: a file that is not
/// JSON, or not an object, is an error nobody should paper over; a single field
/// that has gone missing or odd falls back to its default, because refusing the
/// whole file over one bad colour helps nobody.
///
/// A file from before saved agents existed carried a `bind` on each lane —
/// the same `(agent, folder)`, pinned to that lane. Those are read as saved
/// agents preferring the lane they were on, and never written back in the old
/// shape.
pub fn parse(text: &str) -> Result<Settings, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| format!("{error}"))?;
    let Value::Object(object) = value else {
        return Err("the settings file is not a JSON object".to_owned());
    };

    let mut settings = Settings::default();
    if let Some(count) = object.get("lane_count").and_then(Value::as_u64) {
        settings.set_lane_count(count as usize);
    }
    if let Some(brightness) = object.get("brightness").and_then(Value::as_f64) {
        settings.brightness = (brightness as f32).clamp(MIN_BRIGHTNESS, 1.0);
    }
    if let Some(gain) = object.get("color_gain").and_then(Value::as_object) {
        for (key, slot) in ["r", "g", "b"].into_iter().zip(&mut settings.color_gain) {
            if let Some(value) = gain.get(key).and_then(Value::as_f64) {
                *slot = (value as f32).clamp(COLOR_GAIN_RANGE.0, COLOR_GAIN_RANGE.1);
            }
        }
    }
    if let Some(open) = object.get("settings_open").and_then(Value::as_bool) {
        settings.settings_open = open;
    }
    if let Some(saved) = object.get("saved").and_then(Value::as_array) {
        for entry in saved.iter().filter_map(Value::as_object) {
            // The file counts lanes from one, like the window does.
            let Some(lane) = entry
                .get("lane")
                .and_then(Value::as_u64)
                .filter(|lane| (1..=MAX_LANES as u64).contains(lane))
            else {
                continue;
            };
            if let Some(entry) = saved_agent(entry, lane as usize - 1) {
                settings.remember(entry);
            }
        }
    }
    if let Some(lanes) = object.get("lanes").and_then(Value::as_array) {
        for (index, lane) in lanes.iter().take(MAX_LANES).enumerate() {
            let Some(lane) = lane.as_object() else {
                continue;
            };
            let slot = &mut settings.lanes[index];
            if let Some(name) = lane.get("name").and_then(Value::as_str) {
                slot.name = name.to_owned();
            }
            if let Some(color) = lane
                .get("color")
                .and_then(Value::as_str)
                .and_then(Rgb::from_hex)
            {
                slot.color = color;
            }
            if let Some(legacy) = lane
                .get("bind")
                .and_then(Value::as_object)
                .and_then(|bind| saved_agent(bind, index))
            {
                settings.remember(legacy);
            }
        }
    }
    Ok(settings)
}

/// One `{"agent": …, "folder": …}` object, preferring `lane`. A missing or
/// empty folder is no entry at all.
fn saved_agent(object: &Map<String, Value>, lane: usize) -> Option<SavedAgent> {
    let folder = object.get("folder").and_then(Value::as_str)?;
    if folder.is_empty() {
        return None;
    }
    Some(SavedAgent {
        agent: AgentFilter::parse(object.get("agent").and_then(Value::as_str).unwrap_or("any")),
        folder: PathBuf::from(folder),
        lane,
    })
}

pub fn to_json(settings: &Settings) -> String {
    let mut root = Map::new();
    root.insert("lane_count".to_owned(), Value::from(settings.lane_count));
    root.insert("brightness".to_owned(), Value::from(settings.brightness));
    let mut gain = Map::new();
    for (key, value) in ["r", "g", "b"].into_iter().zip(settings.color_gain) {
        gain.insert(key.to_owned(), Value::from(value));
    }
    root.insert("color_gain".to_owned(), Value::Object(gain));
    root.insert(
        "settings_open".to_owned(),
        Value::Bool(settings.settings_open),
    );
    let lanes: Vec<Value> = settings
        .lanes
        .iter()
        .map(|lane| {
            let mut out = Map::new();
            out.insert("name".to_owned(), Value::String(lane.name.clone()));
            out.insert("color".to_owned(), Value::String(lane.color.to_hex()));
            Value::Object(out)
        })
        .collect();
    root.insert("lanes".to_owned(), Value::Array(lanes));
    let saved: Vec<Value> = settings
        .saved
        .iter()
        .map(|entry| {
            let mut out = Map::new();
            out.insert("agent".to_owned(), Value::from(entry.agent.key()));
            out.insert(
                "folder".to_owned(),
                Value::String(entry.folder.display().to_string()),
            );
            out.insert("lane".to_owned(), Value::from(entry.lane + 1));
            Value::Object(out)
        })
        .collect();
    root.insert("saved".to_owned(), Value::Array(saved));
    serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".to_owned())
}

/// Written through a temp file and renamed, so an interrupted save cannot leave
/// a file that fails to parse — which, given the rule above, would then be
/// refused on every launch afterwards.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, to_json(settings))
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn colour_gain_round_trips_and_clamps() {
        let mut settings = Settings::default();
        settings.color_gain = [0.8, 1.0, 0.6];
        let text = to_json(&settings);
        assert_eq!(parse(&text).unwrap().color_gain, [0.8, 1.0, 0.6]);

        // Out of range clamps; a missing channel stays at unity.
        let parsed = parse(r#"{"color_gain": {"r": 99.0, "b": -3}}"#).unwrap();
        assert_eq!(parsed.color_gain, [2.0, 1.0, 0.25]);

        // A settings file from before the field existed is untouched.
        assert_eq!(parse("{}").unwrap().color_gain, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let mut settings = Settings::default();
        settings.set_lane_count(6);
        settings.lanes[1].name = "Backend".to_owned();
        settings.lanes[1].color = Rgb::new(1, 2, 3);
        settings.saved.push(SavedAgent {
            agent: AgentFilter::Codex,
            folder: PathBuf::from(r"C:\dev\thing"),
            lane: 2,
        });
        settings.saved.push(SavedAgent {
            agent: AgentFilter::Any,
            folder: PathBuf::from("/home/j/other"),
            lane: 5,
        });
        settings.settings_open = true;
        assert_eq!(parse(&to_json(&settings)).unwrap(), settings);
    }

    #[test]
    fn a_legacy_bind_becomes_a_saved_agent_with_that_lane() {
        let settings = parse(
            r##"{
                "lane_count": 4,
                "lanes": [
                    { "name": "", "color": "#50aaff", "bind": null },
                    { "name": "", "color": "#fabe3c", "bind": null },
                    { "name": "", "color": "#5ad282",
                      "bind": { "agent": "claude", "folder": "C:\\dev\\thing" } },
                    { "name": "", "color": "#e16ec8", "bind": null }
                ]
            }"##,
        )
        .unwrap();
        assert_eq!(
            settings.saved,
            vec![SavedAgent {
                agent: AgentFilter::Claude,
                folder: PathBuf::from(r"C:\dev\thing"),
                lane: 2,
            }]
        );
        // Written back in the new shape only.
        let text = to_json(&settings);
        assert!(!text.contains("\"bind\""));
        assert!(text.contains("\"saved\""));
        assert_eq!(parse(&text).unwrap(), settings);
    }

    #[test]
    fn the_file_counts_lanes_from_one_and_drops_what_it_cannot_place() {
        let settings = parse(
            r#"{ "saved": [
                { "agent": "any", "folder": "/a", "lane": 1 },
                { "agent": "any", "folder": "/b", "lane": 0 },
                { "agent": "any", "folder": "/c", "lane": 7 },
                { "agent": "any", "folder": "", "lane": 2 },
                { "agent": "any", "folder": "/A/", "lane": 3 }
            ] }"#,
        )
        .unwrap();
        // Lane 1 in the file is index 0; 0 and 7 are not lanes; an empty folder
        // is nothing; "/A/" is "/a" again and the first entry keeps its lane.
        assert_eq!(settings.saved.len(), 1);
        assert_eq!(settings.saved[0].folder, PathBuf::from("/a"));
        assert_eq!(settings.saved[0].lane, 0);
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused() {
        assert!(parse("{ not json").is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn one_bad_field_does_not_cost_the_whole_file() {
        let settings =
            parse(r#"{ "lane_count": 5, "lanes": [ { "name": "Kept", "color": "purple" } ] }"#)
                .unwrap();
        // 5 does not divide twelve, so it is not one of the offered layouts.
        assert_eq!(settings.lane_count, Settings::default().lane_count);
        assert_eq!(settings.lanes[0].name, "Kept");
        assert_eq!(settings.lanes[0].color, Settings::default().lanes[0].color);
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join("agent-frow-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");

        let mut settings = Settings::default();
        settings.lanes[0].name = "First".to_owned();
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap(), settings);

        settings.lanes[0].name = "Second".to_owned();
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap(), settings);
        // The write goes through a temp file and a rename; a leftover would sit
        // beside the real one forever.
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_agent_matches_the_same_folder_written_differently() {
        let saved = SavedAgent {
            agent: AgentFilter::Any,
            folder: PathBuf::from(r"C:\Dev\Thing\"),
            lane: 0,
        };
        assert!(saved.matches(crate::agents::Agent::Codex, Some(Path::new("c:/dev/thing"))));
        assert!(!saved.matches(crate::agents::Agent::Codex, Some(Path::new("c:/dev/other"))));
        assert!(!saved.matches(crate::agents::Agent::Codex, None));

        let mut settings = Settings::default();
        settings.saved.push(saved);
        assert_eq!(
            settings.saved_matching(
                Some(crate::agents::Agent::Claude),
                Some(Path::new("c:/dev/thing"))
            ),
            Some(0)
        );
        assert_eq!(
            settings.saved_matching(None, Some(Path::new("c:/dev/thing"))),
            None
        );
        assert!(settings.prefers(0));
        assert!(!settings.prefers(1));
    }
}
