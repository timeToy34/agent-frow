//! Lane configuration, saved agents, and keeping both on disk.
//!
//! The F-row has twelve keys: three lanes of four. A lane is where a session
//! is shown; each device carries as many lanes as it has keys for — the F-row
//! three, the numpad's M column five, a deck one per row — and the window and
//! mini mode carry every lane. A saved agent is an
//! `(agent, project folder)` the user asked the app to remember, with the lane
//! it would rather come back to.
//!
//! A file that does not parse is **refused and left untouched**. Starting from
//! defaults is recoverable; silently overwriting colours somebody hand-edited
//! is not.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Keys on the F-row. Fixed by the keyboard, not a preference.
pub const KEYS: usize = 12;

/// How the F-row is cut: a lane is four keys — the first summons its agent,
/// the other three answer it while it is Waiting — the shape of a deck row
/// without its state key. Not a layout to choose: the lane count is a setting
/// of its own, and a lane past [`KEYBOARD_LANES`] simply has no F-row keys.
pub const KEYS_PER_LANE: usize = 4;

/// How many lanes have keys on the keyboard.
pub const KEYBOARD_LANES: usize = KEYS / KEYS_PER_LANE;

/// How many lanes the numpad's M column carries: M1–M5. A lane past them is
/// shown in the window and on a deck, never on the numpad.
pub const NUMPAD_LANES: usize = 5;

/// Configuration is always kept for six lanes, so going down to three and
/// back does not lose the names and colours of the other three.
pub const MAX_LANES: usize = 6;

/// The lane counts a user may pick: the keyboard's three, up to the six
/// configuration is kept for. A lane past the F-row is still on the numpad
/// up to its five and on a deck with the rows, and always in the window and
/// in mini mode.
pub const LANE_COUNTS: RangeInclusive<usize> = KEYBOARD_LANES..=MAX_LANES;

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

/// What a device is sent beyond the palette: how bright, and a per-channel
/// gain. One per connected device, because a keyboard whose blue runs hot
/// and a deck whose LCD is true are two different corrections.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub brightness: f32,
    /// Per-channel gain (R, G, B) multiplied into what the keys are sent —
    /// calibration for LEDs that do not match the screen. Unity is
    /// untouched; the window is never corrected.
    pub color_gain: [f32; 3],
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            brightness: 0.8,
            color_gain: [1.0, 1.0, 1.0],
        }
    }
}

/// Where the mini window was left, and how big: its top-left corner on the
/// screen, its width, and the height of one of its rows — the size is kept
/// by the row so that a row arriving or leaving changes the window by
/// exactly one row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MiniWindow {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub row_height: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub lane_count: usize,
    /// Always [`MAX_LANES`] long; `lane_count` says how many are live.
    pub lanes: Vec<Lane>,
    /// The roster, in the order the user saved them. Earlier entries win when
    /// two would claim the same lane at once.
    pub saved: Vec<SavedAgent>,
    /// What a device is sent when it has no tuning of its own — and what a
    /// settings file from before per-device tuning meant by its one slider.
    pub tuning: Tuning,
    /// Each connected device's own tuning, by the surface's name.
    pub devices: BTreeMap<String, Tuning>,
    /// Devices the user has unticked, by the surface's name: plugged in,
    /// found, and left alone. Absent means on.
    pub disabled: BTreeSet<String>,
    /// Whether the Settings section of the window is unfolded. Remembered so
    /// the window opens the way it was left.
    pub settings_open: bool,
    /// Whether the window is in mini mode — only the lanes, small and on
    /// top. Remembered for the same reason.
    pub mini: bool,
    /// Where the mini window was left and how big, once it has been placed.
    pub mini_window: Option<MiniWindow>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lane_count: KEYBOARD_LANES,
            lanes: (0..MAX_LANES).map(Lane::default_at).collect(),
            saved: Vec::new(),
            tuning: Tuning::default(),
            devices: BTreeMap::new(),
            disabled: BTreeSet::new(),
            settings_open: false,
            mini: false,
            mini_window: None,
        }
    }
}

impl Settings {
    /// What `surface` is sent: its own tuning, or the shared one until it
    /// has been given one.
    pub fn tuning(&self, surface: &str) -> Tuning {
        self.devices.get(surface).copied().unwrap_or(self.tuning)
    }

    /// `surface`'s own tuning, to edit — started from the shared one the
    /// first time.
    pub fn tuning_mut(&mut self, surface: &str) -> &mut Tuning {
        let fallback = self.tuning;
        self.devices.entry(surface.to_owned()).or_insert(fallback)
    }

    /// Whether `surface` is to be driven at all. An unticked device stays
    /// plugged in and found; the app just leaves it alone.
    pub fn device_enabled(&self, surface: &str) -> bool {
        !self.disabled.contains(surface)
    }

    pub fn set_device_enabled(&mut self, surface: &str, enabled: bool) {
        if enabled {
            self.disabled.remove(surface);
        } else {
            self.disabled.insert(surface.to_owned());
        }
    }

    pub fn set_lane_count(&mut self, count: usize) {
        if LANE_COUNTS.contains(&count) {
            self.lane_count = count;
        }
    }

    /// Whether the user gave this lane a name of its own.
    pub fn named(&self, index: usize) -> bool {
        self.lanes
            .get(index)
            .is_some_and(|lane| !lane.name.trim().is_empty())
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
    // The shared tuning keeps the keys a pre-device file used, so an old
    // file still means what it meant.
    read_tuning(&object, &mut settings.tuning);
    if let Some(devices) = object.get("devices").and_then(Value::as_object) {
        for (surface, value) in devices {
            if let Some(entry) = value.as_object() {
                let mut tuning = settings.tuning;
                read_tuning(entry, &mut tuning);
                settings.devices.insert(surface.clone(), tuning);
            }
        }
    }
    if let Some(off) = object.get("disabled").and_then(Value::as_array) {
        for surface in off.iter().filter_map(Value::as_str) {
            settings.disabled.insert(surface.to_owned());
        }
    }
    if let Some(window) = object.get("mini_window").and_then(Value::as_object) {
        let number = |key: &str| window.get(key).and_then(Value::as_f64).map(|v| v as f32);
        if let (Some(x), Some(y), Some(width), Some(row_height)) = (
            number("x"),
            number("y"),
            number("width"),
            number("row_height"),
        ) && [x, y, width, row_height].iter().all(|v| v.is_finite())
        {
            settings.mini_window = Some(MiniWindow {
                x,
                y,
                width,
                row_height,
            });
        }
    }
    if let Some(open) = object.get("settings_open").and_then(Value::as_bool) {
        settings.settings_open = open;
    }
    if let Some(mini) = object.get("mini").and_then(Value::as_bool) {
        settings.mini = mini;
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

/// `brightness` and `color_gain`, from any object that carries them: the
/// file's root for the shared tuning, an entry under `devices` for a device's.
fn read_tuning(object: &Map<String, Value>, tuning: &mut Tuning) {
    if let Some(brightness) = object.get("brightness").and_then(Value::as_f64) {
        tuning.brightness = (brightness as f32).clamp(MIN_BRIGHTNESS, 1.0);
    }
    if let Some(gain) = object.get("color_gain").and_then(Value::as_object) {
        for (key, slot) in ["r", "g", "b"].into_iter().zip(&mut tuning.color_gain) {
            if let Some(value) = gain.get(key).and_then(Value::as_f64) {
                *slot = (value as f32).clamp(COLOR_GAIN_RANGE.0, COLOR_GAIN_RANGE.1);
            }
        }
    }
}

fn write_tuning(object: &mut Map<String, Value>, tuning: Tuning) {
    object.insert("brightness".to_owned(), Value::from(tuning.brightness));
    let mut gain = Map::new();
    for (key, value) in ["r", "g", "b"].into_iter().zip(tuning.color_gain) {
        gain.insert(key.to_owned(), Value::from(value));
    }
    object.insert("color_gain".to_owned(), Value::Object(gain));
}

pub fn to_json(settings: &Settings) -> String {
    let mut root = Map::new();
    root.insert("lane_count".to_owned(), Value::from(settings.lane_count));
    write_tuning(&mut root, settings.tuning);
    let mut devices = Map::new();
    for (surface, tuning) in &settings.devices {
        let mut entry = Map::new();
        write_tuning(&mut entry, *tuning);
        devices.insert(surface.clone(), Value::Object(entry));
    }
    root.insert("devices".to_owned(), Value::Object(devices));
    if !settings.disabled.is_empty() {
        root.insert(
            "disabled".to_owned(),
            Value::Array(
                settings
                    .disabled
                    .iter()
                    .map(|surface| Value::String(surface.clone()))
                    .collect(),
            ),
        );
    }
    root.insert(
        "settings_open".to_owned(),
        Value::Bool(settings.settings_open),
    );
    root.insert("mini".to_owned(), Value::Bool(settings.mini));
    if let Some(window) = settings.mini_window {
        let mut out = Map::new();
        out.insert("x".to_owned(), Value::from(window.x));
        out.insert("y".to_owned(), Value::from(window.y));
        out.insert("width".to_owned(), Value::from(window.width));
        out.insert("row_height".to_owned(), Value::from(window.row_height));
        root.insert("mini_window".to_owned(), Value::Object(out));
    }
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
        let settings = Settings {
            tuning: Tuning {
                color_gain: [0.8, 1.0, 0.6],
                ..Tuning::default()
            },
            ..Settings::default()
        };
        let text = to_json(&settings);
        assert_eq!(parse(&text).unwrap().tuning.color_gain, [0.8, 1.0, 0.6]);

        // Out of range clamps; a missing channel stays at unity.
        let parsed = parse(r#"{"color_gain": {"r": 99.0, "b": -3}}"#).unwrap();
        assert_eq!(parsed.tuning.color_gain, [2.0, 1.0, 0.25]);

        // A settings file from before the field existed is untouched.
        assert_eq!(parse("{}").unwrap().tuning.color_gain, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn each_device_has_its_own_tuning_and_the_old_slider_is_the_fallback() {
        // A pre-device file: its one brightness is what every device gets.
        let old = parse(r#"{"brightness": 0.5}"#).unwrap();
        assert_eq!(old.tuning("Keychron").brightness, 0.5);
        assert_eq!(old.tuning("Stream Deck").brightness, 0.5);

        let mut settings = old;
        settings.tuning_mut("Stream Deck").brightness = 1.0;
        assert_eq!(settings.tuning("Stream Deck").brightness, 1.0);
        assert_eq!(
            settings.tuning("Keychron").brightness,
            0.5,
            "one device's slider moves nothing else"
        );
        // The device's entry started from the shared tuning, gain included.
        assert_eq!(settings.tuning("Stream Deck").color_gain, [1.0, 1.0, 1.0]);

        let parsed = parse(&to_json(&settings)).unwrap();
        assert_eq!(parsed.devices, settings.devices);
        assert_eq!(parsed.tuning, settings.tuning);
        // A device entry missing a field fills it from the shared tuning.
        let partial =
            parse(r#"{"color_gain": {"r": 0.5}, "devices": {"Corsair": {"brightness": 0.3}}}"#)
                .unwrap();
        assert_eq!(partial.tuning("Corsair").color_gain, [0.5, 1.0, 1.0]);
        assert_eq!(partial.tuning("Corsair").brightness, 0.3);
    }

    #[test]
    fn the_mini_window_is_remembered_whole_or_not_at_all() {
        let settings = Settings {
            mini_window: Some(MiniWindow {
                x: 1900.0,
                y: 40.0,
                width: 440.0,
                row_height: 64.0,
            }),
            ..Settings::default()
        };
        assert_eq!(
            parse(&to_json(&settings)).unwrap().mini_window,
            settings.mini_window
        );
        assert_eq!(parse("{}").unwrap().mini_window, None);
        // Half a window is no window: it would open somewhere absurd.
        assert_eq!(
            parse(r#"{"mini_window": {"x": 10, "y": 10}}"#)
                .unwrap()
                .mini_window,
            None
        );
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
        settings.mini = true;
        settings.tuning.brightness = 0.6;
        settings.devices.insert(
            "Stream Deck".to_owned(),
            Tuning {
                brightness: 1.0,
                color_gain: [1.0, 0.9, 0.8],
            },
        );
        settings.disabled.insert("Corsair".to_owned());
        settings.mini_window = Some(MiniWindow {
            x: 12.0,
            y: 34.0,
            width: 500.0,
            row_height: 70.0,
        });
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
            parse(r#"{ "lane_count": 12, "lanes": [ { "name": "Kept", "color": "purple" } ] }"#)
                .unwrap();
        // Twelve lanes is past the six the file keeps room for, so it is not
        // a count.
        assert_eq!(settings.lane_count, Settings::default().lane_count);
        assert_eq!(settings.lanes[0].name, "Kept");
        assert_eq!(settings.lanes[0].color, Settings::default().lanes[0].color);
    }

    #[test]
    fn the_lane_count_is_any_of_three_to_six() {
        let mut settings = Settings::default();
        assert_eq!(settings.lane_count, KEYBOARD_LANES, "the keyboard's three");
        settings.set_lane_count(5);
        assert_eq!(settings.lane_count, 5, "no longer has to divide twelve");
        settings.set_lane_count(2);
        assert_eq!(
            settings.lane_count, 5,
            "fewer than the keyboard has is refused"
        );
        settings.set_lane_count(7);
        assert_eq!(settings.lane_count, 5, "more than is kept for is refused");
        // A file from the days of the 4 × 3 and 6 × 2 layouts still means
        // what it meant: that many lanes.
        assert_eq!(parse(r#"{ "lane_count": 4 }"#).unwrap().lane_count, 4);
        assert_eq!(parse(r#"{ "lane_count": 6 }"#).unwrap().lane_count, 6);
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
