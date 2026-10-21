//! Central configuration (TOML, under `~/.config/rdownloader/config.toml`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Deprecated: a single Tor client isolates every part. Kept for backward
    /// compatible config files; values above 1 are ignored.
    #[serde(default = "default_tor_clients")]
    pub tor_clients: usize,

    /// Max parts per download (1..=10)
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,

    /// Base directory for Tor data
    #[serde(default)]
    pub tor_data_dir: Option<PathBuf>,

    /// Bridge lines to reach the Tor network through censored networks.
    /// Each entry is one bridge line (a leading `Bridge ` prefix is accepted).
    #[serde(default)]
    pub bridges: Vec<String>,

    /// Temp directory for part files
    #[serde(default)]
    pub temp_dir: Option<PathBuf>,

    /// Target directory for downloads
    #[serde(default)]
    pub download_dir: Option<PathBuf>,

    /// Default user agent
    #[serde(default = "default_ua")]
    pub user_agent: String,

    /// Connection timeout (seconds)
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,

    /// Read timeout (seconds)
    #[serde(default = "default_read_timeout")]
    pub read_timeout_secs: u64,

    /// Jobs that may run at the same time (0 = unlimited)
    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent_jobs: usize,

    /// Jobs per host that may run at the same time (0 = unlimited)
    #[serde(default = "default_max_jobs_per_host")]
    pub max_jobs_per_host: usize,

    /// Unpack finished archives (.zip, .7z, .rar) into a folder next to them.
    /// Off by default: archives from the network are untrusted input.
    #[serde(default)]
    pub auto_extract: bool,

    /// Delete the archive files after a successful extraction.
    #[serde(default)]
    pub extract_delete_archives: bool,

    /// Global download speed limit in KiB/s (0 = unlimited)
    #[serde(default)]
    pub max_speed_kib: u64,

    /// Max retries per part
    #[serde(default = "default_retries")]
    pub max_retries: u32,

    /// Tor logging level (error/warn/info/debug/trace)
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Listen address of the headless JSON-RPC/REST server
    #[serde(default = "default_rpc_listen")]
    pub rpc_listen: String,

    /// Optional bearer token for the RPC server. Without a token the server
    /// must stay bound to loopback.
    #[serde(default)]
    pub rpc_token: Option<String>,
}

fn default_tor_clients() -> usize {
    10
}
fn default_max_parts() -> usize {
    10
}
fn default_ua() -> String {
    "rdownloader/0.1".into()
}
fn default_timeout() -> u64 {
    30
}
fn default_read_timeout() -> u64 {
    60
}
fn default_max_concurrent_jobs() -> usize {
    3
}
fn default_max_jobs_per_host() -> usize {
    2
}
fn default_retries() -> u32 {
    3
}
fn default_log_level() -> String {
    "info".into()
}
fn default_rpc_listen() -> String {
    "127.0.0.1:8765".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tor_clients: default_tor_clients(),
            max_parts: default_max_parts(),
            tor_data_dir: None,
            bridges: Vec::new(),
            temp_dir: None,
            download_dir: None,
            user_agent: default_ua(),
            connect_timeout_secs: default_timeout(),
            read_timeout_secs: default_read_timeout(),
            max_concurrent_jobs: default_max_concurrent_jobs(),
            max_jobs_per_host: default_max_jobs_per_host(),
            auto_extract: false,
            extract_delete_archives: false,
            max_speed_kib: 0,
            max_retries: default_retries(),
            log_level: default_log_level(),
            rpc_listen: default_rpc_listen(),
            rpc_token: None,
        }
    }
}

impl Config {
    pub fn describe(&self) -> String {
        format!(
            "tor_clients={}, bridges={}, max_parts={}, retries={}, log={}, rpc={}",
            self.tor_clients,
            self.bridges.len(),
            self.max_parts,
            self.max_retries,
            self.log_level,
            self.rpc_listen
        )
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdownloader")
            .join("config.toml")
    }

    pub fn tor_data_dir(&self) -> PathBuf {
        self.tor_data_dir.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("rdownloader")
                .join("tor")
        })
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.temp_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("rdownloader"))
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir.clone().unwrap_or_else(|| {
            dirs::download_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("rdownloader")
        })
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn load() -> Result<Config> {
    let path = Config::config_path();
    if path.exists() {
        let s =
            std::fs::read_to_string(&path).with_context(|| format!("reading config: {path:?}"))?;
        let cfg: Config = toml::from_str(&s).context("parsing config (TOML)")?;
        Ok(cfg)
    } else {
        let cfg = Config::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(&cfg)?;
        std::fs::write(&path, s)?;
        Ok(cfg)
    }
}

pub fn config() -> &'static Config {
    CONFIG.get_or_init(|| load().unwrap_or_default())
}

pub fn init() -> Result<&'static Config> {
    let cfg = load()?;
    let _ = CONFIG.set(cfg);
    Ok(CONFIG.get().unwrap())
}
