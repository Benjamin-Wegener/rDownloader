//! Zentrale Konfiguration (TOML, unter `~/.config/rdownloader/config.toml`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// An paralleler Tor-Clients (1..=10, wie JDownloader max. Connections)
    #[serde(default = "default_tor_clients")]
    pub tor_clients: usize,

    /// Max. Parts pro Download (1..=10)
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,

    /// Tor-Data-Verzeichnis-Basis
    #[serde(default)]
    pub tor_data_dir: Option<PathBuf>,

    /// Temp-Verzeichnis für Teildateien
    #[serde(default)]
    pub temp_dir: Option<PathBuf>,

    /// Ziel-Verzeichnis für Downloads
    #[serde(default)]
    pub download_dir: Option<PathBuf>,

    /// Standard-User-Agent
    #[serde(default = "default_ua")]
    pub user_agent: String,

    /// Verbindungs-Timeout (Sekunden)
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,

    /// Read-Timeout (Sekunden)
    #[serde(default = "default_read_timeout")]
    pub read_timeout_secs: u64,

    /// Max. Retries pro Part
    #[serde(default = "default_retries")]
    pub max_retries: u32,

    /// Tor-Logging-Level (error/warn/info/debug/trace)
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_tor_clients() -> usize { 10 }
fn default_max_parts() -> usize { 10 }
fn default_ua() -> String { "rdownloader/0.1".into() }
fn default_timeout() -> u64 { 30 }
fn default_read_timeout() -> u64 { 60 }
fn default_retries() -> u32 { 3 }
fn default_log_level() -> String { "info".into() }

impl Default for Config {
    fn default() -> Self {
        Self {
            tor_clients: default_tor_clients(),
            max_parts: default_max_parts(),
            tor_data_dir: None,
            temp_dir: None,
            download_dir: None,
            user_agent: default_ua(),
            connect_timeout_secs: default_timeout(),
            read_timeout_secs: default_read_timeout(),
            max_retries: default_retries(),
            log_level: default_log_level(),
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdownloader")
            .join("config.toml")
    }

    pub fn tor_data_dir(&self) -> PathBuf {
        self.tor_data_dir
            .clone()
            .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("rdownloader").join("tor"))
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.temp_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("rdownloader"))
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir
            .clone()
            .unwrap_or_else(|| dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")).join("rdownloader"))
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn load() -> Result<Config> {
    let path = Config::config_path();
    if path.exists() {
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("Config lesen: {path:?}"))?;
        let cfg: Config = toml::from_str(&s).context("Config parsen (TOML)")?;
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
