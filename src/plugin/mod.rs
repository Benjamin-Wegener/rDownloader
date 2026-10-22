//! Clean-room plugin system (our own, NOT JD's .jar plugins).
//!
//! JD plugins are Java and bound to JDownloader's internal API – a clean-room
//! Rust implementation cannot load them. Instead: our own, safe plugin trait
//! system with the same idea (hoster, captcha, account resolvers).

use crate::t;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

pub mod account;
pub mod captcha;
pub mod container;
pub mod cookies;
pub mod crawler;
pub mod example_redirect;
pub mod netrc;
pub mod permissions;
pub mod wasm;

#[allow(unused_imports)] // public plugin API; wired into the link grabber later
pub use account::{
    AccountManager, AccountPlugin, AccountSession, Cookie, CookieImportAccountPlugin, Credentials,
    Secret,
};
#[allow(unused_imports)]
pub use captcha::{
    CaptchaChallenge, CaptchaManager, CaptchaPlugin, CaptchaSolution, CaptchaSolver,
    ChallengePayload, InteractiveCaptchaSolver, MathCaptchaPlugin,
};
#[allow(unused_imports)]
pub use container::{
    extract_ccf, extract_dlc, import_container_file, parse_metalink, parse_plain_text,
    ContainerContent, ContainerFormat, ContainerItem,
};
#[allow(unused_imports)]
pub use cookies::{
    import_cookie_file, parse_netscape_cookies, read_chromium_cookies, read_firefox_cookies,
    CookieFormat, CookieImportReport, ImportedCookie,
};
#[allow(unused_imports)]
pub use crawler::{CrawledLink, CrawlerPlugin, HtmlLinkCrawlerPlugin};
pub use example_redirect::ExampleRedirectHosterPlugin;
#[allow(unused_imports)]
pub use netrc::{Netrc, NetrcEntry};
#[allow(unused_imports)]
pub use permissions::{PermissionDenied, PluginPermissions};
#[allow(unused_imports)]
pub use wasm::{
    load_dir as load_wasm_plugins, DenyHttpClient, TransportHttpClient, WasmHttpClient, WasmPlugin,
    WasmRuntimeConfig,
};

/// Metadata of a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub kind: PluginKind,
    /// Domains this plugin handles (e.g. "example.com")
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginKind {
    /// Resolves a link into a direct download URL (hoster resolver)
    Hoster,
    /// Captcha solver (e.g. manual, 2captcha API, etc.)
    Captcha,
    /// Account login for hosters (premium etc.)
    Account,
    /// Link crawler (finds more links on a page)
    Crawler,
}

/// Result of a hoster plugin: direct download URL + metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedLink {
    pub url: Url,
    pub filename: Option<String>,
    pub filesize: Option<u64>,
    pub supports_ranges: bool,
    pub additional_headers: HashMap<String, String>,
}

/// Plugin definition: metadata + behavior callbacks.
#[async_trait]
pub trait DownloadPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn can_handle(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        self.meta()
            .domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    async fn resolve(&self, url: Url) -> Result<ResolvedLink>;

    async fn fetch_captcha(&self, _url: &Url) -> Result<Option<CaptchaChallenge>> {
        Ok(None)
    }

    async fn submit_captcha(&self, _url: &Url, _solution: String) -> Result<Option<ResolvedLink>> {
        Ok(None)
    }
}

/// Static registry of all loaded plugins.
#[derive(Clone)]
pub struct PluginRegistry {
    plugins: HashMap<String, Arc<dyn DownloadPlugin>>,
    crawlers: Vec<Arc<dyn CrawlerPlugin>>,
    captchas: Vec<Arc<dyn CaptchaPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            crawlers: Vec::new(),
            captchas: Vec::new(),
        }
    }

    pub fn register(&mut self, plugin: Arc<dyn DownloadPlugin>) {
        let id = plugin.meta().id.clone();
        self.plugins.insert(id, plugin);
    }

    pub fn register_crawler(&mut self, crawler: Arc<dyn CrawlerPlugin>) {
        self.crawlers.push(crawler);
    }

    pub fn register_captcha(&mut self, captcha: Arc<dyn CaptchaPlugin>) {
        self.captchas.push(captcha);
    }

    /// First registered crawler that accepts `url` (registration order = priority).
    pub fn find_crawler_for_url(&self, url: &Url) -> Option<Arc<dyn CrawlerPlugin>> {
        self.crawlers.iter().find(|c| c.can_handle(url)).cloned()
    }

    /// First registered captcha plugin that accepts `url`.
    pub fn find_captcha_for_url(&self, url: &Url) -> Option<Arc<dyn CaptchaPlugin>> {
        self.captchas.iter().find(|c| c.can_handle(url)).cloned()
    }

    pub fn list(&self) -> Vec<PluginMeta> {
        self.plugins
            .values()
            .map(|p| p.meta())
            .chain(self.crawlers.iter().map(|c| c.meta()))
            .chain(self.captchas.iter().map(|c| c.meta()))
            .collect()
    }

    /// Finds the most specific plugin for `url`.
    ///
    /// Plugins with a wildcard domain (`*`, e.g. the direct-download fallback)
    /// are only used when no domain-specific plugin accepts the URL, so the
    /// result does not depend on hash-map iteration order.
    pub fn find_for_url(&self, url: &Url) -> Option<Arc<dyn DownloadPlugin>> {
        let mut fallback = None;
        for plugin in self.plugins.values() {
            if !plugin.can_handle(url) {
                continue;
            }
            let is_fallback = plugin.meta().domains.iter().any(|domain| domain == "*");
            if is_fallback {
                fallback.get_or_insert_with(|| plugin.clone());
            } else {
                return Some(plugin.clone());
            }
        }
        fallback
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

/// Simple direct-download plugin (fallback, works for any URL that is
/// already a direct file – no hoster redirect).
pub struct DirectDownloadPlugin;

#[async_trait::async_trait]
impl DownloadPlugin for DirectDownloadPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "direct".into(),
            name: "Direct Download".into(),
            version: "0.1.0".into(),
            author: "rDownloader".into(),
            description: t!("plugin.direct.description"),
            kind: PluginKind::Hoster,
            domains: vec!["*".into()],
        }
    }

    fn can_handle(&self, _url: &Url) -> bool {
        true // fallback - catches everything
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
    fn specific_plugins_win_over_wildcard_fallback() {
        let mut reg = PluginRegistry::new();
        reg.register(Arc::new(DirectDownloadPlugin));
        reg.register(Arc::new(ExampleRedirectHosterPlugin::new()));
        let url = Url::parse("https://redirector.local/dl/https%3A%2F%2Fcdn.test%2Ff").unwrap();
        let plugin = reg.find_for_url(&url).unwrap();
        assert_eq!(plugin.meta().id, "example-redirect-hoster");

        let direct = Url::parse("https://anywhere.test/file.bin").unwrap();
        assert_eq!(reg.find_for_url(&direct).unwrap().meta().id, "direct");
    }

    #[test]
    fn crawler_registry_lookup() {
        let mut reg = PluginRegistry::new();
        let url = Url::parse("https://x.test/").unwrap();
        assert!(reg.find_crawler_for_url(&url).is_none());
        reg.register_crawler(Arc::new(HtmlLinkCrawlerPlugin));
        assert!(reg.find_crawler_for_url(&url).is_some());
        assert!(reg.list().iter().any(|m| m.kind == PluginKind::Crawler));
    }

    #[test]
    fn captcha_registry_lookup() {
        let mut reg = PluginRegistry::new();
        let url = Url::parse("https://captcha.test/").unwrap();
        assert!(reg.find_captcha_for_url(&url).is_none());
        reg.register_captcha(Arc::new(MathCaptchaPlugin::new(
            vec!["captcha.test".into()],
        )));
        assert!(reg.find_captcha_for_url(&url).is_some());
        assert!(reg.list().iter().any(|m| m.kind == PluginKind::Captcha));
    }
}
