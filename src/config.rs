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

    /// Time windows that change the speed limit or pause downloads (`[[schedule]]`).
    #[serde(default)]
    pub schedule: Vec<crate::download::timetable::Rule>,

    /// Global download speed limit in KiB/s (0 = unlimited)
    #[serde(default)]
    pub max_speed_kib: u64,

    /// Restart single connections that stay far below the other parts of the same
    /// download (rationed: cool-down, per-job and global limits).
    #[serde(default = "default_true")]
    pub restart_slow_parts: bool,

    /// Drop and rebuild connections slower than this many KiB/s (0 = off).
    /// Measured over 30 s of network time, so the speed limit does not count.
    #[serde(default = "default_min_speed_kib")]
    pub min_speed_kib: u64,

    /// Skip a download when the existing local file has the server's size and is
    /// not older than the server's `Last-Modified`.
    #[serde(default)]
    pub conditional_get: bool,

    /// Programs to run on download events and desktop notifications (`[hooks]`).
    #[serde(default)]
    pub hooks: crate::download::hooks::HooksConfig,

    /// Max retries per part
    #[serde(default = "default_retries")]
    pub max_retries: u32,

    /// Collect links you copy in the link grabber (never starts downloads by itself).
    #[serde(default)]
    pub watch_clipboard: bool,

    /// Whether rdownloader should automatically start when logging in
    #[serde(default)]
    pub autostart: bool,

    /// Tor logging level (error/warn/info/debug/trace)
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Interface language (for example `en` or `de`). The
    /// `RDOWNLOADER_LANG` environment variable overrides this value.
    #[serde(default)]
    pub language: Option<String>,

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
fn default_true() -> bool {
    true
}
fn default_min_speed_kib() -> u64 {
    1
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
            read_timeout_secs: default_read_timeout(),
            max_concurrent_jobs: default_max_concurrent_jobs(),
            max_jobs_per_host: default_max_jobs_per_host(),
            auto_extract: false,
            extract_delete_archives: false,
            schedule: Vec::new(),
            max_speed_kib: 0,
            restart_slow_parts: true,
            min_speed_kib: default_min_speed_kib(),
            conditional_get: false,
            hooks: Default::default(),
            max_retries: default_retries(),
            autostart: false,
            watch_clipboard: false,
            log_level: default_log_level(),
            language: None,
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

    /// Where partial downloads live. The default is a permanent folder in the user's
    /// data directory: the system temp folder is cleaned up by the OS (reboot, age),
    /// which would silently destroy the half of a big download.
    pub fn temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .map(|d| d.join("rdownloader").join("partial"))
                .unwrap_or_else(legacy_temp_dir)
        })
    }

    /// Folder of the partial data of job `id`. Downloads started before the default
    /// moved (to `legacy_temp_dir`) are found there and keep going.
    pub fn job_temp_dir(&self, id: uuid::Uuid) -> PathBuf {
        let current = self.temp_dir().join(format!("job-{id}"));
        let legacy = legacy_temp_dir().join(format!("job-{id}"));
        if self.temp_dir.is_none() && !current.exists() && legacy.exists() {
            legacy
        } else {
            current
        }
    }

    /// Directory with dynamically loaded WebAssembly plugins.
    pub fn plugin_dir(&self) -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdownloader")
            .join("plugins")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir.clone().unwrap_or_else(|| {
            dirs::download_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("rdownloader")
        })
    }
}

/// The old default for partial downloads (inside the system temp folder).
pub fn legacy_temp_dir() -> PathBuf {
    std::env::temp_dir().join("rdownloader")
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

/// Reads only the `language` key from an existing config file.
///
/// This runs before clap builds the help text, so `--help` honours the configured
/// language. A missing or broken file is ignored here; [`init`] reports it later.
pub fn language_hint() -> Option<String> {
    let text = std::fs::read_to_string(Config::config_path()).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("language")
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

pub fn config() -> &'static Config {
    CONFIG.get_or_init(|| load().unwrap_or_default())
}

pub fn init() -> Result<&'static Config> {
    let cfg = load()?;
    let _ = CONFIG.set(cfg);
    Ok(CONFIG.get().unwrap())
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    /// The first start writes the default config as TOML; a table placed before
    /// plain values would make that fail.
    #[test]
    fn default_config_roundtrips_through_toml() {
        let text = toml::to_string_pretty(&Config::default()).expect("default config serialises");
        let back: Config = toml::from_str(&text).expect("and parses again");
        assert_eq!(back.hooks, Config::default().hooks);
        assert_eq!(back.max_parts, Config::default().max_parts);
    }

    #[test]
    fn language_is_optional_and_read_from_the_config() {
        assert_eq!(Config::default().language, None);
        let cfg: Config = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(cfg.language.as_deref(), Some("de"));
    }

    #[test]
    fn hooks_section_is_read_from_a_config_file() {
        let cfg: Config =
            toml::from_str("[hooks]\non_complete = [\"/bin/true\"]\nnotify = true\n").unwrap();
        assert_eq!(cfg.hooks.on_complete, ["/bin/true"]);
        assert!(cfg.hooks.notify);
    }

    #[test]
    fn partial_downloads_default_to_a_permanent_folder() {
        let cfg = Config::default();
        assert_ne!(cfg.temp_dir(), legacy_temp_dir(), "not the OS temp folder");
        assert!(cfg.temp_dir().ends_with("partial"));
        let own = Config {
            temp_dir: Some(PathBuf::from("/data/tmp")),
            ..Config::default()
        };
        assert_eq!(own.temp_dir(), PathBuf::from("/data/tmp"));
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            own.job_temp_dir(id),
            PathBuf::from(format!("/data/tmp/job-{id}"))
        );
    }

    #[test]
    fn a_job_started_in_the_old_default_folder_is_still_found() {
        let id = uuid::Uuid::new_v4();
        let legacy = legacy_temp_dir().join(format!("job-{id}"));
        std::fs::create_dir_all(&legacy).unwrap();
        let cfg = Config::default();
        assert_eq!(cfg.job_temp_dir(id), legacy, "resumes where it left off");
        // A configured folder always wins.
        let own = Config {
            temp_dir: Some(PathBuf::from("/data/tmp")),
            ..Config::default()
        };
        assert_eq!(
            own.job_temp_dir(id),
            PathBuf::from(format!("/data/tmp/job-{id}"))
        );
        std::fs::remove_dir_all(legacy).unwrap();
        assert_eq!(
            cfg.job_temp_dir(id),
            cfg.temp_dir().join(format!("job-{id}"))
        );
    }
}
