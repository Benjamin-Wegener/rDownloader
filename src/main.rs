//! rdownloader – Clean-room Rust Download-Manager mit Tor (Arti), Multi-Part & TUI.
//!
//! Inspiriert von JDownloader (Konzept/UX), kein Code übernommen.

// Das Gerüst wächst schrittweise; ungenutzter Code ist bis dahin erwartet.
#![allow(dead_code)]

mod config;
mod contract;
mod download;
mod i18n;
pub mod net;
mod plugin;
mod tor;
mod tracker;
mod ui;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::contract::JobEvent;
use crate::download::{run_job, DownloadJob, JobProgress, JobStatus};
use crate::plugin::{DirectDownloadPlugin, ExampleRedirectHosterPlugin, PluginRegistry};
use crate::tor::TorManager;
use crate::tracker::{RestoreAction, TrackEntry, Tracker};
use crate::ui::{App, SharedApp, UiCommand};

#[derive(Parser, Debug)]
#[command(name = "rdownloader", version)]
struct Cli {
    /// Help texts come from the i18n catalog (see `cli_command`).
    url: Option<String>,

    #[arg(short = 't', long)]
    tor_clients: Option<usize>,

    #[arg(short = 'p', long)]
    parts: Option<usize>,

    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    #[arg(long)]
    headless: bool,

    #[arg(long)]
    no_tor: bool,
}

/// Builds the clap command with all help texts taken from the i18n catalog.
fn cli_command() -> clap::Command {
    Cli::command()
        .about(t!("cli.about"))
        .mut_arg("url", |a| a.help(t!("cli.arg.url")))
        .mut_arg("tor_clients", |a| a.help(t!("cli.arg.tor_clients")))
        .mut_arg("parts", |a| a.help(t!("cli.arg.parts")))
        .mut_arg("output", |a| a.help(t!("cli.arg.output")))
        .mut_arg("headless", |a| a.help(t!("cli.arg.headless")))
        .mut_arg("no_tor", |a| a.help(t!("cli.arg.no_tor")))
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Ok(lang) = std::env::var("RDOWNLOADER_LANG") {
        i18n::set_language(&lang);
    }
    let cli = Cli::from_arg_matches(&cli_command().get_matches())?;

    // Config laden
    let cfg = config::init()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level)),
        )
        .init();

    info!("rdownloader gestartet");
    info!("Config: {}", cfg.describe());

    // Tor-Manager bootstrap (oder deaktiviert)
    // One client is enough: every part gets its own isolation token.
    let configured_clients = cli.tor_clients.unwrap_or(cfg.tor_clients).clamp(1, 10);
    let tor = if cli.no_tor {
        warn!("Tor disabled (--no-tor) - traffic is unprotected!");
        None
    } else {
        if configured_clients > 1 {
            info!(
                "tor_clients={configured_clients} ignored: a single client isolates every part per stream"
            );
        }
        info!("Bootstrapping Tor client (Arti) ...");
        match TorManager::bootstrap(cfg.tor_data_dir()).await {
            Ok(tm) => {
                let health = tm.health();
                info!(
                    "Tor ready (bootstrap {}%, healthy={})",
                    health.bootstrap_percent(),
                    health.is_healthy()
                );
                if let Some(reason) = health.blockage() {
                    warn!("Tor reports blockage: {reason}");
                }
                Some(Arc::new(tm))
            }
            Err(e) => {
                error!("Tor-Bootstrap fehlgeschlagen: {e:#}");
                anyhow::bail!("Tor konnte nicht gestartet werden: {e:#}");
            }
        }
    };

    // Plugin-Registry
    let mut registry = PluginRegistry::new();
    registry.register(Arc::new(DirectDownloadPlugin));
    registry.register(Arc::new(ExampleRedirectHosterPlugin::new()));
    info!("{} Plugin(s) geladen", registry.len());

    // Tracker
    let tracker = Tracker::new()?;
    info!("Tracker: {}", tracker.path().display());

    // Download-Verzeichnis anlegen
    let dl_dir = cfg.download_dir();
    std::fs::create_dir_all(&dl_dir)?;

    // Job-Queue + UI-Command-Channel
    let (job_tx, job_rx) = mpsc::unbounded_channel::<(DownloadJob, oneshot::Sender<Result<()>>)>();
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (_job_event_tx, job_event_rx) = mpsc::unbounded_channel::<JobEvent>();

    // Gemeinsamer Job-Progress-Store für TUI + Engine
    let jobs_progress: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));

    // Wenn CLI-URL angegeben → direkter Download (headless oder mit TUI-Hintergrund)
    if let Some(url_str) = &cli.url {
        let url = url::Url::parse(url_str).context("Ungültige URL")?;
        let plugin = registry
            .find_for_url(&url)
            .context("Kein Plugin für diese URL")?;
        let resolved = plugin.resolve(url).await?;
        let filename = resolved
            .filename
            .clone()
            .unwrap_or_else(|| "download.bin".into());
        let output = match &cli.output {
            Some(p) if p.is_dir() || !p.exists() && p.extension().is_none() => p.join(&filename),
            Some(p) => p.clone(),
            None => dl_dir.join(&filename),
        };
        let parts = cli.parts.unwrap_or(cfg.max_parts).clamp(1, 10);
        let job = DownloadJob::new(resolved.url, output.clone(), parts);

        let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
            id: job.id,
            url: job.url.to_string(),
            filename: filename.clone(),
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        }));

        if cli.headless || cli.tor_clients.is_some() {
            // Headless: direkt ausführen
            if let Some(tor) = tor {
                let cancel = Arc::new(tokio::sync::Notify::new());
                let res = run_job(&job, tor, progress.clone(), cancel).await;
                let p = progress.lock().await;
                tracker.update_from_progress(&p, output.to_str().unwrap_or(""), parts)?;
                res
            } else {
                anyhow::bail!("Headless ohne Tor nicht unterstützt (noch)")
            }
        } else {
            // Mit TUI: Job im Hintergrund, TUI zeigt Fortschritt
            start_job_background(
                job,
                tor.clone(),
                progress.clone(),
                tracker.clone(),
                job_tx.clone(),
                jobs_progress.clone(),
            );
            run_tui(
                ui_cmd_tx,
                ui_cmd_rx,
                job_rx,
                job_event_rx,
                jobs_progress,
                registry,
                tracker,
                tor,
                cfg,
            )
            .await
        }
    } else {
        // Nur TUI
        run_tui(
            ui_cmd_tx,
            ui_cmd_rx,
            job_rx,
            job_event_rx,
            jobs_progress,
            registry,
            tracker,
            tor,
            cfg,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tui(
    ui_cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut ui_cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    _job_rx: mpsc::UnboundedReceiver<(DownloadJob, oneshot::Sender<Result<()>>)>,
    job_event_rx: mpsc::UnboundedReceiver<JobEvent>,
    jobs_progress: Arc<Mutex<Vec<JobProgress>>>,
    registry: PluginRegistry,
    tracker: Tracker,
    tor: Option<Arc<TorManager>>,
    cfg: &config::Config,
) -> Result<()> {
    let app: SharedApp = Arc::new(Mutex::new(App::new(ui_cmd_tx, job_event_rx)));
    let app_for_ui = app.clone();
    let app_for_tui = app.clone();

    // Continue the downloads of the previous session.
    let restored = restore_session(&tracker, tor.clone(), jobs_progress.clone());
    if restored > 0 {
        let mut a = app.lock().unwrap();
        a.status_msg = t!("ui.status.session_restored", count = restored);
    }

    // UI-Command-Handler: neue Downloads enqueue
    let tor_c = tor.clone();
    let registry_c = registry.clone();
    let tracker_c = tracker.clone();
    let jobs_progress_c = jobs_progress.clone();
    let dl_dir = cfg.download_dir();
    let max_parts = cfg.max_parts;
    tokio::spawn(async move {
        while let Some(cmd) = ui_cmd_rx.recv().await {
            match cmd {
                UiCommand::AddUrl(url_str) => {
                    match url::Url::parse(&url_str) {
                        Ok(url) => {
                            let plugin = match registry_c.find_for_url(&url) {
                                Some(p) => p,
                                None => {
                                    let mut a = app_for_ui.lock().unwrap();
                                    a.status_msg = format!("Kein Plugin für {url}");
                                    continue;
                                }
                            };
                            match plugin.resolve(url).await {
                                Ok(resolved) => {
                                    let filename = resolved
                                        .filename
                                        .clone()
                                        .unwrap_or_else(|| "download.bin".into());
                                    let output = dl_dir.join(&filename);
                                    let job =
                                        DownloadJob::new(resolved.url, output.clone(), max_parts);
                                    let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
                                        id: job.id,
                                        url: job.url.to_string(),
                                        filename: filename.clone(),
                                        total_bytes: 0,
                                        bytes_done: 0,
                                        parts: vec![],
                                        status: JobStatus::Queued,
                                    }));
                                    start_job_background(
                                        job,
                                        tor_c.clone(),
                                        progress.clone(),
                                        tracker_c.clone(),
                                        // Direkt in jobs_progress spiegeln
                                        mpsc::unbounded_channel().0,
                                        jobs_progress_c.clone(),
                                    );
                                    let mut a = app_for_ui.lock().unwrap();
                                    a.status_msg = format!("Download gestartet: {filename}");
                                }
                                Err(e) => {
                                    let mut a = app_for_ui.lock().unwrap();
                                    a.status_msg = format!("Resolve-Fehler: {e}");
                                }
                            }
                        }
                        Err(_) => {
                            let mut a = app_for_ui.lock().unwrap();
                            a.status_msg = "Ungültige URL".into();
                        }
                    }
                }
                UiCommand::CancelJob(id) => {
                    let mut jp = jobs_progress_c.lock().unwrap();
                    if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                        j.status = JobStatus::Cancelled;
                    }
                    drop(jp);
                    // A cancelled download must not be continued in a later session.
                    let _ = tracker_c.set_status(id, JobStatus::Cancelled);
                }
                UiCommand::PauseJob(id) => {
                    let mut jp = jobs_progress_c.lock().unwrap();
                    if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                        j.status = JobStatus::Paused;
                    }
                }
                UiCommand::ResumeJob(id) => {
                    let mut jp = jobs_progress_c.lock().unwrap();
                    if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                        if matches!(j.status, JobStatus::Paused) {
                            j.status = JobStatus::Downloading;
                        }
                    }
                }
                UiCommand::RemoveJob(id) => {
                    let mut jp = jobs_progress_c.lock().unwrap();
                    jp.retain(|j| j.id != id);
                    drop(jp);
                    // Removed downloads must not come back on the next start.
                    let _ = tracker_c.remove(id);
                }
            }
        }
    });

    // Fortschritt-Poller: spiegelt Job-Progress in App.jobs (für TUI-Draw)
    {
        let app = app.clone();
        let jobs_progress = jobs_progress.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let snapshot = jobs_progress.lock().unwrap().clone();
                let mut a = app.lock().unwrap();
                a.jobs = snapshot;
            }
        });
    }

    // TUI auf dem Haupt-Thread (blocking)
    // crossterm braucht den Main-Thread auf macOS
    tokio::task::spawn_blocking(move || {
        let _ = ui::run(app_for_tui);
    })
    .await?;

    Ok(())
}

fn start_job_background(
    job: DownloadJob,
    tor: Option<Arc<TorManager>>,
    progress: Arc<tokio::sync::Mutex<JobProgress>>,
    tracker: Tracker,
    _job_tx: mpsc::UnboundedSender<(DownloadJob, oneshot::Sender<Result<()>>)>,
    jobs_progress: Arc<Mutex<Vec<JobProgress>>>,
) {
    let output = job.output.clone();
    let parts = job.max_parts;
    // Register the job with the tracker before the transfer starts: a session
    // that is killed during the download must still be restorable.
    if let Err(e) = tracker.register(TrackEntry::from_job(
        &job,
        &output.to_string_lossy(),
        parts,
    )) {
        warn!("Job {} could not be tracked: {e:#}", job.id);
    }
    tokio::spawn(async move {
        // In jobs_progress eintragen
        {
            let p = progress.lock().await;
            let mut jp = jobs_progress.lock().unwrap();
            jp.retain(|x| x.id != p.id);
            jp.push(p.clone());
        }

        // Mirror the engine's progress into the queue and keep the tracker
        // current, so a killed session can be continued later on.
        let forwarder = {
            let progress = progress.clone();
            let tracker = tracker.clone();
            let jobs_progress = jobs_progress.clone();
            let output = output.to_string_lossy().to_string();
            tokio::spawn(async move {
                let mut tick: u32 = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let p = progress.lock().await.clone();
                    {
                        let mut jp = jobs_progress.lock().unwrap();
                        if let Some(existing) = jp.iter_mut().find(|x| x.id == p.id) {
                            *existing = p.clone();
                        } else {
                            jp.push(p.clone());
                        }
                    }
                    tick = tick.wrapping_add(1);
                    // The tracker is written less often than the queue is drawn.
                    if tick.is_multiple_of(4) {
                        if let Err(e) = tracker.update_from_progress(&p, &output, parts) {
                            warn!("Tracker update failed: {e:#}");
                        }
                    }
                    if matches!(
                        p.status,
                        JobStatus::Done | JobStatus::Cancelled | JobStatus::Failed(_)
                    ) {
                        break;
                    }
                }
            })
        };

        let cancel = Arc::new(tokio::sync::Notify::new());
        let res = if let Some(tor) = tor {
            run_job(&job, tor, progress.clone(), cancel).await
        } else {
            Err(anyhow::anyhow!("Tor nicht verfügbar"))
        };

        forwarder.abort();

        // Finale Progress + Tracker-Update
        {
            let p = progress.lock().await;
            let _ = tracker.update_from_progress(&p, output.to_str().unwrap_or(""), parts);
            let mut jp = jobs_progress.lock().unwrap();
            if let Some(existing) = jp.iter_mut().find(|x| x.id == p.id) {
                *existing = p.clone();
            } else {
                jp.push(p.clone());
            }
        }

        match res {
            Ok(()) => info!("Job fertig: {}", job.output.display()),
            Err(e) => error!("Job fehlgeschlagen: {e:#}"),
        }
    });
}

/// Continues the downloads that were open when the app last stopped.
///
/// The tracker keeps the job id, so the engine finds the resume data of the
/// previous session in the temp directory and continues where it left off.
/// Returns the number of continued downloads.
fn restore_session(
    tracker: &Tracker,
    tor: Option<Arc<TorManager>>,
    jobs_progress: Arc<Mutex<Vec<JobProgress>>>,
) -> usize {
    let entries = tracker.open_entries();
    let mut restored = 0usize;

    for entry in entries {
        match entry.restore_action(tor.is_some()) {
            RestoreAction::Finished => {
                // Merged right before the shutdown: nothing left to continue.
                info!("Download already complete: {}", entry.filename);
                let _ = tracker.set_status(entry.id, JobStatus::Done);
            }
            RestoreAction::KeepPaused => {
                // Paused jobs stay paused, they are only shown in the queue.
                show_in_queue(&entry, &jobs_progress, JobStatus::Paused);
            }
            RestoreAction::Continue => {
                let job = match entry.to_download_job() {
                    Ok(j) => j,
                    Err(e) => {
                        warn!("Download {} cannot be restored: {e:#}", entry.filename);
                        continue;
                    }
                };
                info!("Restoring download: {}", entry.filename);
                let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
                    id: job.id,
                    url: job.url.to_string(),
                    filename: entry.filename.clone(),
                    total_bytes: entry.total_bytes,
                    bytes_done: entry.bytes_done,
                    parts: vec![],
                    status: JobStatus::Queued,
                }));
                start_job_background(
                    job,
                    tor.clone(),
                    progress,
                    tracker.clone(),
                    mpsc::unbounded_channel().0,
                    jobs_progress.clone(),
                );
                restored += 1;
            }
        }
    }

    restored
}

/// Puts a tracked download into the queue without starting it.
fn show_in_queue(
    entry: &TrackEntry,
    jobs_progress: &Arc<Mutex<Vec<JobProgress>>>,
    status: JobStatus,
) {
    let p = JobProgress {
        id: entry.id,
        url: entry.url.clone(),
        filename: entry.filename.clone(),
        total_bytes: entry.total_bytes,
        bytes_done: entry.bytes_done,
        parts: vec![],
        status,
    };
    let mut jp = jobs_progress.lock().unwrap();
    jp.retain(|x| x.id != p.id);
    jp.push(p);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn tracked_entry(dir: &std::path::Path, status: JobStatus) -> TrackEntry {
        TrackEntry {
            id: Uuid::new_v4(),
            url: "http://example.com/a.bin".into(),
            filename: "a.bin".into(),
            output: dir.join("a.bin").to_string_lossy().to_string(),
            total_bytes: 100,
            bytes_done: 10,
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tor_parts: 4,
            etag: None,
            last_modified: None,
        }
    }

    #[test]
    fn restore_session_shows_open_downloads_as_paused_without_tor() {
        let dir = std::env::temp_dir().join(format!("rdl-restore-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = Tracker::at(dir.join("downloads.json")).unwrap();
        let entry = tracked_entry(&dir, JobStatus::Downloading);
        let id = entry.id;
        tracker.add(entry).unwrap();

        let jobs: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));
        // Without a transport nothing is continued, but the download shows up.
        let restored = restore_session(&tracker, None, jobs.clone());
        assert_eq!(restored, 0);

        let list = jobs.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].filename, "a.bin");
        assert!(matches!(list[0].status, JobStatus::Paused));
        drop(list);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_session_skips_finished_downloads() {
        let dir = std::env::temp_dir().join(format!("rdl-restore-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // The output file is already complete.
        std::fs::write(dir.join("a.bin"), b"0123456789").unwrap();

        let tracker = Tracker::at(dir.join("downloads.json")).unwrap();
        let mut entry = tracked_entry(&dir, JobStatus::Downloading);
        entry.total_bytes = 10;
        let id = entry.id;
        tracker.add(entry).unwrap();

        let jobs: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let restored = restore_session(&tracker, None, jobs.clone());
        assert_eq!(restored, 0);
        assert!(jobs.lock().unwrap().is_empty());

        // The entry is closed now, so it is not restored again.
        let list = tracker.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(matches!(list[0].status, JobStatus::Done));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removed_downloads_do_not_come_back() {
        let dir = std::env::temp_dir().join(format!("rdl-restore-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = Tracker::at(dir.join("downloads.json")).unwrap();
        let entry = tracked_entry(&dir, JobStatus::Downloading);
        let id = entry.id;
        tracker.add(entry).unwrap();
        tracker.remove(id).unwrap();

        let jobs: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let restored = restore_session(&tracker, None, jobs.clone());
        assert_eq!(restored, 0);
        assert!(jobs.lock().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }
}
