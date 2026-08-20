//! The tray icon and the window.
//!
//! Two panels. **Lanes** is the product: what each agent is doing right now,
//! where it is doing it, and for how long. **Agents** is the setup that makes
//! the first one possible — which agents this machine has, and whether our hook
//! is registered with them.
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
use crate::settings::{self, Bind, BindAgent, LANE_COUNTS, Rgb};
use crate::state::State;
use crate::tracker::{self, Tracker};

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
    /// The window's Win32 handle, published for the tray thread: reopening a
    /// tray-hidden window cannot go through the UI loop — restore and
    /// foreground need to come from real input on the tray's own thread —
    /// and the hide/reveal style flips need the raw handle too.
    hwnd: Arc<AtomicIsize>,
    quitting: bool,
    /// Whether the taskbar button is currently removed. Minimized IS "in the
    /// tray": one state check in `update` keeps the button in lockstep with
    /// the window, whichever way it was minimized or brought back.
    tray_tabless: bool,
    /// Whether the Run-key startup entry exists. Read once and kept, so the
    /// registry is not asked twice a second for something only a click changes.
    autostart: bool,
}

impl App {
    pub fn new(
        tracker: Arc<Mutex<Tracker>>,
        install_dir: PathBuf,
        settings_path: Option<PathBuf>,
        notice: Option<String>,
    ) -> Self {
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
            tray_tabless: false,
            autostart: crate::autostart::enabled(),
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
                        // Release the registered hotkeys before the immediate
                        // exit. The rest — settings, lighting, sessions — needs
                        // nothing, and going through the UI loop would hang
                        // exactly when the window is hidden.
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
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.ensure_tray(ctx);
        self.publish_hwnd(frame);

        // Closing hides to the tray. Quit is deliberately only on the tray menu,
        // so the app cannot be shut down by the reflex of closing a window.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // Never SW_HIDE: Windows delivers no WM_PAINT to a hidden window,
            // and eframe's scheduler then spins the event loop in Poll forever
            // waiting for a paint that cannot come — a full core burned doing
            // nothing (measured: ~5% total CPU in the tray, ~0.1% open).
            // Minimizing keeps WS_VISIBLE, so paints keep pacing the loop.
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // Minimized IS "in the tray": whichever control got it there — the X
        // above or the title bar's own minimize — the taskbar button comes
        // off, and only once the minimize has actually landed: deleting the
        // tab first is a race the shell wins by re-adding it with the
        // minimize. Brought back by any route (tray click, Alt-Tab), the
        // button returns the same frame. ITaskbarList, never a style flip —
        // winit reapplies its cached styles and hands back a broken frame.
        let minimized = ctx.input(|i| i.viewport().minimized) == Some(true);
        if minimized != self.tray_tabless {
            taskbar_tab(&self.hwnd, !minimized);
            self.tray_tabless = minimized;
        }

        let now = crate::now_ms();
        let mut rescan = false;
        let mut action: Option<(usize, bool)> = None;
        let mut settings_changed = false;
        let mut focus: Option<FocusRequest> = None;
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

            egui::CentralPanel::default().show(ctx, |ui| {
                // One scroll area for the whole window. Two of them nested was
                // also what left the right-hand column of agent cards clipped:
                // the width was measured outside the scroll area and then used
                // inside it, where the scrollbar had already taken some.
                egui::ScrollArea::vertical()
                    .id_salt("window")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        settings_changed = lanes_panel(ui, &mut tracker, now, &mut focus);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(6.0);
                        settings_changed |= keyboard_panel(ui, &mut tracker);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(6.0);
                        action = agents_panel(ui, rows, &tracker, now, card_height, &mut rescan);
                    });
            });

            // A bar rather than a line at the end of the page. What the last
            // action did — an install, or which window a Focus actually
            // raised — is the one thing that must never be below the fold.
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
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
        }

        // On its own thread, deliberately. Two reasons, and both were learned
        // the hard way: raising a window can spend a quarter of a second
        // waiting for the terminal to agree which tab is in front, which is a
        // quarter of a second of frozen window; and UI Automation needs a COM
        // apartment, which the window's own thread has already been put into a
        // different mode of by the windowing library.
        if let Some(request) = focus {
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
        // without any plumbing between the ingress thread and the window.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

impl App {
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
                Ok(()) => format!(
                    "{name}: {} — a backup is beside it",
                    if installing { "installed" } else { "removed" }
                ),
                Err(error) => format!("{name}: {error}"),
            },
            Err(error) => format!("{name}: {error}"),
        }
    }
}

/// What one lane shows. Copied out of the tracker first, so the settings for
/// the same lane can be edited in place without borrowing it twice.
/// What the user asked to bring forward: a lane's session, or one named by
/// identity because it has no lane (the off-keyboard cards).
enum FocusRequest {
    Lane(usize),
    Session(String, String),
}

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
    }
}

/// Draws the lanes. Returns whether anything about the settings changed.
fn lanes_panel(
    ui: &mut egui::Ui,
    tracker: &mut Tracker,
    now: u64,
    focus: &mut Option<FocusRequest>,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.heading("Lanes");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut count = tracker.settings.lane_count;
            for option in LANE_COUNTS.iter().rev() {
                let label = format!("{option} × {}", settings::KEYS / option);
                if ui
                    .selectable_label(count == *option, label)
                    .on_hover_text(format!(
                        "{option} lanes of {} keys each",
                        settings::KEYS / option
                    ))
                    .clicked()
                {
                    count = *option;
                }
            }
            if count != tracker.settings.lane_count {
                tracker.set_lane_count(count);
                changed = true;
            }
        });
    });
    ui.label(
        egui::RichText::new(format!(
            "Twelve F-row keys, {} per lane. A lane keeps its position for as long as its session lives.",
            tracker.settings.keys_per_lane()
        ))
        .weak(),
    );
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
    if actions.rebind {
        tracker.rebind();
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
        ui.label(
            egui::RichText::new(
                "Off the keyboard — fully tracked, no key or light. Nothing here takes a \
                 lane away from a session that already has one; the ⏶ you press is the \
                 exception.",
            )
            .small()
            .weak(),
        );
        ui.add_space(2.0);
    }
    let mut off = OverflowActions::default();
    for view in &overflow {
        ui.push_id(("off-keyboard", &view.source, &view.session_id), |ui| {
            overflow_card(ui, view, now, &mut off);
        });
        ui.add_space(4.0);
    }
    if every_lane_taken {
        empty_overflow_slot(ui);
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

    footnotes(ui, tracker);
    changed
}

/// One lane. Returns whether its settings changed.
/// What a lane card asked its caller to do, gathered rather than passed as a
/// fistful of out-parameters.
#[derive(Default)]
struct LaneActions {
    rebind: bool,
    focus: Option<usize>,
    moved: Option<(usize, usize)>,
    dismiss: Option<usize>,
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
                ui.label(
                    egui::RichText::new(format!("{}", index + 1))
                        .monospace()
                        .strong()
                        .color(accent),
                );
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
                    // colour, binding, keys.
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
                            ui.label(
                                egui::RichText::new(tracker::elapsed(view.since, now)).monospace(),
                            );
                            state_pill(ui, view.state);
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
                            ui.label(
                                egui::RichText::new("empty — the next free agent lands here")
                                    .weak()
                                    .small(),
                            );
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
                // The last thing that happened, and the binding, share a row.
                // Six lanes each spending a row on a button nobody presses twice
                // is most of a window.
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&view.note).small().weak().monospace());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Rightmost, because it is the one thing on this window
                        // that does something to the world outside it.
                        if ui
                            .small_button("Focus")
                            .on_hover_text(
                                "Bring this agent's terminal forward, with its tab in front. \
                                 Also on the lane's marker key, F13–F24.",
                            )
                            .clicked()
                        {
                            actions.focus = Some(index);
                        }
                        changed |=
                            bind_controls(ui, index, Some(view), config, &mut actions.rebind);
                    });
                });
            } else if config.lanes[index].bind.is_some() {
                // Empty, but reserved. Worth saying: it is why the next session
                // went somewhere else. Laid out right-to-left like the row
                // above, or the same widgets come out in the opposite order and
                // the line reads backwards.
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        changed |= bind_controls(ui, index, None, config, &mut actions.rebind);
                    });
                });
            }
        });

    changed
}

/// "This lane is that project" — as widgets, in whatever layout the caller is
/// already in, so it can sit at the end of the line above rather than on one of
/// its own. In a right-to-left layout the first widget added is the rightmost,
/// which is why these read backwards.
fn bind_controls(
    ui: &mut egui::Ui,
    index: usize,
    view: Option<&LaneView>,
    config: &mut settings::Settings,
    rebind: &mut bool,
) -> bool {
    let mut changed = false;
    match &config.lanes[index].bind {
        Some(bind) => {
            let folder = bind.folder.display().to_string();
            if ui.small_button("Unbind").clicked() {
                config.lanes[index].bind = None;
                *rebind = true;
                return true;
            }
            let mut agent = bind.agent;
            egui::ComboBox::from_id_salt(("bind-agent", index))
                .width(96.0)
                .selected_text(egui::RichText::new(agent.label()).small())
                .show_ui(ui, |ui| {
                    for option in [BindAgent::Any, BindAgent::Claude, BindAgent::Codex] {
                        ui.selectable_value(&mut agent, option, option.label());
                    }
                });
            if agent != bind.agent
                && let Some(bind) = &mut config.lanes[index].bind
            {
                bind.agent = agent;
                changed = true;
                *rebind = true;
            }
            // A `\\wsl.localhost\...` path is long enough to push everything
            // else off the row, so it gives way rather than the controls.
            ui.add(egui::Label::new(egui::RichText::new(folder).small().monospace()).truncate())
                .on_hover_text("this lane is reserved for this project");
            ui.label(egui::RichText::new("bound to").small().weak());
        }
        None => {
            if let Some(cwd) = view.and_then(|view| view.cwd.clone())
                && ui
                    .small_button("Bind to this project")
                    .on_hover_text("This project comes back to this lane whenever it is free.")
                    .clicked()
            {
                config.lanes[index].bind = Some(Bind {
                    agent: BindAgent::Any,
                    folder: cwd,
                });
                changed = true;
                *rebind = true;
            }
        }
    }
    changed
}

/// What an off-keyboard card asked for. Sessions there have no lane index, so
/// everything is named by identity.
#[derive(Default)]
struct OverflowActions {
    promote: Option<(String, String)>,
    dismiss: Option<(String, String)>,
    focus: Option<(String, String)>,
}

/// One off-keyboard session: the same card as a lane, minus the colour and the
/// number — this session has no key, and the tag says so in their place.
fn overflow_card(ui: &mut egui::Ui, view: &LaneView, now: u64, actions: &mut OverflowActions) {
    let key = || (view.source.clone(), view.session_id.clone());
    let tint = {
        let [r, g, b] = view.state.tint();
        egui::Color32::from_rgb(r, g, b).gamma_multiply(0.10)
    };
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
                    ui.label(egui::RichText::new(tracker::elapsed(view.since, now)).monospace());
                    state_pill(ui, view.state);
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
                });
            });
        });
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

fn state_pill(ui: &mut egui::Ui, state: State) {
    let [r, g, b] = state.tint();
    let color = egui::Color32::from_rgb(r, g, b);
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 1))
        .corner_radius(4.0)
        .fill(color.gamma_multiply(0.22))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(state.label()).color(color).strong());
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
        // so a lane can read Waiting while the approved tool already runs.
        note(
            "Waiting clears when the next tool finishes — no agent reports that you answered it."
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

/// The keyboard: whether it is there, how bright, and what each state looks
/// like. Returns whether anything about the settings changed.
fn keyboard_panel(ui: &mut egui::Ui, tracker: &mut Tracker) -> bool {
    let mut changed = false;
    let status = tracker.keyboard.clone();

    ui.horizontal(|ui| {
        ui.heading("Keyboard");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut brightness = tracker.settings.brightness;
            if ui
                .add(
                    egui::Slider::new(&mut brightness, settings::MIN_BRIGHTNESS..=1.0)
                        .show_value(false),
                )
                .changed()
            {
                tracker.settings.brightness = brightness;
                changed = true;
            }
            ui.label(egui::RichText::new("Brightness").weak());
            ui.add_space(10.0);
            // Calibration, rightmost after brightness: B, G, R added
            // right-to-left so they read R G B left-to-right.
            for (label, slot) in ["B", "G", "R"]
                .into_iter()
                .zip(tracker.settings.color_gain.iter_mut().rev())
            {
                if ui
                    .add(
                        egui::DragValue::new(slot)
                            .range(settings::COLOR_GAIN_RANGE.0..=settings::COLOR_GAIN_RANGE.1)
                            .speed(0.01)
                            .fixed_decimals(2),
                    )
                    .on_hover_text(
                        "Multiplies what the keys are sent — for a keyboard whose LEDs \
                         do not match the screen. Above 1.00 clips on full channels, so \
                         prefer pulling the strong channels down; the window is never \
                         corrected. Tune with a Preview pattern playing.",
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.label(egui::RichText::new(label).weak());
            }
            ui.label(egui::RichText::new("Color balance").weak());
        });
    });

    // "Dark" and "broken" look identical from the desk, so this always says
    // which one it is.
    let (colour, detail) = if status.connected {
        (egui::Color32::from_rgb(80, 200, 120), status.detail.clone())
    } else if status.detail.is_empty() {
        (egui::Color32::GRAY, "looking for a keyboard…".to_owned())
    } else {
        (egui::Color32::from_rgb(230, 180, 60), status.detail.clone())
    };
    ui.colored_label(colour, detail);

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
             to F13–F24 in iCUE, then the leftmost key of each lane summons its agent."
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
        ui.heading("Agents");
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
/// Straight Win32, from the tray's own thread. The polite route — viewport
/// commands into the UI loop — is also taken, but only works once the window
/// is visible again, because a hidden window gets no redraws and a loop that
/// is not redrawing is not listening.
fn reopen(hwnd: &Arc<AtomicIsize>, ctx: &egui::Context) {
    // The taskbar button is restored by the heal in `App::update` on the
    // first frame after the restore — one owner, whichever way the window
    // comes back.
    #[cfg(windows)]
    {
        let raw = hwnd.load(Ordering::Relaxed);
        if raw != 0 {
            #[link(name = "user32")]
            unsafe extern "system" {
                fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
                fn SetForegroundWindow(hwnd: isize) -> i32;
            }
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

/// Adds or removes the window's taskbar button — the tray's idea of hidden is
/// "minimized, and not on the taskbar". `ITaskbarList` is the documented API
/// for exactly this, and unlike a `WS_EX_TOOLWINDOW` style flip it does not
/// touch the window frame, so winit's cached styles have nothing to fight
/// (the style flip left the button in place and rebuilt the restored frame
/// with a toolwindow caption — one lone close button, no minimize/maximize).
#[cfg(windows)]
fn taskbar_tab(hwnd: &Arc<AtomicIsize>, show: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

    let raw = hwnd.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: COM init/uninit are balanced (S_FALSE still needs the uninit;
    // RPC_E_CHANGED_MODE means the thread already has an apartment and the
    // create below works in it). The handle is this window's own.
    unsafe {
        let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        if let Ok(list) =
            CoCreateInstance::<_, ITaskbarList>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        {
            let _ = list.HrInit();
            let window = HWND(raw as *mut core::ffi::c_void);
            let _ = if show {
                list.AddTab(window)
            } else {
                list.DeleteTab(window)
            };
        }
        if inited {
            CoUninitialize();
        }
    }
}

#[cfg(not(windows))]
fn taskbar_tab(_hwnd: &Arc<AtomicIsize>, _show: bool) {}

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
