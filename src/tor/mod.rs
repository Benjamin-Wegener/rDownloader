//! Tor integration via `arti-client` (the Tor Project's official Rust implementation).
//!
//! Clean-room: no code taken from JDownloader or C Tor.
//!
//! A single bootstrapped [`TorClient`] serves the whole process. Each download
//! part requests its own [`IsolationToken`], so streams only share a circuit
//! when they carry the same isolation id. This keeps part isolation without
//! paying for one client (and one data directory) per part.

use anyhow::{Context, Result};
use arti_client::config::TorClientConfigBuilder;
use arti_client::status::BootstrapStatus;
use arti_client::{IsolationToken, StreamPrefs, TorClient};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tor_rtcompat::PreferredRuntime;
use tracing::{info, warn};

/// Read + write byte stream, usable as a trait object.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// Snapshot of the Tor client's readiness for traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorHealth {
    ready: bool,
    bootstrap_percent: u8,
    blockage: Option<String>,
}

impl TorHealth {
    fn from_status(status: &BootstrapStatus) -> Self {
        let percent = (status.as_frac() * 100.0).round().clamp(0.0, 100.0) as u8;
        Self {
            ready: status.ready_for_traffic(),
            bootstrap_percent: percent,
            blockage: status.blocked().map(|b| b.message().to_string()),
        }
    }

    /// Whether the client can carry traffic right now.
    pub fn is_healthy(&self) -> bool {
        self.ready && self.blockage.is_none()
    }

    pub fn ready(&self) -> bool {
        self.ready
    }

    pub fn bootstrap_percent(&self) -> u8 {
        self.bootstrap_percent
    }

    pub fn blockage(&self) -> Option<&str> {
        self.blockage.as_deref()
    }
}

/// One Tor client plus stable isolation tokens per stream group.
pub struct TorManager {
    client: Arc<TorClient<PreferredRuntime>>,
    isolations: IsolationTokens,
}

impl TorManager {
    /// Bootstraps a single Tor client with state and cache below `data_dir`.
    pub async fn bootstrap(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("Creating Tor data directory: {data_dir:?}"))?;

        let config = TorClientConfigBuilder::from_directories(
            data_dir.join("state"),
            data_dir.join("cache"),
        )
        .build()
        .map_err(|e| anyhow::anyhow!("Building TorClientConfig: {e}"))?;

        info!("Bootstrapping Tor client ({data_dir:?}) ...");
        let client = TorClient::builder()
            .config(config)
            .create_bootstrapped()
            .await
            .map_err(|e| anyhow::anyhow!("Tor client bootstrap: {e}"))?;
        info!("Tor client bootstrapped");

        Ok(Self::from_client(client))
    }

    /// Wraps an already bootstrapped client (tests, custom bootstrapping).
    pub fn from_client(client: Arc<TorClient<PreferredRuntime>>) -> Self {
        Self {
            client,
            isolations: IsolationTokens::default(),
        }
    }

    pub fn client(&self) -> &Arc<TorClient<PreferredRuntime>> {
        &self.client
    }

    /// Current readiness of the Tor client (bootstrap progress and blockage).
    pub fn health(&self) -> TorHealth {
        TorHealth::from_status(&self.client.bootstrap_status())
    }

    /// Drops the isolation token for `isolation`, so the next connection of
    /// that group is built on a fresh circuit. Call this after a stream or
    /// part failed; the existing retry loop then reconnects automatically.
    pub fn invalidate_isolation(&self, isolation: u64) {
        self.isolations.invalidate(isolation);
    }

    /// Opens a fresh stream without a dedicated isolation group.
    pub async fn connect(&self, host: &str, port: u16) -> Result<Box<dyn AsyncReadWrite>> {
        let stream = self
            .client
            .connect((host, port))
            .await
            .map_err(|e| anyhow::anyhow!("Tor stream -> {host}:{port}: {e}"))?;
        Ok(Box::new(stream))
    }

    /// Opens a stream that shares circuits only with streams of the same
    /// `isolation` id. Different ids never share a circuit.
    pub async fn connect_isolated(
        &self,
        host: &str,
        port: u16,
        isolation: u64,
    ) -> Result<Box<dyn AsyncReadWrite>> {
        let prefs = self.stream_prefs(isolation);
        match self.client.connect_with_prefs((host, port), &prefs).await {
            Ok(stream) => Ok(Box::new(stream)),
            Err(error) => {
                // A failed connect usually means the circuit behind this
                // isolation group is unusable. Forget the token so the retry
                // (see `RetryPolicy`) reconnects on a fresh circuit.
                warn!("Tor connect failed (isolation {isolation}); rotating circuit: {error}");
                self.invalidate_isolation(isolation);
                Err(anyhow::anyhow!(
                    "Tor stream -> {host}:{port} (isolation {isolation}): {error}"
                ))
            }
        }
    }

    fn stream_prefs(&self, isolation: u64) -> StreamPrefs {
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(self.isolations.token(isolation));
        prefs
    }
}

/// Stable [`IsolationToken`] per numeric isolation id.
///
/// The same id always maps to the same token (so reconnects of a part may reuse
/// its circuit); different ids always map to different tokens.
#[derive(Default)]
struct IsolationTokens {
    tokens: Mutex<HashMap<u64, IsolationToken>>,
}

impl IsolationTokens {
    fn token(&self, isolation: u64) -> IsolationToken {
        *self
            .tokens
            .lock()
            .expect("isolation map poisoned")
            .entry(isolation)
            .or_insert_with(IsolationToken::new)
    }

    fn invalidate(&self, isolation: u64) {
        self.tokens
            .lock()
            .expect("isolation map poisoned")
            .remove(&isolation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_ids_map_to_stable_distinct_tokens() {
        let tokens = IsolationTokens::default();
        let a = tokens.token(1);
        let b = tokens.token(1);
        let c = tokens.token(2);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn invalidating_an_isolation_forces_a_fresh_token() {
        let tokens = IsolationTokens::default();
        let before = tokens.token(7);
        tokens.invalidate(7);
        let after = tokens.token(7);
        assert_ne!(before, after);
        assert_eq!(after, tokens.token(7));
    }

    #[test]
    fn health_snapshot_of_unbootstrapped_client_is_unhealthy() {
        let health = TorHealth::from_status(&BootstrapStatus::default());
        assert!(!health.ready());
        assert_eq!(health.bootstrap_percent(), 0);
        assert!(health.blockage().is_some());
        assert!(!health.is_healthy());
    }

    #[test]
    fn distinct_ids_never_share_a_token() {
        let tokens = IsolationTokens::default();
        let all: Vec<_> = (0..16).map(|id| tokens.token(id)).collect();
        for (i, left) in all.iter().enumerate() {
            for right in &all[i + 1..] {
                assert_ne!(left, right);
            }
        }
    }
}
