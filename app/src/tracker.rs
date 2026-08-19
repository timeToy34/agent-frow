//! Every session we can currently see, and which lane each one is on.
//!
//! Sessions are never persisted. They come back on their next event, which is
//! the same path an agent started before the app takes — one code path, so
//! there is no second one to be wrong.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agents::Agent;
use crate::event::{Ancestor, Event, Kind, Parsed};
use crate::settings::Settings;
use crate::state::{self, Note, State, Step};

/// How long a resting session is kept after its last event.
///
/// Eviction is what replaces `SessionEnd` where it never comes — Codex's
/// desktop app does not send one, so without this a lane it left behind stays
/// on the keyboard until the app is restarted.
pub const RESTING_IDLE_MS: u64 = 30 * 60 * 1000;

/// How long a Running or Waiting session is kept. Much longer, deliberately:
/// dropping a Waiting lane deletes the one fact this product exists to show,
/// and a turn can legitimately be quiet for a long time while a tool runs.
pub const ACTIVE_IDLE_MS: u64 = 2 * 60 * 60 * 1000;

/// How long a subagent stays on the roster with no events and no
/// `SubagentStop`. The stop event is the real signal — this is the safety net
/// for a subagent that died without one, so a lane cannot read busy forever.
pub const SUBAGENT_IDLE_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Session {
    /// The flavor that sent it: `claude-win`, `codex-wsl`, …
    pub source: String,
    pub session_id: String,
    pub agent: Option<Agent>,
    pub cwd: Option<PathBuf>,
    pub state: State,
    /// When it entered the state it is in.
    pub since: u64,
    pub first_seen: u64,
    pub last_event: u64,
    pub events: u64,
    /// The last thing that happened, for the line under the state.
    pub note: String,
    /// Subagents still at work: agent id → when it was last heard from.
    /// Fed by `SubagentStart` and every event carrying the id; emptied by
    /// `SubagentStop` and, failing that, by silence. Background subagents
    /// outlive the turn that spawned them, which is why a lane can honestly
    /// read Done and busy at once — see [`Session::effective_state`].
    pub subagents: std::collections::BTreeMap<String, u64>,
    pub lane: Option<usize>,
    /// Milestone 4 material, stored and unused.
    pub wt_session: Option<String>,
    /// The processes above the agent, nearest first, with the exe name each
    /// pid had at event time — what summon walks to find and verify the
    /// window the agent is sitting in.
    pub ancestors: Vec<Ancestor>,
}

impl Session {
    /// The project folder's name. Works for a WSL path too: `/` is a separator
    /// on Windows as well, so `Path::file_name` reads both.
    pub fn project(&self) -> Option<String> {
        self.cwd
            .as_ref()
            .and_then(|cwd| cwd.file_name())
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// Whether this agent can report Error and Interrupted at all. Codex has
    /// neither event and emits no `Notification`, so those states are not
    /// "not happening" — they are unobservable, and the window says so rather
    /// than letting their absence read as good news.
    pub fn reports_failure(&self) -> bool {
        self.agent != Some(Agent::Codex)
    }

    /// What the lane should *look like*: the main agent's state, except that a
    /// resting state with subagents still at work shows as Running — work is
    /// genuinely happening on this lane. Waiting, Error and Interrupted always
    /// win: they are the states that need the user.
    pub fn effective_state(&self) -> State {
        if !self.subagents.is_empty() && matches!(self.state, State::Connected | State::Done) {
            return State::Running;
        }
        self.state
    }

    fn idle_allowance(&self) -> u64 {
        if self.effective_state().is_active() {
            ACTIVE_IDLE_MS
        } else {
            RESTING_IDLE_MS
        }
    }
}

/// What the keyboard is doing, reported by the lighting thread so the window
/// can answer "why is the F-row dark?" without anybody reading a log.
#[derive(Debug, Clone, Default)]
pub struct KeyboardStatus {
    pub connected: bool,
    /// How many of our twelve keys this keyboard actually has.
    pub driven: usize,
    pub detail: String,
}

#[derive(Default)]
pub struct Tracker {
    pub settings: Settings,
    pub sessions: Vec<Session>,
    /// When each flavor last reached us, for the Agents panel.
    pub last_seen: BTreeMap<String, u64>,
    pub events: u64,
    /// Notification types that are not in the allowlist, by name and count.
    pub unknown_notifications: BTreeMap<String, u64>,
    /// Hook events we do not register, by name and count.
    pub unrecognised_events: BTreeMap<String, u64>,
    /// Set when the settings file was refused; shown in the window, because
    /// the next thing the user changes will overwrite that file.
    pub settings_error: Option<String>,
    pub keyboard: KeyboardStatus,
    /// A state being auditioned on the physical keyboard: every lane plays it,
    /// in its own colour, until this expires. The window keeps showing the real
    /// sessions — this exists so a pattern can be judged by looking at the keys
    /// without having to manufacture an agent in that state.
    pub preview: Option<Preview>,
    /// What the last summon actually achieved — from the marker key and the
    /// Focus button alike. "Raised, showing the api tab" is a different outcome
    /// from "no tab there matches", and the user can see which they got.
    pub summon: Option<String>,
    /// The last F13–F24 press seen: key index and when. The diagnostic that
    /// tells an unremapped keyboard apart from a broken hook.
    pub last_key: Option<(usize, u64)>,
    /// Why the summon keys are not being captured, when they are not. Its own
    /// field so no later summon report can bury it.
    pub keys_error: Option<String>,
}

/// See [`Tracker::preview`]. Expiry is absolute, so a forgotten preview can
/// never stick: even if the window never draws again, the lighting thread
/// clears it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preview {
    pub state: State,
    pub expires_at: u64,
}

impl Tracker {
    pub fn new(settings: Settings, last_seen: BTreeMap<String, u64>) -> Self {
        Self {
            settings,
            last_seen,
            ..Self::default()
        }
    }

    /// Takes one arriving event.
    pub fn accept(&mut self, parsed: Parsed, now: u64) {
        self.events += 1;
        let event = match parsed {
            Parsed::Event(event) => *event,
            Parsed::Unrecognised { source, name } => {
                self.last_seen.insert(source, now);
                *self.unrecognised_events.entry(name).or_default() += 1;
                return;
            }
        };
        self.last_seen.insert(event.source.clone(), event.at);

        if event.kind == Kind::Notification
            && let Some(kind) = event.notification.as_deref()
            && state::classify(kind) == Note::Unknown
        {
            *self
                .unknown_notifications
                .entry(kind.to_owned())
                .or_default() += 1;
        }

        // Without a session id there is nothing to attach the event to. It
        // still counted, and the flavor is still recorded as alive.
        if event.session_id.is_empty() {
            return;
        }

        match self.find(&event.source, &event.session_id) {
            Some(index) => self.update(index, &event),
            None => self.introduce(&event),
        }
    }

    fn find(&self, source: &str, session_id: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.source == source && session.session_id == session_id)
    }

    fn update(&mut self, index: usize, event: &Event) {
        let step = state::step(self.sessions[index].state, event);
        {
            let session = &mut self.sessions[index];
            session.last_event = event.at;
            session.events += 1;
            session.note = event.note();
            roster(session, event);
            // The project directory is the *main* agent's, and `SessionStart`
            // carries the authoritative launch directory. A subagent may be
            // working in a subfolder (`…/frontend` under a project rooted at
            // `…/`), and its cwd must never become the lane's project — that is
            // exactly the "bound to the wrong folder" bug. A non-start event's
            // cwd is only a fallback, used until a `SessionStart` pins it.
            if !event.subagent
                && event.cwd.is_some()
                && (event.kind == Kind::SessionStart || session.cwd.is_none())
            {
                session.cwd.clone_from(&event.cwd);
            }
            // Other fields fill in whenever they show up, and are never cleared
            // by an event that omits them.
            if event.wt_session.is_some() {
                session.wt_session.clone_from(&event.wt_session);
            }
            if !event.ancestors.is_empty() {
                session.ancestors.clone_from(&event.ancestors);
            }
            match step {
                Step::Stay => {}
                Step::Set(next) if next == session.state => {}
                Step::Set(next) => {
                    session.state = next;
                    session.since = event.at;
                }
                Step::Release => {}
            }
        }
        if step == Step::Release {
            self.sessions.remove(index);
            self.fill_lanes();
        }
    }

    fn introduce(&mut self, event: &Event) {
        let Some(state) = state::adopt(event) else {
            return;
        };
        let mut subagents = std::collections::BTreeMap::new();
        if let Some(id) = &event.agent
            && event.kind != Kind::SubagentStop
        {
            subagents.insert(id.clone(), event.at);
        }
        self.sessions.push(Session {
            source: event.source.clone(),
            session_id: event.session_id.clone(),
            agent: Agent::from_source(&event.source),
            // A session adopted from a subagent's event must not take that
            // subagent's working directory as its project; the main agent's
            // next event (or its SessionStart) sets it.
            cwd: if event.subagent {
                None
            } else {
                event.cwd.clone()
            },
            state,
            since: event.at,
            first_seen: event.at,
            last_event: event.at,
            events: 1,
            note: event.note(),
            subagents,
            lane: None,
            wt_session: event.wt_session.clone(),
            ancestors: event.ancestors.clone(),
        });
        self.fill_lanes();
    }

    /// Drops sessions that have gone quiet for longer than their state allows.
    ///
    /// Called from the window every frame rather than from a timer: there is no
    /// clock in this application, only "what time is it now, given that I am
    /// about to draw something".
    pub fn sweep(&mut self, now: u64) {
        for session in &mut self.sessions {
            // A subagent that went silent without a SubagentStop comes off the
            // roster, or a dead one would keep its lane looking busy forever.
            session
                .subagents
                .retain(|_, last| now.saturating_sub(*last) < SUBAGENT_IDLE_MS);
        }
        let before = self.sessions.len();
        self.sessions
            .retain(|session| now.saturating_sub(session.last_event) < session.idle_allowance());
        if self.sessions.len() != before {
            self.fill_lanes();
        }
    }

    /// Gives every laneless session the best free lane there is.
    ///
    /// Oldest first, so a lane freed by a session ending goes to whoever has
    /// been waiting for one longest rather than to whoever speaks next.
    pub fn fill_lanes(&mut self) {
        let count = self.settings.lane_count;
        // A lane that no longer exists — the layout shrank — is given up here
        // rather than left pointing past the end of the row.
        for session in &mut self.sessions {
            if session.lane.is_some_and(|lane| lane >= count) {
                session.lane = None;
            }
        }
        let mut order: Vec<usize> = (0..self.sessions.len()).collect();
        order.sort_by_key(|index| self.sessions[*index].first_seen);
        for index in order {
            if self.sessions[index].lane.is_some() {
                continue;
            }
            let taken: Vec<usize> = self.sessions.iter().filter_map(|s| s.lane).collect();
            self.sessions[index].lane = claim(&self.settings, &taken, &self.sessions[index]);
        }
    }

    /// What pressing a lane's marker key should raise: the session's reported
    /// Windows ancestry, and the tab names worth trying — what the user called
    /// the lane first, since that is the name they control, then the project.
    ///
    /// `Err` is the reason there is nothing to focus, in the words the window
    /// shows for it.
    pub fn summon_target(&self, lane: usize) -> Result<(Vec<Ancestor>, Vec<String>), String> {
        let Some(session) = self.on_lane(lane) else {
            return Err(format!("lane {} is empty — nothing to focus", lane + 1));
        };
        let project = session.project();
        let mut names = vec![self.settings.display_name(lane, project.as_deref())];
        if let Some(project) = project
            && !names.contains(&project)
        {
            names.push(project);
        }
        Ok((session.ancestors.clone(), names))
    }

    /// Swaps two lanes: name, colour, binding, and whatever session is on
    /// each. The one sanctioned exception to "a live lane is never reordered"
    /// — the user did it, deliberately, so everything travels together and the
    /// lane they picked up is the lane that lands.
    pub fn move_lane(&mut self, from: usize, to: usize) {
        let count = self.settings.lane_count;
        if from >= count || to >= count || from == to {
            return;
        }
        self.settings.lanes.swap(from, to);
        for session in &mut self.sessions {
            session.lane = match session.lane {
                Some(lane) if lane == from => Some(to),
                Some(lane) if lane == to => Some(from),
                other => other,
            };
        }
    }

    /// Drops whatever session is on a lane, at the user's request.
    ///
    /// Display-side only, which is what makes it always safe: if the agent is
    /// actually still alive, its next event re-adopts it. What this really
    /// removes is a session whose agent died without a `SessionEnd` — a killed
    /// terminal, a crash — which otherwise sits until eviction.
    pub fn dismiss(&mut self, lane: usize) {
        let before = self.sessions.len();
        self.sessions.retain(|session| session.lane != Some(lane));
        if self.sessions.len() != before {
            self.fill_lanes();
        }
    }

    /// The session on a lane, if any.
    pub fn on_lane(&self, lane: usize) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|session| session.lane == Some(lane))
    }

    /// Sessions with no lane, in the order they arrived.
    pub fn overflow(&self) -> Vec<&Session> {
        let mut extra: Vec<&Session> = self
            .sessions
            .iter()
            .filter(|session| session.lane.is_none())
            .collect();
        extra.sort_by_key(|session| session.first_seen);
        extra
    }

    pub fn set_lane_count(&mut self, count: usize) {
        self.settings.set_lane_count(count);
        self.fill_lanes();
    }

    /// Re-runs assignment after a binding changed, without moving anybody who
    /// already has a lane.
    pub fn rebind(&mut self) {
        self.fill_lanes();
    }
}

/// Keeps a session's subagent roster true to one event.
///
/// Every event carrying an `agent_id` refreshes that subagent — its tool calls
/// are its heartbeat — a `SubagentStart` enrols it, and its `SubagentStop`
/// retires it. A `SubagentStop` that arrives with no id retires the
/// longest-quiet one: best effort, display-only, and better than ignoring a
/// finish the agent went to the trouble of reporting.
fn roster(session: &mut Session, event: &Event) {
    match (&event.agent, event.kind) {
        (Some(id), Kind::SubagentStop) => {
            session.subagents.remove(id);
        }
        (Some(id), _) => {
            session.subagents.insert(id.clone(), event.at);
        }
        (None, Kind::SubagentStop) => {
            if let Some(oldest) = session
                .subagents
                .iter()
                .min_by_key(|(_, last)| **last)
                .map(|(id, _)| id.clone())
            {
                session.subagents.remove(&oldest);
            }
        }
        _ => {}
    }
}

/// Which lane a session should take.
///
/// A binding wins if its lane is free. Otherwise the first free lane of any
/// kind — including a bound one, because a bound lane standing empty while its
/// session is invisible in the overflow list is worse than a lane being used by
/// the wrong project for a while.
///
/// Nothing here can take a lane away from a session that already holds one.
/// Lane position is identity: a display you glance at teaches you nothing if
/// lane 2 moves while you are looking at it.
fn claim(settings: &Settings, taken: &[usize], session: &Session) -> Option<usize> {
    let free = |index: usize| !taken.contains(&index);
    let lanes = || settings.lanes.iter().enumerate().take(settings.lane_count);

    if let Some(agent) = session.agent {
        let cwd: Option<&Path> = session.cwd.as_deref();
        if let Some((index, _)) = lanes().find(|(index, lane)| {
            free(*index)
                && lane
                    .bind
                    .as_ref()
                    .is_some_and(|bind| bind.matches(agent, cwd))
        }) {
            return Some(index);
        }
    }
    lanes().map(|(index, _)| index).find(|index| free(*index))
}

/// "3s", "1m 20s", "2h 5m" — how long a lane has been in its state.
pub fn elapsed(since_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(since_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}
