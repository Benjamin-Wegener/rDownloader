//! rdownloader – clean-room Rust download manager with Tor (Arti), multi-part & TUI.
//!
//! Inspired by JDownloader (concept/UX), no code taken.

// The scaffold grows step by step; unused code is expected until then.
#![allow(dead_code)]

mod config;
mod contract;
mod download;
mod gui;
mod i18n;
pub mod net;
mod plugin;
mod tor;
mod tracker;
mod ui;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{CommandFactory, FromArgMatches, Parser};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::contract::JobEvent;
use crate::download::{run_job, DownloadJob, JobProgress, JobStatus};
use crate::net::rpc::{self, JobControl};
use crate::net::TorTransport;
use crate::plugin::{
    load_wasm_plugins, DenyHttpClient, DirectDownloadPlugin, ExampleRedirectHosterPlugin,
    HtmlLinkCrawlerPlugin, PluginRegistry, TransportHttpClient, WasmHttpClient, WasmRuntimeConfig,
};
use crate::tor::TorManager;
use crate::tracker::{RestoreAction, TrackEntry, Tracker};
use crate::ui::{App, GrabberEvent, SharedApp, UiCommand};

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
    daemon: bool,

    /// Open the desktop window (the default when a display is available).
    #[arg(long, conflicts_with = "tui")]
    gui: bool,

    /// Use the terminal interface instead of the window.
    #[arg(long)]
    tui: bool,

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
        .mut_arg("daemon", |a| a.help(t!("cli.arg.daemon")))
        .mut_arg("gui", |a| a.help(t!("cli.arg.gui")))
        .mut_arg("tui", |a| a.help(t!("cli.arg.tui")))
        .mut_arg("no_tor", |a| a.help(t!("cli.arg.no_tor")))
}

/// Which interface shows the queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Frontend {
    Tui,
    Gui,
}

/// The window is the default; the terminal UI is used on request or when there is
/// no display (for example over SSH).
fn choose_frontend(gui: bool, tui: bool, display_available: bool) -> Frontend {
    if tui {
        Frontend::Tui
    } else if gui || display_available {
        Frontend::Gui
    } else {
        Frontend::Tui
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // The language must be known before clap builds its help text. The config is
    // loaded in full below; here only the language key is read (without creating
    // the file), so `--help` works even with a missing or broken config.
    let requested_language = std::env::var("RDOWNLOADER_LANG")
        .ok()
        .or_else(config::language_hint);
    if let Some(lang) = &requested_language {
        i18n::set_language(lang);
    }

    let cli = Cli::from_arg_matches(&cli_command().get_matches())?;
    let frontend = choose_frontend(cli.gui, cli.tui, gui::display_available());

    let cfg = config::init()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level)),
        )
        .init();

    if let Some(lang) = requested_language {
        if !i18n::set_language(&lang) {
            warn!(
                "Unknown language {lang:?}; available: {}",
                i18n::languages().join(", ")
            );
        }
    }

    download::timetable::spawn(cfg.schedule.clone());
    info!("rDownloader started");
    info!("Config: {}", cfg.describe());

    // Tor manager bootstrap (or disabled)
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
        match TorManager::bootstrap(cfg.tor_data_dir(), &cfg.bridges).await {
            Ok(tm) => {
                let health = tm.health();
                info!(
                    "Tor client bootstrapped (bootstrap {}%, healthy={})",
                    health.bootstrap_percent(),
                    health.is_healthy()
                );
                if let Some(reason) = health.blockage() {
                    warn!("Tor reports blockage: {reason}");
                }
                Some(Arc::new(tm))
            }
            Err(e) => {
                error!("Tor bootstrap failed: {e:#}");
                anyhow::bail!(t!("main.error.tor_start", e = format!("{e:#}")));
            }
        }
    };

    // Plugin registry
    let mut registry = PluginRegistry::new();
    registry.register(Arc::new(DirectDownloadPlugin));
    registry.register(Arc::new(ExampleRedirectHosterPlugin::new()));
    // The link grabber crawls pages with the crawler plugins.
    registry.register_crawler(Arc::new(HtmlLinkCrawlerPlugin));

    // Dynamically loaded WebAssembly plugins (sandboxed, permissions from sidecar).
    let wasm_http: Arc<dyn WasmHttpClient> = match &tor {
        Some(tm) => Arc::new(TransportHttpClient::new(
            Arc::new(TorTransport::new(tm.clone())),
            tokio::runtime::Handle::current(),
            0x5057_4153_4d00_0001,
        )),
        None => Arc::new(DenyHttpClient),
    };
    let plugin_dir = cfg.plugin_dir();
    for result in load_wasm_plugins(&plugin_dir, &WasmRuntimeConfig::default(), wasm_http) {
        match result {
            Ok(plugin) => {
                let meta = plugin.meta().clone();
                info!(
                    "WebAssembly plugin loaded: {} ({}) from {}",
                    meta.name,
                    meta.id,
                    plugin_dir.display()
                );
                registry.register(Arc::new(plugin));
            }
            Err(error) => warn!("WebAssembly plugin rejected: {error:#}"),
        }
    }
    info!("{} plugin(s) loaded", registry.len());

    // Tracker
    let tracker = Tracker::new()?;
    info!("Tracker: {}", tracker.path().display());

    // Create the download directory
    let dl_dir = cfg.download_dir();
    std::fs::create_dir_all(&dl_dir)?;

    // Job queue + UI command channel
    let (job_tx, job_rx) = mpsc::unbounded_channel::<(DownloadJob, oneshot::Sender<Result<()>>)>();
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (_job_event_tx, job_event_rx) = mpsc::unbounded_channel::<JobEvent>();
    // Results of the crawler plugins for the link grabber.
    let (grabber_event_tx, grabber_event_rx) = mpsc::unbounded_channel::<GrabberEvent>();

    // Shared job progress store for TUI + engine
    let jobs_progress: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));

    // Headless control API: no TUI, jobs are added and controlled over RPC.
    if cli.daemon {
        let control = DaemonControl {
            registry,
            tor,
            tracker,
            jobs: jobs_progress,
            dl_dir,
            max_parts: cfg.max_parts,
            handles: Mutex::new(HashMap::new()),
        };
        return run_daemon(control, cfg).await;
    }

    // If a CLI URL is given → direct download (headless or with the TUI in the background)
    if let Some(url_str) = &cli.url {
        let url = url::Url::parse(url_str).context(t!("ui.status.invalid_url"))?;
        let plugin = registry
            .find_for_url(&url)
            .context(t!("ui.status.no_plugin", url = url_str))?;
        let resolved = plugin.resolve(url).await?;
        let proposed_name = resolved
            .filename
            .as_deref()
            .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME);
        let output = match &cli.output {
            Some(p) if p.is_dir() || !p.exists() && p.extension().is_none() => {
                rdownloader::download::resolve_destination(p, proposed_name)
            }
            Some(p) => p.clone(),
            None => rdownloader::download::resolve_destination(&dl_dir, proposed_name),
        };
        let filename = output
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME)
            .to_string();
        let parts = cli.parts.unwrap_or(cfg.max_parts).clamp(1, 10);
        let mut job = DownloadJob::new(resolved.url, output.clone(), parts);
        job.existing_copy = rdownloader::download::conditional::existing_copy(
            output.parent().unwrap_or(std::path::Path::new(".")),
            proposed_name,
        );

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
            // Headless: run directly
            if let Some(tor) = tor {
                let cancel = download::control::get_or_create(job.id).cancel.clone();
                let res = run_job(&job, tor, progress.clone(), cancel).await;
                let p = progress.lock().await;
                tracker.update_from_progress(&p, output.to_str().unwrap_or(""), parts)?;
                res
            } else {
                anyhow::bail!(t!("main.error.headless_without_tor"))
            }
        } else {
            // With TUI: job in the background, the TUI shows progress
            let _handle = start_job_background(
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
                grabber_event_rx,
                grabber_event_tx.clone(),
                jobs_progress,
                registry,
                tracker,
                tor,
                cfg,
                frontend,
            )
            .await
        }
    } else {
        // TUI only
        run_tui(
            ui_cmd_tx,
            ui_cmd_rx,
            job_rx,
            job_event_rx,
            grabber_event_rx,
            grabber_event_tx.clone(),
            jobs_progress,
            registry,
            tracker,
            tor,
            cfg,
            frontend,
        )
        .await
    }
}

/// State behind the headless RPC API.
struct DaemonControl {
    registry: PluginRegistry,
    tor: Option<Arc<TorManager>>,
    tracker: Tracker,
    jobs: Arc<Mutex<Vec<JobProgress>>>,
    dl_dir: PathBuf,
    max_parts: usize,
    handles: Mutex<HashMap<uuid::Uuid, AbortHandle>>,
}

impl DaemonControl {
    fn set_status(&self, id: uuid::Uuid, status: JobStatus) -> Result<(), String> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|job| job.id == id)
            .ok_or_else(|| t!("rpc.error.job_not_found", id = id))?;
        job.status = status;
        Ok(())
    }
}

#[async_trait]
impl JobControl for DaemonControl {
    fn list_jobs(&self) -> Vec<JobProgress> {
        self.jobs.lock().unwrap().clone()
    }

    fn job(&self, id: uuid::Uuid) -> Option<JobProgress> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }

    async fn add_job(&self, url_str: &str) -> Result<uuid::Uuid, String> {
        let tor = self
            .tor
            .clone()
            .ok_or_else(|| t!("main.error.tor_unavailable"))?;
        let url = url::Url::parse(url_str).map_err(|_| t!("ui.status.invalid_url"))?;
        let plugin = self
            .registry
            .find_for_url(&url)
            .ok_or_else(|| t!("ui.status.no_plugin", url = url_str))?;
        let resolved = plugin.resolve(url).await.map_err(|e| format!("{e:#}"))?;
        let proposed_name = resolved
            .filename
            .as_deref()
            .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME);
        let output = rdownloader::download::resolve_destination(&self.dl_dir, proposed_name);
        let filename = output
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME)
            .to_string();
        let mut job = DownloadJob::new(resolved.url, output, self.max_parts);
        job.existing_copy =
            rdownloader::download::conditional::existing_copy(&self.dl_dir, proposed_name);
        let id = job.id;
        let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
            id,
            url: job.url.to_string(),
            filename,
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        }));
        let handle = start_job_background(
            job,
            Some(tor),
            progress,
            self.tracker.clone(),
            mpsc::unbounded_channel().0,
            self.jobs.clone(),
        );
        self.handles
            .lock()
            .unwrap()
            .insert(id, handle.abort_handle());
        Ok(id)
    }

    fn pause_job(&self, id: uuid::Uuid) -> Result<(), String> {
        let snapshot = self
            .job(id)
            .ok_or_else(|| t!("rpc.error.job_not_found", id = id))?;
        if snapshot.status == JobStatus::Paused {
            return Ok(());
        }
        if !matches!(
            snapshot.status,
            JobStatus::Queued | JobStatus::Resolving | JobStatus::Downloading | JobStatus::Merging
        ) {
            return Err(t!("rpc.error.job_not_running"));
        }
        if let Some(entry) = self.tracker.list().into_iter().find(|entry| entry.id == id) {
            let _ = self
                .tracker
                .update_from_progress(&snapshot, &entry.output, entry.tor_parts);
        }
        match download::control::get(id) {
            // The job is running in this process: stop its transfer, keep the task.
            Some(control) => control.pause(),
            None => {
                if let Some(handle) = self.handles.lock().unwrap().remove(&id) {
                    handle.abort();
                }
            }
        }
        self.set_status(id, JobStatus::Paused)?;
        let _ = self.tracker.set_status(id, JobStatus::Paused);
        Ok(())
    }

    fn resume_job(&self, id: uuid::Uuid) -> Result<(), String> {
        let current = self
            .job(id)
            .ok_or_else(|| t!("rpc.error.job_not_found", id = id))?;
        if current.status != JobStatus::Paused {
            return Err(t!("rpc.error.job_not_paused"));
        }
        // Still running in this process (paused in place): just let it read again.
        if let Some(control) = download::control::get(id) {
            control.resume();
            self.set_status(id, JobStatus::Downloading)?;
            let _ = self.tracker.set_status(id, JobStatus::Downloading);
            return Ok(());
        }
        let handle = resume_from_tracker(&self.tracker, self.tor.clone(), &self.jobs, id)?;
        self.handles
            .lock()
            .unwrap()
            .insert(id, handle.abort_handle());
        self.set_status(id, JobStatus::Downloading)?;
        let _ = self.tracker.set_status(id, JobStatus::Downloading);
        Ok(())
    }

    fn cancel_job(&self, id: uuid::Uuid) -> Result<(), String> {
        if let Some(control) = download::control::get(id) {
            control.cancel();
        }
        if let Some(handle) = self.handles.lock().unwrap().remove(&id) {
            handle.abort();
        }
        // An aborted task cannot clean up after itself.
        download::control::remove(id);
        self.set_status(id, JobStatus::Cancelled)?;
        let _ = self.tracker.set_status(id, JobStatus::Cancelled);
        Ok(())
    }

    fn remove_job(&self, id: uuid::Uuid) -> Result<(), String> {
        if let Some(control) = download::control::get(id) {
            control.cancel();
        }
        if let Some(handle) = self.handles.lock().unwrap().remove(&id) {
            handle.abort();
        }
        // An aborted task cannot clean up after itself.
        download::control::remove(id);
        {
            let mut jobs = self.jobs.lock().unwrap();
            let before = jobs.len();
            jobs.retain(|job| job.id != id);
            if jobs.len() == before {
                return Err(t!("rpc.error.job_not_found", id = id));
            }
        }
        let _ = self.tracker.remove(id);
        Ok(())
    }
}

async fn run_daemon(control: DaemonControl, cfg: &config::Config) -> Result<()> {
    let restored = restore_session(&control.tracker, control.tor.clone(), control.jobs.clone());
    let restored_count = restored.len();
    for (id, handle) in restored {
        control
            .handles
            .lock()
            .unwrap()
            .insert(id, handle.abort_handle());
    }
    if restored_count > 0 {
        info!("Restored {restored_count} download(s) from the previous session");
    }
    let addr: SocketAddr = cfg
        .rpc_listen
        .parse()
        .with_context(|| t!("main.error.rpc_bind", addr = &cfg.rpc_listen))?;
    if cfg.rpc_token.is_none() && !addr.ip().is_loopback() {
        anyhow::bail!(t!("main.error.rpc_public_without_token"));
    }
    info!(
        "Headless daemon started with {} job(s) in the queue",
        control.jobs.lock().unwrap().len()
    );
    rpc::serve(addr, cfg.rpc_token.clone(), Arc::new(control)).await
}

#[allow(clippy::too_many_arguments)]
/// Why a URL could not be queued.
enum EnqueueError {
    /// The URL could not be parsed.
    Invalid,
    /// No plugin handles the URL.
    NoPlugin,
    /// The plugin could not resolve the link.
    Resolve(String),
}

impl EnqueueError {
    /// The message for the status line.
    fn status_msg(&self) -> String {
        match self {
            EnqueueError::Invalid => t!("ui.status.invalid_url"),
            EnqueueError::NoPlugin => t!("ui.status.no_plugin", url = "?"),
            EnqueueError::Resolve(e) => t!("ui.status.resolve_error", e = e),
        }
    }
}

/// Resolves a URL with its plugin and starts the download.
///
/// Returns the file name of the download.
#[allow(clippy::too_many_arguments)]
async fn enqueue_url(
    url_str: &str,
    registry: &PluginRegistry,
    tor: Option<Arc<TorManager>>,
    tracker: &Tracker,
    jobs_progress: &Arc<Mutex<Vec<JobProgress>>>,
    dl_dir: &Path,
    max_parts: usize,
) -> Result<String, EnqueueError> {
    let url = url::Url::parse(url_str).map_err(|_| EnqueueError::Invalid)?;
    let plugin = registry.find_for_url(&url).ok_or(EnqueueError::NoPlugin)?;
    let resolved = plugin
        .resolve(url)
        .await
        .map_err(|e| EnqueueError::Resolve(format!("{e:#}")))?;
    // The same link as a stopped download: continue that one, its partial data is kept.
    if let Some(entry) = tracker
        .list()
        .into_iter()
        .find(|e| e.url == resolved.url.as_str() && matches!(e.status, JobStatus::Paused))
    {
        if download::control::get(entry.id).is_none() {
            if let Ok(_handle) = resume_from_tracker(tracker, tor.clone(), jobs_progress, entry.id)
            {
                return Ok(entry.filename);
            }
        }
    }
    let proposed_name = resolved
        .filename
        .as_deref()
        .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME);
    let output = rdownloader::download::resolve_destination(dl_dir, proposed_name);
    let filename = output
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(rdownloader::download::DEFAULT_FALLBACK_FILENAME)
        .to_string();
    let mut job = DownloadJob::new(resolved.url, output.clone(), max_parts);
    job.existing_copy = rdownloader::download::conditional::existing_copy(dl_dir, proposed_name);
    let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
        id: job.id,
        url: job.url.to_string(),
        filename: filename.clone(),
        total_bytes: 0,
        bytes_done: 0,
        parts: vec![],
        status: JobStatus::Queued,
    }));
    let _handle = start_job_background(
        job,
        tor,
        progress,
        tracker.clone(),
        // Mirror directly into jobs_progress
        mpsc::unbounded_channel().0,
        jobs_progress.clone(),
    );
    Ok(filename)
}

#[allow(clippy::too_many_arguments)]
async fn run_tui(
    ui_cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut ui_cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    _job_rx: mpsc::UnboundedReceiver<(DownloadJob, oneshot::Sender<Result<()>>)>,
    job_event_rx: mpsc::UnboundedReceiver<JobEvent>,
    grabber_event_rx: mpsc::UnboundedReceiver<GrabberEvent>,
    grabber_event_tx: mpsc::UnboundedSender<GrabberEvent>,
    jobs_progress: Arc<Mutex<Vec<JobProgress>>>,
    registry: PluginRegistry,
    tracker: Tracker,
    tor: Option<Arc<TorManager>>,
    cfg: &config::Config,
    frontend: Frontend,
) -> Result<()> {
    let app: SharedApp = Arc::new(Mutex::new(App::new(
        ui_cmd_tx,
        job_event_rx,
        grabber_event_rx,
    )));
    let app_for_ui = app.clone();
    let app_for_tui = app.clone();

    // Continue the downloads of the previous session.
    let restored = restore_session(&tracker, tor.clone(), jobs_progress.clone());
    if !restored.is_empty() {
        let mut a = app.lock().unwrap();
        a.status_msg = t!("ui.status.session_restored", count = restored.len());
    }

    // UI command handler: enqueue new downloads
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
                    let result = enqueue_url(
                        &url_str,
                        &registry_c,
                        tor_c.clone(),
                        &tracker_c,
                        &jobs_progress_c,
                        &dl_dir,
                        max_parts,
                    )
                    .await;
                    let mut a = app_for_ui.lock().unwrap();
                    a.status_msg = match result {
                        Ok(filename) => t!("ui.status.started", file = filename),
                        Err(e) => e.status_msg(),
                    };
                }
                UiCommand::AddUrls(urls) => {
                    // The confirmed links of the link grabber.
                    let mut queued = 0usize;
                    let mut failed = 0usize;
                    for url_str in urls {
                        let result = enqueue_url(
                            &url_str,
                            &registry_c,
                            tor_c.clone(),
                            &tracker_c,
                            &jobs_progress_c,
                            &dl_dir,
                            max_parts,
                        )
                        .await;
                        match result {
                            Ok(_) => queued += 1,
                            Err(_) => failed += 1,
                        }
                    }
                    let mut a = app_for_ui.lock().unwrap();
                    a.status_msg = t!("ui.status.enqueued_many", count = queued, failed = failed);
                }
                UiCommand::CrawlPage(page_str) => {
                    let Some(tor) = tor_c.clone() else {
                        let mut a = app_for_ui.lock().unwrap();
                        a.status_msg = t!("ui.linkgrabber.crawl_no_transport");
                        continue;
                    };
                    let Ok(url) = url::Url::parse(&page_str) else {
                        let mut a = app_for_ui.lock().unwrap();
                        a.status_msg = t!("ui.status.invalid_url");
                        continue;
                    };
                    let Some(crawler) = registry_c.find_crawler_for_url(&url) else {
                        let mut a = app_for_ui.lock().unwrap();
                        a.status_msg = t!("ui.linkgrabber.no_crawler", page = url);
                        continue;
                    };
                    // The crawl runs in the background; the result comes back
                    // over the grabber event channel.
                    let tx = grabber_event_tx.clone();
                    tokio::spawn(async move {
                        let transport = TorTransport::new(tor);
                        let event = match crawler.crawl(&url, &transport).await {
                            Ok(links) => GrabberEvent::LinksFound { page: url, links },
                            Err(e) => GrabberEvent::CrawlFailed {
                                page: url,
                                error: format!("{e:#}"),
                            },
                        };
                        let _ = tx.send(event);
                    });
                }
                UiCommand::CancelJob(id) => {
                    match download::control::get(id) {
                        // Running: the job cleans up after itself when it ends.
                        Some(control) => control.cancel(),
                        None => discard_partial_data(id),
                    }
                    let mut jp = jobs_progress_c.lock().unwrap();
                    if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                        j.status = JobStatus::Cancelled;
                    }
                    drop(jp);
                    // A cancelled download must not be continued in a later session.
                    let _ = tracker_c.set_status(id, JobStatus::Cancelled);
                }
                UiCommand::StopJob(id) => {
                    if let Some(control) = download::control::get(id) {
                        control.stop();
                        let mut jp = jobs_progress_c.lock().unwrap();
                        if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                            j.status = JobStatus::Paused;
                        }
                    }
                }
                UiCommand::PauseJob(id) => {
                    if let Some(control) = download::control::get(id) {
                        control.pause();
                    }
                    let mut jp = jobs_progress_c.lock().unwrap();
                    if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                        j.status = JobStatus::Paused;
                    }
                }
                UiCommand::ResumeJob(id) => match download::control::get(id) {
                    // Paused in place (connections still open): let it read again.
                    Some(control) if control.interrupt().is_none() => {
                        control.resume();
                        let mut jp = jobs_progress_c.lock().unwrap();
                        if let Some(j) = jp.iter_mut().find(|j| j.id == id) {
                            if matches!(j.status, JobStatus::Paused) {
                                j.status = JobStatus::Downloading;
                            }
                        }
                    }
                    // Still shutting down after a stop: nothing to resume yet.
                    Some(_) => {}
                    // Stopped, failed or restored after a restart: start it again from the
                    // tracker; the engine continues with the partial data.
                    None => {
                        let result =
                            resume_from_tracker(&tracker_c, tor_c.clone(), &jobs_progress_c, id);
                        if let Err(message) = result {
                            app_for_ui.lock().unwrap().status_msg = message;
                        }
                    }
                },
                UiCommand::RemoveJob(id) => {
                    // A removed download must stop, not only disappear from the list, and its
                    // partial data goes with it.
                    match download::control::get(id) {
                        Some(control) => control.cancel(),
                        None => discard_partial_data(id),
                    }
                    let mut jp = jobs_progress_c.lock().unwrap();
                    jp.retain(|j| j.id != id);
                    drop(jp);
                    // Removed downloads must not come back on the next start.
                    let _ = tracker_c.remove(id);
                }
                UiCommand::CleanPartial => {
                    // Everything that can still be resumed is kept: running jobs and every
                    // tracked download that is not finished or cancelled.
                    let mut keep: std::collections::HashSet<uuid::Uuid> = tracker_c
                        .list()
                        .into_iter()
                        .filter(|e| !matches!(e.status, JobStatus::Done | JobStatus::Cancelled))
                        .map(|e| e.id)
                        .collect();
                    keep.extend(jobs_progress_c.lock().unwrap().iter().map(|j| j.id));
                    let cfg = config::config();
                    let roots = [cfg.temp_dir(), config::legacy_temp_dir()];
                    let orphans = download::partial::find_orphans(&roots, &keep);
                    let (count, bytes) = download::partial::remove_orphans(&orphans);
                    app_for_ui.lock().unwrap().status_msg = t!(
                        "ui.status.partial_cleaned",
                        count = count,
                        size = ui::fmt_bytes(bytes)
                    );
                }
                UiCommand::MoveJob(id, to) => {
                    let mut jp = jobs_progress_c.lock().unwrap();
                    ui::move_job(&mut jp, id, to);
                }
                UiCommand::RemoveHistory(id) => {
                    // The entry leaves the history and the tracker file.
                    let _ = tracker_c.remove(id);
                }
            }
        }
    });

    // Progress poller: mirrors job progress into App.jobs (for the TUI draw)
    {
        let app = app.clone();
        let jobs_progress = jobs_progress.clone();
        let tor_for_health = tor.clone();
        let tracker = tracker.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let snapshot = jobs_progress.lock().unwrap().clone();
                let history = tracker.history();
                let mut a = app.lock().unwrap();
                a.jobs = snapshot;
                a.history = history;
                a.tor_health = tor_for_health.as_ref().map(|t| t.health());
            }
        });
    }

    match frontend {
        // TUI: blocking loop on a helper thread.
        Frontend::Tui => {
            tokio::task::spawn_blocking(move || {
                let _ = ui::run(app_for_tui);
            })
            .await?;
        }
        // Window: must run on the main thread (macOS); the downloads keep running
        // on the runtime's worker threads meanwhile.
        Frontend::Gui => gui::run(app_for_tui)?,
    }

    Ok(())
}

/// The status a job shows after it was interrupted. A stopped job is *Paused*: its
/// partial data is kept, it survives a restart and *Resume* continues it.
fn interrupted_status(kind: download::control::Interrupt) -> JobStatus {
    match kind {
        download::control::Interrupt::Stop => JobStatus::Paused,
        download::control::Interrupt::Cancel => JobStatus::Cancelled,
    }
}

/// Deletes the partial data of a job that is gone for good.
fn discard_partial_data(id: uuid::Uuid) {
    let dir = config::config().job_temp_dir(id);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            warn!("Could not delete the partial data {}: {e}", dir.display());
        }
    }
}

/// Aborts the wrapped task when dropped, so aborting a download also stops
/// its progress forwarder.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl AbortOnDrop {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_job_background(
    job: DownloadJob,
    tor: Option<Arc<TorManager>>,
    progress: Arc<tokio::sync::Mutex<JobProgress>>,
    tracker: Tracker,
    _job_tx: mpsc::UnboundedSender<(DownloadJob, oneshot::Sender<Result<()>>)>,
    jobs_progress: Arc<Mutex<Vec<JobProgress>>>,
) -> tokio::task::JoinHandle<()> {
    let output = job.output.clone();
    let parts = job.max_parts;
    // Register the job with the tracker before the transfer starts: a session
    // that is killed during the download must still be restorable.
    if let Err(e) = tracker.register(TrackEntry::from_job(&job, &output.to_string_lossy(), parts)) {
        warn!("Job {} could not be tracked: {e:#}", job.id);
    }
    tokio::spawn(async move {
        // Insert into jobs_progress
        {
            let p = progress.lock().await;
            let mut jp = jobs_progress.lock().unwrap();
            jp.retain(|x| x.id != p.id);
            jp.push(p.clone());
        }

        // Mirror the engine's progress into the queue and keep the tracker
        // current, so a killed session can be continued later on.
        let control = download::control::get_or_create(job.id);
        let forwarder = {
            let control = control.clone();
            let progress = progress.clone();
            let tracker = tracker.clone();
            let jobs_progress = jobs_progress.clone();
            let output = output.to_string_lossy().to_string();
            AbortOnDrop(tokio::spawn(async move {
                let mut tick: u32 = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let mut p = progress.lock().await.clone();
                    // The engine keeps the connections open while paused; show that.
                    if control.is_paused() && p.status == JobStatus::Downloading {
                        p.status = JobStatus::Paused;
                    }
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
            }))
        };

        // Jobs wait here (status stays "queued") until the scheduler has a free slot
        // overall and for this host.
        let _slot = download::scheduler::global()
            .acquire(&download::scheduler::host_key(&job.url))
            .await;

        let hooks = &config::config().hooks;
        let hook_ctx = |size: u64, error: Option<String>| download::hooks::HookContext {
            job_id: job.id,
            url: job.url.to_string(),
            file: job.output.clone(),
            size,
            error,
        };
        download::hooks::fire(download::hooks::HookEvent::Start, hook_ctx(0, None), hooks);

        // Cancelled while waiting for a free slot: do not start at all.
        if let Some(kind) = control.interrupt() {
            let status = interrupted_status(kind);
            progress.lock().await.status = status.clone();
            let _ = tracker.set_status(job.id, status);
            if kind == download::control::Interrupt::Cancel {
                discard_partial_data(job.id);
            }
            download::control::remove(job.id);
            return;
        }

        let cancel = control.cancel.clone();
        let res = if let Some(tor) = tor {
            run_job(&job, tor, progress.clone(), cancel).await
        } else {
            Err(anyhow::anyhow!(t!("main.error.tor_unavailable")))
        };

        let interrupt = control.interrupt();
        forwarder.abort();
        download::control::remove(job.id);

        // Stop and cancel are the user's decision, not a failure: the engine ended with an
        // error only because its connections were closed.
        if let Some(kind) = interrupt {
            progress.lock().await.status = interrupted_status(kind);
            if kind == download::control::Interrupt::Cancel {
                discard_partial_data(job.id);
            }
        }

        // Final progress + tracker update
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

        if let Some(kind) = interrupt {
            info!(
                "Job {} {}: {}",
                job.id,
                if kind == download::control::Interrupt::Stop {
                    "stopped"
                } else {
                    "cancelled"
                },
                job.output.display()
            );
            return;
        }

        let final_size = progress.lock().await.total_bytes;
        match &res {
            Ok(()) => download::hooks::fire(
                download::hooks::HookEvent::Complete,
                hook_ctx(final_size, None),
                hooks,
            ),
            Err(e) => download::hooks::fire(
                download::hooks::HookEvent::Error,
                hook_ctx(final_size, Some(format!("{e:#}"))),
                hooks,
            ),
        }

        match res {
            Ok(()) => {
                info!("Job done: {}", job.output.display());
                let cfg = config::config();
                if cfg.auto_extract {
                    let (path, delete) = (job.output.clone(), cfg.extract_delete_archives);
                    let outcome = tokio::task::spawn_blocking(move || {
                        download::extract::auto_extract(&path, Default::default(), delete)
                    })
                    .await;
                    match outcome {
                        Ok(Ok(Some(r))) => {
                            info!(
                                "Extracted {} files ({} bytes) to {}",
                                r.files,
                                r.bytes,
                                r.dest.display()
                            )
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => warn!("Extraction failed: {e:#}"),
                        Err(e) => warn!("Extraction task failed: {e}"),
                    }
                }
            }
            Err(e) => error!("Job failed: {e:#}"),
        }
    })
}

/// Starts a paused, stopped or failed download again from its tracker entry. The job keeps
/// its id, so the engine finds the partial data and continues where it left off.
fn resume_from_tracker(
    tracker: &Tracker,
    tor: Option<Arc<TorManager>>,
    jobs_progress: &Arc<Mutex<Vec<JobProgress>>>,
    id: uuid::Uuid,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let tor = tor.ok_or_else(|| t!("main.error.tor_unavailable"))?;
    let entry = tracker
        .list()
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| t!("rpc.error.job_not_found", id = id))?;
    let job = entry.to_download_job().map_err(|e| format!("{e:#}"))?;
    let progress = Arc::new(tokio::sync::Mutex::new(JobProgress {
        id,
        url: entry.url,
        filename: entry.filename,
        total_bytes: entry.total_bytes,
        bytes_done: entry.bytes_done,
        parts: vec![],
        status: JobStatus::Queued,
    }));
    Ok(start_job_background(
        job,
        Some(tor),
        progress,
        tracker.clone(),
        mpsc::unbounded_channel().0,
        jobs_progress.clone(),
    ))
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
) -> Vec<(uuid::Uuid, tokio::task::JoinHandle<()>)> {
    let entries = tracker.open_entries();
    let mut restored = Vec::new();

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
                let handle = start_job_background(
                    job,
                    tor.clone(),
                    progress,
                    tracker.clone(),
                    mpsc::unbounded_channel().0,
                    jobs_progress.clone(),
                );
                restored.push((entry.id, handle));
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
        assert!(restored.is_empty());

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
        assert!(restored.is_empty());
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
        assert!(restored.is_empty());
        assert!(jobs.lock().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod frontend_tests {
    use super::*;

    #[test]
    fn the_window_is_the_default_with_a_display() {
        assert_eq!(choose_frontend(false, false, true), Frontend::Gui);
    }

    #[test]
    fn without_a_display_the_terminal_ui_is_used() {
        assert_eq!(choose_frontend(false, false, false), Frontend::Tui);
    }

    #[test]
    fn flags_override_the_default() {
        assert_eq!(choose_frontend(false, true, true), Frontend::Tui);
        assert_eq!(choose_frontend(true, false, false), Frontend::Gui);
    }

    #[test]
    fn stop_means_paused_and_cancel_means_cancelled() {
        use download::control::Interrupt;
        assert_eq!(interrupted_status(Interrupt::Stop), JobStatus::Paused);
        assert_eq!(interrupted_status(Interrupt::Cancel), JobStatus::Cancelled);
    }

    #[test]
    fn cancelling_deletes_the_partial_data_and_nothing_else() {
        let id = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let cfg = config::config();
        let (mine, theirs) = (cfg.job_temp_dir(id), cfg.job_temp_dir(other));
        for d in [&mine, &theirs] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("part-000"), b"data").unwrap();
        }
        discard_partial_data(id);
        assert!(!mine.exists());
        assert!(theirs.exists(), "another download's data stays");
        discard_partial_data(id); // already gone: no error
        std::fs::remove_dir_all(theirs).unwrap();
    }

    #[test]
    fn a_stopped_download_cannot_be_resumed_without_tor_or_unknown_ids() {
        let dir = std::env::temp_dir().join(format!("rdl-resume-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = Tracker::at(dir.join("downloads.json")).unwrap();
        let jobs: Arc<Mutex<Vec<JobProgress>>> = Arc::new(Mutex::new(Vec::new()));
        assert!(resume_from_tracker(&tracker, None, &jobs, uuid::Uuid::new_v4()).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
