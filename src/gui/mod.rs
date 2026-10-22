//! Native desktop window (egui). It shows the same state as the terminal UI and
//! drives the same download engine in this process: no server, no browser.
//!
//! The window never touches the network or the disk itself. It reads the shared
//! [`App`] model, and every button sends a [`UiCommand`] through the same channel
//! the terminal UI uses. All visible text comes from the i18n catalog.

mod icons;
mod model;

use anyhow::{anyhow, Result};
use crossterm::event::KeyCode;
use eframe::egui;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::download::{JobProgress, JobStatus};
use crate::plugin::captcha::ChallengePayload;
use crate::t;
use crate::tor::TorHealth;
use crate::ui::{detail, fmt_bytes, import, App, InputPrompt, MoveTo, SharedApp, UiCommand};
use icons::Icon;
use model::{SearchScope, StateFilter, Totals};

/// How often the window redraws when nothing else triggers it (live progress).
const REPAINT_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Downloads,
    History,
}

/// Opens the window and blocks until it is closed. Must run on the main thread
/// (a requirement of macOS).
pub fn run(app: SharedApp) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rdownloader")
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([560.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rdownloader",
        options,
        Box::new(move |_cc| Ok(Box::new(GuiApp::new(app)))),
    )
    .map_err(|e| anyhow!("GUI failed: {e}"))
}

/// Whether a window can be opened in this session (always on macOS and Windows,
/// on Linux only with an X11 or Wayland display).
pub fn display_available() -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

// ───────────────────────── pure helpers (unit-tested) ─────────────────────────

/// The links in `text`: whitespace separated, http/https/ftp only.
pub fn links_in(text: &str) -> Vec<String> {
    import::parse_urls(text)
        .urls
        .iter()
        .map(|u| u.to_string())
        .collect()
}

/// Which buttons make sense for a job in `status`.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Actions {
    pub pause: bool,
    pub resume: bool,
    pub cancel: bool,
    pub remove: bool,
}

pub fn actions_for(status: &JobStatus) -> Actions {
    use JobStatus::*;
    Actions {
        pause: matches!(status, Downloading),
        resume: matches!(status, Paused),
        cancel: matches!(status, Queued | Resolving | Downloading | Paused | Merging),
        remove: true,
    }
}

/// Progress as a fraction 0..=1, or `None` when the size is not known yet.
pub fn fraction(job: &JobProgress) -> Option<f32> {
    if matches!(job.status, JobStatus::Done) {
        return Some(1.0);
    }
    (job.total_bytes > 0).then(|| (job.bytes_done as f64 / job.total_bytes as f64).min(1.0) as f32)
}

/// One line under the progress bar: bytes, speed, ETA, parts.
pub fn progress_line(job: &JobProgress, bytes_per_sec: u64) -> String {
    let mut parts = vec![if job.total_bytes > 0 {
        format!(
            "{} / {}",
            fmt_bytes(job.bytes_done),
            fmt_bytes(job.total_bytes)
        )
    } else {
        fmt_bytes(job.bytes_done)
    }];
    if matches!(job.status, JobStatus::Downloading) && bytes_per_sec > 0 {
        parts.push(detail::fmt_speed(bytes_per_sec));
        if let Some(secs) = detail::eta_secs(
            job.total_bytes.saturating_sub(job.bytes_done),
            bytes_per_sec,
        )
        .filter(|_| job.total_bytes > 0)
        {
            parts.push(t!("gui.job.eta", eta = detail::fmt_eta(secs)));
        }
    }
    if job.parts.len() > 1 {
        parts.push(t!("gui.job.parts", n = job.parts.len()));
    }
    parts.join("  ·  ")
}

fn status_text(status: &JobStatus) -> String {
    match status {
        JobStatus::Queued => t!("ui.job.queued"),
        JobStatus::Resolving => t!("ui.job.resolving"),
        JobStatus::Downloading => t!("ui.job.downloading"),
        JobStatus::Merging => t!("ui.job.merging"),
        JobStatus::Done => t!("ui.job.done"),
        JobStatus::Paused => t!("ui.job.paused"),
        JobStatus::Cancelled => t!("ui.job.cancelled"),
        JobStatus::Failed(_) => t!("ui.job.failed", error = ""),
    }
    .trim_end_matches([':', ' '])
    .to_string()
}

fn status_color(status: &JobStatus, dark: bool) -> egui::Color32 {
    use JobStatus::*;
    let (ok, busy, bad, idle) = if dark {
        (
            egui::Color32::from_rgb(0x4a, 0xde, 0x80),
            egui::Color32::from_rgb(0x7d, 0xa7, 0xff),
            egui::Color32::from_rgb(0xf8, 0x71, 0x71),
            egui::Color32::from_rgb(0x9a, 0xa3, 0xaf),
        )
    } else {
        (
            egui::Color32::from_rgb(0x15, 0x80, 0x3d),
            egui::Color32::from_rgb(0x1d, 0x4e, 0xd8),
            egui::Color32::from_rgb(0xb9, 0x1c, 0x1c),
            egui::Color32::from_rgb(0x5d, 0x66, 0x73),
        )
    };
    match status {
        Done => ok,
        Resolving | Downloading | Merging => busy,
        Failed(_) => bad,
        Queued | Paused | Cancelled => idle,
    }
}

/// Tor state for the title bar: text and whether it is good, in progress or bad.
pub fn tor_label(health: Option<&TorHealth>) -> (String, Tone) {
    match health {
        None => (t!("gui.tor.off"), Tone::Warn),
        Some(h) if h.blockage().is_some() => (t!("gui.tor.blocked"), Tone::Bad),
        Some(h) if h.is_healthy() => (t!("gui.tor.ready"), Tone::Good),
        Some(h) => (
            t!("gui.tor.starting", percent = h.bootstrap_percent()),
            Tone::Warn,
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Good,
    Warn,
    Bad,
}

/// What the toolbar buttons that act on many downloads do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bulk {
    StartPaused,
    PauseRunning,
    StopActive,
    ClearFinished,
}

/// The commands for a bulk action over `jobs`.
pub fn bulk_commands(jobs: &[JobProgress], action: Bulk) -> Vec<UiCommand> {
    jobs.iter()
        .filter_map(|j| match (action, &j.status) {
            (Bulk::StartPaused, JobStatus::Paused) => Some(UiCommand::ResumeJob(j.id)),
            (Bulk::PauseRunning, JobStatus::Downloading) => Some(UiCommand::PauseJob(j.id)),
            (
                Bulk::StopActive,
                JobStatus::Queued
                | JobStatus::Resolving
                | JobStatus::Downloading
                | JobStatus::Paused
                | JobStatus::Merging,
            ) => Some(UiCommand::CancelJob(j.id)),
            (Bulk::ClearFinished, JobStatus::Done) => Some(UiCommand::RemoveJob(j.id)),
            _ => None,
        })
        .collect()
}

// ───────────────────────── the window ─────────────────────────

struct GuiApp {
    app: SharedApp,
    tab: Tab,
    selected: Option<Uuid>,
    filter: StateFilter,
    scope: SearchScope,
    query: String,
    add_open: bool,
    add_text: String,
    overview_open: bool,
    captcha_input: String,
}

impl GuiApp {
    fn new(app: SharedApp) -> Self {
        Self {
            app,
            tab: Tab::Downloads,
            selected: None,
            filter: StateFilter::All,
            scope: SearchScope::Name,
            query: String::new(),
            add_open: false,
            add_text: String::new(),
            overview_open: true,
            captcha_input: String::new(),
        }
    }

    fn send(app: &App, command: UiCommand) {
        let _ = app.cmd_tx.send(command);
    }

    fn add_links(app: &mut App, text: &str) -> bool {
        let links = links_in(text);
        match links.len() {
            0 => false,
            1 => {
                Self::send(app, UiCommand::AddUrl(links[0].clone()));
                true
            }
            _ => {
                Self::send(app, UiCommand::AddUrls(links));
                true
            }
        }
    }

    fn bulk(app: &App, action: Bulk) {
        for command in bulk_commands(&app.jobs, action) {
            Self::send(app, command);
        }
    }

    // ── frame parts ──

    fn toolbar(&mut self, ui: &mut egui::Ui, app: &mut App) {
        let has = |action| !bulk_commands(&app.jobs, action).is_empty();
        let can_move = self
            .selected
            .is_some_and(|id| app.jobs.iter().any(|j| j.id == id));
        let (start, pause, stop, clear) = (
            has(Bulk::StartPaused),
            has(Bulk::PauseRunning),
            has(Bulk::StopActive),
            has(Bulk::ClearFinished),
        );

        ui.horizontal(|ui| {
            if icons::button(ui, Icon::Play, &t!("gui.toolbar.start"), start).clicked() {
                Self::bulk(app, Bulk::StartPaused);
            }
            if icons::button(ui, Icon::Pause, &t!("gui.toolbar.pause"), pause).clicked() {
                Self::bulk(app, Bulk::PauseRunning);
            }
            if icons::button(ui, Icon::Stop, &t!("gui.toolbar.stop"), stop).clicked() {
                Self::bulk(app, Bulk::StopActive);
            }
            ui.separator();
            for (icon, to, key) in [
                (Icon::ToTop, MoveTo::Top, "gui.toolbar.to_top"),
                (Icon::Up, MoveTo::Up, "gui.toolbar.up"),
                (Icon::Down, MoveTo::Down, "gui.toolbar.down"),
                (Icon::ToBottom, MoveTo::Bottom, "gui.toolbar.to_bottom"),
            ] {
                if icons::button(ui, icon, &t!(key), can_move).clicked() {
                    if let Some(id) = self.selected {
                        Self::send(app, UiCommand::MoveJob(id, to));
                    }
                }
            }
            ui.separator();
            if icons::button(ui, Icon::Clear, &t!("gui.toolbar.clear"), clear).clicked() {
                Self::bulk(app, Bulk::ClearFinished);
            }
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Downloads, t!("gui.tab.downloads"));
            ui.selectable_value(&mut self.tab, Tab::History, t!("gui.tab.history"));
        });
    }

    fn overview(&self, ui: &mut egui::Ui, app: &App) {
        let totals = Totals::compute(&app.jobs, |id| app.speeds.speed(id));
        let eta = totals
            .eta_secs
            .map_or_else(|| t!("gui.value.none"), detail::fmt_eta);
        let rows = [
            (t!("gui.overview.links"), totals.links.to_string()),
            (t!("gui.overview.running"), totals.running.to_string()),
            (t!("gui.overview.finished"), totals.finished.to_string()),
            (t!("gui.overview.total"), fmt_bytes(totals.total_bytes)),
            (t!("gui.overview.loaded"), fmt_bytes(totals.loaded_bytes)),
            (
                t!("gui.overview.remaining"),
                fmt_bytes(totals.remaining_bytes),
            ),
            (
                t!("gui.overview.speed"),
                detail::fmt_speed(totals.bytes_per_sec),
            ),
            (t!("gui.overview.eta"), eta),
            (
                t!("gui.overview.connections"),
                totals.connections.to_string(),
            ),
        ];
        ui.strong(t!("gui.overview.title"));
        egui::Grid::new("overview")
            .num_columns(6)
            .spacing([18.0, 4.0])
            .show(ui, |ui| {
                for (n, (label, value)) in rows.iter().enumerate() {
                    ui.weak(label);
                    ui.label(value);
                    if n % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, _app: &mut App) {
        ui.horizontal(|ui| {
            if ui.button(t!("gui.add.title")).clicked() {
                self.add_open = true;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text(t!("gui.search.hint"))
                    .desired_width((ui.available_width() - 330.0).max(120.0)),
            );
            egui::ComboBox::from_id_salt("scope")
                .selected_text(scope_text(self.scope))
                .width(110.0)
                .show_ui(ui, |ui| {
                    for scope in [SearchScope::Name, SearchScope::Host, SearchScope::Url] {
                        ui.selectable_value(&mut self.scope, scope, scope_text(scope));
                    }
                });
            egui::ComboBox::from_id_salt("filter")
                .selected_text(filter_text(self.filter))
                .width(130.0)
                .show_ui(ui, |ui| {
                    for filter in [
                        StateFilter::All,
                        StateFilter::Running,
                        StateFilter::Paused,
                        StateFilter::Finished,
                        StateFilter::Failed,
                    ] {
                        ui.selectable_value(&mut self.filter, filter, filter_text(filter));
                    }
                });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, app: &App) {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.overview_open, t!("gui.overview.title"))
                .clicked()
            {
                self.overview_open = !self.overview_open;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, tone) = tor_label(app.tor_health.as_ref());
                ui.colored_label(tone_color(tone, ui.visuals().dark_mode), text);
                let totals = Totals::compute(&app.jobs, |id| app.speeds.speed(id));
                ui.separator();
                ui.weak(detail::fmt_speed(totals.bytes_per_sec));
                ui.separator();
                ui.weak(t!("gui.footer.running", n = totals.running));
            });
        });
    }

    fn downloads(&mut self, ui: &mut egui::Ui, app: &mut App) {
        if self
            .selected
            .is_some_and(|id| !app.jobs.iter().any(|j| j.id == id))
        {
            self.selected = None;
        }
        let visible: Vec<JobProgress> = app
            .jobs
            .iter()
            .filter(|j| model::matches(j, self.filter, self.scope, &self.query))
            .cloned()
            .collect();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if app.jobs.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| ui.weak(t!("gui.queue.empty")));
                    return;
                }
                if visible.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| ui.weak(t!("gui.queue.no_match")));
                    return;
                }
                let dark = ui.visuals().dark_mode;
                for job in &visible {
                    let speed = app.speeds.speed(job.id);
                    let selected = self.selected == Some(job.id);
                    let accent = ui.visuals().selection.bg_fill;
                    let frame = egui::Frame::group(ui.style()).stroke(if selected {
                        egui::Stroke::new(2.0, accent)
                    } else {
                        ui.visuals().widgets.noninteractive.bg_stroke
                    });
                    let response = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.job_card(ui, app, job, speed, dark);
                        })
                        .response;
                    if ui
                        .interact(
                            response.rect,
                            ui.make_persistent_id(("row", job.id)),
                            egui::Sense::click(),
                        )
                        .clicked()
                    {
                        self.selected = Some(job.id);
                    }
                    ui.add_space(4.0);
                }
            });
    }

    fn job_card(
        &mut self,
        ui: &mut egui::Ui,
        app: &mut App,
        job: &JobProgress,
        speed: u64,
        dark: bool,
    ) {
        ui.horizontal(|ui| {
            let name = if job.filename.is_empty() {
                &job.url
            } else {
                &job.filename
            };
            ui.label(egui::RichText::new(name).strong())
                .on_hover_text(&job.url);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(status_color(&job.status, dark), status_text(&job.status));
                ui.weak(model::host_of(&job.url));
            });
        });

        let bar = match fraction(job) {
            Some(f) => egui::ProgressBar::new(f).show_percentage(),
            None => {
                egui::ProgressBar::new(0.0).animate(matches!(job.status, JobStatus::Downloading))
            }
        };
        ui.add(bar);
        ui.weak(progress_line(job, speed));
        if let JobStatus::Failed(error) = &job.status {
            ui.colored_label(status_color(&job.status, dark), error);
        }

        let actions = actions_for(&job.status);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(actions.pause, egui::Button::new(t!("gui.action.pause")))
                .clicked()
            {
                Self::send(app, UiCommand::PauseJob(job.id));
            }
            if ui
                .add_enabled(actions.resume, egui::Button::new(t!("gui.action.resume")))
                .clicked()
            {
                Self::send(app, UiCommand::ResumeJob(job.id));
            }
            if ui
                .add_enabled(actions.cancel, egui::Button::new(t!("gui.action.cancel")))
                .clicked()
            {
                Self::send(app, UiCommand::CancelJob(job.id));
            }
            if ui
                .add_enabled(actions.remove, egui::Button::new(t!("gui.action.remove")))
                .clicked()
            {
                app.speeds.forget(job.id);
                Self::send(app, UiCommand::RemoveJob(job.id));
            }
        });
        egui::CollapsingHeader::new(t!("gui.job.details"))
            .id_salt(job.id)
            .show(ui, |ui| {
                ui.monospace(detail::detail_text(job, speed, app.tor_health.as_ref()));
            });
    }

    fn history(&mut self, ui: &mut egui::Ui, app: &mut App) {
        if app.history.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.weak(t!("ui.history.empty")));
            return;
        }
        let items = app.history.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for item in &items {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&item.filename).strong())
                                .on_hover_text(&item.url);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.colored_label(
                                        status_color(&item.status, ui.visuals().dark_mode),
                                        status_text(&item.status),
                                    );
                                },
                            );
                        });
                        ui.weak(t!(
                            "ui.history.stats",
                            size = fmt_bytes(item.total_bytes),
                            duration = detail::fmt_eta(item.duration_secs),
                            speed = detail::fmt_speed(item.avg_bytes_per_sec)
                        ));
                        if ui.button(t!("gui.history.remove")).clicked() {
                            Self::send(app, UiCommand::RemoveHistory(item.id));
                        }
                    });
                    ui.add_space(4.0);
                }
            });
    }

    /// The "add links" window: paste text, links are picked out of it.
    fn add_dialog(&mut self, ctx: &egui::Context, app: &mut App) {
        if !self.add_open {
            return;
        }
        let mut open = true;
        let mut text = std::mem::take(&mut self.add_text);
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new(t!("gui.add.title"))
            .open(&mut open)
            .collapsible(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .show(ctx, |ui| {
                ui.weak(t!("gui.add.hint"));
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if ui.button(t!("gui.add.clipboard")).clicked() {
                        if let Ok(clip) = import::read_clipboard() {
                            if !text.is_empty() && !text.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str(&clip);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        submit = ui.button(t!("gui.add.button")).clicked();
                        cancel = ui.button(t!("gui.captcha.cancel")).clicked();
                    });
                });
            });
        if submit && Self::add_links(app, &text) {
            text.clear();
            open = false;
        }
        if cancel {
            open = false;
        }
        self.add_text = text;
        self.add_open = open;
    }

    /// A plugin asked for a captcha answer; the answer goes back through the model.
    fn captcha_dialog(&mut self, ctx: &egui::Context, app: &mut App) {
        let InputPrompt::Captcha { challenge, .. } = &app.input_prompt else {
            return;
        };
        let prompt = match &challenge.payload {
            ChallengePayload::Text { prompt } => prompt.clone(),
            ChallengePayload::Image { instructions, .. } => {
                instructions.clone().unwrap_or_default()
            }
            ChallengePayload::Token { service, .. } => service.clone(),
        };
        let host = challenge.host.clone();
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new(t!("gui.captcha.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(t!("gui.captcha.asks", host = host));
                ui.label(prompt);
                let edit = ui.text_edit_singleline(&mut self.captcha_input);
                edit.request_focus();
                ui.horizontal(|ui| {
                    submit = ui.button(t!("gui.captcha.submit")).clicked()
                        || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    cancel = ui.button(t!("gui.captcha.cancel")).clicked();
                });
            });
        if submit {
            app.input_buffer = std::mem::take(&mut self.captcha_input);
            app.handle_key(KeyCode::Enter);
        } else if cancel {
            self.captcha_input.clear();
            app.handle_key(KeyCode::Esc);
        }
    }

    /// One frame of the window (separate from `eframe::App::ui` so tests can run it).
    fn show(&mut self, ui: &mut egui::Ui) {
        let app = self.app.clone();
        let mut a = app.lock().unwrap();

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, &mut a));
        egui::Panel::top("tabs").show(ui, |ui| self.tabs(ui));
        // Bottom panels stack upwards in the order they are added.
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, &a));
        egui::Panel::bottom("commands").show(ui, |ui| self.command_bar(ui, &mut a));
        if self.overview_open {
            egui::Panel::bottom("overview").show(ui, |ui| self.overview(ui, &a));
        }
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Downloads => self.downloads(ui, &mut a),
            Tab::History => self.history(ui, &mut a),
        });

        let ctx = ui.ctx().clone();
        self.add_dialog(&ctx, &mut a);
        self.captcha_dialog(&ctx, &mut a);
    }
}

impl eframe::App for GuiApp {
    /// Runs before every frame: applies engine events, no drawing.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut a = self.app.lock().unwrap();
        while let Ok(event) = a.job_event_rx.try_recv() {
            a.apply_event(event);
        }
        while let Ok(event) = a.grabber_event_rx.try_recv() {
            a.apply_grabber_event(event);
        }
        a.sample_speeds(Instant::now());
        ctx.request_repaint_after(REPAINT_EVERY);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn scope_text(scope: SearchScope) -> String {
    match scope {
        SearchScope::Name => t!("gui.scope.name"),
        SearchScope::Host => t!("gui.scope.host"),
        SearchScope::Url => t!("gui.scope.url"),
    }
}

fn filter_text(filter: StateFilter) -> String {
    match filter {
        StateFilter::All => t!("gui.filter.all"),
        StateFilter::Running => t!("gui.filter.running"),
        StateFilter::Paused => t!("gui.filter.paused"),
        StateFilter::Finished => t!("gui.filter.finished"),
        StateFilter::Failed => t!("gui.filter.failed"),
    }
}

fn tone_color(tone: Tone, dark: bool) -> egui::Color32 {
    match (tone, dark) {
        (Tone::Good, true) => egui::Color32::from_rgb(0x4a, 0xde, 0x80),
        (Tone::Good, false) => egui::Color32::from_rgb(0x15, 0x80, 0x3d),
        (Tone::Warn, true) => egui::Color32::from_rgb(0xfb, 0xbf, 0x24),
        (Tone::Warn, false) => egui::Color32::from_rgb(0xb4, 0x53, 0x09),
        (Tone::Bad, true) => egui::Color32::from_rgb(0xf8, 0x71, 0x71),
        (Tone::Bad, false) => egui::Color32::from_rgb(0xb9, 0x1c, 0x1c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::{PartProgress, PartStatus};
    use uuid::Uuid;

    fn job(status: JobStatus, done: u64, total: u64) -> JobProgress {
        JobProgress {
            id: Uuid::nil(),
            url: "http://x.test/a.bin".into(),
            filename: "a.bin".into(),
            total_bytes: total,
            bytes_done: done,
            parts: vec![],
            status,
        }
    }

    #[test]
    fn links_are_extracted_from_pasted_text() {
        let text =
            "see http://a.test/1 and\nhttps://b.test/2, ftp://c.test/3 mailto:x@y.z nonsense";
        assert_eq!(
            links_in(text),
            ["http://a.test/1", "https://b.test/2", "ftp://c.test/3"]
        );
        assert!(links_in("nothing here").is_empty());
    }

    #[test]
    fn buttons_follow_the_job_state() {
        let a = actions_for(&JobStatus::Downloading);
        assert_eq!(
            (a.pause, a.resume, a.cancel, a.remove),
            (true, false, true, true)
        );
        let a = actions_for(&JobStatus::Paused);
        assert_eq!((a.pause, a.resume, a.cancel), (false, true, true));
        let a = actions_for(&JobStatus::Done);
        assert_eq!(
            (a.pause, a.resume, a.cancel, a.remove),
            (false, false, false, true)
        );
        let a = actions_for(&JobStatus::Failed("x".into()));
        assert_eq!((a.cancel, a.remove), (false, true));
        assert!(actions_for(&JobStatus::Queued).cancel);
    }

    #[test]
    fn fraction_handles_unknown_size_and_completion() {
        assert_eq!(fraction(&job(JobStatus::Downloading, 50, 200)), Some(0.25));
        assert_eq!(fraction(&job(JobStatus::Downloading, 50, 0)), None);
        assert_eq!(fraction(&job(JobStatus::Done, 0, 0)), Some(1.0));
        assert_eq!(
            fraction(&job(JobStatus::Downloading, 900, 200)),
            Some(1.0),
            "never above 100%"
        );
    }

    #[test]
    fn progress_line_shows_speed_eta_and_parts() {
        let mut j = job(JobStatus::Downloading, 1024, 11 * 1024);
        j.parts = vec![
            PartProgress {
                index: 0,
                bytes_done: 0,
                total: 0,
                status: PartStatus::Downloading,
            },
            PartProgress {
                index: 1,
                bytes_done: 0,
                total: 0,
                status: PartStatus::Downloading,
            },
        ];
        let line = progress_line(&j, 1024);
        assert!(line.contains("1.00 KB") || line.contains("1024"), "{line}");
        assert!(line.contains("/s"), "{line}");
        assert!(line.contains("10s"), "{line}"); // 10 KiB left at 1 KiB/s
        assert!(line.contains("2 parts"), "{line}");

        let paused = progress_line(&job(JobStatus::Paused, 5, 10), 999);
        assert!(!paused.contains("/s"), "no speed while paused: {paused}");
        let unknown = progress_line(&job(JobStatus::Downloading, 2048, 0), 100);
        assert!(!unknown.contains("ETA"), "no ETA without a size: {unknown}");
    }

    #[test]
    fn status_text_has_no_dangling_colon() {
        assert!(!status_text(&JobStatus::Failed("boom".into())).ends_with(':'));
        assert_eq!(status_text(&JobStatus::Done), t!("ui.job.done"));
    }

    #[test]
    fn tor_label_without_tor_is_a_warning() {
        let (text, tone) = tor_label(None);
        assert_eq!(tone, Tone::Warn);
        assert_eq!(text, t!("gui.tor.off"));
    }

    #[test]
    fn a_display_check_does_not_panic() {
        let _ = display_available();
    }

    // ── the window renders without a display ──

    fn shared_app() -> (SharedApp, tokio::sync::mpsc::UnboundedReceiver<UiCommand>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        let (_gtx, grx) = tokio::sync::mpsc::unbounded_channel();
        (
            std::sync::Arc::new(std::sync::Mutex::new(App::new(cmd_tx, erx, grx))),
            cmd_rx,
        )
    }

    /// Runs `frames` frames of the window and returns how many shapes were drawn.
    fn render(gui: &mut GuiApp, frames: usize) -> usize {
        let ctx = egui::Context::default();
        let mut shapes = 0;
        for _ in 0..frames {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| gui.show(ui));
            // Nothing uploads textures in a test; egui asserts they were handled.
            out.textures_delta.clear();
            shapes = out.shapes.len();
        }
        shapes
    }

    #[test]
    fn empty_queue_renders() {
        let (app, _rx) = shared_app();
        let mut gui = GuiApp::new(app);
        assert!(render(&mut gui, 2) > 0);
    }

    #[test]
    fn queue_with_every_kind_of_job_renders() {
        let (app, _rx) = shared_app();
        {
            let mut a = app.lock().unwrap();
            for (i, status) in [
                JobStatus::Queued,
                JobStatus::Resolving,
                JobStatus::Downloading,
                JobStatus::Paused,
                JobStatus::Merging,
                JobStatus::Done,
                JobStatus::Cancelled,
                JobStatus::Failed("connection refused".into()),
            ]
            .into_iter()
            .enumerate()
            {
                let mut j = job(status, 300 * i as u64, 1000);
                j.id = Uuid::from_u128(i as u128 + 1);
                j.filename = format!("file-{i}.bin");
                a.jobs.push(j);
            }
            // One download whose size is unknown, with a hostile name.
            let mut j = job(JobStatus::Downloading, 5000, 0);
            j.id = Uuid::from_u128(99);
            j.filename = "<script>alert(1)</script>\u{202e}gpj.exe".into();
            a.jobs.push(j);
        }
        let mut gui = GuiApp::new(app);
        assert!(render(&mut gui, 3) > 50);
        gui.tab = Tab::History;
        assert!(render(&mut gui, 2) > 0);
    }

    #[test]
    fn adding_links_sends_the_matching_commands() {
        let (app, mut rx) = shared_app();
        let mut a = app.lock().unwrap();
        assert!(!GuiApp::add_links(&mut a, "no links"));
        assert!(rx.try_recv().is_err());
        assert!(GuiApp::add_links(&mut a, " http://a.test/1 "));
        assert_eq!(
            rx.try_recv().unwrap(),
            UiCommand::AddUrl("http://a.test/1".into())
        );
        assert!(GuiApp::add_links(
            &mut a,
            "http://a.test/1\nhttp://a.test/2"
        ));
        assert_eq!(
            rx.try_recv().unwrap(),
            UiCommand::AddUrls(vec!["http://a.test/1".into(), "http://a.test/2".into()])
        );
    }

    // ── frame: toolbar actions, dialog, filter, overview ──

    fn jobs_in_every_state() -> Vec<JobProgress> {
        [
            JobStatus::Queued,
            JobStatus::Downloading,
            JobStatus::Paused,
            JobStatus::Done,
            JobStatus::Failed("x".into()),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, status)| {
            let mut j = job(status, 10, 100);
            j.id = Uuid::from_u128(i as u128 + 1);
            j.filename = format!("f{i}.bin");
            j
        })
        .collect()
    }

    #[test]
    fn bulk_actions_pick_the_right_downloads() {
        let jobs = jobs_in_every_state();
        let ids = |c: Vec<UiCommand>| c.into_iter().collect::<Vec<_>>();
        assert_eq!(
            ids(bulk_commands(&jobs, Bulk::StartPaused)),
            vec![UiCommand::ResumeJob(Uuid::from_u128(3))]
        );
        assert_eq!(
            ids(bulk_commands(&jobs, Bulk::PauseRunning)),
            vec![UiCommand::PauseJob(Uuid::from_u128(2))]
        );
        assert_eq!(
            ids(bulk_commands(&jobs, Bulk::ClearFinished)),
            vec![UiCommand::RemoveJob(Uuid::from_u128(4))]
        );
        // Queued, running and paused downloads are stopped; finished and failed ones are left alone.
        assert_eq!(
            ids(bulk_commands(&jobs, Bulk::StopActive)),
            vec![
                UiCommand::CancelJob(Uuid::from_u128(1)),
                UiCommand::CancelJob(Uuid::from_u128(2)),
                UiCommand::CancelJob(Uuid::from_u128(3)),
            ]
        );
        assert!(bulk_commands(&[], Bulk::StopActive).is_empty());
    }

    #[test]
    fn the_add_dialog_renders_and_collapses() {
        let (app, _rx) = shared_app();
        let mut gui = GuiApp::new(app);
        gui.add_open = true;
        gui.add_text = "http://a.test/1".into();
        assert!(render(&mut gui, 2) > 0);
        assert!(gui.add_open, "stays open until submitted or cancelled");
    }

    #[test]
    fn filters_and_search_render_without_matches() {
        let (app, _rx) = shared_app();
        app.lock().unwrap().jobs = jobs_in_every_state();
        let mut gui = GuiApp::new(app);
        gui.filter = StateFilter::Finished;
        gui.query = "nothing like this".into();
        assert!(render(&mut gui, 2) > 0);
        gui.query.clear();
        gui.scope = SearchScope::Host;
        assert!(render(&mut gui, 1) > 0);
    }

    #[test]
    fn overview_can_be_hidden_and_shown() {
        let (app, _rx) = shared_app();
        app.lock().unwrap().jobs = jobs_in_every_state();
        let mut gui = GuiApp::new(app);
        let shown = render(&mut gui, 2);
        gui.overview_open = false;
        let hidden = render(&mut gui, 2);
        assert!(
            shown > hidden,
            "the overview panel draws shapes ({shown} vs {hidden})"
        );
    }

    #[test]
    fn a_selection_of_a_removed_download_is_dropped() {
        let (app, _rx) = shared_app();
        app.lock().unwrap().jobs = jobs_in_every_state();
        let mut gui = GuiApp::new(app.clone());
        gui.selected = Some(Uuid::from_u128(2));
        render(&mut gui, 1);
        assert_eq!(gui.selected, Some(Uuid::from_u128(2)));
        app.lock().unwrap().jobs.clear();
        render(&mut gui, 1);
        assert_eq!(gui.selected, None);
    }
}
