//! Clean-room Plugin-System (eigenes, NICHT JDs .jar-Plugins).
//!
//! JD-Plugins sind Java, gebunden an JDownloads interne API – eine clean-room
//! Rust-Implementierung kann diese nicht laden. Stattdessen: eigenes, sicheres
//! Plugin-Trait-System mit gleicher Idee (Hoster-, Captcha-, Account-Resolver).

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

pub mod crawler;
pub mod example_redirect;

#[allow(unused_imports)] // public plugin API; wired into the link grabber later
pub use crawler::{CrawledLink, CrawlerPlugin, HtmlLinkCrawlerPlugin};
pub use example_redirect::ExampleRedirectHosterPlugin;

/// Metadaten eines Plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub kind: PluginKind,
    /// Domains die dieses Plugin handhaben (z.B. "example.com")
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginKind {
    /// Löst einen Link in eine direkte Download-URL auf (Hoster-Resolver)
    Hoster,
    /// Captcha-Löser (z.B. manuell, 2captcha-API, etc.)
    Captcha,
    /// Account-Login für Hoster (Premium etc.)
    Account,
    /// Link-Crawler (findet mehr Links auf einer Seite)
    Crawler,
}

/// Ergebnis eines Hoster-Plugins: Direkte Download-URL + Metadaten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedLink {
    pub url: Url,
    pub filename: Option<String>,
    pub filesize: Option<u64>,
    pub supports_ranges: bool,
    pub additional_headers: HashMap<String, String>,
}

/// Captcha-Herausforderung, die ein Plugin zurückgeben kann.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptchaChallenge {
    pub challenge_type: String, // "image", "text", "recaptcha", "hcaptcha", ...
    pub image_base64: Option<String>,
    pub text_prompt: Option<String>,
}

/// Plugin-Definition: Metadaten + Verhaltens-Callback.
#[async_trait]
pub trait DownloadPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn can_handle(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        self.meta().domains.iter().any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    async fn resolve(&self, url: Url) -> Result<ResolvedLink>;

    async fn fetch_captcha(&self, _url: &Url) -> Result<Option<CaptchaChallenge>> {
        Ok(None)
    }

    async fn submit_captcha(&self, _url: &Url, _solution: String) -> Result<Option<ResolvedLink>> {
        Ok(None)
    }
}

/// Statische Registry aller geladenen Plugins.
#[derive(Clone)]
pub struct PluginRegistry {
    plugins: HashMap<String, Arc<dyn DownloadPlugin>>,
    crawlers: Vec<Arc<dyn CrawlerPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self { plugins: HashMap::new(), crawlers: Vec::new() }
    }

    pub fn register(&mut self, plugin: Arc<dyn DownloadPlugin>) {
        let id = plugin.meta().id.clone();
        self.plugins.insert(id, plugin);
    }

    pub fn register_crawler(&mut self, crawler: Arc<dyn CrawlerPlugin>) {
        self.crawlers.push(crawler);
    }

    /// First registered crawler that accepts `url` (registration order = priority).
    pub fn find_crawler_for_url(&self, url: &Url) -> Option<Arc<dyn CrawlerPlugin>> {
        self.crawlers.iter().find(|c| c.can_handle(url)).cloned()
    }

    pub fn list(&self) -> Vec<PluginMeta> {
        self.plugins.values().map(|p| p.meta()).chain(self.crawlers.iter().map(|c| c.meta())).collect()
    }

    pub fn find_for_url(&self, url: &Url) -> Option<Arc<dyn DownloadPlugin>> {
        self.plugins.values().find(|p| p.can_handle(url)).cloned()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn DownloadPlugin>> {
        self.plugins.get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Einfaches Direkt-Download-Plugin (Fallback, funktioniert für jeden URL der
/// bereits eine direkte Datei ist – kein Hoster-Redirect).
pub struct DirectDownloadPlugin;

#[async_trait::async_trait]
impl DownloadPlugin for DirectDownloadPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "direct".into(),
            name: "Direct Download".into(),
            version: "0.1.0".into(),
            author: "rdownloader".into(),
            description: "Direkter Download ohne Redirect-Auflösung".into(),
            kind: PluginKind::Hoster,
            domains: vec!["*".into()],
        }
    }

    fn can_handle(&self, _url: &Url) -> bool {
        true // Fallback – fängt alles
    }

    async fn resolve(&self, url: Url) -> Result<ResolvedLink> {
        let filename = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "download.bin".into());
        Ok(ResolvedLink {
            url,
            filename: Some(filename),
            filesize: None,
            supports_ranges: true,
            additional_headers: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn crawler_registry_lookup() {
        let mut reg = PluginRegistry::new();
        let url = Url::parse("https://x.test/").unwrap();
        assert!(reg.find_crawler_for_url(&url).is_none());
        reg.register_crawler(Arc::new(HtmlLinkCrawlerPlugin));
        assert!(reg.find_crawler_for_url(&url).is_some());
        assert!(reg.list().iter().any(|m| m.kind == PluginKind::Crawler));
    }
}
