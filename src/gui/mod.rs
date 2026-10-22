//! Native desktop window (egui). It shows the same state as the terminal UI and
//! drives the same download engine in this process: no server, no browser.
//!
//! The window never touches the network or the disk itself. It reads the shared
//! [`App`] model, and every button sends a [`UiCommand`] through the same channel
//! the terminal UI uses. All visible text comes from the i18n catalog.

pub mod autostart;
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
use crate::ui::linkgrabber::{LinkSource, Package};
use crate::ui::{detail, fmt_bytes, import, App, InputPrompt, MoveTo, SharedApp, UiCommand};
use icons::Icon;
use model::{
    build_and_sort_packages, ColumnConfig, ColumnId, GuiItem, GuiPackage, SearchScope, SortConfig,
    SortDirection, StateFilter, Totals,
};

/// How often the window redraws when nothing else triggers it (live progress).
const REPAINT_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Downloads,
    LinkGrabber,
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
    add_save_to: String,
    add_package: String,
    add_comment: String,
    add_direct: bool,
    overview_open: bool,
    captcha_input: String,
    columns: ColumnConfig,
    sort: SortConfig,
    collapsed_packages: std::collections::HashSet<String>,
    grabber_collapsed_packages: std::collections::HashSet<usize>,
    settings_open: bool,
    settings_cfg: crate::config::Config,
    picker_open: bool,
    picker_path: std::path::PathBuf,
    picker_target_settings: bool,
}

impl GuiApp {
    fn new(app: SharedApp) -> Self {
        let initial_cfg = crate::config::load().unwrap_or_default();
        let initial_download_dir = initial_cfg
            .download_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                dirs::download_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .to_string_lossy()
                    .to_string()
            });
        Self {
            app,
            tab: Tab::Downloads,
            selected: None,
            filter: StateFilter::All,
            scope: SearchScope::Name,
            query: String::new(),
            add_open: false,
            add_text: String::new(),
            add_save_to: initial_download_dir,
            add_package: String::new(),
            add_comment: String::new(),
            add_direct: false,
            overview_open: true,
            captcha_input: String::new(),
            columns: ColumnConfig::default(),
            sort: SortConfig::default(),
            collapsed_packages: std::collections::HashSet::new(),
            grabber_collapsed_packages: std::collections::HashSet::new(),
            settings_open: false,
            settings_cfg: initial_cfg,
            picker_open: false,
            picker_path: dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from(".")),
            picker_target_settings: false,
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
            ui.separator();
            if icons::button(ui, Icon::Settings, &t!("gui.toolbar.settings"), true).clicked() {
                self.settings_open = true;
                self.settings_cfg = crate::config::load().unwrap_or_default();
            }
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui, app: &App) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Downloads, t!("gui.tab.downloads"));
            let grabber_label = {
                let count = app.grabber.links().len();
                if count > 0 {
                    format!("{} ({count})", t!("gui.tab.grabber"))
                } else {
                    t!("gui.tab.grabber")
                }
            };
            ui.selectable_value(&mut self.tab, Tab::LinkGrabber, grabber_label);
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

        let visible_jobs: Vec<JobProgress> = app
            .jobs
            .iter()
            .filter(|j| model::matches(j, self.filter, self.scope, &self.query))
            .cloned()
            .collect();

        if app.jobs.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.weak(t!("gui.queue.empty")));
            return;
        }
        if visible_jobs.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.weak(t!("gui.queue.no_match")));
            return;
        }

        // Convert visible jobs to GuiItems and group into packages
        let default_save = app.export_dir.to_str().unwrap_or("~/Downloads").to_string();
        let items: Vec<GuiItem> = visible_jobs
            .iter()
            .map(|j| {
                let speed = app.speeds.speed(j.id);
                let conn = if app.tor_health.is_some() {
                    t!("gui.connection.tor")
                } else {
                    t!("gui.connection.direct")
                };
                GuiItem::from_job(j, speed, &default_save, "", conn)
            })
            .collect();

        let default_pkg_title = t!("gui.package.default");
        let packages = build_and_sort_packages(items, self.sort, &default_pkg_title);

        self.table_header(ui);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for pkg in &packages {
                    self.package_row(ui, app, pkg);
                }
            });
    }

    fn table_header(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::canvas(ui.style());
        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                for col in ColumnId::ALL {
                    if !self.columns.is_visible(col) {
                        continue;
                    }
                    let title = t!(col.title_key());
                    let label = if self.sort.column == col {
                        let arrow = match self.sort.direction {
                            SortDirection::Ascending => " ▲",
                            SortDirection::Descending => " ▼",
                        };
                        format!("{title}{arrow}")
                    } else {
                        title
                    };
                    let button = egui::Button::new(egui::RichText::new(label).strong())
                        .min_size(egui::vec2(col.default_width(), 20.0));
                    if ui.add(button).clicked() {
                        if self.sort.column == col {
                            self.sort.direction = self.sort.direction.reverse();
                        } else {
                            self.sort.column = col;
                            self.sort.direction = SortDirection::Ascending;
                        }
                    }
                }

                // Column selector popup button at the right end of the header
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button(t!("gui.col.columns"), |ui| {
                        for col in ColumnId::ALL {
                            let mut visible = self.columns.is_visible(col);
                            let title = t!(col.title_key());
                            if ui.checkbox(&mut visible, title).changed() {
                                self.columns.set_visible(col, visible);
                            }
                        }
                    });
                });
            });
        });
        ui.separator();
    }

    fn package_row(&mut self, ui: &mut egui::Ui, app: &mut App, pkg: &GuiPackage) {
        let dark = ui.visuals().dark_mode;
        let is_collapsed = self.collapsed_packages.contains(&pkg.id);
        let arrow = if is_collapsed { "▶ " } else { "▼ " };

        let frame = egui::Frame::group(ui.style()).fill(if dark {
            egui::Color32::from_rgb(0x24, 0x27, 0x30)
        } else {
            egui::Color32::from_rgb(0xe5, 0xe9, 0xf0)
        });

        let pkg_resp = frame
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.columns.is_visible(ColumnId::Name) {
                        ui.allocate_ui(egui::vec2(ColumnId::Name.default_width(), 20.0), |ui| {
                            let text = format!("{arrow}{}", pkg.name);
                            if ui
                                .selectable_label(false, egui::RichText::new(text).strong())
                                .clicked()
                            {
                                if is_collapsed {
                                    self.collapsed_packages.remove(&pkg.id);
                                } else {
                                    self.collapsed_packages.insert(pkg.id.clone());
                                }
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Size) {
                        ui.allocate_ui(egui::vec2(ColumnId::Size.default_width(), 20.0), |ui| {
                            ui.label(fmt_bytes(pkg.total_size()));
                        });
                    }
                    if self.columns.is_visible(ColumnId::Host) {
                        ui.allocate_ui(egui::vec2(ColumnId::Host.default_width(), 20.0), |ui| {
                            ui.label(t!("gui.package.items", count = pkg.items.len()));
                        });
                    }
                    if self.columns.is_visible(ColumnId::Connection) {
                        ui.allocate_ui(
                            egui::vec2(ColumnId::Connection.default_width(), 20.0),
                            |ui| {
                                ui.label("");
                            },
                        );
                    }
                    if self.columns.is_visible(ColumnId::Status) {
                        ui.allocate_ui(egui::vec2(ColumnId::Status.default_width(), 20.0), |ui| {
                            let st = pkg.status_summary();
                            ui.colored_label(status_color(&st, dark), status_text(&st));
                        });
                    }
                    if self.columns.is_visible(ColumnId::Speed) {
                        ui.allocate_ui(egui::vec2(ColumnId::Speed.default_width(), 20.0), |ui| {
                            let speed = pkg.total_speed();
                            if speed > 0 {
                                ui.label(detail::fmt_speed(speed));
                            } else {
                                ui.label("–");
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Eta) {
                        ui.allocate_ui(egui::vec2(ColumnId::Eta.default_width(), 20.0), |ui| {
                            if let Some(eta) = pkg.eta() {
                                ui.label(detail::fmt_eta(eta));
                            } else {
                                ui.label("–");
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Loaded) {
                        ui.allocate_ui(egui::vec2(ColumnId::Loaded.default_width(), 20.0), |ui| {
                            ui.label(fmt_bytes(pkg.loaded_bytes()));
                        });
                    }
                    if self.columns.is_visible(ColumnId::Progress) {
                        ui.allocate_ui(
                            egui::vec2(ColumnId::Progress.default_width(), 20.0),
                            |ui| {
                                let bar = match pkg.progress() {
                                    Some(f) => egui::ProgressBar::new(f).show_percentage(),
                                    None => egui::ProgressBar::new(0.0),
                                };
                                ui.add(bar);
                            },
                        );
                    }
                    if self.columns.is_visible(ColumnId::SaveTo) {
                        ui.allocate_ui(egui::vec2(ColumnId::SaveTo.default_width(), 20.0), |ui| {
                            ui.label(&pkg.save_to);
                        });
                    }
                    if self.columns.is_visible(ColumnId::Comment) {
                        ui.allocate_ui(egui::vec2(ColumnId::Comment.default_width(), 20.0), |ui| {
                            ui.label(&pkg.comment);
                        });
                    }
                });
            })
            .response;

        pkg_resp.context_menu(|ui| {
            if !pkg.save_to.is_empty() && ui.button(t!("gui.context.open_folder")).clicked() {
                let _ = std::process::Command::new(if cfg!(target_os = "macos") {
                    "open"
                } else if cfg!(target_os = "windows") {
                    "explorer"
                } else {
                    "xdg-open"
                })
                .arg(&pkg.save_to)
                .spawn();
                ui.close();
            }
            if ui.button(t!("gui.context.start")).clicked() {
                for item in &pkg.items {
                    if actions_for(&item.status).resume {
                        Self::send(app, UiCommand::ResumeJob(item.id));
                    }
                }
                ui.close();
            }
            if ui.button(t!("gui.context.pause")).clicked() {
                for item in &pkg.items {
                    if actions_for(&item.status).pause {
                        Self::send(app, UiCommand::PauseJob(item.id));
                    }
                }
                ui.close();
            }
            if ui.button(t!("gui.context.cancel")).clicked() {
                for item in &pkg.items {
                    if actions_for(&item.status).cancel {
                        Self::send(app, UiCommand::CancelJob(item.id));
                    }
                }
                ui.close();
            }
            if ui.button(t!("gui.context.remove")).clicked() {
                for item in &pkg.items {
                    app.speeds.forget(item.id);
                    Self::send(app, UiCommand::RemoveJob(item.id));
                }
                ui.close();
            }
        });

        if !is_collapsed {
            for item in &pkg.items {
                self.item_row(ui, app, item);
            }
        }
    }

    fn item_row(&mut self, ui: &mut egui::Ui, app: &mut App, item: &GuiItem) {
        let dark = ui.visuals().dark_mode;
        let selected = self.selected == Some(item.id);
        let accent = ui.visuals().selection.bg_fill;

        let frame = egui::Frame::group(ui.style()).stroke(if selected {
            egui::Stroke::new(2.0, accent)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        });

        let response = frame
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.columns.is_visible(ColumnId::Name) {
                        ui.allocate_ui(egui::vec2(ColumnId::Name.default_width(), 20.0), |ui| {
                            let label = format!("   {}", item.name);
                            ui.label(label).on_hover_text(&item.raw.url);
                        });
                    }
                    if self.columns.is_visible(ColumnId::Size) {
                        ui.allocate_ui(egui::vec2(ColumnId::Size.default_width(), 20.0), |ui| {
                            if item.size > 0 {
                                ui.label(fmt_bytes(item.size));
                            } else {
                                ui.label("–");
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Host) {
                        ui.allocate_ui(egui::vec2(ColumnId::Host.default_width(), 20.0), |ui| {
                            ui.label(&item.host);
                        });
                    }
                    if self.columns.is_visible(ColumnId::Connection) {
                        ui.allocate_ui(
                            egui::vec2(ColumnId::Connection.default_width(), 20.0),
                            |ui| {
                                ui.label(&item.connection);
                            },
                        );
                    }
                    if self.columns.is_visible(ColumnId::Status) {
                        ui.allocate_ui(egui::vec2(ColumnId::Status.default_width(), 20.0), |ui| {
                            ui.colored_label(
                                status_color(&item.status, dark),
                                status_text(&item.status),
                            );
                        });
                    }
                    if self.columns.is_visible(ColumnId::Speed) {
                        ui.allocate_ui(egui::vec2(ColumnId::Speed.default_width(), 20.0), |ui| {
                            if item.speed > 0 {
                                ui.label(detail::fmt_speed(item.speed));
                            } else {
                                ui.label("–");
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Eta) {
                        ui.allocate_ui(egui::vec2(ColumnId::Eta.default_width(), 20.0), |ui| {
                            if let Some(eta) = item.eta {
                                ui.label(detail::fmt_eta(eta));
                            } else {
                                ui.label("–");
                            }
                        });
                    }
                    if self.columns.is_visible(ColumnId::Loaded) {
                        ui.allocate_ui(egui::vec2(ColumnId::Loaded.default_width(), 20.0), |ui| {
                            ui.label(fmt_bytes(item.loaded));
                        });
                    }
                    if self.columns.is_visible(ColumnId::Progress) {
                        ui.allocate_ui(
                            egui::vec2(ColumnId::Progress.default_width(), 20.0),
                            |ui| {
                                let bar = match item.progress {
                                    Some(f) => egui::ProgressBar::new(f).show_percentage(),
                                    None => egui::ProgressBar::new(0.0)
                                        .animate(matches!(item.status, JobStatus::Downloading)),
                                };
                                ui.add(bar);
                            },
                        );
                    }
                    if self.columns.is_visible(ColumnId::SaveTo) {
                        ui.allocate_ui(egui::vec2(ColumnId::SaveTo.default_width(), 20.0), |ui| {
                            ui.label(&item.save_to);
                        });
                    }
                    if self.columns.is_visible(ColumnId::Comment) {
                        ui.allocate_ui(egui::vec2(ColumnId::Comment.default_width(), 20.0), |ui| {
                            ui.label(&item.comment);
                        });
                    }
                });

                if selected {
                    ui.add_space(2.0);
                    ui.weak(progress_line(&item.raw, item.speed));
                    if let JobStatus::Failed(error) = &item.status {
                        ui.colored_label(status_color(&item.status, dark), error);
                    }
                    let actions = actions_for(&item.status);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(actions.pause, egui::Button::new(t!("gui.action.pause")))
                            .clicked()
                        {
                            Self::send(app, UiCommand::PauseJob(item.id));
                        }
                        if ui
                            .add_enabled(actions.resume, egui::Button::new(t!("gui.action.resume")))
                            .clicked()
                        {
                            Self::send(app, UiCommand::ResumeJob(item.id));
                        }
                        if ui
                            .add_enabled(actions.cancel, egui::Button::new(t!("gui.action.cancel")))
                            .clicked()
                        {
                            Self::send(app, UiCommand::CancelJob(item.id));
                        }
                        if ui
                            .add_enabled(actions.remove, egui::Button::new(t!("gui.action.remove")))
                            .clicked()
                        {
                            app.speeds.forget(item.id);
                            Self::send(app, UiCommand::RemoveJob(item.id));
                        }
                    });
                    egui::CollapsingHeader::new(t!("gui.job.details"))
                        .id_salt(item.id)
                        .show(ui, |ui| {
                            ui.monospace(detail::detail_text(
                                &item.raw,
                                item.speed,
                                app.tor_health.as_ref(),
                            ));
                        });
                }
            })
            .response;

        let interact_resp = ui.interact(
            response.rect,
            ui.make_persistent_id(("row", item.id)),
            egui::Sense::click(),
        );
        if interact_resp.clicked() {
            self.selected = Some(item.id);
        }
        interact_resp.context_menu(|ui| {
            self.selected = Some(item.id);
            let actions = actions_for(&item.status);
            if actions.resume && ui.button(t!("gui.context.start")).clicked() {
                Self::send(app, UiCommand::ResumeJob(item.id));
                ui.close();
            }
            if actions.pause && ui.button(t!("gui.context.pause")).clicked() {
                Self::send(app, UiCommand::PauseJob(item.id));
                ui.close();
            }
            if actions.cancel && ui.button(t!("gui.context.cancel")).clicked() {
                Self::send(app, UiCommand::CancelJob(item.id));
                ui.close();
            }
            if actions.remove && ui.button(t!("gui.context.remove")).clicked() {
                app.speeds.forget(item.id);
                Self::send(app, UiCommand::RemoveJob(item.id));
                ui.close();
            }
            ui.separator();
            if ui.button(t!("gui.context.copy_url")).clicked() {
                ui.ctx().copy_text(item.raw.url.clone());
                ui.close();
            }
            if !item.save_to.is_empty() && ui.button(t!("gui.context.open_folder")).clicked() {
                let _ = std::process::Command::new(if cfg!(target_os = "macos") {
                    "open"
                } else if cfg!(target_os = "windows") {
                    "explorer"
                } else {
                    "xdg-open"
                })
                .arg(&item.save_to)
                .spawn();
                ui.close();
            }
        });
        ui.add_space(2.0);
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

    fn link_grabber(&mut self, ui: &mut egui::Ui, app: &mut App) {
        let counts = app.grabber.counts();

        // Toolbar for the LinkGrabber tab
        ui.horizontal(|ui| {
            let can_start_all = counts.total > 0;
            let can_start_sel = counts.selected > 0;

            if ui
                .add_enabled(
                    can_start_all,
                    egui::Button::new(t!("gui.grabber.start_all")),
                )
                .clicked()
            {
                app.grabber.select_all(true);
                let urls = app.grabber.take_selected();
                if !urls.is_empty() {
                    let s_urls = urls.into_iter().map(|u| u.to_string()).collect();
                    Self::send(app, UiCommand::AddUrls(s_urls));
                    self.tab = Tab::Downloads;
                }
            }

            if ui
                .add_enabled(
                    can_start_sel,
                    egui::Button::new(t!("gui.grabber.start_selected")),
                )
                .clicked()
            {
                let urls = app.grabber.take_selected();
                if !urls.is_empty() {
                    let s_urls = urls.into_iter().map(|u| u.to_string()).collect();
                    Self::send(app, UiCommand::AddUrls(s_urls));
                    self.tab = Tab::Downloads;
                }
            }

            ui.separator();

            if ui.button(t!("gui.grabber.select_all")).clicked() {
                app.grabber.select_all(true);
            }
            if ui.button(t!("gui.grabber.deselect_all")).clicked() {
                app.grabber.select_all(false);
            }
            if ui
                .add_enabled(counts.total > 0, egui::Button::new(t!("gui.grabber.clear")))
                .clicked()
            {
                app.grabber.clear();
            }

            ui.separator();

            // Active filters indicators and clear button
            let host_filter_count = app.grabber.filter().excluded_hosts.len();
            let ext_filter_count = app.grabber.filter().excluded_extensions.len();
            let total_filters = host_filter_count + ext_filter_count;

            if total_filters > 0 {
                ui.weak(t!("gui.grabber.filter.active", count = total_filters));
                if ui.button(t!("gui.grabber.filter.clear")).clicked() {
                    app.grabber.filter_mut().excluded_hosts.clear();
                    app.grabber.filter_mut().excluded_extensions.clear();
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(t!(
                    "gui.grabber.summary",
                    selected = counts.selected,
                    total = counts.total,
                    filtered = counts.filtered
                ));
            });
        });

        ui.separator();

        if app.grabber.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.weak(t!("gui.grabber.empty")));
            return;
        }

        // Table / tree of packages and links
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let dark = ui.visuals().dark_mode;
                let packages_len = app.grabber.packages().len();

                for pkg_idx in 0..packages_len {
                    let is_collapsed = self.grabber_collapsed_packages.contains(&pkg_idx);
                    let arrow = if is_collapsed { "▶ " } else { "▼ " };

                    let (source_label, link_indices) = {
                        let pkg = &app.grabber.packages()[pkg_idx];
                        (package_label(pkg), pkg.links.clone())
                    };

                    let all_selected = !link_indices.is_empty()
                        && link_indices
                            .iter()
                            .all(|&i| app.grabber.link(i).map(|l| l.selected).unwrap_or(false));

                    let _any_selected = link_indices
                        .iter()
                        .any(|&i| app.grabber.link(i).map(|l| l.selected).unwrap_or(false));

                    // Package Header Card
                    let frame = egui::Frame::group(ui.style()).fill(if dark {
                        egui::Color32::from_rgb(0x28, 0x2c, 0x34)
                    } else {
                        egui::Color32::from_rgb(0xe0, 0xe5, 0xee)
                    });

                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut checked = all_selected;
                            if ui.checkbox(&mut checked, "").clicked() {
                                app.grabber.toggle_package(pkg_idx);
                            }

                            let title = format!("{arrow}{source_label}");
                            if ui
                                .selectable_label(false, egui::RichText::new(title).strong())
                                .clicked()
                            {
                                if is_collapsed {
                                    self.grabber_collapsed_packages.remove(&pkg_idx);
                                } else {
                                    self.grabber_collapsed_packages.insert(pkg_idx);
                                }
                            }

                            ui.weak(t!(
                                "ui.linkgrabber.package_count",
                                count = link_indices.len()
                            ));

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button(t!("gui.grabber.remove")).clicked() {
                                        app.grabber.remove_package(pkg_idx);
                                    }
                                },
                            );
                        });
                    });

                    // Child links
                    if !is_collapsed {
                        for &link_idx in &link_indices {
                            let link = match app.grabber.link(link_idx) {
                                Some(l) => l.clone(),
                                None => continue,
                            };

                            let is_excluded = app.grabber.filter().is_excluded(&link);
                            let host_excluded = app.grabber.filter().excludes_host(&link.host);
                            let ext_excluded =
                                app.grabber.filter().excludes_extension(&link.extension);

                            let item_frame = egui::Frame::group(ui.style()).fill(if is_excluded {
                                if dark {
                                    egui::Color32::from_rgb(0x20, 0x1c, 0x1c)
                                } else {
                                    egui::Color32::from_rgb(0xf8, 0xee, 0xee)
                                }
                            } else {
                                ui.visuals().widgets.noninteractive.bg_fill
                            });

                            item_frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let mut sel = link.selected;
                                    if ui.checkbox(&mut sel, "").clicked() {
                                        app.grabber.toggle_link(link_idx);
                                    }

                                    let name_text = if is_excluded {
                                        egui::RichText::new(&link.filename).strikethrough()
                                    } else {
                                        egui::RichText::new(&link.filename)
                                    };
                                    ui.label(name_text).on_hover_text(link.url.as_str());

                                    if let Some(sz) = link.size {
                                        ui.weak(fmt_bytes(sz));
                                    }

                                    // Host filter toggle
                                    let host_label = if host_excluded {
                                        format!("{}: [x]", link.host)
                                    } else {
                                        link.host.clone()
                                    };
                                    if ui
                                        .small_button(host_label)
                                        .on_hover_text(t!("gui.grabber.filter.host"))
                                        .clicked()
                                    {
                                        app.grabber.filter_mut().toggle_host(&link.host);
                                    }

                                    // Extension filter toggle
                                    if !link.extension.is_empty() {
                                        let ext_label = if ext_excluded {
                                            format!(".{}: [x]", link.extension)
                                        } else {
                                            format!(".{}", link.extension)
                                        };
                                        if ui
                                            .small_button(ext_label)
                                            .on_hover_text(t!("gui.grabber.filter.ext"))
                                            .clicked()
                                        {
                                            app.grabber
                                                .filter_mut()
                                                .toggle_extension(&link.extension);
                                        }
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.button(t!("gui.grabber.remove")).clicked() {
                                                app.grabber.remove_link(link_idx);
                                            }
                                        },
                                    );
                                });
                            });
                            ui.add_space(2.0);
                        }
                    }
                    ui.add_space(4.0);
                }
            });
    }

    /// The "add links" window: paste text, links are picked out of it, choose folder/package.
    fn add_dialog(&mut self, ctx: &egui::Context, app: &mut App) {
        if !self.add_open {
            return;
        }
        let mut open = true;
        let mut text = std::mem::take(&mut self.add_text);
        let mut submit = false;
        let mut cancel = false;
        let mut browse = false;

        egui::Window::new(t!("gui.add.title"))
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .show(ctx, |ui| {
                ui.weak(t!("gui.add.hint"));
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(4.0);

                // Save directory row
                ui.horizontal(|ui| {
                    ui.label(t!("gui.add.save_to"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_save_to)
                            .desired_width(ui.available_width() - 80.0),
                    );
                    if ui.button(t!("gui.add.browse")).clicked() {
                        browse = true;
                    }
                });

                // Package & Comment row
                ui.horizontal(|ui| {
                    ui.label(t!("gui.add.package"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_package)
                            .desired_width((ui.available_width() - 150.0).max(80.0)),
                    );
                    ui.label(t!("gui.add.comment"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_comment)
                            .desired_width(ui.available_width()),
                    );
                });

                // Checkbox: direct to queue or into link grabber
                ui.checkbox(&mut self.add_direct, t!("gui.add.direct_to_queue"));

                ui.add_space(6.0);

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
                        cancel = ui.button(t!("gui.settings.cancel")).clicked();
                    });
                });
            });

        if browse {
            self.picker_target_settings = false;
            let current = std::path::PathBuf::from(&self.add_save_to);
            self.picker_path = if current.is_dir() {
                current
            } else {
                dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            };
            self.picker_open = true;
        }

        if submit {
            let links = links_in(&text);
            if !links.is_empty() {
                if self.add_direct {
                    if links.len() == 1 {
                        Self::send(app, UiCommand::AddUrl(links[0].clone()));
                    } else {
                        Self::send(app, UiCommand::AddUrls(links));
                    }
                } else {
                    let parsed: Vec<url::Url> = links
                        .into_iter()
                        .filter_map(|s| url::Url::parse(&s).ok())
                        .collect();
                    let source = if self.add_package.trim().is_empty() {
                        LinkSource::Manual
                    } else {
                        LinkSource::File(self.add_package.trim().to_string())
                    };
                    app.grabber.add_links(source, parsed);
                    self.tab = Tab::LinkGrabber;
                }
                text.clear();
                open = false;
            }
        }
        if cancel {
            open = false;
        }
        self.add_text = text;
        self.add_open = open;
    }

    /// Settings dialog with tabs: General, Network/Tor, Limits/Retries
    fn settings_dialog(&mut self, ctx: &egui::Context, _app: &mut App) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let mut browse_download = false;
        let mut browse_temp = false;

        egui::Window::new(t!("gui.settings.title"))
            .open(&mut open)
            .collapsible(false)
            .default_width(540.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .show(ctx, |ui| {
                ui.heading(t!("gui.settings.general"));
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.download_dir"));
                    let mut dir_str = self
                        .settings_cfg
                        .download_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if ui.text_edit_singleline(&mut dir_str).changed() {
                        self.settings_cfg.download_dir = if dir_str.trim().is_empty() {
                            None
                        } else {
                            Some(std::path::PathBuf::from(dir_str.trim()))
                        };
                    }
                    if ui.button(t!("gui.add.browse")).clicked() {
                        browse_download = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.temp_dir"));
                    let mut temp_str = self
                        .settings_cfg
                        .temp_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if ui.text_edit_singleline(&mut temp_str).changed() {
                        self.settings_cfg.temp_dir = if temp_str.trim().is_empty() {
                            None
                        } else {
                            Some(std::path::PathBuf::from(temp_str.trim()))
                        };
                    }
                    if ui.button(t!("gui.add.browse")).clicked() {
                        browse_temp = true;
                    }
                });

                ui.checkbox(
                    &mut self.settings_cfg.auto_extract,
                    t!("gui.settings.auto_extract"),
                );
                ui.checkbox(
                    &mut self.settings_cfg.extract_delete_archives,
                    t!("gui.settings.extract_delete"),
                );
                ui.checkbox(
                    &mut self.settings_cfg.autostart,
                    t!("gui.settings.autostart"),
                );

                ui.separator();
                ui.heading(t!("gui.settings.network"));
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.max_parts"));
                    ui.add(egui::DragValue::new(&mut self.settings_cfg.max_parts).range(1..=32));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.log_level"));
                    egui::ComboBox::from_id_salt("log_level")
                        .selected_text(&self.settings_cfg.log_level)
                        .show_ui(ui, |ui| {
                            for lvl in ["error", "warn", "info", "debug", "trace"] {
                                ui.selectable_value(
                                    &mut self.settings_cfg.log_level,
                                    lvl.to_string(),
                                    lvl,
                                );
                            }
                        });
                });

                ui.separator();
                ui.heading(t!("gui.settings.limits"));
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.max_concurrent_jobs"));
                    ui.add(
                        egui::DragValue::new(&mut self.settings_cfg.max_concurrent_jobs)
                            .range(0..=100),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.max_jobs_per_host"));
                    ui.add(
                        egui::DragValue::new(&mut self.settings_cfg.max_jobs_per_host)
                            .range(0..=20),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.max_speed"));
                    ui.add(
                        egui::DragValue::new(&mut self.settings_cfg.max_speed_kib)
                            .range(0..=1_000_000),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.min_speed"));
                    ui.add(
                        egui::DragValue::new(&mut self.settings_cfg.min_speed_kib)
                            .range(0..=10_000),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t!("gui.settings.max_retries"));
                    ui.add(egui::DragValue::new(&mut self.settings_cfg.max_retries).range(0..=20));
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    save = ui.button(t!("gui.settings.save")).clicked();
                    cancel = ui.button(t!("gui.settings.cancel")).clicked();
                });
            });

        if browse_download {
            self.picker_target_settings = true;
            let current = self.settings_cfg.download_dir.clone().unwrap_or_else(|| {
                dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            self.picker_path = current;
            self.picker_open = true;
        } else if browse_temp {
            self.picker_target_settings = true;
            let current = self
                .settings_cfg
                .temp_dir
                .clone()
                .unwrap_or_else(std::env::temp_dir);
            self.picker_path = current;
            self.picker_open = true;
        }

        if save {
            let path = crate::config::Config::config_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(s) = toml::to_string_pretty(&self.settings_cfg) {
                let _ = std::fs::write(&path, s);
            }
            // Apply autostart setting to platform
            let _ = autostart::set_autostart(self.settings_cfg.autostart);
            open = false;
        }
        if cancel {
            open = false;
        }
        self.settings_open = open;
    }

    /// File / directory picker dialog for selecting a folder.
    fn picker_dialog(&mut self, ctx: &egui::Context) {
        if !self.picker_open {
            return;
        }
        let mut open = true;
        let mut select = false;
        let mut cancel = false;
        let mut nav_up = false;
        let mut nav_into: Option<std::path::PathBuf> = None;

        egui::Window::new(t!("gui.picker.title"))
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let path_display = self.picker_path.to_string_lossy().to_string();
                ui.label(t!("gui.picker.current", path = path_display));

                ui.horizontal(|ui| {
                    if self.picker_path.parent().is_some()
                        && ui.button(t!("gui.picker.up")).clicked()
                    {
                        nav_up = true;
                    }
                    select = ui.button(t!("gui.picker.select")).clicked();
                    cancel = ui.button(t!("gui.settings.cancel")).clicked();
                });

                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        if let Ok(entries) = std::fs::read_dir(&self.picker_path) {
                            let mut dirs: Vec<std::path::PathBuf> = entries
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| p.is_dir())
                                .collect();
                            dirs.sort();
                            for d in dirs {
                                let name = d
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "/".into());
                                if ui.button(format!("📁 {name}")).clicked() {
                                    nav_into = Some(d);
                                }
                            }
                        }
                    });
            });

        if nav_up {
            if let Some(parent) = self.picker_path.parent() {
                self.picker_path = parent.to_path_buf();
            }
        }
        if let Some(into) = nav_into {
            self.picker_path = into;
        }
        if select {
            let chosen = self.picker_path.to_string_lossy().to_string();
            if self.picker_target_settings {
                self.settings_cfg.download_dir = Some(self.picker_path.clone());
            } else {
                self.add_save_to = chosen;
            }
            open = false;
        }
        if cancel {
            open = false;
        }
        self.picker_open = open;
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
        egui::Panel::top("tabs").show(ui, |ui| self.tabs(ui, &a));
        // Bottom panels stack upwards in the order they are added.
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, &a));
        egui::Panel::bottom("commands").show(ui, |ui| self.command_bar(ui, &mut a));
        if self.overview_open {
            egui::Panel::bottom("overview").show(ui, |ui| self.overview(ui, &a));
        }
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Downloads => self.downloads(ui, &mut a),
            Tab::LinkGrabber => self.link_grabber(ui, &mut a),
            Tab::History => self.history(ui, &mut a),
        });

        let ctx = ui.ctx().clone();
        self.add_dialog(&ctx, &mut a);
        self.settings_dialog(&ctx, &mut a);
        self.picker_dialog(&ctx);
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

fn package_label(package: &Package) -> String {
    match &package.source {
        LinkSource::Manual => t!("ui.linkgrabber.package_manual"),
        LinkSource::Clipboard => t!("ui.linkgrabber.package_clipboard"),
        LinkSource::File(name) => t!("ui.linkgrabber.package_file", name = name),
        LinkSource::Crawl(page) => t!("ui.linkgrabber.package_crawl", page = page.clone()),
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

    #[test]
    fn table_columns_and_sorting_render() {
        let (app, _rx) = shared_app();
        app.lock().unwrap().jobs = jobs_in_every_state();
        let mut gui = GuiApp::new(app);
        // Test rendering with different sorting configs
        gui.sort = SortConfig {
            column: ColumnId::Size,
            direction: SortDirection::Descending,
        };
        assert!(render(&mut gui, 2) > 0);

        // Toggle columns visibility
        gui.columns.toggle(ColumnId::Comment);
        gui.columns.toggle(ColumnId::SaveTo);
        assert!(render(&mut gui, 2) > 0);

        // Test collapsed package
        let pkg_id = t!("gui.package.default");
        gui.collapsed_packages.insert(pkg_id);
        assert!(render(&mut gui, 2) > 0);
    }

    #[test]
    fn link_grabber_tab_renders_and_interacts() {
        let (app, _rx) = shared_app();
        {
            let mut a = app.lock().unwrap();
            a.grabber.add_links(
                LinkSource::Manual,
                [
                    url::Url::parse("https://alpha.test/archive.zip").unwrap(),
                    url::Url::parse("https://beta.test/image.png").unwrap(),
                ],
            );
        }
        let mut gui = GuiApp::new(app.clone());
        gui.tab = Tab::LinkGrabber;
        assert!(render(&mut gui, 2) > 0);

        // Toggle collapse
        gui.grabber_collapsed_packages.insert(0);
        assert!(render(&mut gui, 1) > 0);
        gui.grabber_collapsed_packages.clear();

        // Filter host and extension
        {
            let mut a = app.lock().unwrap();
            a.grabber.filter_mut().toggle_host("alpha.test");
            a.grabber.filter_mut().toggle_extension("png");
        }
        assert!(render(&mut gui, 1) > 0);

        // Clear and render empty grabber tab
        {
            let mut a = app.lock().unwrap();
            a.grabber.clear();
        }
        assert!(render(&mut gui, 1) > 0);
    }

    #[test]
    fn dialogs_and_menus_render() {
        let (app, _rx) = shared_app();
        app.lock().unwrap().jobs = jobs_in_every_state();
        let mut gui = GuiApp::new(app);

        // Add dialog open
        gui.add_open = true;
        gui.add_text = "https://example.com/file.zip".into();
        gui.add_package = "MyPackage".into();
        gui.add_comment = "test comment".into();
        gui.add_direct = true;
        assert!(render(&mut gui, 2) > 0);
        gui.add_open = false;

        // Settings dialog open
        gui.settings_open = true;
        assert!(render(&mut gui, 2) > 0);
        gui.settings_open = false;

        // Picker dialog open
        gui.picker_open = true;
        gui.picker_path = std::env::temp_dir();
        assert!(render(&mut gui, 2) > 0);
        gui.picker_open = false;
    }
}
