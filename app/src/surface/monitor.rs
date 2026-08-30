//! The monitor as a surface: mini mode.
//!
//! The window folded down to a Stream Deck's picture of the lanes — one row
//! per agent, five keys to a row: the name, the three numbers, the state —
//! lit from the same [`palette`] the keyboards are: the resting glow, the
//! runner, the double-pulse, the marker key, the dark red. A row past the
//! lanes is an off-keyboard session, which has no key on any board but has
//! one here; an empty lane has no row at all, since a row that says nothing
//! only costs the space it takes.
//!
//! A key is a summon and nothing more. The keyboard's and the deck's answer
//! keys are not repeated: a click on the screen is a click, and the surface
//! mirrors the agents rather than driving them. Like the window it lives in,
//! it is never brightness-scaled or colour-corrected — those exist to make
//! LEDs match a screen, and this is the screen.
//!
//! The rows are built here as plain data — the deck's own key vocabulary,
//! [`Face`], [`Label`] and [`Ink`] — so they can be checked without a window;
//! the painting is the one part that needs egui.

use std::time::Duration;

use eframe::egui;

use super::palette;
use super::streamdeck::canvas::{Ink, Label, name_words};
use super::streamdeck::surface::{Face, Role, role, row_colors};
use crate::gauges::Gauges;
use crate::settings::Rgb;
use crate::state::State;
use crate::tracker::{self, Session, Tracker};

/// What an off-keyboard row is lit in: it has no lane, so it has no lane
/// colour. A grey that is plainly not any lane's — and not red, which is
/// Error's alone.
const OFF_KEYBOARD_COLOUR: Rgb = Rgb::new(150, 155, 165);

/// Keys to a row: the name, ctx, 5h, 7d, the state — the deck's row.
const COLS: usize = 5;

/// The smallest the mini window may be squeezed to: one row whose keys can
/// still carry a word each.
pub const MIN_SIZE: [f32; 2] = [300.0, 60.0];

/// The width the mini window starts at, before the user resizes it.
pub const DEFAULT_WIDTH: f32 = 440.0;

/// What one row costs vertically before the user resizes: a key tall enough
/// for a word over a number.
pub const DEFAULT_ROW_HEIGHT: f32 = 64.0;

/// A row cannot be squeezed below this: a number still has to be read.
const MIN_ROW_HEIGHT: f32 = 36.0;

/// The panel's margin, all round.
const MARGIN: f32 = 6.0;

/// The gap between two keys, and between two rows.
const GAP: f32 = 6.0;

/// The corner that resizes an undecorated window.
const GRIP: f32 = 16.0;

/// ~30 Hz while something moves: the runner and the pulse are what this
/// surface is for.
const ANIMATED_FRAME: Duration = Duration::from_millis(33);

/// The window's ordinary pace when nothing moves — enough to keep an elapsed
/// time honest.
const RESTING_FRAME: Duration = Duration::from_millis(500);

/// An unlit key. The keyboard's off is darkness; on a dark ground the keycap
/// still has to be seen, or a row has no shape.
const KEYCAP: egui::Color32 = egui::Color32::from_gray(26);

/// What clicking a key brings forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The session on this lane.
    Lane(usize),
    /// A session named by identity, because it has no lane.
    Session { source: String, id: String },
}

/// One row of the monitor — a lane with a session on it, or an off-keyboard
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub state: State,
    /// The lane's colour; [`OFF_KEYBOARD_COLOUR`] for a session without one.
    pub colour: Rgb,
    /// Five keys, left to right, as the palette lights and labels them
    /// this instant.
    pub keys: Vec<Face>,
    pub target: Target,
    pub off_keyboard: bool,
}

/// What a gesture on the monitor asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Focus(Target),
    /// Back to the full window.
    Leave,
    /// The window is being dragged by its background — it has no title bar.
    Move,
    /// The window is being dragged by its corner.
    Resize,
}

/// The rows: every lane with a session on it, in lane order, then each
/// off-keyboard session. A preview overrides the state of every lane the
/// way it does on the keyboards — no time, no numbers; the off-keyboard rows
/// are not lanes and stay real.
pub fn rows(tracker: &Tracker, now: u64, elapsed_ms: u64) -> Vec<Row> {
    let settings = &tracker.settings;
    let tick = Tick {
        now,
        elapsed_ms,
        preview: tracker.preview.map(|preview| preview.state),
    };
    let mut rows = Vec::new();
    for lane in 0..settings.lane_count {
        let Some(session) = tracker.on_lane(lane) else {
            continue;
        };
        let colour = settings
            .lanes
            .get(lane)
            .map(|lane| lane.color)
            .unwrap_or(palette::OFF);
        rows.push(row_of(
            session,
            settings.display_name(lane, session.project().as_deref()),
            colour,
            Target::Lane(lane),
            false,
            tick,
        ));
    }
    for session in tracker.overflow() {
        rows.push(row_of(
            session,
            session
                .project()
                .unwrap_or_else(|| "unknown project".to_owned()),
            OFF_KEYBOARD_COLOUR,
            Target::Session {
                source: session.source.clone(),
                id: session.session_id.clone(),
            },
            true,
            Tick {
                preview: None,
                ..tick
            },
        ));
    }
    rows
}

/// The moment a row is built for: the clock for "how long", the animation's
/// clock, and the preview when one is playing.
#[derive(Clone, Copy)]
struct Tick {
    now: u64,
    elapsed_ms: u64,
    preview: Option<State>,
}

fn row_of(
    session: &Session,
    name: String,
    colour: Rgb,
    target: Target,
    off_keyboard: bool,
    tick: Tick,
) -> Row {
    let Tick {
        now,
        elapsed_ms,
        preview,
    } = tick;
    let keys = match preview {
        Some(state) => faces(state, colour, &name, "", None, None, elapsed_ms),
        None => faces(
            session.effective_state(),
            colour,
            &name,
            &tracker::elapsed(session.since, now),
            Some(session.gauges),
            session.failure,
            elapsed_ms,
        ),
    };
    Row {
        name,
        state: preview.unwrap_or_else(|| session.effective_state()),
        colour,
        keys,
        target,
        off_keyboard,
    }
}

/// One row's keys, the deck's way: the name first, the state over how long
/// last, and between them the numbers — context used, the five-hour limit,
/// the seven-day limit. In Error the reason takes the time's place under the
/// word, when there is one. Idle is one dim name key and the rest dark, words
/// included; a preview has no time and no numbers.
fn faces(
    state: State,
    colour: Rgb,
    name: &str,
    elapsed: &str,
    gauges: Option<Gauges>,
    reason: Option<&str>,
    elapsed_ms: u64,
) -> Vec<Face> {
    let colours = row_colors(Some(state), colour, COLS, elapsed_ms);
    (0..COLS)
        .map(|col| {
            let key_colour = colours.get(col).copied().unwrap_or(palette::OFF);
            let ink = if state == State::Idle {
                Ink::Quiet
            } else {
                Ink::on(key_colour, colour)
            };
            let label = match role(col, COLS) {
                Role::Name => Label::Name(name.to_owned()),
                Role::Status if state == State::Idle => Label::None,
                Role::Status => Label::Status {
                    state: state.label(),
                    elapsed: match (state, reason) {
                        (State::Error, Some(reason)) => reason.to_owned(),
                        _ => elapsed.to_owned(),
                    },
                },
                Role::Middle(_) if state == State::Idle => Label::None,
                Role::Middle(index) => match (gauges, index) {
                    (Some(gauges), 0) => Label::Gauge {
                        name: "ctx",
                        value: gauges.context_used,
                    },
                    (Some(gauges), 1) => Label::Gauge {
                        name: "5h",
                        value: gauges.five_hour,
                    },
                    (Some(gauges), 2) => Label::Gauge {
                        name: "7d",
                        value: gauges.seven_day,
                    },
                    _ => Label::None,
                },
            };
            Face {
                colour: key_colour,
                label,
                ink,
            }
        })
        .collect()
}

/// The window size for `rows` rows at the user's width and row height,
/// never below [`MIN_SIZE`]. No rows is still one row's worth: a window
/// with nothing in it has to be somewhere to be double-clicked.
pub fn window_size(rows: usize, width: f32, row_height: f32) -> egui::Vec2 {
    let rows = rows.max(1) as f32;
    let row_height = row_height.max(MIN_ROW_HEIGHT);
    let height = rows * row_height + GAP * (rows - 1.0) + MARGIN * 2.0;
    egui::vec2(width.max(MIN_SIZE[0]), height.max(MIN_SIZE[1]))
}

/// The row height that fills `inner_height` with `rows` rows — what the
/// user meant by dragging the corner to that size.
pub fn row_height_for(inner_height: f32, rows: usize) -> f32 {
    let rows = rows.max(1) as f32;
    ((inner_height - MARGIN * 2.0 - GAP * (rows - 1.0)) / rows).max(MIN_ROW_HEIGHT)
}

/// How soon the next frame is due: thirty a second while a row moves, the
/// window's resting pace otherwise.
pub fn repaint_after(rows: &[Row]) -> Duration {
    if rows.iter().any(|row| palette::moves(row.state)) {
        ANIMATED_FRAME
    } else {
        RESTING_FRAME
    }
}

/// The panel the rows sit in: the window's own ground, a small margin.
pub fn panel_frame(visuals: &egui::Visuals) -> egui::Frame {
    egui::Frame::new()
        .fill(visuals.panel_fill)
        .inner_margin(MARGIN)
}

fn colour32(colour: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(colour.r, colour.g, colour.b)
}

fn ink_colour(ink: Ink) -> egui::Color32 {
    match ink {
        Ink::Quiet => egui::Color32::from_gray(140),
        // The deck's ramp, on the screen: as dark as the key is bright.
        Ink::Lit(darkness) => egui::Color32::from_gray(255 - darkness),
    }
}

/// Draws the rows, filling whatever the window is, and says what was done to
/// them.
///
/// The whole area senses clicks and drags *underneath* its widgets — that is
/// what `UiBuilder::sense` is for — so a key takes its own click, a drag
/// anywhere else moves the window (it has no title bar), a double-click
/// anywhere else is the way back to the full window, and the bottom-right
/// corner resizes. `notice` is what the last focus did, shown for a moment
/// over the rows so a focus that found no tab is not silent here.
pub fn paint(ui: &mut egui::Ui, rows: &[Row], notice: Option<&str>) -> Option<Action> {
    let mut action = None;
    let response = ui
        .scope_builder(
            egui::UiBuilder::new().sense(egui::Sense::click_and_drag()),
            |ui| {
                let full = ui.available_rect_before_wrap();
                ui.allocate_rect(full, egui::Sense::hover());
                let painter = ui.painter().with_clip_rect(full);

                if rows.is_empty() {
                    painter.text(
                        full.center(),
                        egui::Align2::CENTER_CENTER,
                        "no agents running",
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_gray(120),
                    );
                }

                let count = rows.len().max(1) as f32;
                let row_height = ((full.height() - GAP * (count - 1.0)) / count).max(1.0);
                let key_width = ((full.width() - GAP * (COLS as f32 - 1.0)) / COLS as f32).max(1.0);
                for (index, row) in rows.iter().enumerate() {
                    let top = full.top() + index as f32 * (row_height + GAP);
                    for (col, face) in row.keys.iter().enumerate() {
                        let left = full.left() + col as f32 * (key_width + GAP);
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(left, top),
                            egui::vec2(key_width, row_height),
                        );
                        let id = ui.id().with(("key", index, col));
                        let response = ui.interact(rect, id, egui::Sense::click());
                        key(&painter, rect, face, response.hovered());
                        if response
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            // Built under the pointer only, not for every
                            // key of every frame.
                            .on_hover_ui(|ui| {
                                ui.label(if row.off_keyboard {
                                    format!("Focus {} (off the keyboard)", row.name)
                                } else {
                                    format!("Focus {}", row.name)
                                });
                            })
                            .clicked()
                        {
                            action = Some(Action::Focus(row.target.clone()));
                        }
                    }
                }

                if let Some(notice) = notice {
                    let strip = egui::Rect::from_min_max(
                        egui::pos2(full.left(), full.bottom() - 20.0),
                        full.max,
                    );
                    painter.rect_filled(strip, 3.0, egui::Color32::from_black_alpha(200));
                    painter.text(
                        egui::pos2(strip.left() + 6.0, strip.center().y),
                        egui::Align2::LEFT_CENTER,
                        notice,
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_gray(200),
                    );
                }

                // The corner: two short diagonals, and a drag there resizes.
                let grip = egui::Rect::from_min_max(
                    egui::pos2(full.right() - GRIP, full.bottom() - GRIP),
                    full.max,
                );
                let grip_response = ui.interact(grip, ui.id().with("grip"), egui::Sense::drag());
                let stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(90));
                for offset in [4.0, 9.0] {
                    painter.line_segment(
                        [
                            egui::pos2(full.right() - offset, full.bottom() - 2.0),
                            egui::pos2(full.right() - 2.0, full.bottom() - offset),
                        ],
                        stroke,
                    );
                }
                if grip_response
                    .on_hover_cursor(egui::CursorIcon::ResizeNwSe)
                    .drag_started()
                {
                    action = Some(Action::Resize);
                }
            },
        )
        .response;
    if action.is_none() {
        if response.double_clicked() {
            action = Some(Action::Leave);
        } else if response.drag_started() {
            action = Some(Action::Move);
        }
    }
    action
}

/// One key: its colour, and its words in the ink that reads on it.
fn key(painter: &egui::Painter, rect: egui::Rect, face: &Face, hovered: bool) {
    let fill = if face.colour == palette::OFF {
        KEYCAP
    } else {
        colour32(face.colour)
    };
    let stroke = if hovered {
        egui::Stroke::new(1.0, egui::Color32::WHITE)
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_gray(40))
    };
    painter.rect_filled(rect, 5.0, fill);
    painter.rect_stroke(rect, 5.0, stroke, egui::StrokeKind::Inside);

    // Word over number, or name alone: sized to the key so a shorter row
    // gets smaller type rather than clipped type.
    let big = (rect.height() * 0.30).clamp(10.0, 20.0);
    let small = (big * 0.75).max(9.0);
    let colour = ink_colour(face.ink);
    let painter = painter.with_clip_rect(rect);
    let (room, height) = (rect.width() - 8.0, rect.height() - 6.0);
    let galleys: Vec<_> = match &face.label {
        Label::None => Vec::new(),
        Label::Name(name) => vec![fit_name(&painter, name, big, colour, room, height)],
        Label::Status { state, elapsed } if elapsed.is_empty() => {
            vec![fit(&painter, state, big, colour, room)]
        }
        Label::Status { state, elapsed } => vec![
            fit(&painter, state, big, colour, room),
            fit(&painter, elapsed, small, colour, room),
        ],
        Label::Gauge { name, value } => vec![
            fit(&painter, name, small, colour, room),
            fit(
                &painter,
                &value
                    .map(|v| format!("{v}%"))
                    .unwrap_or_else(|| "—".to_owned()),
                big,
                colour,
                room,
            ),
        ],
        // The triangles egui's own font has; ▲ and ▼ come out as boxes.
        Label::Up => vec![fit(&painter, "⏶", big, colour, room)],
        Label::Down => vec![fit(&painter, "⏷", big, colour, room)],
        Label::Enter => vec![fit(&painter, "Enter", big, colour, room)],
    };
    if galleys.is_empty() {
        return;
    }
    let total: f32 = galleys.iter().map(|galley| galley.size().y).sum();
    let mut y = rect.center().y - total / 2.0;
    for galley in galleys {
        let height = galley.size().y;
        painter.galley(egui::pos2(rect.center().x, y), galley, colour);
        y += height;
    }
}

/// The most lines a name is spread over — the deck's number.
const NAME_LINES: usize = 3;

/// A name the deck's way: its words — dots and dashes break like spaces, so
/// `ai-agent-keeb` reads as three words rather than one long one — at the
/// largest size that sets them whole on at most [`NAME_LINES`] lines within
/// the key. Fewer, larger lines win; when nothing fits whole, one line at
/// the floor, cut.
fn fit_name(
    painter: &egui::Painter,
    name: &str,
    size: f32,
    colour: egui::Color32,
    room: f32,
    height: f32,
) -> std::sync::Arc<egui::Galley> {
    let words = name_words(name);
    let text = words.join(" ");
    let floor = (size * 0.45).max(7.0);
    let mut scale = size;
    while scale >= floor {
        let font = egui::FontId::proportional(scale);
        // Whole words only: egui breaks inside a word that fits no line, and
        // that is exactly the cut this is here to avoid.
        let whole = words.iter().all(|word| {
            painter
                .layout_no_wrap(word.clone(), font.clone(), colour)
                .size()
                .x
                <= room
        });
        if whole {
            let galley = centred(painter, &text, scale, colour, Some(room), false);
            if galley.rows.len() <= NAME_LINES && galley.size().y <= height {
                return galley;
            }
        }
        scale -= 1.0;
    }
    centred(painter, &text, floor, colour, Some(room), true)
}

/// One line of a key, on one line: smaller before cut, and cut with an
/// ellipsis before spilling.
fn fit(
    painter: &egui::Painter,
    text: &str,
    size: f32,
    colour: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    for scale in [1.0, 0.85, 0.7] {
        let galley = centred(painter, text, size * scale, colour, None, false);
        if galley.size().x <= max_width {
            return galley;
        }
    }
    centred(painter, text, size * 0.7, colour, Some(max_width), true)
}

/// `text` set centred — every row of it, not only the block — at `size`,
/// wrapped at `wrap` when given, and cut to one row with an ellipsis when
/// `elide`. Painted at a point, it spreads either side of it.
fn centred(
    painter: &egui::Painter,
    text: &str,
    size: f32,
    colour: egui::Color32,
    wrap: Option<f32>,
    elide: bool,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(size),
        colour,
        wrap.unwrap_or(f32::INFINITY),
    );
    job.halign = egui::Align::Center;
    if elide {
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
    }
    painter.layout_job(job)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::tracker::Preview;
    use std::path::PathBuf;

    fn session(state: State, lane: Option<usize>, cwd: &str) -> Session {
        Session {
            source: "claude-win".to_owned(),
            session_id: format!("s-{}", cwd.replace('/', "-")),
            agent: None,
            cwd: Some(PathBuf::from(cwd)),
            state,
            since: 0,
            first_seen: 0,
            last_event: 0,
            events: 1,
            note: String::new(),
            subagents: Default::default(),
            lane,
            wt_session: None,
            ancestors: Vec::new(),
            gauges: Default::default(),
            failure: None,
        }
    }

    fn tracker(lane_count: usize) -> Tracker {
        let mut tracker = Tracker::default();
        tracker.settings.set_lane_count(lane_count);
        tracker
    }

    fn label(row: &Row, col: usize) -> &Label {
        &row.keys[col].label
    }

    #[test]
    fn only_lanes_with_a_session_are_rows_then_the_off_keyboard_sessions() {
        let mut tracker = tracker(3);
        tracker
            .sessions
            .push(session(State::Running, Some(1), "/home/j/api"));
        tracker
            .sessions
            .push(session(State::Done, None, "/home/j/extra"));
        let rows = rows(&tracker, 5_000, 0);
        assert_eq!(rows.len(), 2, "one lane in use and one off the keyboard");
        let api = &rows[0];
        assert_eq!(api.name, "api");
        assert_eq!(api.target, Target::Lane(1));
        assert_eq!(api.state, State::Running);
        assert_eq!(api.colour, tracker.settings.lanes[1].color);
        assert_eq!(api.keys.len(), COLS);
        assert!(!api.off_keyboard);
        let extra = &rows[1];
        assert!(extra.off_keyboard);
        assert_eq!(extra.name, "extra");
        assert_eq!(extra.colour, OFF_KEYBOARD_COLOUR);
        assert_eq!(extra.keys.len(), COLS);
        assert_eq!(
            extra.target,
            Target::Session {
                source: "claude-win".to_owned(),
                id: "s--home-j-extra".to_owned(),
            }
        );
    }

    #[test]
    fn an_empty_board_has_no_rows() {
        assert!(rows(&tracker(4), 0, 0).is_empty());
    }

    #[test]
    fn a_row_reads_name_numbers_state_left_to_right() {
        let mut tracker = tracker(3);
        let mut running = session(State::Running, Some(0), "/home/j/api");
        running.gauges = Gauges {
            context_used: Some(42),
            five_hour: None,
            seven_day: Some(3),
        };
        tracker.sessions.push(running);
        let row = rows(&tracker, 80_000, 0).remove(0);
        assert_eq!(*label(&row, 0), Label::Name("api".to_owned()));
        assert_eq!(
            *label(&row, 1),
            Label::Gauge {
                name: "ctx",
                value: Some(42)
            }
        );
        assert_eq!(
            *label(&row, 2),
            Label::Gauge {
                name: "5h",
                value: None
            }
        );
        assert_eq!(
            *label(&row, 3),
            Label::Gauge {
                name: "7d",
                value: Some(3)
            }
        );
        assert_eq!(
            *label(&row, 4),
            Label::Status {
                state: "Running",
                elapsed: "1m 20s".to_owned()
            }
        );
    }

    #[test]
    fn a_preview_lights_a_row_with_its_state_and_no_time_or_numbers() {
        let mut tracker = tracker(3);
        tracker
            .sessions
            .push(session(State::Done, Some(0), "/home/j/api"));
        tracker.preview = Some(Preview {
            state: State::Waiting,
            expires_at: 10,
        });
        let rows = rows(&tracker, 5, 0);
        assert_eq!(rows.len(), 1, "a preview adds no rows for empty lanes");
        let row = &rows[0];
        assert_eq!(row.state, State::Waiting);
        assert_eq!(
            *label(row, 4),
            Label::Status {
                state: "Waiting",
                elapsed: String::new()
            }
        );
        assert_eq!(*label(row, 1), Label::None, "a preview has no numbers");
        assert_eq!(row.keys[0].colour, row.colour, "the marker key at full");
    }

    #[test]
    fn waiting_beats_the_middle_keys_and_never_puts_up_answer_keys() {
        let mut tracker = tracker(3);
        tracker
            .sessions
            .push(session(State::Waiting, Some(0), "/home/j/api"));
        let colour = tracker.settings.lanes[0].color;
        let mut seen_high = false;
        let mut seen_low = false;
        for elapsed in (0..1200).step_by(25) {
            let row = rows(&tracker, 0, elapsed).remove(0);
            assert_eq!(row.keys[0].colour, colour, "the marker never moves");
            assert_eq!(
                row.keys[4].colour,
                palette::base(colour),
                "the state key holds still, as on the deck"
            );
            for key in &row.keys[1..4] {
                assert!(
                    matches!(key.label, Label::Gauge { .. }),
                    "the middle keys are numbers, never ▲▼Enter: {:?}",
                    key.label
                );
            }
            if row.keys[1].colour == colour {
                seen_high = true;
            }
            if row.keys[1].colour == palette::base(colour) {
                seen_low = true;
            }
        }
        assert!(
            seen_high && seen_low,
            "the pulse beats between glow and full"
        );
    }

    #[test]
    fn idle_is_one_quiet_name_key_and_error_says_why() {
        let mut tracker = tracker(3);
        tracker
            .sessions
            .push(session(State::Idle, Some(0), "/home/j/api"));
        let mut failed = session(State::Error, Some(1), "/home/j/web");
        failed.failure = Some("rate limit");
        tracker.sessions.push(failed);
        let rows = rows(&tracker, 60_000, 0);
        let idle = &rows[0];
        assert_eq!(*label(idle, 0), Label::Name("api".to_owned()));
        assert!(idle.keys[1..].iter().all(|key| key.label == Label::None));
        assert!(idle.keys.iter().all(|key| key.ink == Ink::Quiet));
        let error = &rows[1];
        assert_eq!(
            *label(error, 4),
            Label::Status {
                state: "Error",
                elapsed: "rate limit".to_owned()
            }
        );
        assert!(
            error.keys[1..4]
                .iter()
                .all(|key| key.colour == palette::DARK_RED),
            "red in the middle, as everywhere"
        );
    }

    #[test]
    fn the_window_fits_its_rows_and_a_resize_is_read_back_as_a_row_height() {
        let one = window_size(1, DEFAULT_WIDTH, DEFAULT_ROW_HEIGHT);
        let three = window_size(3, DEFAULT_WIDTH, DEFAULT_ROW_HEIGHT);
        assert!(three.y > one.y);
        assert_eq!(
            window_size(0, DEFAULT_WIDTH, DEFAULT_ROW_HEIGHT),
            one,
            "no rows is still one row's worth"
        );
        assert!(one.x >= MIN_SIZE[0] && one.y >= MIN_SIZE[1]);
        assert_eq!(window_size(2, 10.0, 1.0).x, MIN_SIZE[0], "never narrower");
        // What the user dragged the corner to comes back as the row height
        // that reproduces it.
        let height = row_height_for(three.y, 3);
        assert!((height - DEFAULT_ROW_HEIGHT).abs() < 0.01, "{height}");
        assert_eq!(row_height_for(0.0, 3), MIN_ROW_HEIGHT);
    }

    #[test]
    fn only_a_moving_row_asks_for_frames() {
        let mut tracker = tracker(3);
        tracker
            .sessions
            .push(session(State::Done, Some(0), "/home/j/api"));
        assert_eq!(repaint_after(&rows(&tracker, 0, 0)), RESTING_FRAME);
        tracker.sessions[0].state = State::Running;
        assert_eq!(repaint_after(&rows(&tracker, 0, 0)), ANIMATED_FRAME);
        assert_eq!(repaint_after(&[]), RESTING_FRAME);
    }
}
