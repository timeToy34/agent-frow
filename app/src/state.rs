//! What a lane is doing, and how an event changes it.
//!
//! A total function over (state, event) with **no request ids, no queues, no
//! tombstones and no timers**. That is the whole design. The previous
//! application tried to correlate a permission prompt to the tool call it
//! belonged to, using a `tool_use_id` that those payloads do not carry; it
//! invented one, nothing ever resolved it, and lanes stuck in Waiting forever.
//!
//! Here, *activity clears Waiting*. Nothing has to be correlated to anything,
//! so nothing can fail to correlate.

use crate::event::{Event, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Alive, nothing run yet.
    Connected,
    Running,
    /// Needs the user.
    Waiting,
    Done,
    Error,
    Interrupted,
}

impl State {
    /// Every state, in the order the window and the settings file list them.
    pub const ALL: [Self; 6] = [
        Self::Connected,
        Self::Running,
        Self::Waiting,
        Self::Done,
        Self::Error,
        Self::Interrupted,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Running => "Running",
            Self::Waiting => "Waiting",
            Self::Done => "Done",
            Self::Error => "Error",
            Self::Interrupted => "Interrupted",
        }
    }

    /// The colour this state gets before the user changes it. Here rather than
    /// in the window, because the same six colours go on the keyboard and the
    /// two must not drift apart.
    pub fn tint(self) -> [u8; 3] {
        match self {
            Self::Connected => [120, 130, 150],
            Self::Running => [80, 170, 255],
            Self::Waiting => [250, 190, 60],
            Self::Done => [90, 210, 130],
            Self::Error => [235, 90, 90],
            Self::Interrupted => [200, 120, 230],
        }
    }

    /// Whether a turn is open. Used only to decide how long a silent session is
    /// kept: dropping a Waiting lane deletes the one fact this product exists
    /// to show.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
}

/// What an event does to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Liveness only. The session is still there; its state is unchanged.
    Stay,
    Set(State),
    /// The session is over.
    Release,
}

/// How a `Notification` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// Somebody has to answer something.
    NeedsUser,
    /// A prompt has sat unanswered for a while.
    Idle,
    /// Known, and says nothing about whether a turn is open.
    Ignored,
    /// Not in the allowlist. Counted, never acted on.
    Unknown,
}

/// Classified by name, allowlist only.
///
/// A notification type nobody has seen before is not evidence of anything, so
/// it changes no state. It is counted instead, because "an agent release added
/// a notification we ignore" should be a number somebody can read rather than
/// behaviour nobody can explain.
pub fn classify(notification_type: &str) -> Note {
    match notification_type {
        "permission_prompt"
        | "agent_needs_input"
        | "elicitation_dialog"
        | "elicitation_url_dialog" => Note::NeedsUser,
        "idle_prompt" => Note::Idle,
        "agent_completed" | "auth_success" | "elicitation_complete" | "elicitation_response" => {
            Note::Ignored
        }
        _ => Note::Unknown,
    }
}

/// The transition table.
pub fn step(current: State, event: &Event) -> Step {
    // A subagent's events carry the *parent's* `session_id`, so acting on them
    // changes the wrong lane. Worse than the stuck lane it replaces: one
    // subagent finishing a tool would clear a Waiting another one raised, and
    // Running is the state you stop looking at.
    if event.subagent {
        return Step::Stay;
    }

    match event.kind {
        // Claude fires SessionStart when it compacts *mid-turn*. Without this
        // guard a live turn drops to Connected and stays there until Stop.
        Kind::SessionStart if event.start_source.as_deref() == Some("compact") => Step::Stay,
        Kind::SessionStart => Step::Set(State::Connected),

        Kind::UserPromptSubmit => Step::Set(State::Running),
        Kind::PermissionRequest => Step::Set(State::Waiting),

        // The prompt was answered with a no — by the user, a rule, or an
        // interrupt. Either way it is no longer pending, and the turn is
        // formally still open: Running until Stop says otherwise, or until the
        // idle notification says the turn died with the prompt. From Waiting
        // only, like tool activity — a denial arriving after Stop must not
        // resurrect a finished turn.
        Kind::PermissionDenied if current == State::Waiting => Step::Set(State::Running),
        Kind::PermissionDenied => Step::Stay,

        Kind::Notification => match event.notification.as_deref().map(classify) {
            Some(Note::NeedsUser) => Step::Set(State::Waiting),
            // "A prompt has sat unanswered" is equally true of a permission
            // dialog nobody has answered. Firing it from Waiting would clear the
            // lane at exactly the moment nobody is at the desk.
            Some(Note::Idle) if current == State::Running => Step::Set(State::Interrupted),
            _ => Step::Stay,
        },

        // Activity clears Waiting, and only Waiting. Several hook processes post
        // concurrently, so a PostToolUse emitted before Stop can arrive after
        // it; promoting from anything else would resurrect a finished turn.
        Kind::PostToolUse | Kind::PostToolUseFailure if current == State::Waiting => {
            Step::Set(State::Running)
        }
        Kind::PostToolUse | Kind::PostToolUseFailure => Step::Stay,

        // A subagent starting or stopping says nothing about what the *main*
        // agent is doing — the tracker keeps the roster; the lane state is the
        // main agent's alone. (The subagent guard above already catches these
        // when the id is present; this is the answer when it is not.)
        Kind::SubagentStart | Kind::SubagentStop => Step::Stay,

        Kind::Stop => Step::Set(State::Done),
        Kind::StopFailure => Step::Set(State::Error),
        Kind::SessionEnd => Step::Release,
    }
}

/// The state a session we have never seen starts in, read from the event that
/// introduced it.
///
/// This is the headline requirement: an agent may be started before or after
/// the app, and the next event picks it up. There is no handshake and nothing
/// to seed — a `PostToolUse` from a session we have never heard of means a tool
/// just ran, so a turn is open, so: Running.
///
/// `None` means "do not create a session from this" — only `SessionEnd`, which
/// is the end of something we never saw the start of.
pub fn adopt(event: &Event) -> Option<State> {
    // A subagent's event proves the session is alive, and proves nothing about
    // what the main agent is doing — a background subagent outlives the turn
    // that spawned it. Connected is the claim that can be stood behind; the
    // subagent roster is what makes the lane read as busy.
    if event.subagent {
        return Some(State::Connected);
    }
    Some(match event.kind {
        // Mid-turn compaction, from a session we do not know: the guard above
        // says "unchanged", and what it is unchanged *from* is a turn in
        // progress.
        Kind::SessionStart if event.start_source.as_deref() == Some("compact") => State::Running,
        Kind::SessionStart => State::Connected,
        Kind::UserPromptSubmit => State::Running,
        Kind::PermissionRequest => State::Waiting,
        Kind::Notification => match event.notification.as_deref().map(classify) {
            Some(Note::NeedsUser) => State::Waiting,
            // Idle means a prompt is unanswered, and from here there is no way
            // to tell whose. Connected is the part we can actually stand
            // behind: something is alive over there.
            _ => State::Connected,
        },
        Kind::PostToolUse | Kind::PostToolUseFailure | Kind::PermissionDenied => State::Running,
        // Without an agent_id these say only that the session exists.
        Kind::SubagentStart | Kind::SubagentStop => State::Connected,
        Kind::Stop => State::Done,
        Kind::StopFailure => State::Error,
        Kind::SessionEnd => return None,
    })
}
