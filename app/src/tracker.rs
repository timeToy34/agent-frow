//! Every session we can currently see, and which lane each one is on.
//!
//! Sessions are never persisted. They come back on their next event, which is
//! the same path an agent started before the app takes — one code path, so
//! there is no second one to be wrong.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agents::Agent;
use crate::event::{Ancestor, Event, Kind, Parsed, failure_word};
use crate::gauges::Gauges;
use crate::settings::{SavedAgent, Settings};
use crate::state::{self, Note, State, Step};

/// How long a Done or Connected session stays lit after its last event before
/// silence is worth reporting. Nothing is evicted — the lane demotes to
/// [`State::Idle`] and waits for its user, however long the lunch break.
pub const RESTING_IDLE_MS: u64 = 30 * 60 * 1000;

/// How long a silent Running session keeps glowing before demoting to Idle.
/// Much longer, deliberately: a turn can legitimately be quiet for a long
/// time while a tool runs. Waiting never demotes at all — dropping or dimming
/// a Waiting lane hides the one fact this product exists to show.
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
    /// Context and limits, as last reported; unknown until something
    /// reports them.
    pub gauges: Gauges,
    /// Why the lane is in Error, while it is: the word a `StopFailure`
    /// carried. Cleared the moment the lane is anything else.
    pub failure: Option<&'static str>,
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

    /// Whether this agent can report Error at all. Codex has no failure event
    /// and emits no `Notification`, so that state is not "not happening" — it
    /// is unobservable, and the window says so rather than letting its
    /// absence read as good news.
    pub fn reports_failure(&self) -> bool {
        self.agent != Some(Agent::Codex)
    }

    /// What the lane should *look like*: the main agent's state, except that a
    /// resting state with subagents still at work shows as Running — work is
    /// genuinely happening on this lane. Waiting and Error always win: they
    /// are the states that need the user.
    pub fn effective_state(&self) -> State {
        if !self.subagents.is_empty() && matches!(self.state, State::Connected | State::Done) {
            return State::Running;
        }
        self.state
    }

    /// How long this session's silence takes to become worth reporting, or
    /// `None` for the states that hold regardless: Waiting and Error are
    /// exactly what a user stepped away from and comes back to, and Idle is
    /// already the report.
    fn demotes_after(&self) -> Option<u64> {
        match self.state {
            State::Done | State::Connected => Some(if self.effective_state().is_active() {
                // Subagents still on the roster keep the lane reading busy;
                // give them the long allowance before calling it quiet.
                ACTIVE_IDLE_MS
            } else {
                RESTING_IDLE_MS
            }),
            State::Running => Some(ACTIVE_IDLE_MS),
            State::Waiting | State::Error | State::Idle => None,
        }
    }
}

/// What one keyboard surface is doing, reported by its lighting thread so
/// the window can answer "why is the F-row dark?" without anybody reading a
/// log. One per surface: a Corsair and a Keychron each say their own piece.
#[derive(Debug, Clone, Default)]
pub struct KeyboardStatus {
    /// Which surface is talking: "Corsair", "Keychron", "Stream Deck".
    pub surface: &'static str,
    pub connected: bool,
    /// How many keys this surface is driving.
    pub driven: usize,
    pub detail: String,
}

impl KeyboardStatus {
    /// Driving `driven` of the twelve F-row keys on `model`. A whole row
    /// says the model and nothing more — that it is driven is what the line
    /// being lit means.
    pub fn connected(surface: &'static str, model: &str, driven: usize) -> Self {
        let keys = crate::settings::KEYS;
        let detail = if driven == keys {
            model.to_owned()
        } else {
            format!(
                "{model}: only {driven} of the {keys} F-row keys exist here, so the lanes are incomplete"
            )
        };
        Self::driving(surface, detail, driven)
    }

    /// Driving `driven` keys of something that is not an F-row, described in
    /// the surface's own words.
    pub fn driving(surface: &'static str, detail: String, driven: usize) -> Self {
        Self {
            surface,
            connected: true,
            driven,
            detail,
        }
    }

    /// Not driving anything, and this is why.
    pub fn unavailable(surface: &'static str, detail: String) -> Self {
        Self {
            surface,
            connected: false,
            driven: 0,
            detail,
        }
    }

    /// Not driving anything yet, and still looking.
    pub fn searching(surface: &'static str) -> Self {
        Self::unavailable(surface, String::new())
    }

    /// Left alone: unticked in the window.
    pub fn off(surface: &'static str) -> Self {
        Self::unavailable(surface, "off".to_owned())
    }
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
    /// One entry per lighting surface, in the order they first reported.
    pub keyboards: Vec<KeyboardStatus>,
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

    /// A surface saying what it is doing. Replaces that surface's previous
    /// word; other surfaces keep theirs.
    pub fn report_keyboard(&mut self, status: KeyboardStatus) {
        match self
            .keyboards
            .iter_mut()
            .find(|known| known.surface == status.surface)
        {
            Some(known) => *known = status,
            None => self.keyboards.push(status),
        }
    }

    /// Whether any surface is driving keys right now.
    pub fn keyboard_connected(&self) -> bool {
        self.keyboards.iter().any(|status| status.connected)
    }

    /// Takes one arriving event.
    pub fn accept(&mut self, parsed: Parsed, now: u64) {
        let event = match parsed {
            Parsed::Event(event) => *event,
            Parsed::Unrecognised { source, name } => {
                self.events += 1;
                self.last_seen.insert(source, now);
                *self.unrecognised_events.entry(name).or_default() += 1;
                return;
            }
        };
        self.last_seen.insert(event.source.clone(), event.at);

        // A status line is numbers for a session we hold, and nothing else:
        // not an event to count, not activity, not a session to introduce.
        // One for a session we do not hold is ordinary — Claude re-runs it
        // after a session has ended, and the app may have started late.
        if event.kind == Kind::StatusLine {
            if let Some(gauges) = event.gauges
                && let Some(index) = self.find(&event.source, &event.session_id)
            {
                self.sessions[index].gauges.merge(gauges);
            }
            return;
        }
        self.events += 1;

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
        let mut learned_cwd = false;
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
                let first_cwd = session.cwd.is_none();
                session.cwd.clone_from(&event.cwd);
                // A session adopted from a subagent's event started without a
                // cwd, so it could not be recognised as a saved agent and may
                // be waiting off the keyboard. Now it can: give assignment
                // another look.
                if first_cwd && session.lane.is_none() {
                    learned_cwd = true;
                }
            }
            // Other fields fill in whenever they show up, and are never cleared
            // by an event that omits them.
            if event.wt_session.is_some() {
                session.wt_session.clone_from(&event.wt_session);
            }
            if !event.ancestors.is_empty() {
                session.ancestors.clone_from(&event.ancestors);
            }
            if let Some(gauges) = event.gauges {
                session.gauges.merge(gauges);
            }
            if event.kind == Kind::StopFailure {
                session.failure = Some(failure_word(event.error_type.as_deref()));
            }
            match step {
                Step::Stay => {}
                Step::Set(next) if next == session.state => {}
                Step::Set(next) => {
                    session.state = next;
                    session.since = event.at;
                    if next != State::Error {
                        session.failure = None;
                    }
                }
                Step::Release => {}
            }
        }
        if step == Step::Release {
            self.sessions.remove(index);
            self.fill_lanes();
        } else if learned_cwd {
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
            gauges: event.gauges.unwrap_or_default(),
            failure: (event.kind == Kind::StopFailure)
                .then(|| failure_word(event.error_type.as_deref())),
        });
        self.fill_lanes();
    }

    /// Demotes sessions that have gone quiet for longer than their state
    /// allows. Demotes, never drops: eviction was a timer guessing "probably
    /// dead", while [`State::Idle`] states a fact — nothing heard for this
    /// long — and the session keeps its lane for whenever its user returns.
    /// Only `SessionEnd` and the user's ✕ remove a session.
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
        for index in 0..self.sessions.len() {
            let session = &self.sessions[index];
            if let Some(allowance) = session.demotes_after()
                && now.saturating_sub(session.last_event) >= allowance
            {
                let session = &mut self.sessions[index];
                session.state = State::Idle;
                session.since = now;
            }
        }
    }

    /// Gives every laneless session the best free lane there is.
    ///
    /// Oldest first, so a lane freed by a session ending goes to whoever has
    /// been waiting for one longest rather than to whoever speaks next.
    pub fn fill_lanes(&mut self) {
        let count = self.settings.lane_count;
        // A lane that no longer exists — the lane count shrank — is given up here
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
        Ok(self.summon_of(session, Some(lane)))
    }

    /// [`Self::summon_target`] for a session named by identity rather than by
    /// lane — the off-keyboard cards use it. Without a lane there is no lane
    /// name, so the project is the only tab worth trying.
    pub fn summon_session(
        &self,
        source: &str,
        session_id: &str,
    ) -> Result<(Vec<Ancestor>, Vec<String>), String> {
        let Some(index) = self.find(source, session_id) else {
            return Err("that session is gone — nothing to focus".to_owned());
        };
        let session = &self.sessions[index];
        Ok(self.summon_of(session, session.lane))
    }

    fn summon_of(&self, session: &Session, lane: Option<usize>) -> (Vec<Ancestor>, Vec<String>) {
        let project = session.project();
        let mut names = Vec::new();
        if let Some(lane) = lane {
            names.push(self.settings.display_name(lane, project.as_deref()));
        }
        if let Some(project) = project
            && !names.contains(&project)
        {
            names.push(project);
        }
        (session.ancestors.clone(), names)
    }

    /// Swaps two lanes: name, colour, the saved agents that prefer each, and
    /// whatever session is on each. The one sanctioned exception to "a live
    /// lane is never reordered" — the user did it, deliberately, so everything
    /// travels together and the lane they picked up is the lane that lands.
    pub fn move_lane(&mut self, from: usize, to: usize) {
        let count = self.settings.lane_count;
        if from >= count || to >= count || from == to {
            return;
        }
        self.settings.lanes.swap(from, to);
        let swapped = |lane: usize| {
            if lane == from {
                to
            } else if lane == to {
                from
            } else {
                lane
            }
        };
        for entry in &mut self.settings.saved {
            entry.lane = swapped(entry.lane);
        }
        for session in &mut self.sessions {
            session.lane = session.lane.map(swapped);
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

    /// Gives an off-keyboard session the bottom lane, at the user's request.
    ///
    /// The incumbent, if any, steps off the keyboard — with ⏶⏷ this is the
    /// second sanctioned exception to "nothing takes a lane away from a
    /// session that already has one": the user did it, deliberately. No
    /// reassignment runs afterwards: whenever a session is off the keyboard
    /// every lane is occupied, so re-running it would only re-seat the
    /// session that just stepped down.
    pub fn promote(&mut self, source: &str, session_id: &str) {
        let Some(index) = self.find(source, session_id) else {
            return;
        };
        if self.sessions[index].lane.is_some() || self.settings.lane_count == 0 {
            return;
        }
        let bottom = self.settings.lane_count - 1;
        for session in &mut self.sessions {
            if session.lane == Some(bottom) {
                session.lane = None;
            }
        }
        self.sessions[index].lane = Some(bottom);
    }

    /// [`Self::dismiss`] for a session named by identity rather than by lane —
    /// how an off-keyboard card is dismissed. Same safety: if the agent is
    /// actually still alive, its next event re-adopts it.
    pub fn dismiss_session(&mut self, source: &str, session_id: &str) {
        let before = self.sessions.len();
        self.sessions
            .retain(|session| !(session.source == source && session.session_id == session_id));
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

    /// Whether a press on this lane's answer keys may type: a session on it
    /// whose effective state is Waiting, and no preview playing — a preview
    /// is a look, not a question. The one question every surface with answer
    /// keys asks, so the F-row and a Stream Deck can never disagree on it.
    pub fn answerable(&self, lane: usize) -> bool {
        self.preview.is_none()
            && self
                .on_lane(lane)
                .is_some_and(|session| session.effective_state() == State::Waiting)
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

    /// Re-runs assignment after the saved roster changed, without moving
    /// anybody who already has a lane.
    pub fn reseat(&mut self) {
        self.fill_lanes();
    }

    /// Whether any session we can see is this saved agent. The roster in the
    /// window lists only the ones that are not — a running one is shown where
    /// it runs.
    pub fn running(&self, saved: &SavedAgent) -> bool {
        self.sessions.iter().any(|session| {
            session
                .agent
                .is_some_and(|agent| saved.matches(agent, session.cwd.as_deref()))
        })
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
/// A saved agent's preferred lane, if it is free. Otherwise the first free
/// lane, whoever might have preferred it — a preference is not a reservation,
/// and landing elsewhere never rewrites it; `settings` is read-only here so
/// that cannot happen by accident.
///
/// The one refinement: a session whose working directory is not known yet
/// cannot be recognised as anybody's save. Hooks post concurrently, and a
/// session adopted from a *subagent's* event deliberately carries no cwd —
/// handing it a lane somebody else prefers is how two agents once came up
/// reversed after an app restart, summoning each other's windows. So while its
/// cwd is unknown it takes a lane nobody prefers when there is one, and any
/// free lane when there is not.
///
/// Nothing here can take a lane away from a session that already holds one.
/// Lane position is identity: a display you glance at teaches you nothing if
/// lane 2 moves while you are looking at it.
fn claim(settings: &Settings, taken: &[usize], session: &Session) -> Option<usize> {
    let count = settings.lane_count;
    let free = |index: usize| index < count && !taken.contains(&index);

    if let Some(agent) = session.agent {
        let cwd: Option<&Path> = session.cwd.as_deref();
        if let Some(entry) = settings
            .saved
            .iter()
            .find(|entry| free(entry.lane) && entry.matches(agent, cwd))
        {
            return Some(entry.lane);
        }
    }
    if session.cwd.is_none()
        && let Some(index) = (0..count).find(|index| free(*index) && !settings.prefers(*index))
    {
        return Some(index);
    }
    (0..count).find(|index| free(*index))
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

/// "12s", "12m", "1h 12m" — how long a lane has been waiting on the user.
/// Seconds only inside the first minute, when they say the question just
/// appeared; past that, minutes: the number is a count, not a stopwatch,
/// and a key that changes once a minute is written once a minute.
pub fn held(since_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(since_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// What a surface shows for a lane in `state` — the state it shows, which
/// for a lane with subagents may not be the one it holds: [`held`] in
/// Waiting, [`elapsed`] anywhere else.
pub fn clock(state: State, since_ms: u64, now_ms: u64) -> String {
    match state {
        State::Waiting => held(since_ms, now_ms),
        _ => elapsed(since_ms, now_ms),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn driving_says_exactly_what_the_surface_said() {
        let status = KeyboardStatus::driving("Stream Deck", "Mk2: 15 keys".to_owned(), 15);
        assert!(status.connected);
        assert_eq!(status.driven, 15);
        assert_eq!(status.detail, "Mk2: 15 keys");
        let f_row = KeyboardStatus::connected("Keychron", "V3 Ultra", 12);
        assert_eq!(f_row.detail, "V3 Ultra", "a whole row is just the model");
        let partial = KeyboardStatus::connected("Keychron", "V0", 10);
        assert!(partial.detail.contains("only 10 of the 12"));
    }

    #[test]
    fn elapsed_is_a_stopwatch() {
        assert_eq!(elapsed(0, 3_000), "3s");
        assert_eq!(elapsed(0, 80_000), "1m 20s");
        assert_eq!(elapsed(0, 7_500_000), "2h 5m");
        assert_eq!(
            elapsed(10, 0),
            "0s",
            "a clock behind the event is not negative"
        );
    }

    #[test]
    fn held_is_a_count() {
        assert_eq!(held(0, 0), "0s");
        assert_eq!(held(0, 59_000), "59s");
        assert_eq!(held(0, 60_000), "1m");
        assert_eq!(held(0, 80_000), "1m", "no seconds past the first minute");
        assert_eq!(held(0, 3_599_000), "59m");
        assert_eq!(held(0, 3_600_000), "1h 0m");
        assert_eq!(held(0, 5_400_000), "1h 30m");
    }

    #[test]
    fn the_clock_counts_in_waiting_and_ticks_everywhere_else() {
        assert_eq!(clock(State::Waiting, 0, 80_000), "1m");
        assert_eq!(clock(State::Running, 0, 80_000), "1m 20s");
        assert_eq!(clock(State::Done, 0, 80_000), "1m 20s");
    }
}
