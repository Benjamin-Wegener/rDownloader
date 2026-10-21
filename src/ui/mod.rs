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
use crate::download::{JobProgress, JobStatus};

#[derive(Clone, Copy, PartialEq)]
pub enum AppTab {
    Downloads,
    LinkGrabber,
    Plugins,
    Settings,
}

/// Befehl, den die UI an die Download-Engine senden kann.
#[derive(Debug, Clone)]
pub enum UiCommand {
    AddUrl(String),
    CancelJob(uuid::Uuid),
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
}

impl App {
    pub fn new(cmd_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>) -> Self {
        Self {
            tab: AppTab::Downloads,
            jobs: Vec::new(),
            should_quit: false,
            list_state: ListState::default(),
            input_mode: false,
            input_buffer: String::new(),
            status_msg: "rdownloader | q=Quit · a=Add URL · Tab=Wechseln · ↑↓=Auswahl".into(),
            cmd_tx,
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
                        self.status_msg = format!("URL in Warteschlange: {url}");
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
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some(i) = self.list_state.selected() {
                    if let Some(j) = self.jobs.get(i) {
                        let id = j.id;
                        let _ = self.cmd_tx.send(UiCommand::CancelJob(id));
                        self.status_msg = format!("Abbruch angefordert: {}", j.filename);
                    }
                }
            }
            _ => {}
        }
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
            let a = app.lock().unwrap();
            if a.should_quit {
                break;
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
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        AppTab::Downloads => draw_downloads(f, chunks[1], app),
        AppTab::LinkGrabber => draw_linkgrabber(f, chunks[1]),
        AppTab::Plugins => draw_plugins(f, chunks[1]),
        AppTab::Settings => draw_settings(f, chunks[1]),
    }

    let status = if app.input_mode {
        Paragraph::new(format!("URL einfuegen: {}_", app.input_buffer))
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
        let p = Paragraph::new("Keine Downloads – druecke 'a' um eine URL hinzuzufuegen.")
            .block(Block::default().borders(Borders::ALL).title("Download-Queue"))
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
                _ => Color::Gray,
            };
            let pct = (j.bytes_done * 100).checked_div(j.total_bytes).unwrap_or(0);
            let status_str = match &j.status {
                JobStatus::Failed(e) => format!(" FAILED({e})"),
                JobStatus::Done => " DONE".to_string(),
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
        .block(Block::default().borders(Borders::ALL).title("Download-Queue"))
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
                .block(Block::default().borders(Borders::ALL).title("Fortschritt"))
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                .percent((pct_f * 100.0) as u16)
                .label(format!(
                    "{} / {} ({} Tor-Parts)",
                    fmt_bytes(j.bytes_done),
                    fmt_bytes(j.total_bytes),
                    j.parts.len()
                ));
            f.render_widget(gauge, inner[1]);
        }
    }
}

fn draw_linkgrabber(f: &mut Frame, area: Rect) {
    let p = Paragraph::new(
        "LinkGrabber (Roadmap-Phase):\n\n- Clipboard-Integration (Auto-Einfang)\n- Batch-Import (Textdatei, M3U, etc.)\n- Crawler-Plugins (finden mehr Links auf einer Seite)\n- Vorschau vor Enqueue\n\nNoch nicht implementiert – siehe ROADMAP.md",
    )
    .block(Block::default().borders(Borders::ALL).title("LinkGrabber"))
    .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn draw_plugins(f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = vec![ListItem::new(Line::from(vec![
        Span::styled("[direct] ", Style::default().fg(Color::Cyan)),
        Span::raw("Direct Download v0.1.0 – Fallback, nimmt jede URL"),
    ]))];
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Plugins (clean-room Rust, NICHT JD .jar)"),
    );
    f.render_widget(list, area);
}

fn draw_settings(f: &mut Frame, area: Rect) {
    let cfg = config::config();
    let text = format!(
        "Einstellungen (~/.config/rdownloader/config.toml):\n\n  Tor-Clients:         {}\n  Max. Parts/Download: {}\n  Tor-Data-Dir:        {}\n  Temp-Dir:            {}\n  Download-Dir:        {}\n  User-Agent:          {}\n  Connect-Timeout:     {}s\n  Read-Timeout:        {}s\n  Max Retries:         {}\n  Log-Level:           {}\n\nTOML-Datei direkt bearbeiten – wirkt nach Neustart.",
        cfg.tor_clients,
        cfg.max_parts,
        cfg.tor_data_dir().display(),
        cfg.temp_dir().display(),
        cfg.download_dir().display(),
        cfg.user_agent,
        cfg.connect_timeout_secs,
        cfg.read_timeout_secs,
        cfg.max_retries,
        cfg.log_level,
    );
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Settings"))
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
