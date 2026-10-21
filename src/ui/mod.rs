//! TUI-Oberfläche (ratatui/crossterm) – JDownloader-ähnlich: Linkliste, Fortschritt, Statusleiste.
//!
//! Nutzt `std::sync::Mutex` für App-State (synchronous draw-Loop), Kommunikation
//! zu Download-Tasks läuft über `tokio::sync::mpsc`.

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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::contract::{JobEvent, JobId};
use crate::download::{JobProgress, JobStatus};
use crate::t;

#[derive(Clone, Copy, PartialEq)]
pub enum AppTab {
    Downloads,
    LinkGrabber,
    Plugins,
    Settings,
}

/// Befehl, den die UI an die Download-Engine senden kann.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    AddUrl(String),
    PauseJob(uuid::Uuid),
    ResumeJob(uuid::Uuid),
    CancelJob(uuid::Uuid),
    RemoveJob(uuid::Uuid),
}

pub struct App {
    pub tab: AppTab,
    pub jobs: Vec<JobProgress>,
    pub should_quit: bool,
    pub list_state: ListState,
    pub input_mode: bool,
    pub input_buffer: String,
    pub status_msg: String,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    pub job_event_rx: tokio::sync::mpsc::UnboundedReceiver<JobEvent>,
}

impl App {
    pub fn new(
        cmd_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
        job_event_rx: tokio::sync::mpsc::UnboundedReceiver<JobEvent>,
    ) -> Self {
        Self {
            tab: AppTab::Downloads,
            jobs: Vec::new(),
            should_quit: false,
            list_state: ListState::default(),
            input_mode: false,
            input_buffer: String::new(),
            status_msg: t!("ui.status.help"),
            cmd_tx,
            job_event_rx,
        }
    }

    fn next_tab(&mut self) {
        self.tab = match self.tab {
            AppTab::Downloads => AppTab::LinkGrabber,
            AppTab::LinkGrabber => AppTab::Plugins,
            AppTab::Plugins => AppTab::Settings,
            AppTab::Settings => AppTab::Downloads,
        };
    }

    fn prev_tab(&mut self) {
        self.tab = match self.tab {
            AppTab::Downloads => AppTab::Settings,
            AppTab::LinkGrabber => AppTab::Downloads,
            AppTab::Plugins => AppTab::LinkGrabber,
            AppTab::Settings => AppTab::Plugins,
        };
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.input_mode {
            match code {
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_buffer.clear();
                }
                KeyCode::Enter => {
                    let url = self.input_buffer.trim().to_string();
                    self.input_mode = false;
                    self.input_buffer.clear();
                    if !url.is_empty() {
                        let _ = self.cmd_tx.send(UiCommand::AddUrl(url.clone()));
                        self.status_msg = t!("ui.status.queued_url", url = url);
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
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('a') => {
                self.input_mode = true;
                self.input_buffer.clear();
            }
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Up => {
                let i = self.list_state.selected().unwrap_or(0);
                if i > 0 {
                    self.list_state.select(Some(i - 1));
                }
            }
            KeyCode::Down => {
                let i = self.list_state.selected().unwrap_or(0);
                if i + 1 < self.jobs.len() {
                    self.list_state.select(Some(i + 1));
                }
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

    /// Wendet ein Vertrag-B-`JobEvent` auf den App-State an (live-Update der Queue).
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
            JobEvent::Speed { .. } => {}
            JobEvent::Finished { id, result } => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    if !matches!(
                        j.status,
                        JobStatus::Cancelled | JobStatus::Done | JobStatus::Failed(_)
                    ) {
                        j.status = match result {
                            Ok(()) => JobStatus::Done,
                            Err(_) => JobStatus::Failed("finished with error".into()),
                        };
                    }
                }
            }
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

/// Startet die TUI-Loop (blocking, auf dem Haupt-Thread).
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
            // Live-Updates über Vertrag B: Job-Events sofort in den Queue-State einpflegen.
            while let Ok(event) = a.job_event_rx.try_recv() {
                a.apply_event(event);
            }
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

    let titles = [" Downloads ", " LinkGrabber ", " Plugins ", " Settings "];
    let tab_idx = match app.tab {
        AppTab::Downloads => 0,
        AppTab::LinkGrabber => 1,
        AppTab::Plugins => 2,
        AppTab::Settings => 3,
    };
    let tabs = Tabs::new(titles.to_vec())
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
        AppTab::LinkGrabber => draw_linkgrabber(f, chunks[1]),
        AppTab::Plugins => draw_plugins(f, chunks[1]),
        AppTab::Settings => draw_settings(f, chunks[1]),
    }

    let status = if app.input_mode {
        Paragraph::new(t!("ui.status.input_url", buffer = app.input_buffer))
            .block(Block::default().borders(Borders::ALL).title("Add URL"))
            .style(Style::default().fg(Color::Green))
    } else {
        Paragraph::new(app.status_msg.clone())
            .block(Block::default().borders(Borders::ALL).title("Status"))
            .wrap(Wrap { trim: true })
    };
    f.render_widget(status, chunks[2]);
}

fn draw_downloads(f: &mut Frame, area: Rect, app: &App) {
    // Aufteilung: Liste oben, Gauge unten
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(5)])
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

    // Gauge für ausgewählten Job
    if let Some(sel) = app.list_state.selected() {
        if let Some(j) = app.jobs.get(sel) {
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

fn draw_linkgrabber(f: &mut Frame, area: Rect) {
    let p = Paragraph::new(t!("ui.linkgrabber.body"))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!("ui.linkgrabber.title")),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
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
        connect_timeout = cfg.connect_timeout_secs.to_string(),
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

fn fmt_bytes(b: u64) -> String {
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
        let mut app = App::new(_cmd_tx, _evt_rx);
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
        let mut app = App::new(_cmd_tx, _evt_rx);
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
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let job = FakeJob::new("dl.bin", 60, 20);
        let id = job.id;
        job.spawn(evt_tx);
        let mut app = App::new(_cmd_tx, evt_rx);
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
        let (_cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let job = FakeJob::new("dl.bin", 1_000_000, 1);
        let id = job.id;
        let cmd_tx = job.spawn(evt_tx);
        let mut app = App::new(_cmd_tx, evt_rx);
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
        let mut app = App::new(cmd_tx, evt_rx);
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
}
