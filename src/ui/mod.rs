//! TUI (ratatui/crossterm) – JDownloader-like: link list, progress, status bar.
//!
//! Uses `std::sync::Mutex` for app state (synchronous draw loop); communication
//! with download tasks runs over `tokio::sync::mpsc`.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

pub(crate) mod detail;
pub(crate) mod import;
pub mod linkgrabber;

pub use linkgrabber::{GrabberEvent, LinkGrabber, LinkSource, Package};

use crate::config;
use crate::contract::{JobEvent, JobId};
use crate::download::{JobProgress, JobStatus};
use crate::plugin::captcha::{CaptchaChallenge, ChallengePayload};
use crate::t;
use crate::tor::TorHealth;
use crate::tracker::export::{self, ExportFormat};
use crate::tracker::history::HistoryItem;
use linkgrabber::GrabberRow;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AppTab {
    Downloads,
    History,
    LinkGrabber,
    Plugins,
    Settings,
}

/// Command the UI can send to the download engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    AddUrl(String),
    /// Adds several URLs at once (the confirmed links of the link grabber).
    AddUrls(Vec<String>),
    /// Crawls a page with a crawler plugin and feeds the links to the grabber.
    CrawlPage(String),
    PauseJob(uuid::Uuid),
    ResumeJob(uuid::Uuid),
    CancelJob(uuid::Uuid),
    RemoveJob(uuid::Uuid),
    /// Removes a finished download from the history.
    RemoveHistory(uuid::Uuid),
    /// Change the position of a download in the queue list.
    MoveJob(uuid::Uuid, MoveTo),
}

/// Where a download is moved to in the queue list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTo {
    Top,
    Up,
    Down,
    Bottom,
}

/// Moves the job `id` inside `jobs`; does nothing if it is unknown or already there.
pub fn move_job(jobs: &mut Vec<JobProgress>, id: uuid::Uuid, to: MoveTo) {
    let Some(from) = jobs.iter().position(|j| j.id == id) else {
        return;
    };
    let target = match to {
        MoveTo::Top => 0,
        MoveTo::Up => from.saturating_sub(1),
        MoveTo::Down => (from + 1).min(jobs.len() - 1),
        MoveTo::Bottom => jobs.len() - 1,
    };
    if target != from {
        let job = jobs.remove(from);
        jobs.insert(target, job);
    }
}

/// Active interactive prompt state in the UI.
pub enum InputPrompt {
    AddUrl,
    ImportFile,
    /// Collects a single URL in the link grabber.
    GrabUrl,
    /// Collects the links of a file in the link grabber.
    GrabImport,
    /// Crawls a page and collects the links it finds.
    CrawlPage,
    Captcha {
        challenge: CaptchaChallenge,
        reply_tx: oneshot::Sender<String>,
    },
}

pub struct App {
    pub tab: AppTab,
    pub jobs: Vec<JobProgress>,
    /// The finished downloads with their statistics.
    pub history: Vec<HistoryItem>,
    /// Directory the history exports are written to.
    pub export_dir: PathBuf,
    pub should_quit: bool,
    pub list_state: ListState,
    /// Selection of the history list.
    pub history_state: ListState,
    pub input_mode: bool,
    pub input_buffer: String,
    pub input_prompt: InputPrompt,
    pub status_msg: String,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    pub job_event_rx: tokio::sync::mpsc::UnboundedReceiver<JobEvent>,
    /// The links collected in the link grabber (staging area of the queue).
    pub grabber: LinkGrabber,
    /// Results of the crawler plugins for the link grabber.
    pub grabber_event_rx: tokio::sync::mpsc::UnboundedReceiver<GrabberEvent>,
    /// Selection of the link grabber list.
    pub grabber_state: ListState,
    /// Whether the detail panel of the selected job is shown.
    pub detail_open: bool,
    /// Latest Tor readiness snapshot (`None` without Tor).
    pub tor_health: Option<TorHealth>,
    pub speeds: detail::SpeedTracker,
}

impl App {
    pub fn new(
        cmd_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
        job_event_rx: tokio::sync::mpsc::UnboundedReceiver<JobEvent>,
        grabber_event_rx: tokio::sync::mpsc::UnboundedReceiver<GrabberEvent>,
    ) -> Self {
        Self {
            tab: AppTab::Downloads,
            jobs: Vec::new(),
            history: Vec::new(),
            export_dir: crate::tracker::Tracker::data_dir(),
            should_quit: false,
            list_state: ListState::default(),
            history_state: ListState::default(),
            input_mode: false,
            input_buffer: String::new(),
            input_prompt: InputPrompt::AddUrl,
            status_msg: t!("ui.status.help"),
            cmd_tx,
            job_event_rx,
            grabber: LinkGrabber::new(),
            grabber_event_rx,
            grabber_state: ListState::default(),
            detail_open: false,
            tor_health: None,
            speeds: detail::SpeedTracker::default(),
        }
    }

    /// Triggers an interactive Captcha challenge dialog.
    pub fn prompt_captcha(
        &mut self,
        challenge: CaptchaChallenge,
        reply_tx: oneshot::Sender<String>,
    ) {
        self.input_mode = true;
        self.input_buffer.clear();
        self.input_prompt = InputPrompt::Captcha {
            challenge,
            reply_tx,
        };
    }

    /// Queues every URL of a batch import and reports the outcome in the status line.
    pub(crate) fn enqueue_import(&mut self, result: anyhow::Result<import::Import>) {
        self.status_msg = match result {
            Err(e) => t!("ui.status.import_failed", error = e),
            Ok(imp) if imp.urls.is_empty() => t!("ui.status.import_none"),
            Ok(imp) => {
                for url in &imp.urls {
                    let _ = self.cmd_tx.send(UiCommand::AddUrl(url.to_string()));
                }
                t!(
                    "ui.status.import_done",
                    count = imp.urls.len(),
                    skipped = imp.skipped
                )
            }
        };
    }

    /// Collects links in the link grabber and reports the result in the status line.
    fn collect_links(&mut self, source: LinkSource, urls: Vec<url::Url>) {
        let added = self.grabber.add_links(source, urls);
        self.status_msg = if added.added == 0 {
            t!("ui.linkgrabber.duplicate_only")
        } else {
            t!(
                "ui.linkgrabber.added",
                count = added.added,
                skipped = added.duplicates
            )
        };
    }

    /// The row the cursor is on in the link grabber list.
    fn current_grabber_row(&self) -> Option<GrabberRow> {
        let index = self.grabber_state.selected()?;
        self.grabber.rows().get(index).copied()
    }

    /// The link the cursor is on (the first link of the package if the cursor
    /// is on a package header).
    fn current_grabber_link(&self) -> Option<&linkgrabber::LinkCandidate> {
        match self.current_grabber_row()? {
            GrabberRow::Link(i) => self.grabber.link(i),
            GrabberRow::Package(p) => {
                let first = *self.grabber.packages().get(p)?.links.first()?;
                self.grabber.link(first)
            }
        }
    }

    /// Marks the link under the cursor or unmarks it.
    fn toggle_grabber_link(&mut self) {
        let Some(GrabberRow::Link(i)) = self.current_grabber_row() else {
            return;
        };
        self.grabber.toggle_link(i);
        self.status_msg = self.grabber_counts_msg();
    }

    /// Marks all links of the package under the cursor or unmarks them.
    fn toggle_grabber_package(&mut self) {
        let Some(GrabberRow::Package(p)) = self.current_grabber_row() else {
            return;
        };
        self.grabber.toggle_package(p);
        self.status_msg = self.grabber_counts_msg();
    }

    /// Marks every link or unmarks all of them.
    fn toggle_grabber_all(&mut self) {
        let all = self.grabber.links().iter().all(|l| l.selected);
        self.grabber.select_all(!all);
        self.status_msg = self.grabber_counts_msg();
    }

    /// Excludes the host of the link under the cursor (or includes it again).
    fn toggle_grabber_host_filter(&mut self) {
        let Some(host) = self.current_grabber_link().map(|l| l.host.clone()) else {
            return;
        };
        let excluded = self.grabber.filter_mut().toggle_host(&host);
        self.status_msg = t!(
            "ui.linkgrabber.filter_host",
            host = host,
            state = filter_state(excluded)
        );
    }

    /// Excludes the file type of the link under the cursor (or includes it again).
    fn toggle_grabber_type_filter(&mut self) {
        let Some(extension) = self.current_grabber_link().map(|l| l.extension.clone()) else {
            return;
        };
        if extension.is_empty() {
            self.status_msg = t!("ui.linkgrabber.filter_no_type");
            return;
        }
        let excluded = self.grabber.filter_mut().toggle_extension(&extension);
        self.status_msg = t!(
            "ui.linkgrabber.filter_type",
            extension = extension,
            state = filter_state(excluded)
        );
    }

    /// Removes the link or the package under the cursor.
    fn remove_grabber_entry(&mut self) {
        let row = self.current_grabber_row();
        let index = self.grabber_state.selected().unwrap_or(0);
        match row {
            Some(GrabberRow::Link(i)) => {
                if let Some(link) = self.grabber.remove_link(i) {
                    self.status_msg = t!("ui.linkgrabber.removed", file = link.filename);
                }
            }
            Some(GrabberRow::Package(p)) => {
                if let Some(package) = self.grabber.remove_package(p) {
                    self.status_msg = t!(
                        "ui.linkgrabber.package_removed",
                        name = package_label(&package)
                    );
                }
            }
            None => {}
        }
        // The cursor stays on the list (the row under it moved up).
        let rows = self.grabber.rows().len();
        self.grabber_state.select(if rows == 0 {
            None
        } else {
            Some(index.min(rows - 1))
        });
    }

    /// Transfers the marked links into the download queue.
    fn confirm_grabber(&mut self) {
        let urls = self.grabber.take_selected();
        if urls.is_empty() {
            self.status_msg = t!("ui.linkgrabber.nothing_selected");
            return;
        }
        let count = urls.len();
        let urls: Vec<String> = urls.into_iter().map(|u| u.to_string()).collect();
        let _ = self.cmd_tx.send(UiCommand::AddUrls(urls));
        self.status_msg = t!("ui.linkgrabber.transferred", count = count);
    }

    /// The counters of the link grabber (marked links, filtered links).
    fn grabber_counts_msg(&self) -> String {
        let counts = self.grabber.counts();
        t!(
            "ui.linkgrabber.counts",
            selected = counts.selected,
            total = counts.total,
            filtered = counts.filtered
        )
    }

    fn next_tab(&mut self) {
        self.tab = match self.tab {
            AppTab::Downloads => AppTab::History,
            AppTab::History => AppTab::LinkGrabber,
            AppTab::LinkGrabber => AppTab::Plugins,
            AppTab::Plugins => AppTab::Settings,
            AppTab::Settings => AppTab::Downloads,
        };
    }

    fn prev_tab(&mut self) {
        self.tab = match self.tab {
            AppTab::Downloads => AppTab::Settings,
            AppTab::History => AppTab::Downloads,
            AppTab::LinkGrabber => AppTab::History,
            AppTab::Plugins => AppTab::LinkGrabber,
            AppTab::Settings => AppTab::Plugins,
        };
    }

    /// Moves the selection of the visible list one entry up.
    fn select_previous(&mut self) {
        match self.tab {
            AppTab::History => {
                let i = self.history_state.selected().unwrap_or(0);
                if i > 0 {
                    self.history_state.select(Some(i - 1));
                }
            }
            AppTab::LinkGrabber => {
                let i = self.grabber_state.selected().unwrap_or(0);
                if i > 0 {
                    self.grabber_state.select(Some(i - 1));
                }
            }
            _ => {
                let i = self.list_state.selected().unwrap_or(0);
                if i > 0 {
                    self.list_state.select(Some(i - 1));
                }
            }
        }
    }

    /// Moves the selection of the visible list one entry down.
    fn select_next(&mut self) {
        match self.tab {
            AppTab::History => {
                let i = self.history_state.selected().unwrap_or(0);
                if i + 1 < self.history.len() {
                    self.history_state.select(Some(i + 1));
                }
            }
            AppTab::LinkGrabber => {
                let rows = self.grabber.rows().len();
                match self.grabber_state.selected() {
                    None => {
                        if rows > 0 {
                            self.grabber_state.select(Some(0));
                        }
                    }
                    Some(i) => {
                        if i + 1 < rows {
                            self.grabber_state.select(Some(i + 1));
                        }
                    }
                }
            }
            _ => {
                let i = self.list_state.selected().unwrap_or(0);
                if i + 1 < self.jobs.len() {
                    self.list_state.select(Some(i + 1));
                }
            }
        }
    }

    /// Writes the history to the export file of `format`.
    fn export_history(&mut self, format: ExportFormat) {
        let path = export::path_in(&self.export_dir, format);
        match export::write(&path, &self.history, format) {
            Ok(()) => {
                self.status_msg = t!(
                    "ui.status.export_ok",
                    count = self.history.len(),
                    path = path.display().to_string()
                );
            }
            Err(e) => {
                self.status_msg = t!("ui.status.export_failed", error = format!("{e:#}"));
            }
        }
    }

    /// Removes the selected entry from the history.
    fn remove_history_entry(&mut self) {
        let Some(i) = self.history_state.selected() else {
            return;
        };
        if i >= self.history.len() {
            return;
        }
        let item = self.history.remove(i);
        let _ = self.cmd_tx.send(UiCommand::RemoveHistory(item.id));
        self.status_msg = t!("ui.status.history_removed", file = item.filename);
        if self.history.is_empty() {
            self.history_state.select(None);
        } else if i >= self.history.len() {
            self.history_state.select(Some(self.history.len() - 1));
        }
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode) {
        if self.input_mode {
            match code {
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_buffer.clear();
                    self.input_prompt = InputPrompt::AddUrl;
                }
                KeyCode::Enter => {
                    let text = self.input_buffer.trim().to_string();
                    let prompt = std::mem::replace(&mut self.input_prompt, InputPrompt::AddUrl);
                    self.input_mode = false;
                    self.input_buffer.clear();

                    match prompt {
                        InputPrompt::AddUrl => {
                            if !text.is_empty() {
                                let _ = self.cmd_tx.send(UiCommand::AddUrl(text.clone()));
                                self.status_msg = t!("ui.status.queued_url", url = text);
                            }
                        }
                        InputPrompt::ImportFile => {
                            if !text.is_empty() {
                                let result = import::import_file(&import::expand_home(&text));
                                self.enqueue_import(result);
                            }
                        }
                        InputPrompt::GrabUrl => {
                            if !text.is_empty() {
                                match url::Url::parse(&text) {
                                    Ok(url) => self.collect_links(LinkSource::Manual, vec![url]),
                                    Err(_) => self.status_msg = t!("ui.status.invalid_url"),
                                }
                            }
                        }
                        InputPrompt::GrabImport => {
                            if !text.is_empty() {
                                let path = import::expand_home(&text);
                                match import::import_file(&path) {
                                    Ok(imp) => {
                                        let source =
                                            LinkSource::File(path.to_string_lossy().into());
                                        self.collect_links(source, imp.urls);
                                    }
                                    Err(e) => {
                                        self.status_msg =
                                            t!("ui.status.import_failed", error = format!("{e:#}"));
                                    }
                                }
                            }
                        }
                        InputPrompt::CrawlPage => {
                            if !text.is_empty() {
                                let _ = self.cmd_tx.send(UiCommand::CrawlPage(text.clone()));
                                self.status_msg = t!("ui.linkgrabber.crawl_started", page = text);
                            }
                        }
                        InputPrompt::Captcha {
                            challenge,
                            reply_tx,
                        } => {
                            let _ = reply_tx.send(text);
                            self.status_msg = format!("Submitted captcha for {}", challenge.host);
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                }
                _ => {}
            }
            return;
        }
        // The link grabber has its own keys; the global ones (quit, tabs,
        // cursor) keep working.
        if self.tab == AppTab::LinkGrabber && self.handle_grabber_key(code) {
            return;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('a') => {
                self.input_mode = true;
                self.input_buffer.clear();
                self.input_prompt = InputPrompt::AddUrl;
            }
            KeyCode::Char('i') => {
                self.input_mode = true;
                self.input_buffer.clear();
                self.input_prompt = InputPrompt::ImportFile;
            }
            KeyCode::Char('v') => {
                let result = import::read_clipboard().map(|text| import::parse_urls(&text));
                self.enqueue_import(result);
            }
            KeyCode::Enter => self.detail_open = !self.detail_open,
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Up => self.select_previous(),
            KeyCode::Down => self.select_next(),
            KeyCode::Char('e') if self.tab == AppTab::History => {
                self.export_history(ExportFormat::Csv);
            }
            KeyCode::Char('E') if self.tab == AppTab::History => {
                self.export_history(ExportFormat::Json);
            }
            KeyCode::Char('x') if self.tab == AppTab::History => {
                self.remove_history_entry();
            }
            KeyCode::Char('p') => {
                if let Some(i) = self.list_state.selected() {
                    if let Some(j) = self.jobs.get_mut(i) {
                        let id = j.id;
                        let filename = j.filename.clone();
                        j.status = JobStatus::Paused;
                        let _ = self.cmd_tx.send(UiCommand::PauseJob(id));
                        self.status_msg = t!("ui.status.pause_requested", file = filename);
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(i) = self.list_state.selected() {
                    if let Some(j) = self.jobs.get_mut(i) {
                        let id = j.id;
                        let filename = j.filename.clone();
                        if matches!(j.status, JobStatus::Paused) {
                            j.status = JobStatus::Downloading;
                        }
                        let _ = self.cmd_tx.send(UiCommand::ResumeJob(id));
                        self.status_msg = t!("ui.status.resume_requested", file = filename);
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(i) = self.list_state.selected() {
                    if let Some(j) = self.jobs.get_mut(i) {
                        let id = j.id;
                        let filename = j.filename.clone();
                        j.status = JobStatus::Cancelled;
                        let _ = self.cmd_tx.send(UiCommand::CancelJob(id));
                        self.status_msg = t!("ui.status.cancel_requested", file = filename);
                    }
                }
            }
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some(i) = self.list_state.selected() {
                    if i < self.jobs.len() {
                        let removed = self.jobs.remove(i);
                        self.speeds.forget(removed.id);
                        let _ = self.cmd_tx.send(UiCommand::RemoveJob(removed.id));
                        self.status_msg = t!("ui.status.remove_requested", file = removed.filename);
                        if self.jobs.is_empty() {
                            self.list_state.select(None);
                        } else if i >= self.jobs.len() {
                            self.list_state.select(Some(self.jobs.len() - 1));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Handles the keys of the link grabber tab.
    ///
    /// Returns `true` if the key was used by the grabber.
    fn handle_grabber_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('a') => {
                self.input_mode = true;
                self.input_buffer.clear();
                self.input_prompt = InputPrompt::GrabUrl;
                true
            }
            KeyCode::Char('i') => {
                self.input_mode = true;
                self.input_buffer.clear();
                self.input_prompt = InputPrompt::GrabImport;
                true
            }
            KeyCode::Char('v') => {
                match import::read_clipboard() {
                    Ok(text) => {
                        let imp = import::parse_urls(&text);
                        self.collect_links(LinkSource::Clipboard, imp.urls);
                    }
                    Err(e) => {
                        self.status_msg = t!("ui.status.import_failed", error = format!("{e:#}"));
                    }
                }
                true
            }
            KeyCode::Char('c') => {
                self.input_mode = true;
                self.input_buffer.clear();
                self.input_prompt = InputPrompt::CrawlPage;
                true
            }
            KeyCode::Char(' ') => {
                self.toggle_grabber_link();
                true
            }
            KeyCode::Char('p') => {
                self.toggle_grabber_package();
                true
            }
            KeyCode::Char('A') => {
                self.toggle_grabber_all();
                true
            }
            KeyCode::Char('f') => {
                self.toggle_grabber_host_filter();
                true
            }
            KeyCode::Char('F') => {
                self.toggle_grabber_type_filter();
                true
            }
            KeyCode::Char('x') => {
                self.remove_grabber_entry();
                true
            }
            KeyCode::Enter => {
                self.confirm_grabber();
                true
            }
            _ => false,
        }
    }

    /// Applies a contract-B `JobEvent` to the app state (live queue update).
    pub fn apply_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Status { id, status } => {
                let j = self.ensure_job(id);
                j.status = status;
            }
            JobEvent::Resolved {
                id,
                filename,
                total_bytes,
            } => {
                let j = self.ensure_job(id);
                j.filename = filename;
                j.total_bytes = total_bytes.unwrap_or(0);
                if matches!(j.status, JobStatus::Queued | JobStatus::Resolving) {
                    j.status = JobStatus::Downloading;
                }
            }
            JobEvent::Progress {
                id,
                bytes_done,
                parts,
            } => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.bytes_done = bytes_done;
                    j.parts = parts;
                }
            }
            JobEvent::Speed { id, bytes_per_sec } => {
                self.speeds.set_reported(id, bytes_per_sec, Instant::now());
            }
            JobEvent::Finished { id, result } => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    if !matches!(
                        j.status,
                        JobStatus::Cancelled | JobStatus::Done | JobStatus::Failed(_)
                    ) {
                        j.status = match result {
                            Ok(()) => JobStatus::Done,
                            Err(e) => JobStatus::Failed(e),
                        };
                    }
                }
            }
        }
    }

    /// Applies a crawler result to the link grabber.
    pub fn apply_grabber_event(&mut self, event: GrabberEvent) {
        match event {
            GrabberEvent::LinksFound { page, links } => {
                let added = self.grabber.add_crawled(&page, links);
                self.status_msg = t!(
                    "ui.linkgrabber.crawl_done",
                    count = added.added,
                    page = page.to_string()
                );
            }
            GrabberEvent::CrawlFailed { page, error } => {
                self.status_msg = t!(
                    "ui.linkgrabber.crawl_failed",
                    error = error,
                    page = page.to_string()
                );
            }
        }
    }

    /// Feeds the current progress of all jobs into the speed estimator.
    pub fn sample_speeds(&mut self, now: Instant) {
        for j in &self.jobs {
            self.speeds.observe(j.id, j.bytes_done, now);
        }
    }

    fn ensure_job(&mut self, id: JobId) -> &mut JobProgress {
        if !self.jobs.iter().any(|j| j.id == id) {
            self.jobs.push(JobProgress {
                id,
                url: String::new(),
                filename: String::new(),
                total_bytes: 0,
                bytes_done: 0,
                parts: vec![],
                status: JobStatus::Queued,
            });
        }
        self.jobs.iter_mut().find(|j| j.id == id).unwrap()
    }
}

pub type SharedApp = Arc<Mutex<App>>;

/// Starts the TUI loop (blocking, on the main thread).
pub fn run(app: SharedApp) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    loop {
        {
            let mut a = app.lock().unwrap();
            if a.should_quit {
                break;
            }
            // Live updates via contract B: apply job events to the queue state immediately.
            while let Ok(event) = a.job_event_rx.try_recv() {
                a.apply_event(event);
            }
            // Results of the crawler plugins for the link grabber.
            while let Ok(event) = a.grabber_event_rx.try_recv() {
                a.apply_grabber_event(event);
            }
            a.sample_speeds(Instant::now());
        }
        terminal.draw(|f| {
            let a = app.lock().unwrap();
            draw(f, &a);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let mut a = app.lock().unwrap();
                    a.handle_key(key.code);
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    let titles = vec![
        format!(" {} ", t!("ui.tab.downloads")),
        format!(" {} ", t!("ui.tab.history")),
        format!(" {} ", t!("ui.tab.linkgrabber")),
        format!(" {} ", t!("ui.tab.plugins")),
        format!(" {} ", t!("ui.tab.settings")),
    ];
    let tab_idx = match app.tab {
        AppTab::Downloads => 0,
        AppTab::History => 1,
        AppTab::LinkGrabber => 2,
        AppTab::Plugins => 3,
        AppTab::Settings => 4,
    };
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("rdownloader"))
        .select(tab_idx)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        AppTab::Downloads => draw_downloads(f, chunks[1], app),
        AppTab::History => draw_history(f, chunks[1], app),
        AppTab::LinkGrabber => draw_linkgrabber(f, chunks[1], app),
        AppTab::Plugins => draw_plugins(f, chunks[1]),
        AppTab::Settings => draw_settings(f, chunks[1]),
    }

    let status = if app.input_mode {
        match &app.input_prompt {
            InputPrompt::AddUrl => {
                Paragraph::new(t!("ui.status.input_url", buffer = app.input_buffer))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.status.input_title")),
                    )
                    .style(Style::default().fg(Color::Green))
            }
            InputPrompt::ImportFile => {
                Paragraph::new(t!("ui.status.input_import", buffer = app.input_buffer))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.import.title")),
                    )
                    .style(Style::default().fg(Color::Green))
            }
            InputPrompt::GrabUrl => {
                Paragraph::new(t!("ui.linkgrabber.input_url", buffer = app.input_buffer))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.linkgrabber.title")),
                    )
                    .style(Style::default().fg(Color::Green))
            }
            InputPrompt::GrabImport => {
                Paragraph::new(t!("ui.linkgrabber.input_import", buffer = app.input_buffer))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.linkgrabber.title")),
                    )
                    .style(Style::default().fg(Color::Green))
            }
            InputPrompt::CrawlPage => {
                Paragraph::new(t!("ui.linkgrabber.input_crawl", buffer = app.input_buffer))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.linkgrabber.title")),
                    )
                    .style(Style::default().fg(Color::Green))
            }
            InputPrompt::Captcha { challenge, .. } => {
                let image_default = t!("ui.status.captcha_image_default");
                let prompt_text = match &challenge.payload {
                    ChallengePayload::Text { prompt } => prompt.as_str(),
                    ChallengePayload::Image { instructions, .. } => {
                        instructions.as_deref().unwrap_or(image_default.as_str())
                    }
                    ChallengePayload::Token { service, .. } => service.as_str(),
                };
                Paragraph::new(t!(
                    "ui.status.captcha_prompt",
                    host = challenge.host,
                    prompt = prompt_text,
                    buffer = app.input_buffer
                ))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t!("ui.status.captcha_title")),
                )
                .style(Style::default().fg(Color::Yellow))
            }
        }
    } else {
        Paragraph::new(app.status_msg.clone())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.status.title")),
            )
            .wrap(Wrap { trim: true })
    };
    f.render_widget(status, chunks[2]);
}

fn draw_downloads(f: &mut Frame, area: Rect, app: &App) {
    // Layout: list on top, gauge or details below
    let bottom = if app.detail_open { 12 } else { 5 };
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(bottom)])
        .split(area);

    if app.jobs.is_empty() {
        let p = Paragraph::new(t!("ui.queue.empty"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.queue.title")),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner[0]);
        return;
    }

    let items: Vec<ListItem> = app
        .jobs
        .iter()
        .map(|j| {
            let status_color = match j.status {
                JobStatus::Done => Color::Green,
                JobStatus::Failed(_) => Color::Red,
                JobStatus::Downloading | JobStatus::Resolving | JobStatus::Merging => Color::Yellow,
                JobStatus::Paused => Color::Blue,
                JobStatus::Cancelled => Color::DarkGray,
                _ => Color::Gray,
            };
            let pct = (j.bytes_done * 100).checked_div(j.total_bytes).unwrap_or(0);
            let status_str = match &j.status {
                JobStatus::Failed(e) => t!("ui.queue.failed", e = e),
                JobStatus::Done => t!("ui.queue.done").to_string(),
                JobStatus::Paused => t!("ui.queue.paused").to_string(),
                JobStatus::Cancelled => t!("ui.queue.cancelled").to_string(),
                _ => String::new(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{pct:3}% "), Style::default().fg(status_color)),
                Span::raw(format!("{} ", j.filename)),
                Span::styled(
                    format!("[{}/{}]{}", j.bytes_done, j.total_bytes, status_str),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!("ui.queue.title")),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol(">> ");
    f.render_stateful_widget(list, inner[0], &mut app.list_state.clone());

    // Detail panel or gauge for the selected job
    if let Some(sel) = app.list_state.selected() {
        if let Some(j) = app.jobs.get(sel) {
            if app.detail_open {
                let text = detail::detail_text(j, app.speeds.speed(j.id), app.tor_health.as_ref());
                let p = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t!("ui.detail.title")),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(p, inner[1]);
                return;
            }
            let pct_f = if j.total_bytes > 0 {
                j.bytes_done as f64 / j.total_bytes as f64
            } else {
                0.0
            };
            let gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t!("ui.progress.title")),
                )
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                .percent((pct_f * 100.0) as u16)
                .label(t!(
                    "ui.progress.label",
                    done = fmt_bytes(j.bytes_done),
                    total = fmt_bytes(j.total_bytes),
                    parts = j.parts.len()
                ));
            f.render_widget(gauge, inner[1]);
        }
    }
}

/// History view: the finished downloads with their statistics and the export
/// keys of the history.
fn draw_history(f: &mut Frame, area: Rect, app: &App) {
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    if app.history.is_empty() {
        let p = Paragraph::new(t!("ui.history.empty"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.history.title")),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner[0]);
    } else {
        let items: Vec<ListItem> = app
            .history
            .iter()
            .map(|it| {
                let color = match it.status {
                    JobStatus::Done => Color::Green,
                    JobStatus::Failed(_) => Color::Red,
                    JobStatus::Cancelled => Color::DarkGray,
                    _ => Color::Gray,
                };
                let stats = t!(
                    "ui.history.stats",
                    size = fmt_bytes(it.total_bytes),
                    duration = detail::fmt_eta(it.duration_secs),
                    speed = detail::fmt_speed(it.avg_bytes_per_sec)
                );
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{} ", it.filename)),
                    Span::styled(
                        detail::job_status_label(&it.status),
                        Style::default().fg(color),
                    ),
                    Span::styled(format!("  {stats}"), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.history.title")),
            )
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol(">> ");
        f.render_stateful_widget(list, inner[0], &mut app.history_state.clone());
    }

    let hint = Paragraph::new(t!("ui.history.hint"))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!("ui.history.title")),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(hint, inner[1]);
}

fn draw_plugins(f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = vec![ListItem::new(Line::from(vec![
        Span::styled("[direct] ", Style::default().fg(Color::Cyan)),
        Span::raw(t!("ui.plugins.item")),
    ]))];
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t!("ui.plugins.title")),
    );
    f.render_widget(list, area);
}

fn draw_settings(f: &mut Frame, area: Rect) {
    let cfg = config::config();
    let text = t!(
        "ui.settings.body",
        tor_clients = cfg.tor_clients.to_string(),
        max_parts = cfg.max_parts.to_string(),
        tor_data_dir = cfg.tor_data_dir().display().to_string(),
        temp_dir = cfg.temp_dir().display().to_string(),
        download_dir = cfg.download_dir().display().to_string(),
        user_agent = cfg.user_agent.clone(),
        read_timeout = cfg.read_timeout_secs.to_string(),
        max_retries = cfg.max_retries.to_string(),
        log_level = cfg.log_level.clone(),
    );
    let p = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!("ui.settings.title")),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

/// Link grabber: the collected links, their packages and the filters.
fn draw_linkgrabber(f: &mut Frame, area: Rect, app: &App) {
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(area);

    if app.grabber.is_empty() {
        let p = Paragraph::new(t!("ui.linkgrabber.empty"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.linkgrabber.title")),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner[0]);
    } else {
        let items: Vec<ListItem> = app
            .grabber
            .rows()
            .iter()
            .map(|row| match row {
                GrabberRow::Package(i) => {
                    let package = &app.grabber.packages()[*i];
                    let marked = package
                        .links
                        .iter()
                        .filter(|&&l| app.grabber.link(l).is_some_and(|c| c.selected))
                        .count();
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("▸ {}", package_label(package)),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {}", t!("ui.linkgrabber.package_count", count = marked)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                }
                GrabberRow::Link(i) => {
                    let link = &app.grabber.links()[*i];
                    let filtered = app.grabber.filter().is_excluded(link);
                    let color = if filtered {
                        Color::DarkGray
                    } else {
                        Color::White
                    };
                    let mark = if link.selected { 'x' } else { ' ' };
                    let mark_color = if link.selected {
                        Color::Green
                    } else {
                        Color::DarkGray
                    };
                    let size = match link.size {
                        Some(bytes) => fmt_bytes(bytes),
                        None => t!("ui.linkgrabber.size_unknown").to_string(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("  [{mark}] "), Style::default().fg(mark_color)),
                        Span::styled(pad(&link.filename, 26), Style::default().fg(color)),
                        Span::styled(
                            pad(&link.extension, 6),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(pad(&link.host, 20), Style::default().fg(color)),
                        Span::styled(size, Style::default().fg(Color::DarkGray)),
                    ]))
                }
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!("ui.linkgrabber.title")),
            )
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol(">> ");
        f.render_stateful_widget(list, inner[0], &mut app.grabber_state.clone());
    }

    let counts = app.grabber.counts();
    let hint = Paragraph::new(vec![
        Line::from(t!("ui.linkgrabber.hint")),
        Line::from(t!(
            "ui.linkgrabber.counts",
            selected = counts.selected,
            total = counts.total,
            filtered = counts.filtered
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(t!("ui.linkgrabber.title")),
    )
    .wrap(Wrap { trim: true });
    f.render_widget(hint, inner[1]);
}

/// The header of a package in the link grabber list.
fn package_label(package: &Package) -> String {
    match &package.source {
        LinkSource::Manual => t!("ui.linkgrabber.package_manual"),
        LinkSource::Clipboard => t!("ui.linkgrabber.package_clipboard"),
        LinkSource::File(path) => {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            t!("ui.linkgrabber.package_file", name = name)
        }
        LinkSource::Crawl(page) => t!("ui.linkgrabber.package_crawl", page = page.clone()),
    }
}

/// The state of a filter (excluded or included again).
fn filter_state(excluded: bool) -> String {
    if excluded {
        t!("ui.linkgrabber.filter_excluded")
    } else {
        t!("ui.linkgrabber.filter_included")
    }
}

/// Shortens a text to `width` characters and pads it with spaces.
fn pad(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

pub(crate) fn fmt_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = b as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{FakeJob, JobCommand, JobEvent};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    #[test]
    fn apply_event_resolved_progress_finished() {
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, _evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(_cmd_tx, _evt_rx, _grab_rx);
        let id = Uuid::new_v4();

        app.apply_event(JobEvent::Status {
            id,
            status: JobStatus::Resolving,
        });
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(app.jobs[0].status, JobStatus::Resolving);

        app.apply_event(JobEvent::Resolved {
            id,
            filename: "a.bin".into(),
            total_bytes: Some(100),
        });
        let j = &app.jobs[0];
        assert_eq!(j.filename, "a.bin");
        assert_eq!(j.total_bytes, 100);
        assert_eq!(j.status, JobStatus::Downloading);

        app.apply_event(JobEvent::Progress {
            id,
            bytes_done: 40,
            parts: vec![],
        });
        assert_eq!(app.jobs[0].bytes_done, 40);

        app.apply_event(JobEvent::Finished { id, result: Ok(()) });
        assert_eq!(app.jobs[0].status, JobStatus::Done);
    }

    #[test]
    fn apply_event_unknown_progress_is_ignored() {
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, _evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(_cmd_tx, _evt_rx, _grab_rx);
        let id = Uuid::new_v4();
        app.apply_event(JobEvent::Progress {
            id,
            bytes_done: 5,
            parts: vec![],
        });
        assert!(app.jobs.is_empty());
    }

    #[tokio::test]
    async fn fakejob_live_updates_reach_queue() {
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let job = FakeJob::new("dl.bin", 60, 20);
        let id = job.id;
        job.spawn(evt_tx);
        let mut app = App::new(_cmd_tx, evt_rx, _grab_rx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while let Ok(ev) = app.job_event_rx.try_recv() {
            app.apply_event(ev);
        }
        let j = app
            .jobs
            .iter()
            .find(|j| j.id == id)
            .expect("job present in queue");
        assert_eq!(j.status, JobStatus::Done);
        assert_eq!(j.bytes_done, 60);
        assert_eq!(j.filename, "dl.bin");
    }

    #[tokio::test]
    async fn fakejob_cancel_reaches_queue() {
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let job = FakeJob::new("dl.bin", 1_000_000, 1);
        let id = job.id;
        let cmd_tx = job.spawn(evt_tx);
        let mut app = App::new(_cmd_tx, evt_rx, _grab_rx);
        cmd_tx.send(JobCommand::Cancel).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while let Ok(ev) = app.job_event_rx.try_recv() {
            app.apply_event(ev);
        }
        let j = app
            .jobs
            .iter()
            .find(|j| j.id == id)
            .expect("job present in queue");
        assert!(matches!(j.status, JobStatus::Cancelled));
    }

    #[test]
    fn handle_key_pause_resume_cancel_remove() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        app.jobs.push(JobProgress {
            id: id1,
            url: "http://example.com/file1".into(),
            filename: "file1.bin".into(),
            total_bytes: 100,
            bytes_done: 20,
            parts: vec![],
            status: JobStatus::Downloading,
        });
        app.jobs.push(JobProgress {
            id: id2,
            url: "http://example.com/file2".into(),
            filename: "file2.bin".into(),
            total_bytes: 200,
            bytes_done: 50,
            parts: vec![],
            status: JobStatus::Downloading,
        });
        app.list_state.select(Some(0));

        // Test Pause 'p'
        app.handle_key(KeyCode::Char('p'));
        assert_eq!(app.jobs[0].status, JobStatus::Paused);
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::PauseJob(id1));

        // Test Resume 'r'
        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.jobs[0].status, JobStatus::Downloading);
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::ResumeJob(id1));

        // Test Cancel 'c'
        app.handle_key(KeyCode::Char('c'));
        assert_eq!(app.jobs[0].status, JobStatus::Cancelled);
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::CancelJob(id1));

        // Test Remove 'd'
        app.handle_key(KeyCode::Char('d'));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::RemoveJob(id1));
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(app.jobs[0].id, id2);
    }

    #[tokio::test]
    async fn interactive_captcha_prompt_in_tui() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);

        let challenge = CaptchaChallenge::new_text("dl.example.com", "Calculate 5 * 5");
        let (reply_tx, reply_rx) = oneshot::channel();

        app.prompt_captcha(challenge, reply_tx);
        assert!(app.input_mode);

        // User enters '25' and presses Enter
        for c in "25".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);

        assert!(!app.input_mode);
        let answer = reply_rx.await.expect("received captcha answer");
        assert_eq!(answer, "25");
        assert!(app.status_msg.contains("dl.example.com"));
    }

    #[test]
    fn batch_import_sends_one_command_per_link() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);

        app.enqueue_import(Ok(import::parse_urls(
            "http://a.test/1\nhttp://a.test/2\nsftp://x/y\n",
        )));
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::AddUrl("http://a.test/1".into())
        );
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::AddUrl("http://a.test/2".into())
        );
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.status_msg.contains('2'));

        app.enqueue_import(Ok(import::parse_urls("nothing here")));
        assert!(cmd_rx.try_recv().is_err());

        app.enqueue_import(Err(anyhow::anyhow!("boom")));
        assert!(app.status_msg.contains("boom"));
    }

    #[test]
    fn import_key_opens_file_prompt() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        app.handle_key(KeyCode::Char('i'));
        assert!(app.input_mode);
        assert!(matches!(app.input_prompt, InputPrompt::ImportFile));
    }

    fn history_item(filename: &str) -> HistoryItem {
        let now = chrono::Utc::now();
        HistoryItem {
            id: Uuid::new_v4(),
            filename: filename.into(),
            url: "http://example.com/a.bin".into(),
            output: "/tmp/a.bin".into(),
            status: JobStatus::Done,
            total_bytes: 1000,
            bytes_done: 1000,
            parts: 2,
            started_at: now,
            finished_at: now,
            duration_secs: 5,
            avg_bytes_per_sec: 200,
        }
    }

    #[test]
    fn history_tab_exports_and_removes_entries() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        let dir = std::env::temp_dir().join(format!("rdl-ui-history-{}", Uuid::new_v4()));
        app.export_dir = dir.clone();
        let item = history_item("a.bin");
        let id = item.id;
        app.history = vec![item];
        app.history_state.select(Some(0));
        app.tab = AppTab::History;

        // Export as CSV and as JSON.
        app.handle_key(KeyCode::Char('e'));
        assert!(dir.join("history.csv").exists(), "CSV export missing");
        assert!(app.status_msg.contains("history.csv"), "{}", app.status_msg);
        app.handle_key(KeyCode::Char('E'));
        assert!(dir.join("history.json").exists(), "JSON export missing");

        // Remove the entry from the history.
        app.handle_key(KeyCode::Char('x'));
        assert!(app.history.is_empty());
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::RemoveHistory(id));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_keys_only_work_in_the_history_tab() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        let dir = std::env::temp_dir().join(format!("rdl-ui-history-{}", Uuid::new_v4()));
        app.export_dir = dir.clone();
        app.history = vec![history_item("a.bin")];
        app.tab = AppTab::Downloads;

        app.handle_key(KeyCode::Char('e'));
        assert!(!dir.join("history.csv").exists(), "export in the wrong tab");
        assert!(app.history.len() == 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tab_order_includes_the_history() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        assert_eq!(app.tab, AppTab::Downloads);
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.tab, AppTab::History);
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.tab, AppTab::LinkGrabber);
        app.handle_key(KeyCode::BackTab);
        assert_eq!(app.tab, AppTab::History);
    }

    /// An app in the link grabber tab.
    fn grabber_app() -> (App, mpsc::UnboundedReceiver<UiCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        app.tab = AppTab::LinkGrabber;
        (app, cmd_rx)
    }

    fn grab_url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn link_grabber_collects_a_url_and_transfers_it_to_the_queue() {
        let (mut app, mut cmd_rx) = grabber_app();

        // 'a' opens the prompt of the link grabber.
        app.handle_key(KeyCode::Char('a'));
        assert!(app.input_mode);
        assert!(matches!(app.input_prompt, InputPrompt::GrabUrl));
        for c in "http://a.test/1.bin".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);
        assert!(!app.input_mode);
        assert_eq!(app.grabber.links().len(), 1);
        assert_eq!(app.grabber.links()[0].filename, "1.bin");
        assert_eq!(app.grabber.links()[0].extension, "bin");

        // The link is collected, but nothing is downloaded yet.
        assert!(cmd_rx.try_recv().is_err());

        // Enter confirms and transfers the marked links.
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::AddUrls(vec!["http://a.test/1.bin".into()])
        );
        assert!(app.grabber.is_empty());
    }

    #[test]
    fn link_grabber_filters_by_host_and_file_type() {
        let (mut app, _cmd_rx) = grabber_app();
        app.grabber.add_links(
            LinkSource::Manual,
            [
                grab_url("http://a.test/1.bin"),
                grab_url("http://a.test/2.html"),
            ],
        );
        // Rows: package header, first link, second link.
        app.grabber_state.select(Some(2));

        // 'f' excludes the host of the link under the cursor.
        app.handle_key(KeyCode::Char('f'));
        assert!(app.grabber.filter().excludes_host("a.test"));
        assert_eq!(app.grabber.counts().filtered, 2);
        assert_eq!(app.grabber.counts().selected, 0);

        // 'f' again includes it.
        app.handle_key(KeyCode::Char('f'));
        assert!(!app.grabber.filter().excludes_host("a.test"));
        assert_eq!(app.grabber.counts().filtered, 0);

        // 'F' excludes the file type of the link under the cursor.
        app.handle_key(KeyCode::Char('F'));
        assert!(app.grabber.filter().excludes_extension("html"));
        assert_eq!(app.grabber.counts().filtered, 1);
        assert_eq!(app.grabber.counts().selected, 1);
    }

    #[test]
    fn link_grabber_marks_links_and_packages() {
        let (mut app, _cmd_rx) = grabber_app();
        app.grabber.add_links(
            LinkSource::Manual,
            [
                grab_url("http://a.test/1.bin"),
                grab_url("http://a.test/2.bin"),
            ],
        );

        // Space unmarks the link under the cursor.
        app.grabber_state.select(Some(1));
        app.handle_key(KeyCode::Char(' '));
        assert!(!app.grabber.links()[0].selected);
        assert_eq!(app.grabber.counts().selected, 1);

        // 'p' marks the whole package.
        app.grabber_state.select(Some(0));
        app.handle_key(KeyCode::Char('p'));
        assert!(app.grabber.links().iter().all(|l| l.selected));

        // 'A' unmarks everything.
        app.handle_key(KeyCode::Char('A'));
        assert!(app.grabber.links().iter().all(|l| !l.selected));
    }

    #[test]
    fn link_grabber_removes_the_link_under_the_cursor() {
        let (mut app, _cmd_rx) = grabber_app();
        app.grabber.add_links(
            LinkSource::Manual,
            [
                grab_url("http://a.test/1.bin"),
                grab_url("http://a.test/2.bin"),
            ],
        );
        app.grabber_state.select(Some(1));
        app.handle_key(KeyCode::Char('x'));
        assert_eq!(app.grabber.links().len(), 1);
        assert_eq!(app.grabber.links()[0].filename, "2.bin");
    }

    #[test]
    fn link_grabber_takes_the_links_of_a_crawled_page() {
        use crate::plugin::crawler::CrawledLink;
        let (mut app, _cmd_rx) = grabber_app();
        let page = grab_url("http://a.test/page.html");
        let links = vec![CrawledLink {
            url: grab_url("http://a.test/f1.bin"),
            title: Some("one".into()),
        }];
        app.apply_grabber_event(GrabberEvent::LinksFound {
            page: page.clone(),
            links,
        });
        assert_eq!(app.grabber.links().len(), 1);
        assert_eq!(
            app.grabber.packages()[0].source,
            LinkSource::Crawl(page.to_string())
        );
    }

    #[test]
    fn link_grabber_does_not_disturb_the_download_tab() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<JobEvent>();
        let (_grab_tx, _grab_rx) = mpsc::unbounded_channel::<GrabberEvent>();
        let mut app = App::new(cmd_tx, evt_rx, _grab_rx);
        app.tab = AppTab::Downloads;

        // 'a' in the download tab still queues the URL directly.
        app.handle_key(KeyCode::Char('a'));
        assert!(matches!(app.input_prompt, InputPrompt::AddUrl));
        for c in "http://a.test/1.bin".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::AddUrl("http://a.test/1.bin".into())
        );
        assert!(app.grabber.is_empty());
    }

    #[test]
    fn link_grabber_tab_renders_the_collected_links() {
        use ratatui::backend::TestBackend;

        let (mut app, _cmd_rx) = grabber_app();
        app.grabber.add_links(
            LinkSource::Manual,
            [
                grab_url("http://a.test/1.bin"),
                grab_url("http://a.test/2.html"),
            ],
        );
        app.grabber_state.select(Some(1));

        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("1.bin"), "link missing in the list: {text}");
        assert!(text.contains("2.html"), "link missing in the list: {text}");
        assert!(text.contains("a.test"), "host missing in the list: {text}");
    }

    #[test]
    fn jobs_can_be_moved_in_the_queue() {
        let mk = |n: u128| JobProgress {
            id: Uuid::from_u128(n),
            url: String::new(),
            filename: String::new(),
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        };
        let order = |jobs: &[JobProgress]| jobs.iter().map(|j| j.id.as_u128()).collect::<Vec<_>>();
        let mut jobs = vec![mk(1), mk(2), mk(3), mk(4)];
        move_job(&mut jobs, Uuid::from_u128(3), MoveTo::Top);
        assert_eq!(order(&jobs), [3, 1, 2, 4]);
        move_job(&mut jobs, Uuid::from_u128(3), MoveTo::Up);
        assert_eq!(order(&jobs), [3, 1, 2, 4], "already first");
        move_job(&mut jobs, Uuid::from_u128(1), MoveTo::Down);
        assert_eq!(order(&jobs), [3, 2, 1, 4]);
        move_job(&mut jobs, Uuid::from_u128(3), MoveTo::Bottom);
        assert_eq!(order(&jobs), [2, 1, 4, 3]);
        move_job(&mut jobs, Uuid::from_u128(4), MoveTo::Up);
        assert_eq!(order(&jobs), [2, 4, 1, 3]);
        move_job(&mut jobs, Uuid::from_u128(99), MoveTo::Top);
        assert_eq!(order(&jobs), [2, 4, 1, 3], "unknown job is ignored");
        move_job(&mut Vec::new(), Uuid::from_u128(1), MoveTo::Down);
    }
}
