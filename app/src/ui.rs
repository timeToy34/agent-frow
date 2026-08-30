//! The tray icon and the window.
//!
//! **Lanes** is the product: what each agent is doing right now, where it is
//! doing it, and for how long — on the keyboard, off it, and the saved agents
//! that are not running. **Settings** folds away underneath it: the keyboard,
//! and which agents this machine has and whether our hook is registered with
//! them.
//!
//! Closing the window hides it to the tray; the ingress keeps running either
//! way, so a lane that changes while the window is closed is correct the moment
//! it is opened again.

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::agents::{self, Found};
use crate::install;
use crate::lastseen;
use crate::settings::{self, AgentFilter, KEYBOARD_LANES, LANE_COUNTS, Rgb, SavedAgent};
use crate::state::State;
use crate::surface::monitor::{self, Target};
use crate::tracker::{self, Tracker};

/// The full window's size when first opened, and the smallest it may be
/// made — also what mini mode puts back on the way out.
pub const FULL_SIZE: [f32; 2] = [760.0, 720.0];
pub const FULL_MIN_SIZE: [f32; 2] = [420.0, 300.0];

/// How long the mini rows show what the last focus did.
const NOTICE_FOR: std::time::Duration = std::time::Duration::from_secs(6);

/// How long the mini window has to hold still before its place and size are
/// written down — a drag is many frames, and one write.
const GEOMETRY_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// One detected agent and everything the window says about it.
pub struct Row {
    pub found: Found,
    pub installed: Vec<String>,
    pub missing: Vec<String>,
    pub stale: Vec<String>,
}

impl Row {
    fn scan(found: Found) -> Self {
        Self {
            installed: install::installed_events(&found),
            missing: install::missing_events(&found),
            stale: install::stale_events(&found),
            found,
        }
    }
}

pub struct App {
    tracker: Arc<Mutex<Tracker>>,
    rows: Vec<Row>,
    install_dir: PathBuf,
    settings_path: Option<PathBuf>,
    status: Option<String>,
    /// Tallest agent card seen last frame, so a row of them can share a height.
    card_height: f32,
    /// Kept alive for the life of the app: dropping it removes the icon.
    tray: Option<TrayIcon>,
    /// The window's Win32 handle, published for the tray thread: bringing a
    /// hidden window to the front needs `SetForegroundWindow` called with the
    /// user's click as the permission, on the thread that received it.
    hwnd: Arc<AtomicIsize>,
    quitting: bool,
    /// Whether the Run-key startup entry exists. Read once and kept, so the
    /// registry is not asked twice a second for something only a click changes.
    autostart: bool,
    /// Mini mode: only the rows, small and on top — the monitor as a surface.
    mini: bool,
    /// The full window's size and place, kept while in mini mode so leaving
    /// it puts the window back the way it was.
    full_size: Option<egui::Vec2>,
    full_pos: Option<egui::Pos2>,
    /// How many rows the mini window was last sized for.
    mini_rows: usize,
    /// The size the mini window was last told to be, until it gets there —
    /// so our own resize is not read back as the user's.
    expected_size: Option<egui::Vec2>,
    /// The mini window's size last frame, to tell a change from a still.
    last_inner: Option<egui::Vec2>,
    /// When the mini window's place or size last changed without yet being
    /// written down.
    geometry_dirty: Option<std::time::Instant>,
    /// What the last focus said, and when it was first seen — shown over the
    /// mini rows for a moment.
    notice: Option<String>,
    notice_at: Option<std::time::Instant>,
}

impl App {
    pub fn new(
        tracker: Arc<Mutex<Tracker>>,
        install_dir: PathBuf,
        settings_path: Option<PathBuf>,
        notice: Option<String>,
    ) -> Self {
        let mini = tracker
            .lock()
            .map(|tracker| tracker.settings.mini)
            .unwrap_or(false);
        Self {
            tracker,
            rows: agents::detect().into_iter().map(Row::scan).collect(),
            install_dir,
            settings_path,
            status: notice,
            card_height: 0.0,
            tray: None,
            hwnd: Arc::new(AtomicIsize::new(0)),
            quitting: false,
            autostart: crate::autostart::enabled(),
            mini,
            full_size: None,
            full_pos: None,
            mini_rows: 0,
            expected_size: None,
            last_inner: None,
            geometry_dirty: None,
            notice: None,
            notice_at: None,
        }
    }

    fn rescan(&mut self) {
        self.rows = agents::detect().into_iter().map(Row::scan).collect();
    }

    /// Builds the tray on the first frame.
    ///
    /// It has to happen on the thread that pumps messages, and that is this one
    /// — eframe's event loop delivers to any window on it, including the hidden
    /// one the tray creates. Building it in `new()` would put it on whichever
    /// thread constructed the app instead.
    fn ensure_tray(&mut self, ctx: &egui::Context) {
        if self.tray.is_some() {
            return;
        }
        let menu = Menu::new();
        let show = MenuItem::new("Open Agent F-Row", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
        if menu.append_items(&[&show, &quit]).is_err() {
            return;
        }
        let Ok(tray) = TrayIconBuilder::new()
            .with_tooltip("Agent F-Row")
            .with_icon(icon())
            .with_menu(Box::new(menu))
            // Left click opens the window; only right click gets the menu.
            // The crate's default hands the menu to both, which makes the
            // ordinary click feel broken.
            .with_menu_on_left_click(false)
            .build()
        else {
            return;
        };
        self.tray = Some(tray);

        // The tray's receivers are global and blocking, so they get their own
        // thread — and that thread acts for itself rather than messaging the
        // UI loop: bringing the window back needs Win32 calls made with the
        // user's click as the foreground permission, on the thread that
        // received it.
        let ctx = ctx.clone();
        let hwnd = Arc::clone(&self.hwnd);
        std::thread::spawn(move || {
            let menu_events = MenuEvent::receiver();
            let tray_events = TrayIconEvent::receiver();
            loop {
                if let Ok(event) = menu_events.try_recv() {
                    if event.id == show_id {
                        reopen(&hwnd, &ctx);
                    } else if event.id == quit_id {
                        // Hand the Keychron and the Stream Deck back and
                        // release the registered hotkeys before the immediate
                        // exit — none happens by itself, since exit does not
                        // unwind. The rest — settings, sessions, iCUE — needs
                        // nothing, and going through the UI loop would hang
                        // exactly when the window is hidden.
                        crate::surface::keychron::restore_now();
                        crate::surface::streamdeck::restore_now();
                        crate::keys::unhook_now();
                        std::process::exit(0);
                    }
                }
                match tray_events.try_recv() {
                    // A released left click opens the window — the ordinary
                    // thing to want from a tray icon. Double clicks arrive as
                    // a click first, so they are covered by the same arm.
                    Ok(TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    })
                    | Ok(TrayIconEvent::DoubleClick {
                        button: tray_icon::MouseButton::Left,
                        ..
                    }) => reopen(&hwnd, &ctx),
                    _ => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }
}

impl eframe::App for App {
    /// Runs before every `ui`, and — while the window is hidden in the tray —
    /// only when something asks for a repaint. Nothing in here draws, and
    /// nothing in here asks for a repaint: a hidden window must go quiet.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.ensure_tray(ctx);
        self.publish_hwnd(frame);

        // Closing hides to the tray. Quit is deliberately only on the tray menu,
        // so the app cannot be shut down by the reflex of closing a window.
        // Hidden is SW_HIDE: no taskbar button, no Alt-Tab entry, and eframe
        // runs no UI pass for a hidden window, so the loop sleeps until the
        // tray asks for it back. Minimize is left alone — that is a taskbar
        // thing, not a tray thing.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.mini {
            self.mini_ui(ui);
            return;
        }
        let now = crate::now_ms();
        let mut rescan = false;
        let mut action: Option<(usize, bool)> = None;
        let mut settings_changed = false;
        let mut focus: Option<FocusRequest> = None;
        let mut enter_mini = false;
        let mut autostart_error: Option<String> = None;

        {
            let Ok(mut tracker) = self.tracker.lock() else {
                return;
            };
            // The only clock in the application: what time is it, given that we
            // are about to draw. No timer thread, nothing to wake up.
            tracker.sweep(now);

            let rows = &self.rows;
            let install_dir = &self.install_dir;
            let status = &self.status;
            let card_height = &mut self.card_height;
            let autostart = &mut self.autostart;
            let autostart_error = &mut autostart_error;

            // A bar rather than a line at the end of the page. What the last
            // action did — an install, or which window a Focus actually
            // raised — is the one thing that must never be below the fold.
            // Built before the page it sits under: panels first, the central
            // area takes what is left.
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.add_space(2.0);
                if let Some(summon) = &tracker.summon {
                    ui.label(summon.clone());
                }
                if let Some(status) = status {
                    ui.label(status.clone());
                }
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Listening on 127.0.0.1:{} · {} events received · installed to {}",
                            crate::ingress::PORT,
                            tracker.events,
                            install_dir.display()
                        ))
                        .small()
                        .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut wanted = *autostart;
                        if ui.checkbox(&mut wanted, "Start with Windows").changed() {
                            // The registry is the truth; the checkbox follows
                            // it rather than the click, so a refused change
                            // shows as a checkbox that did not move.
                            let outcome = if wanted {
                                crate::autostart::enable()
                            } else {
                                crate::autostart::disable()
                            };
                            if let Err(error) = outcome {
                                *autostart_error = Some(error);
                            }
                            *autostart = crate::autostart::enabled();
                        }
                    });
                });
                ui.add_space(2.0);
            });

            egui::CentralPanel::default().show(ui, |ui| {
                // One scroll area for the whole window. Two of them nested was
                // also what left the right-hand column of agent cards clipped:
                // the width was measured outside the scroll area and then used
                // inside it, where the scrollbar had already taken some.
                egui::ScrollArea::vertical()
                    .id_salt("window")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        settings_changed =
                            lanes_panel(ui, &mut tracker, now, &mut focus, &mut enter_mini);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(6.0);
                        let (changed, clicked) =
                            settings_section(ui, rows, &mut tracker, now, card_height, &mut rescan);
                        settings_changed |= changed;
                        action = clicked;
                    });
            });
        }

        if let Some(request) = focus {
            self.request_focus(ui.ctx(), request);
        }
        if enter_mini {
            self.set_mini(ui.ctx(), true);
        }
        if let Some(error) = autostart_error {
            self.status = Some(error);
        }
        if settings_changed {
            self.save_settings();
        }
        if let Some((index, installing)) = action {
            self.status = Some(self.run_action(index, installing));
            self.rescan();
        } else if rescan {
            self.rescan();
        }

        // Cheap, and it keeps every elapsed time and the eviction sweep honest
        // without any plumbing between the ingress thread and the window. Only
        // from here, never from `logic`, and never for a hidden window: this
        // is the one thing that paces the loop. egui cannot tell a hidden
        // window from a shown one on Windows — it knows minimized and
        // occluded, and Windows reports neither for SW_HIDE — so left alone
        // it would keep running this whole pass, twice a second, into a
        // window nobody can see. Windows is asked directly, not a flag of our
        // own: a flag can be wrong, `IsWindowVisible` cannot.
        if self.window_shown() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

impl App {
    /// Mini mode: the monitor as a surface. Only the rows, at the pace the
    /// palette asks for, and no settings — the way back is a double-click.
    fn mini_ui(&mut self, ui: &mut egui::Ui) {
        let now = crate::now_ms();
        // egui's own clock, continuous across frames: the animation's time.
        let elapsed_ms = (ui.input(|i| i.time) * 1000.0) as u64;
        let (rows, summon) = {
            let Ok(mut tracker) = self.tracker.lock() else {
                return;
            };
            // The clock, as in the full view.
            tracker.sweep(now);
            // The report is cloned out once, when it changes, not per frame.
            let changed = (tracker.summon != self.notice).then(|| tracker.summon.clone());
            (monitor::rows(&tracker, now, elapsed_ms), changed)
        };
        let notice = self.notice(summon);
        let mut action = None;
        egui::CentralPanel::default()
            .frame(monitor::panel_frame(ui.visuals()))
            .show(ui, |ui| {
                action = monitor::paint(ui, &rows, notice);
            });
        match action {
            Some(monitor::Action::Focus(Target::Lane(lane))) => {
                self.request_focus(ui.ctx(), FocusRequest::Lane(lane));
            }
            Some(monitor::Action::Focus(Target::Session { source, id })) => {
                self.request_focus(ui.ctx(), FocusRequest::Session(source, id));
            }
            Some(monitor::Action::Leave) => self.set_mini(ui.ctx(), false),
            // No title bar: the background is the handle and the corner the
            // resize, and Windows runs the drag from there.
            Some(monitor::Action::Move) => {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            Some(monitor::Action::Resize) => {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                        egui::ResizeDirection::SouthEast,
                    ));
            }
            None => {}
        }
        if self.mini {
            self.track_mini_geometry(ui.ctx(), rows.len());
        }
        // Thirty a second while something moves, the resting pace otherwise
        // — and never for a hidden window, as in the full view.
        if self.window_shown() {
            ui.ctx()
                .request_repaint_after(monitor::repaint_after(&rows));
        }
    }

    /// What the last focus said, for a moment after it said it.
    fn notice(&mut self, changed: Option<Option<String>>) -> Option<&str> {
        if let Some(summon) = changed {
            self.notice = summon;
            self.notice_at = self.notice.as_ref().map(|_| std::time::Instant::now());
        }
        self.notice_at
            .filter(|at| at.elapsed() < NOTICE_FOR)
            .and(self.notice.as_deref())
    }

    /// Keeps the mini window the size of its rows, and remembers where the
    /// user put it and how big they made it.
    ///
    /// The place is theirs entirely: wherever the window is dragged is
    /// where it opens next time. The size is theirs by the row: a drag of
    /// the corner is read back as a width and a height per row, so a row
    /// arriving or leaving grows or shrinks the window by exactly one row
    /// and the keys keep the size they were given. Written down once the
    /// window has held still for a moment, not on every frame of a drag.
    fn track_mini_geometry(&mut self, ctx: &egui::Context, rows: usize) {
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        let Ok(mut tracker) = self.tracker.lock() else {
            return;
        };
        let known = tracker.settings.mini_window;
        let mut window = known.unwrap_or(settings::MiniWindow {
            x: outer.map_or(0.0, |rect| rect.min.x),
            y: outer.map_or(0.0, |rect| rect.min.y),
            width: monitor::DEFAULT_WIDTH,
            row_height: monitor::DEFAULT_ROW_HEIGHT,
        });
        // A window that has never been written down is, as soon as it has a
        // place at all.
        let mut changed = known.is_none() && outer.is_some();
        if let Some(outer) = outer
            && (outer.min.x != window.x || outer.min.y != window.y)
        {
            window.x = outer.min.x;
            window.y = outer.min.y;
            changed = true;
        }
        if let Some(inner) = inner
            && self.last_inner != Some(inner.size())
        {
            let size = inner.size();
            // A change we asked for lands once; any other is the user's.
            if self.expected_size.take().is_none() && self.last_inner.is_some() {
                window.width = size.x;
                window.row_height = monitor::row_height_for(size.y, rows);
                changed = true;
            }
            self.last_inner = Some(size);
        }
        if rows != self.mini_rows {
            self.mini_rows = rows;
            let size = monitor::window_size(rows, window.width, window.row_height);
            if self.last_inner != Some(size) {
                self.expected_size = Some(size);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
        }
        if changed {
            tracker.settings.mini_window = Some(window);
            self.geometry_dirty = Some(std::time::Instant::now());
        }
        drop(tracker);
        if self
            .geometry_dirty
            .is_some_and(|since| since.elapsed() >= GEOMETRY_SETTLE)
        {
            self.geometry_dirty = None;
            self.save_settings();
        }
    }

    /// Switches between the full window and mini mode, and remembers which.
    ///
    /// Mini has no title bar, is sized to its rows, and sits on top — a
    /// surface on the desk rather than a window among windows — and opens
    /// where it was last left. Leaving it puts back the size, place and
    /// level the full window had.
    fn set_mini(&mut self, ctx: &egui::Context, mini: bool) {
        if mini == self.mini {
            return;
        }
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        let (rows, window, summon) = {
            let Ok(mut tracker) = self.tracker.lock() else {
                return;
            };
            tracker.settings.mini = mini;
            // The first time in, the mini window is where the full window
            // was, at the default size — written down now, with the mode,
            // rather than by the geometry tracker half a second later.
            if mini
                && tracker.settings.mini_window.is_none()
                && let Some(outer) = outer
            {
                tracker.settings.mini_window = Some(settings::MiniWindow {
                    x: outer.min.x,
                    y: outer.min.y,
                    width: monitor::DEFAULT_WIDTH,
                    row_height: monitor::DEFAULT_ROW_HEIGHT,
                });
            }
            let rows = (0..tracker.settings.lane_count)
                .filter(|lane| tracker.on_lane(*lane).is_some())
                .count()
                + tracker.overflow().len();
            (rows, tracker.settings.mini_window, tracker.summon.clone())
        };
        self.mini = mini;
        self.geometry_dirty = None;
        self.save_settings();
        if mini {
            self.full_size = inner.map(|rect| rect.size());
            self.full_pos = outer.map(|rect| rect.min);
            // What the last focus said was read in the full view already.
            self.notice = summon;
            self.notice_at = None;
            let (width, row_height) = window
                .map(|window| (window.width, window.row_height))
                .unwrap_or((monitor::DEFAULT_WIDTH, monitor::DEFAULT_ROW_HEIGHT));
            let size = monitor::window_size(rows, width, row_height);
            self.mini_rows = rows;
            self.last_inner = inner.map(|rect| rect.size());
            self.expected_size = Some(size);
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(
                monitor::MIN_SIZE.into(),
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            if let Some(window) = window {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    window.x, window.y,
                )));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnTop,
            ));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::Normal,
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(FULL_MIN_SIZE.into()));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                self.full_size.unwrap_or(FULL_SIZE.into()),
            ));
            if let Some(pos) = self.full_pos {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        }
    }

    /// Brings an agent's window forward — on its own thread, deliberately.
    /// Two reasons, and both were learned the hard way: raising a window can
    /// spend a quarter of a second waiting for the terminal to agree which
    /// tab is in front, which is a quarter of a second of frozen window; and
    /// UI Automation needs a COM apartment, which the window's own thread has
    /// already been put into a different mode of by the windowing library.
    fn request_focus(&self, ctx: &egui::Context, request: FocusRequest) {
        let tracker = Arc::clone(&self.tracker);
        let target = tracker.lock().ok().map(|tracker| match &request {
            FocusRequest::Lane(lane) => tracker.summon_target(*lane),
            FocusRequest::Session(source, id) => tracker.summon_session(source, id),
        });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = match target {
                Some(Ok((ancestors, names))) => crate::focus::raise(&ancestors, &names).detail,
                Some(Err(reason)) => reason,
                None => return,
            };
            if let Ok(mut tracker) = tracker.lock() {
                tracker.summon = Some(report);
            }
            // The window is behind the terminal we just raised and would
            // otherwise not draw again until something else woke it.
            ctx.request_repaint();
        });
    }

    fn save_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let Ok(mut tracker) = self.tracker.lock() else {
            return;
        };
        match settings::save(&path, &tracker.settings) {
            // Whatever was wrong with the old file is now moot: this one is
            // ours and it parses.
            Ok(()) => tracker.settings_error = None,
            Err(error) => tracker.settings_error = Some(error),
        }
    }

    fn run_action(&mut self, index: usize, installing: bool) -> String {
        let Some(row) = self.rows.get(index) else {
            return "that agent is no longer there".to_owned();
        };
        let name = row.found.flavor.describe();
        // The binaries go first: a configuration naming an executable that is
        // not there yet is a hook that silently does nothing.
        if installing && let Err(error) = install::install_binaries(&self.install_dir) {
            return format!("{name}: {error}");
        }
        let plan = if installing {
            install::plan_install(&row.found, &self.install_dir)
        } else {
            install::plan_remove(&row.found)
        };
        match plan {
            // Clicking a button and seeing nothing happen is worse than a
            // message, so this says which of the two nothings it was.
            Ok(plan) if plan.is_noop() && installing => {
                format!("{name}: already registered — nothing to change")
            }
            Ok(plan) if plan.is_noop() => format!("{name}: nothing of ours to remove"),
            Ok(plan) => match install::apply(&plan) {
                Ok(()) => {
                    let notes = if plan.notes.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", plan.notes.join("; "))
                    };
                    format!(
                        "{name}: {}{notes} — a backup is beside it",
                        if installing { "installed" } else { "removed" }
                    )
                }
                Err(error) => format!("{name}: {error}"),
            },
            Err(error) => format!("{name}: {error}"),
        }
    }
}

/// What the user asked to bring forward: a lane's session, or one named by
/// identity because it has no lane (the off-keyboard cards).
enum FocusRequest {
    Lane(usize),
    Session(String, String),
}

/// What one lane shows. Copied out of the tracker first, so the settings for
/// the same lane can be edited in place without borrowing it twice.
struct LaneView {
    state: State,
    since: u64,
    note: String,
    session_id: String,
    project: Option<String>,
    cwd: Option<PathBuf>,
    agent: Option<agents::Agent>,
    source: String,
    /// Subagents still at work on this session.
    subagents: usize,
    /// Context and limits, as last reported.
    gauges: crate::gauges::Gauges,
    /// Why the lane is in Error, when it is known.
    failure: Option<&'static str>,
}

fn view_of(session: &tracker::Session) -> LaneView {
    LaneView {
        state: session.state,
        since: session.since,
        note: session.note.clone(),
        session_id: session.session_id.clone(),
        project: session.project(),
        cwd: session.cwd.clone(),
        agent: session.agent,
        source: session.source.clone(),
        subagents: session.subagents.len(),
        gauges: session.gauges,
        failure: session.failure,
    }
}

/// Draws the lanes. Returns whether anything about the settings changed.
fn lanes_panel(
    ui: &mut egui::Ui,
    tracker: &mut Tracker,
    now: u64,
    focus: &mut Option<FocusRequest>,
    mini: &mut bool,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.heading("Lanes");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Rightmost: the monitor as a surface.
            if ui
                .small_button("Mini mode")
                .on_hover_text(
                    "Only the lanes, small and on top — the monitor as a surface. \
                     Double-click a lane to get there; double-click there to come back.",
                )
                .clicked()
            {
                *mini = true;
            }
            ui.add_space(8.0);
            // How many lanes, plainly. The keyboard's three always have keys;
            // a lane past them is for an agent with none.
            let mut count = tracker.settings.lane_count;
            for option in LANE_COUNTS.rev() {
                let hover = if option == KEYBOARD_LANES {
                    "Three lanes, all on the keyboard".to_owned()
                } else {
                    format!(
                        "{option} lanes — lanes 1–{KEYBOARD_LANES} on the keyboard, the rest in \
                         the window, in mini mode and on a deck with the rows"
                    )
                };
                if ui
                    .selectable_label(count == option, option.to_string())
                    .on_hover_text(hover)
                    .clicked()
                {
                    count = option;
                }
            }
            if count != tracker.settings.lane_count {
                tracker.set_lane_count(count);
                changed = true;
            }
        });
    });
    let keys = "Four keys per lane on the keyboard: any of them summons the agent, and while \
                it is Waiting the three after the first are ⏶ ⏷ Enter. A lane keeps its \
                position for as long as its session lives.";
    let caption = if tracker.settings.lane_count > KEYBOARD_LANES {
        format!(
            "Lanes {}–{} have no keys. {keys}",
            KEYBOARD_LANES + 1,
            tracker.settings.lane_count
        )
    } else {
        keys.to_owned()
    };
    ui.label(egui::RichText::new(caption).weak());
    ui.add_space(6.0);

    let views: Vec<Option<LaneView>> = (0..tracker.settings.lane_count)
        .map(|lane| tracker.on_lane(lane).map(view_of))
        .collect();

    let mut actions = LaneActions::default();
    for (index, view) in views.iter().enumerate() {
        ui.push_id(index, |ui| {
            changed |= lane_card(
                ui,
                index,
                view.as_ref(),
                &mut tracker.settings,
                now,
                &mut actions,
            );
        });
        ui.add_space(4.0);
    }
    if actions.mini {
        *mini = true;
    }
    if let Some(lane) = actions.focus {
        *focus = Some(FocusRequest::Lane(lane));
    }
    if let Some(lane) = actions.dismiss {
        tracker.dismiss(lane);
    }
    if let Some((from, to)) = actions.moved {
        tracker.move_lane(from, to);
        changed = true;
    }

    // Off the keyboard: sessions that arrived after every lane was taken.
    // Full cards, because an agent without a key is still an agent — tracked,
    // focusable, dismissible — just unlit. The board never runs out of room:
    // when every lane is occupied, the landing spot for the next agent is
    // drawn before it arrives.
    let overflow: Vec<LaneView> = tracker.overflow().into_iter().map(view_of).collect();
    let every_lane_taken = views.iter().all(Option::is_some);
    if !overflow.is_empty() || every_lane_taken {
        ui.add_space(6.0);
        group_label(
            ui,
            "Off the keyboard",
            "fully tracked, no key or light. Nothing here takes a lane away from a \
             session that already has one; the ⏶ you press is the exception.",
        );
        ui.add_space(2.0);
    }
    let mut off = OverflowActions::default();
    for view in &overflow {
        ui.push_id(("off-keyboard", &view.source, &view.session_id), |ui| {
            overflow_card(ui, view, now, &tracker.settings, &mut off);
        });
        ui.add_space(4.0);
    }
    if every_lane_taken {
        empty_overflow_slot(ui);
    }
    if off.mini {
        *mini = true;
    }
    if let Some((source, id)) = off.focus {
        *focus = Some(FocusRequest::Session(source, id));
    }
    if let Some((source, id)) = off.promote {
        tracker.promote(&source, &id);
    }
    if let Some((source, id)) = off.dismiss {
        tracker.dismiss_session(&source, &id);
    }

    // Saved agents that are not running. A running one is shown where it
    // runs, tagged; this is the rest of the roster — what would come back,
    // and where it would rather land — and where a save is edited or dropped.
    let idle: Vec<usize> = (0..tracker.settings.saved.len())
        .filter(|index| !tracker.running(&tracker.settings.saved[*index]))
        .collect();
    if !idle.is_empty() {
        ui.add_space(6.0);
        group_label(
            ui,
            "Saved agents",
            "remembered from a lane, not running now. Each takes its preferred lane \
             when it starts, if the lane is free; otherwise another lane.",
        );
        ui.add_space(2.0);
        for index in idle {
            ui.push_id(("saved", index), |ui| {
                changed |= saved_card(ui, index, &mut tracker.settings, &mut actions);
            });
            ui.add_space(4.0);
        }
    }
    // After every card has drawn: a roster index is only stable until
    // something is removed.
    if let Some(index) = actions.forget
        && index < tracker.settings.saved.len()
    {
        tracker.settings.saved.remove(index);
        changed = true;
        actions.reseat = true;
    }
    if actions.reseat {
        tracker.reseat();
    }

    footnotes(ui, tracker);
    changed
}

/// One lane. Returns whether its settings changed.
/// What a lane card asked its caller to do, gathered rather than passed as a
/// fistful of out-parameters.
#[derive(Default)]
struct LaneActions {
    /// The saved roster changed; assignment gets another look.
    reseat: bool,
    /// Drop this roster entry, by index — applied once every card has drawn.
    forget: Option<usize>,
    focus: Option<usize>,
    moved: Option<(usize, usize)>,
    dismiss: Option<usize>,
    /// A double-click on a card: into mini mode.
    mini: bool,
}

fn lane_card(
    ui: &mut egui::Ui,
    index: usize,
    view: Option<&LaneView>,
    config: &mut settings::Settings,
    now: u64,
    actions: &mut LaneActions,
) -> bool {
    let mut changed = false;
    let lane_color = config.lanes[index].color;
    let accent = egui::Color32::from_rgb(lane_color.r, lane_color.g, lane_color.b);
    let tint = match view {
        Some(view) => {
            let [r, g, b] = view.state.tint();
            egui::Color32::from_rgb(r, g, b).gamma_multiply(0.10)
        }
        None => egui::Color32::TRANSPARENT,
    };

    // The card is a double-click into mini mode. Sensed *underneath* its own
    // widgets — that is what `UiBuilder::sense` is for — so the colour
    // button, the name and every button keep their clicks.
    let card = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 6))
            .corner_radius(6.0)
            .fill(tint)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    // The lane's own colour, which is what it will be on the
                    // keyboard. Editing it here is the whole point of it being a
                    // setting rather than a constant.
                    let mut rgb = [lane_color.r, lane_color.g, lane_color.b];
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        config.lanes[index].color = Rgb::new(rgb[0], rgb[1], rgb[2]);
                        changed = true;
                    }
                    // Which keys this lane is on the keyboard — or that it is
                    // on none, which is worth seeing without hovering.
                    let keys = crate::keys::lane_keys_label(index);
                    ui.label(
                        egui::RichText::new(format!("{}", index + 1))
                            .monospace()
                            .strong()
                            .color(accent),
                    )
                    .on_hover_text(match &keys {
                        Some(span) => format!("{span} on the keyboard"),
                        None => format!(
                            "Not on the keyboard — only lanes 1–{KEYBOARD_LANES} have keys; \
                             shown here, in mini mode and on a deck with the rows"
                        ),
                    });
                    if keys.is_none() {
                        ui.label(egui::RichText::new("no keys").small().weak());
                    }
                    // The name is load-bearing: milestone 4 finds a terminal tab by
                    // it. Empty means "whatever project is on this lane".
                    let hint = view
                        .and_then(|view| view.project.clone())
                        .unwrap_or_else(|| format!("Lane {}", index + 1));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut config.lanes[index].name)
                                .desired_width(150.0)
                                .hint_text(hint),
                        )
                        .changed()
                    {
                        changed = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Rightmost: dismiss the session, for the agent that died
                        // without saying so. Only when there is one to dismiss.
                        if view.is_some()
                            && ui
                                .add(egui::Button::new("❌").small())
                                .on_hover_text(
                                    "Remove this session. If the agent is actually still \
                                     alive, its next event brings it back.",
                                )
                                .clicked()
                        {
                            actions.dismiss = Some(index);
                        }
                        // Moving a lane is the user's call — the app itself never
                        // reorders one. Everything travels together: session, name,
                        // colour, saved preference, keys.
                        if ui
                            .add_enabled(
                                index + 1 < config.lane_count,
                                egui::Button::new("⏷").small(),
                            )
                            .on_hover_text("Move this lane down")
                            .clicked()
                        {
                            actions.moved = Some((index, index + 1));
                        }
                        if ui
                            .add_enabled(index > 0, egui::Button::new("⏶").small())
                            .on_hover_text("Move this lane up")
                            .clicked()
                        {
                            actions.moved = Some((index, index - 1));
                        }
                        match view {
                            Some(view) => {
                                status_and_clock(ui, view, now);
                                if let Some(word) = view.failure {
                                    let [r, g, b] = State::Error.tint();
                                    ui.label(
                                        egui::RichText::new(word)
                                            .small()
                                            .color(egui::Color32::from_rgb(r, g, b)),
                                    );
                                }
                                // A turn can be done while its subagents are not —
                                // say so, or a busy lane reads as finished.
                                if view.subagents > 0 {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} subagent{} busy",
                                            view.subagents,
                                            if view.subagents == 1 { "" } else { "s" }
                                        ))
                                        .small()
                                        .color(egui::Color32::from_rgb(80, 170, 255)),
                                    );
                                }
                            }
                            None => {
                                // Information, not a reservation: whoever comes
                                // next lands here, saved or not.
                                let preferred: Vec<String> =
                                    config.preferring(index).map(SavedAgent::project).collect();
                                let text = if preferred.is_empty() {
                                    "empty — the next free agent lands here".to_owned()
                                } else {
                                    format!(
                                        "empty — the next free agent lands here · preferred by {}",
                                        preferred.join(", ")
                                    )
                                };
                                ui.label(egui::RichText::new(text).weak().small());
                            }
                        }
                    });
                });

                // An empty lane is one line. Six of them each explaining themselves
                // is most of a window spent saying nothing is happening.
                if let Some(view) = view {
                    let identity = ui.label(
                        egui::RichText::new(format!(
                            "{} · {} · {}",
                            view.project
                                .clone()
                                .unwrap_or_else(|| "unknown project".to_owned()),
                            view.agent
                                .map(|agent| agent.label())
                                .unwrap_or("unknown agent"),
                            agents::host_label(&view.source),
                        ))
                        .small(),
                    );
                    if let Some(cwd) = &view.cwd {
                        identity.on_hover_text(cwd.display().to_string());
                    }
                    // The numbers, when any are known: how full the context is,
                    // and how much of the two limits is used.
                    if let Some(line) = view.gauges.sentence() {
                        ui.label(egui::RichText::new(line).small().weak().monospace());
                    }
                    // The last thing that happened, and the save, share a row. Six
                    // lanes each spending a row on a button nobody presses twice is
                    // most of a window.
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&view.note).small().weak().monospace());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Rightmost, because it is the one thing on this window
                            // that does something to the world outside it.
                            let hover = match crate::keys::lane_keys_label(index) {
                                Some(span) => {
                                    let first = index * settings::KEYS_PER_LANE;
                                    format!(
                                        "Bring this agent's terminal forward, with its tab in \
                                         front. Also on {span}; while the lane is Waiting, \
                                         {} {} {} answer it with Up, Down and Enter.",
                                        crate::keys::key_label(first + 1),
                                        crate::keys::key_label(first + 2),
                                        crate::keys::key_label(first + 3),
                                    )
                                }
                                None => "Bring this agent's terminal forward, with its tab in \
                                         front. This lane has no keys on the keyboard; this \
                                         button is it."
                                    .to_owned(),
                            };
                            if ui.small_button("Focus").on_hover_text(hover).clicked() {
                                actions.focus = Some(index);
                            }
                            changed |= save_controls(ui, index, view, config, actions);
                        });
                    });
                }
            });
    });
    if card.response.double_clicked() {
        actions.mini = true;
    }

    changed
}

/// "Remember this agent here" — as widgets, in whatever layout the caller is
/// already in, so it can sit at the end of the note row rather than on one of
/// its own. In a right-to-left layout the first widget added is the rightmost,
/// which is why these read backwards.
fn save_controls(
    ui: &mut egui::Ui,
    index: usize,
    view: &LaneView,
    config: &mut settings::Settings,
    actions: &mut LaneActions,
) -> bool {
    let mut changed = false;
    match config.saved_matching(view.agent, view.cwd.as_deref()) {
        Some(saved) => {
            if ui
                .small_button("Forget")
                .on_hover_text(
                    "Drop this agent from the saved roster. Nothing moves; it just stops \
                     coming back to a lane on purpose.",
                )
                .clicked()
            {
                actions.forget = Some(saved);
                return false;
            }
            if saved_pickers(ui, ("lane-saved", index), saved, config, Some(index)) {
                changed = true;
                actions.reseat = true;
            }
            ui.label(egui::RichText::new("saved").small().weak());
        }
        None => {
            // Only a session we could recognise again is worth remembering:
            // one with a known agent and a known folder.
            if let (Some(cwd), Some(_)) = (&view.cwd, view.agent)
                && ui
                    .small_button("Save")
                    .on_hover_text(
                        "Remember this agent and project. It comes back to this lane \
                         whenever the lane is free; otherwise it takes another lane, or \
                         waits off the keyboard.",
                    )
                    .clicked()
            {
                config.remember(SavedAgent {
                    agent: AgentFilter::Any,
                    folder: cwd.clone(),
                    lane: index,
                });
                changed = true;
                actions.reseat = true;
            }
        }
    }
    changed
}

/// The two things about a saved agent the user can change: which agent it
/// accepts, and which lane it would rather have. Added agent first, so in a
/// right-to-left row the lane reads first. `here` is the lane the session is
/// on when drawn on a lane card, so a preference that did not come true can
/// say why.
fn saved_pickers(
    ui: &mut egui::Ui,
    salt: (&str, usize),
    saved: usize,
    config: &mut settings::Settings,
    here: Option<usize>,
) -> bool {
    let (was_agent, was_lane) = (config.saved[saved].agent, config.saved[saved].lane);
    let count = config.lane_count;

    let mut agent = was_agent;
    egui::ComboBox::from_id_salt((salt, "agent"))
        .width(96.0)
        .selected_text(egui::RichText::new(agent.label()).small())
        .show_ui(ui, |ui| {
            for option in AgentFilter::ALL {
                ui.selectable_value(&mut agent, option, option.label());
            }
        });

    let mut lane = was_lane;
    let text = if lane < count {
        format!("prefers lane {}", lane + 1)
    } else {
        format!("prefers lane {} (hidden)", lane + 1)
    };
    let picker = egui::ComboBox::from_id_salt((salt, "lane"))
        .width(124.0)
        .selected_text(egui::RichText::new(text).small())
        .show_ui(ui, |ui| {
            for option in 0..count {
                ui.selectable_value(&mut lane, option, format!("lane {}", option + 1));
            }
        });
    if let Some(here) = here
        && here != was_lane
    {
        picker.response.on_hover_text(format!(
            "Lane {} was taken when this agent started. It stays here — nothing moves a \
             session that has a lane.",
            was_lane + 1
        ));
    }

    if agent != was_agent || lane != was_lane {
        let entry = &mut config.saved[saved];
        entry.agent = agent;
        entry.lane = lane;
        return true;
    }
    false
}

/// One saved agent that is not running: what would come back, and where it
/// would rather land. Drawn like the empty off-keyboard slot — an outline, no
/// state — because there is no state: nothing is happening.
fn saved_card(
    ui: &mut egui::Ui,
    index: usize,
    config: &mut settings::Settings,
    actions: &mut LaneActions,
) -> bool {
    let mut changed = false;
    let project = config.saved[index].project();
    let folder = config.saved[index].folder.display().to_string();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(6.0)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("saved").small().weak());
                ui.label(egui::RichText::new(project).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Forget")
                        .on_hover_text("Drop this agent from the saved roster.")
                        .clicked()
                    {
                        actions.forget = Some(index);
                    }
                    if saved_pickers(ui, ("roster", index), index, config, None) {
                        changed = true;
                        actions.reseat = true;
                    }
                });
            });
            // A `\\wsl.localhost\...` path is long enough to push everything
            // else off a row, so it gets a row of its own and gives way.
            ui.add(egui::Label::new(egui::RichText::new(folder).small().monospace()).truncate());
        });
    changed
}

/// A group under the lane cards: a small title, then what the group is.
fn group_label(ui: &mut egui::Ui, title: &str, caption: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(title).small().strong());
        ui.label(egui::RichText::new(format!("— {caption}")).small().weak());
    });
}

/// What an off-keyboard card asked for. Sessions there have no lane index, so
/// everything is named by identity.
#[derive(Default)]
struct OverflowActions {
    promote: Option<(String, String)>,
    dismiss: Option<(String, String)>,
    focus: Option<(String, String)>,
    /// A double-click on a card: into mini mode.
    mini: bool,
}

/// One off-keyboard session: the same card as a lane, minus the colour and the
/// number — this session has no key, and the tag says so in their place.
fn overflow_card(
    ui: &mut egui::Ui,
    view: &LaneView,
    now: u64,
    config: &settings::Settings,
    actions: &mut OverflowActions,
) {
    let key = || (view.source.clone(), view.session_id.clone());
    let tint = {
        let [r, g, b] = view.state.tint();
        egui::Color32::from_rgb(r, g, b).gamma_multiply(0.10)
    };
    // The card is a double-click into mini mode, sensed *underneath* its own
    // widgets — `UiBuilder::sense` — so its buttons keep their clicks.
    let card = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 6))
            .corner_radius(6.0)
            .fill(tint)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("off keyboard").small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("❌").small())
                            .on_hover_text(
                                "Remove this session. If the agent is actually still \
                                 alive, its next event brings it back.",
                            )
                            .clicked()
                        {
                            actions.dismiss = Some(key());
                        }
                        if ui
                            .add(egui::Button::new("⏶").small())
                            .on_hover_text(
                                "Take the bottom lane. Its session steps off the keyboard; \
                                 the lane arrows move this one up from there.",
                            )
                            .clicked()
                        {
                            actions.promote = Some(key());
                        }
                        status_and_clock(ui, view, now);
                        if let Some(word) = view.failure {
                            let [r, g, b] = State::Error.tint();
                            ui.label(
                                egui::RichText::new(word)
                                    .small()
                                    .color(egui::Color32::from_rgb(r, g, b)),
                            );
                        }
                        if view.subagents > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} subagent{} busy",
                                    view.subagents,
                                    if view.subagents == 1 { "" } else { "s" }
                                ))
                                .small()
                                .color(egui::Color32::from_rgb(80, 170, 255)),
                            );
                        }
                    });
                });
                let identity = ui.label(
                    egui::RichText::new(format!(
                        "{} · {} · {}",
                        view.project
                            .clone()
                            .unwrap_or_else(|| "unknown project".to_owned()),
                        view.agent
                            .map(|agent| agent.label())
                            .unwrap_or("unknown agent"),
                        agents::host_label(&view.source),
                    ))
                    .small(),
                );
                if let Some(cwd) = &view.cwd {
                    identity.on_hover_text(cwd.display().to_string());
                }
                if let Some(line) = view.gauges.sentence() {
                    ui.label(egui::RichText::new(line).small().weak().monospace());
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&view.note).small().weak().monospace());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("Focus")
                            .on_hover_text(
                                "Bring this agent's window forward. Off the keyboard there \
                                 is no marker key; this button is it.",
                            )
                            .clicked()
                        {
                            actions.focus = Some(key());
                        }
                        // A saved agent that did not get a lane is still a saved
                        // agent; say so, and say where it would rather be.
                        if let Some(saved) = config.saved_matching(view.agent, view.cwd.as_deref())
                        {
                            ui.label(
                                egui::RichText::new(format!(
                                    "saved · prefers lane {}",
                                    config.saved[saved].lane + 1
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    });
                });
            });
    });
    if card.response.double_clicked() {
        actions.mini = true;
    }
}

/// The landing spot for the next agent, drawn whenever every lane is taken —
/// the board visibly never runs out of room.
fn empty_overflow_slot(ui: &mut egui::Ui) {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(6.0)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("off keyboard").small().weak());
                ui.label(
                    egui::RichText::new("empty — the next agent lands here")
                        .weak()
                        .small(),
                );
            });
        });
}

/// The state and its clock, right to left: the stopwatch beside the pill —
/// except in Waiting, where how long is the signal and rides in the pill
/// itself, "Waiting 12m", one thing read in one glance.
fn status_and_clock(ui: &mut egui::Ui, view: &LaneView, now: u64) {
    let held = (view.state == State::Waiting).then(|| tracker::held(view.since, now));
    if held.is_none() {
        ui.label(egui::RichText::new(tracker::elapsed(view.since, now)).monospace());
    }
    state_pill(ui, view.state, held.as_deref());
}

/// The state as a tinted word — with how long after it, when given.
fn state_pill(ui: &mut egui::Ui, state: State, held: Option<&str>) {
    let [r, g, b] = state.tint();
    let color = egui::Color32::from_rgb(r, g, b);
    let text = match held {
        Some(held) => format!("{} {held}", state.label()),
        None => state.label().to_owned(),
    };
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 1))
        .corner_radius(4.0)
        .fill(color.gamma_multiply(0.22))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(color).strong());
        });
}

/// The things that are true about this display and are not visible in it.
fn footnotes(ui: &mut egui::Ui, tracker: &Tracker) {
    let mut note = |text: String| {
        ui.label(egui::RichText::new(text).small().weak());
    };

    if tracker
        .sessions
        .iter()
        .any(|session| session.state == State::Waiting)
    {
        // Stated rather than hidden: no agent emits an event when the user
        // *answers* a prompt. The next observable event is the tool finishing,
        // so a lane can read Waiting while the approved tool already runs —
        // and Codex reports a command only once its process has exited, so a
        // server or a long install it was allowed to start holds the lane on
        // Waiting until some later command finishes.
        note(
            "Waiting clears when the next tool finishes — no agent reports that you answered it. \
             Codex reports a command only when it exits, so an approved server or long install \
             holds Waiting until a later command finishes."
                .to_owned(),
        );
    }
    if tracker
        .sessions
        .iter()
        .any(|session| !session.reports_failure())
    {
        note("Codex has no error event, so that state cannot be shown for it.".to_owned());
    }
    if !tracker.unknown_notifications.is_empty() {
        let list: Vec<String> = tracker
            .unknown_notifications
            .iter()
            .map(|(name, count)| format!("{name} ×{count}"))
            .collect();
        note(format!(
            "notification types not recognised: {}",
            list.join(", ")
        ));
    }
    if !tracker.unrecognised_events.is_empty() {
        let list: Vec<String> = tracker
            .unrecognised_events
            .iter()
            .map(|(name, count)| format!("{name} ×{count}"))
            .collect();
        note(format!(
            "hook events we do not register: {}",
            list.join(", ")
        ));
    }
    if let Some(error) = &tracker.settings_error {
        ui.colored_label(
            egui::Color32::from_rgb(230, 180, 60),
            format!(
                "settings not read ({error}) — showing defaults. Changing anything here \
                 overwrites that file."
            ),
        );
    }
}

/// Keyboard and Agents under one fold. Lanes is what the window is for; the
/// setup sits beneath it and opens when wanted. Folded, the header still says
/// the two things that matter from across the room: whether the keyboard is
/// there, and whether every agent's hook is in place. Returns whether the
/// settings changed, and which agent card's install/remove button was pressed.
fn settings_section(
    ui: &mut egui::Ui,
    rows: &[Row],
    tracker: &mut Tracker,
    now: u64,
    card_height: &mut f32,
    rescan: &mut bool,
) -> (bool, Option<(usize, bool)>) {
    let mut changed = false;
    let mut action = None;
    let id = ui.make_persistent_id("settings-fold");
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        tracker.settings.settings_open,
    );
    // Drawn by hand rather than with `show_header`, which puts the caret
    // before the title: this header reads like the Lanes one — the same
    // heading, the caret right after the word, the summary at the right edge.
    let mut caret_clicked = false;
    let row = ui.horizontal(|ui| {
        ui.heading("Settings");
        caret_clicked = state
            .show_toggle_button(ui, egui::collapsing_header::paint_default_icon)
            .clicked();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            settings_summary(ui, rows, &tracker.keyboards, &tracker.settings);
        });
    });
    // The whole row is a handle, not only the caret. Registered after the
    // row's own widgets so it cannot take the caret's click for itself and
    // count it twice.
    let row_clicked = ui
        .interact(row.response.rect, id.with("row"), egui::Sense::click())
        .clicked();
    if caret_clicked || row_clicked {
        state.toggle(ui);
    }
    state.show_body_unindented(ui, |ui| {
        ui.add_space(4.0);
        changed |= keyboard_panel(ui, tracker);
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        action = agents_panel(ui, rows, tracker, now, card_height, rescan);
    });
    // Remembered with the rest of the settings, so the window opens the way
    // it was left.
    let open = egui::collapsing_header::CollapsingState::load(ui.ctx(), id)
        .is_some_and(|state| state.is_open());
    if open != tracker.settings.settings_open {
        tracker.settings.settings_open = open;
        changed = true;
    }
    (changed, action)
}

/// What the folded Settings header says. Right-to-left: added last, read
/// first.
fn settings_summary(
    ui: &mut egui::Ui,
    rows: &[Row],
    keyboards: &[tracker::KeyboardStatus],
    settings: &settings::Settings,
) {
    let incomplete: Vec<String> = rows
        .iter()
        .filter(|row| !row.missing.is_empty() || !row.stale.is_empty())
        .map(|row| row.found.flavor.describe())
        .collect();
    if incomplete.is_empty() {
        ui.label(
            egui::RichText::new(format!(
                "{} agent{}",
                rows.len(),
                if rows.len() == 1 { "" } else { "s" }
            ))
            .small()
            .weak(),
        );
    } else {
        ui.label(
            egui::RichText::new(format!("hooks to install: {}", incomplete.join(", ")))
                .small()
                .color(egui::Color32::from_rgb(230, 180, 60)),
        );
    }
    ui.label(egui::RichText::new("·").small().weak());
    // The devices by name, so "connected" says which; until every surface
    // has given up it is still "looking".
    let driving: Vec<&str> = keyboards
        .iter()
        .filter(|status| status.connected)
        .map(|status| status.surface)
        .collect();
    let (colour, text) = if !driving.is_empty() {
        (egui::Color32::from_rgb(80, 200, 120), driving.join(" + "))
    } else if keyboards.iter().all(|status| status.detail.is_empty()) {
        (egui::Color32::GRAY, "looking for a keyboard…".to_owned())
    } else if keyboards
        .iter()
        .all(|status| !settings.device_enabled(status.surface))
    {
        (egui::Color32::GRAY, "devices off".to_owned())
    } else {
        (
            egui::Color32::from_rgb(230, 180, 60),
            "no keyboard".to_owned(),
        )
    };
    ui.label(egui::RichText::new(text).small().color(colour));
}

/// The devices: which are there, which are left alone, how bright, and what
/// each state looks like. Returns whether anything about the settings changed.
fn keyboard_panel(ui: &mut egui::Ui, tracker: &mut Tracker) -> bool {
    let mut changed = false;
    let keyboards = tracker.keyboards.clone();

    ui.horizontal(|ui| {
        ui.strong("Devices");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(
                    "untick a device to leave it alone · ☀ brightness and 🎨 colour balance \
                     are each device's own",
                )
                .small()
                .weak(),
            );
        });
    });

    // "Dark" and "broken" look identical from the desk, so every surface
    // always has its line, saying which it is: the device it found, why it
    // found none, or that it was told to leave the device alone. The tick in
    // front is that last thing — an unticked device stays plugged in and is
    // simply not driven.
    if keyboards.is_empty() {
        ui.colored_label(egui::Color32::GRAY, "looking for a keyboard…");
    }
    let any_connected = keyboards.iter().any(|status| status.connected);
    for status in &keyboards {
        let mut enabled = tracker.settings.device_enabled(status.surface);
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut enabled, "")
                .on_hover_text(if enabled {
                    "Untick to leave this device alone: it stays plugged in, the app just \
                     stops driving it."
                } else {
                    "Tick to drive this device again."
                })
                .changed()
            {
                tracker.settings.set_device_enabled(status.surface, enabled);
                changed = true;
            }
            // Controls first, at the right; the device's line takes what is
            // left and gives way, so a long name never runs under a slider.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if enabled && status.connected {
                    // Each device its own controls: a keyboard whose blue
                    // runs hot and a deck whose LCD is true are two different
                    // corrections, and the deck — a screen — takes no colour
                    // balance. Drawn from a copy and written back only on a
                    // change: a device gets an entry of its own when its
                    // slider moves, never from being looked at.
                    let balance = status.surface != crate::surface::streamdeck::surface::SURFACE;
                    let mut tuning = tracker.settings.tuning(status.surface);
                    if device_tuning(ui, &mut tuning, balance) {
                        *tracker.settings.tuning_mut(status.surface) = tuning;
                        changed = true;
                    }
                }
                let (colour, line) = if !enabled {
                    (
                        ui.visuals().weak_text_color(),
                        format!("{} — off", status.surface),
                    )
                } else if status.connected {
                    (egui::Color32::from_rgb(80, 200, 120), status.detail.clone())
                } else if status.detail.is_empty() {
                    (egui::Color32::GRAY, format!("{}: looking…", status.surface))
                } else if any_connected {
                    // Another device is lit, so this one's absence is a
                    // footnote, not a warning.
                    (
                        ui.visuals().weak_text_color(),
                        format!("{}: {}", status.surface, status.detail),
                    )
                } else {
                    (
                        egui::Color32::from_rgb(230, 180, 60),
                        format!("{}: {}", status.surface, status.detail),
                    )
                };
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(&line).color(colour)).truncate())
                        .on_hover_text(&line);
                });
            });
        });
    }

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(
            "Every pattern is the lane's own colour; red only ever means Error, so keep \
             lane colours away from red.",
        )
        .small()
        .weak(),
    );

    // The summon keys, staged so the broken stage names itself: registration
    // failed; no F13–F24 hotkey arrived (the remap is not set up); or a press
    // went missing between the hotkey pump and its worker.
    ui.add_space(2.0);
    if let Some(error) = &tracker.keys_error {
        ui.colored_label(
            egui::Color32::from_rgb(230, 180, 60),
            format!("Summon keys are not being captured: {error}"),
        );
    } else if tracker.last_key.is_none() {
        // Only until the first press lands: a summon key that works needs no
        // caption, and one that has never fired needs the right one.
        let (received, queued, handled) = crate::keys::stages();
        let line = if received == 0 {
            "Summon: F13–F24 are registered, but none have arrived yet — remap the F-row \
             to F13–F24 in your keyboard's software (iCUE, or the Keychron Launcher \
             keymap), then any key of a lane summons its agent, and the three after the \
             first answer it while it is Waiting."
                .to_owned()
        } else {
            // A press was seen and went missing on the way. Naming the stage
            // is what makes this debuggable at all.
            format!(
                "Summon: presses are arriving but not landing \
                 (received {received} · queued {queued} · handled {handled})"
            )
        };
        ui.label(egui::RichText::new(line).small().weak());
    }

    // Audition a pattern on the physical keys, every lane in its own colour.
    // The window keeps showing the real sessions — this drives the keyboard
    // only, and dies on its own so it can never be left stuck.
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("Preview").weak());
        let active = tracker.preview.map(|preview| preview.state);
        for state in State::ALL {
            if ui
                .selectable_label(active == Some(state), state.label())
                .clicked()
            {
                tracker.preview = if active == Some(state) {
                    None
                } else {
                    Some(tracker::Preview {
                        state,
                        expires_at: crate::now_ms() + PREVIEW_MS,
                    })
                };
            }
        }
        if ui.selectable_label(active.is_none(), "Live").clicked() {
            tracker.preview = None;
        }
        if active.is_some() {
            ui.label(
                egui::RichText::new("on the keyboard only — the lanes above stay real")
                    .small()
                    .weak(),
            );
        }
    });
    changed
}

/// One device's brightness and, for a keyboard, its colour balance — each
/// named by an icon that says what it is on hover, since the words took the
/// room the sliders need. In a right-to-left row, so brightness is rightmost
/// and the balance to its left. Returns whether anything changed.
fn device_tuning(ui: &mut egui::Ui, tuning: &mut settings::Tuning, balance: bool) -> bool {
    let mut changed = false;
    if ui
        .add(
            egui::Slider::new(&mut tuning.brightness, settings::MIN_BRIGHTNESS..=1.0)
                .show_value(false),
        )
        .changed()
    {
        changed = true;
    }
    ui.label(egui::RichText::new("☀").weak())
        .on_hover_text("Brightness");
    if !balance {
        return changed;
    }
    ui.add_space(10.0);
    // Calibration: B, G, R added right-to-left so they read R G B.
    for (label, slot) in ["B", "G", "R"]
        .into_iter()
        .zip(tuning.color_gain.iter_mut().rev())
    {
        if ui
            .add(
                egui::DragValue::new(slot)
                    .range(settings::COLOR_GAIN_RANGE.0..=settings::COLOR_GAIN_RANGE.1)
                    .speed(0.01)
                    .fixed_decimals(2),
            )
            .on_hover_text(
                "Multiplies what this keyboard is sent — for LEDs that do not match \
                 the screen. Above 1.00 clips on full channels, so prefer pulling the \
                 strong channels down; the window is never corrected. Tune with a \
                 Preview pattern playing.",
            )
            .changed()
        {
            changed = true;
        }
        ui.label(egui::RichText::new(label).weak());
    }
    ui.label(egui::RichText::new("🎨").weak())
        .on_hover_text("Colour balance — R, G and B, what this keyboard is sent");
    changed
}

/// How long a preview plays before the keyboard goes back to the truth on its
/// own. Long enough to walk over and look at the keys, short enough that a
/// forgotten click cannot leave the row lying for the evening.
const PREVIEW_MS: u64 = 30_000;

/// How narrow an agent card may get before dropping to fewer columns.
///
/// Wide enough that a Windows config path wraps to two lines rather than five.
const CARD_MIN_WIDTH: f32 = 330.0;

/// Space between an agent card's border and its contents.
const CARD_PADDING: f32 = 8.0;

/// The setup panel: which agents exist, and whether our hook is registered.
///
/// Returns the (index, installing) of a button that was pressed.
fn agents_panel(
    ui: &mut egui::Ui,
    rows: &[Row],
    tracker: &Tracker,
    now: u64,
    card_height: &mut f32,
    rescan: &mut bool,
) -> Option<(usize, bool)> {
    ui.horizontal(|ui| {
        ui.strong("Agents");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Rescan").clicked() {
                *rescan = true;
            }
        });
    });
    ui.label(
        egui::RichText::new(
            "Found by looking for .claude and .codex in your Windows profile and in every WSL home.",
        )
        .weak(),
    );
    ui.add_space(8.0);

    if rows.is_empty() {
        ui.label("No agents found on this machine.");
        return None;
    }

    // As many columns as comfortably fit, so a wide window uses its width and a
    // narrow one falls back to a single column rather than squeezing four cards
    // into slivers.
    //
    // Width is computed once and then imposed on every card. Letting cards size
    // themselves is a feedback loop: one long unwrapped path widens the
    // content, which widens the available width, which lets it stay unwrapped.
    let available = ui.available_width();
    let columns = ((available / CARD_MIN_WIDTH).floor() as usize).clamp(1, 3);
    let gap = ui.spacing().item_spacing.x;
    let card_width = ((available - gap * (columns as f32 - 1.0)) / columns as f32).floor();

    let min_height = *card_height;
    let mut tallest: f32 = 0.0;
    let mut clicked = None;
    for (row_index, chunk) in rows.chunks(columns).enumerate() {
        ui.horizontal_top(|ui| {
            for (offset, row) in chunk.iter().enumerate() {
                let last = tracker.last_seen.get(&row.found.flavor.source()).copied();
                // The card's box is painted at an exact size rather than left to
                // a Frame to work out. Auto-sizing here kept producing a border
                // two and a half times the height of its own contents, so the
                // rectangle is now simply the rectangle we asked for.
                let height = min_height.max(1.0) + CARD_PADDING * 2.0;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(card_width, height), egui::Sense::hover());
                let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                ui.painter()
                    .rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
                let mut content = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink(CARD_PADDING))
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                let (action, content_height) = agent_card(&mut content, row, last, now);
                // Content height only. The padding is added once, where the box
                // is sized — adding it here too put twice as much space below
                // the contents as above them.
                tallest = tallest.max(content_height);
                if let Some(installing) = action {
                    clicked = Some((row_index * columns + offset, installing));
                }
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
    *card_height = tallest;
    clicked
}

/// One agent. Returns `Some(true)` for install, `Some(false)` for remove, and
/// the height its contents actually needed.
fn agent_card(
    ui: &mut egui::Ui,
    row: &Row,
    last_seen: Option<u64>,
    now: u64,
) -> (Option<bool>, f32) {
    let mut action = None;
    {
        let ui = &mut *ui;
        // Without this a long \\wsl.localhost path pushes the card wider than
        // the column instead of wrapping inside it.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);

        // Buttons share the title's row: they are the widest thing in the card
        // and a row of their own costs height on every card for nothing.
        //
        // The row is allocated at an explicit height rather than left to size
        // itself. `with_layout` takes all the space it is offered, and offered
        // the rest of a card it takes the rest of the card — which is what made
        // every card stretch to the bottom of the window.
        let row_height = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_height),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                // Right-to-left, so the first added sits rightmost.
                if !row.installed.is_empty() && ui.button("Remove").clicked() {
                    action = Some(false);
                }
                if ui.button("Install").clicked() {
                    action = Some(true);
                }
                // The title takes whatever is left, on the other side.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.strong(row.found.flavor.describe());
                });
            },
        );

        ui.label(
            egui::RichText::new(row.found.flavor.source())
                .weak()
                .small(),
        );
        ui.label(
            egui::RichText::new(row.found.config.display().to_string())
                .small()
                .weak(),
        );
        ui.add_space(2.0);

        // "Registered" and "correct" are different claims. Conflating them is
        // how a configuration that had drifted stayed invisible.
        let (colour, status) = if row.installed.is_empty() {
            (egui::Color32::GRAY, "not registered".to_owned())
        } else if row.missing.is_empty() && row.stale.is_empty() {
            (
                egui::Color32::from_rgb(80, 200, 120),
                format!("{} hooks registered", row.installed.len()),
            )
        } else {
            (
                egui::Color32::from_rgb(230, 180, 60),
                "registered, needs updating".to_owned(),
            )
        };
        ui.colored_label(colour, status);
        ui.label(format!(
            "last event: {}",
            lastseen::describe(last_seen, now)
        ));

        if !row.missing.is_empty() {
            ui.label(egui::RichText::new(format!("missing: {}", row.missing.join(", "))).small());
        }
        if !row.stale.is_empty() {
            ui.label(
                egui::RichText::new(format!("left by an older build: {}", row.stale.join(", ")))
                    .small(),
            );
        }
        // Registering a hook changes nothing until the agent re-reads its
        // configuration, so say so wherever it is not yet working.
        if row.installed.is_empty() || !row.missing.is_empty() || last_seen.is_none() {
            ui.label(
                egui::RichText::new(row.found.flavor.trust_hint())
                    .small()
                    .weak(),
            );
        }
    }
    // What this card actually needs. The caller takes the largest across all of
    // them and paints every box that size, so they match without any card
    // knowing about the others.
    (action, ui.min_rect().height())
}

impl App {
    /// Whether the window is on screen at all, straight from Windows.
    /// `true` until the handle is known, and always off Windows.
    fn window_shown(&self) -> bool {
        #[cfg(windows)]
        {
            let raw = self.hwnd.load(Ordering::Relaxed);
            if raw != 0 {
                #[link(name = "user32")]
                unsafe extern "system" {
                    fn IsWindowVisible(hwnd: isize) -> i32;
                }
                // SAFETY: a plain user32 query; a stale handle answers 0.
                return unsafe { IsWindowVisible(raw) } != 0;
            }
        }
        true
    }

    /// Publishes the window's Win32 handle for the tray thread, once known.
    fn publish_hwnd(&self, frame: &eframe::Frame) {
        if self.hwnd.load(Ordering::Relaxed) != 0 {
            return;
        }
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = frame.window_handle()
                && let RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                self.hwnd.store(win32.hwnd.get(), Ordering::Relaxed);
            }
        }
        #[cfg(not(windows))]
        let _ = frame;
    }
}

/// Brings the window back from the tray.
///
/// Straight Win32, from the tray's own thread: Windows grants the foreground
/// only to the thread that just received the user's input, and that is this
/// one. The polite route — viewport commands into the UI loop — is taken as
/// well; it re-syncs winit's idea of visibility, and would on its own bring
/// the window back (eframe processes a hidden window's commands within
/// 100 ms), only without the right to put it in front.
fn reopen(hwnd: &Arc<AtomicIsize>, ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let raw = hwnd.load(Ordering::Relaxed);
        if raw != 0 {
            #[link(name = "user32")]
            unsafe extern "system" {
                fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
                fn SetForegroundWindow(hwnd: isize) -> i32;
            }
            // Shows a hidden window and un-minimizes a minimized one.
            const SW_RESTORE: i32 = 9;
            // SAFETY: plain user32 calls; a stale handle makes them no-ops.
            unsafe {
                ShowWindow(raw, SW_RESTORE);
                SetForegroundWindow(raw);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = hwnd;
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// The tray icon: the same four-key cluster as everywhere else, from
/// [`crate::icon`], so the tray, the window and the executable cannot drift
/// apart.
fn icon() -> tray_icon::Icon {
    const SIZE: u32 = 32;
    // A tray with no icon is worse than an ugly one, so fall back to a blank
    // square rather than refusing to build.
    tray_icon::Icon::from_rgba(crate::icon::rgba(SIZE), SIZE, SIZE).unwrap_or_else(|_| {
        let blank = vec![40u8; (SIZE * SIZE * 4) as usize];
        #[allow(clippy::expect_used)]
        tray_icon::Icon::from_rgba(blank, SIZE, SIZE).expect("a blank square is a valid icon")
    })
}
