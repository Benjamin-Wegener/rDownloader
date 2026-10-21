//! Tor-Integration via `arti-client` (offizielle Rust-Tor-Implementierung des Tor-Projekts).
//!
//! Clean-room: Kein Code aus JDownloader oder C-Tor übernommen.
//! Baut pro Download-Part einen eigenen Tor-Circuit via isolierte Clients.

use anyhow::{Context, Result};
use arti_client::config::TorClientConfigBuilder;
use arti_client::TorClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tor_rtcompat::PreferredRuntime;
use tracing::info;

/// Wrapper für AsyncRead + AsyncWrite, weil `arti_client`'s DataStream beides ist
/// aber wir es als trait-object für Flexibilität brauchen.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// Hält mehrere unabhängige Tor-Clients (je mit eigenem Data-Directory = eigener Circuit-Pool).
pub struct TorManager {
    clients: Vec<Arc<TorClient<PreferredRuntime>>>,
}

impl TorManager {
    /// Erstellt `count` unabhängige Tor-Client-Instanzen (je eigener State/Circuit-Pool).
    pub async fn bootstrap(count: usize, data_dir_base: PathBuf) -> Result<Self> {
        let mut clients = Vec::with_capacity(count);
        for i in 0..count {
            let dir = data_dir_base.join(format!("tor-{i}"));
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Tor-Data-Dir anlegen: {dir:?}"))?;

            // TorClientConfig mit eigenem state/cache Dir pro Instanz
            let config =
                TorClientConfigBuilder::from_directories(dir.join("state"), dir.join("cache"))
                    .build()
                    .map_err(|e| anyhow::anyhow!("TorClientConfig bauen: {e}"))?;

            info!("Tor-Client {i}: bootstrap ({dir:?}) …");
            let client = TorClient::builder()
                .config(config)
                .create_bootstrapped()
                .await
                .map_err(|e| anyhow::anyhow!("Tor-Client {i} bootstrap: {e}"))?;

            info!("Tor-Client {i}: bootstrapped OK");
            clients.push(client);
        }
        Ok(Self { clients })
    }

    pub fn client(&self, index: usize) -> Result<Arc<TorClient<PreferredRuntime>>> {
        self.clients
            .get(index % self.clients.len())
            .cloned()
            .context("Tor-Client Index ungültig")
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Baut eine frische Tor-Stream-Verbindung zum Ziel (Host:Port).
    /// Gibt ein AsyncRead+AsyncWrite-Paar zurück.
    pub async fn connect(
        &self,
        index: usize,
        host: &str,
        port: u16,
    ) -> Result<Box<dyn AsyncReadWrite>> {
        let client = self.client(index)?;
        let stream = client
            .connect((host, port))
            .await
            .map_err(|e| anyhow::anyhow!("Tor-Stream {index} → {host}:{port}: {e}"))?;
        Ok(Box::new(stream))
    }
}
