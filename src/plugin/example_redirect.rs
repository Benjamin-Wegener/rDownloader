use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use url::Url;

use super::{DownloadPlugin, PluginKind, PluginMeta, ResolvedLink};
use crate::t;

/// An example hoster plugin that resolves redirect URLs / forwarding links.
///
/// Handles domains such as `example-hoster.local`, `redirector.local`, `hoster.example.com`.
///
/// Supports typical redirect forms of share hosters:
/// 1. Query parameters such as `?target=...` or `?url=...` or `?download=...`
/// 2. Path-based redirects: `/dl/<target-url>`
/// 3. HTTP redirects via the Location header (when resolved with an HTTP client)
pub struct ExampleRedirectHosterPlugin {
    meta: PluginMeta,
}

impl ExampleRedirectHosterPlugin {
    pub fn new() -> Self {
        Self {
            meta: PluginMeta {
                id: "example-redirect-hoster".into(),
                name: "Example Redirect Hoster".into(),
                version: "0.1.0".into(),
                author: "rdownloader".into(),
                description: t!("plugin.redirect.description"),
                kind: PluginKind::Hoster,
                domains: vec![
                    "example-hoster.local".into(),
                    "redirector.local".into(),
                    "hoster.example.com".into(),
                    "test-hoster.com".into(),
                ],
            },
        }
    }

    /// Helper to extract a target URL from a redirect URL
    pub fn extract_redirect_target(url: &Url) -> Result<Url> {
        // 1. Look for known query parameters ("url", "target", "link", "download", "dest")
        for (key, val) in url.query_pairs() {
            if matches!(
                key.as_ref(),
                "url" | "target" | "link" | "download" | "dest" | "r"
            ) {
                if let Ok(parsed) = Url::parse(&val) {
                    return Ok(parsed);
                }
            }
        }

        // 2. Path-based forwarding, e.g. /dl/https://... or /download/https://...
        let path = url.path();
        for prefix in &["/dl/", "/download/", "/goto/", "/out/"] {
            if let Some(rest) = path.strip_prefix(prefix) {
                if let Ok(parsed) = Url::parse(rest) {
                    return Ok(parsed);
                }
            }
        }

        bail!(t!("plugin.redirect.no_target", url = url))
    }
}

impl Default for ExampleRedirectHosterPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DownloadPlugin for ExampleRedirectHosterPlugin {
    fn meta(&self) -> PluginMeta {
        self.meta.clone()
    }

    async fn resolve(&self, url: Url) -> Result<ResolvedLink> {
        let target_url =
            Self::extract_redirect_target(&url).context(t!("plugin.redirect.resolve_failed"))?;

        let filename = target_url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "download.bin".into());

        let mut headers = HashMap::new();
        headers.insert("Referer".into(), url.to_string());

        Ok(ResolvedLink {
            url: target_url,
            filename: Some(filename),
            filesize: None,
            supports_ranges: true,
            additional_headers: headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_redirect_query_resolution() {
        let plugin = ExampleRedirectHosterPlugin::new();
        let incoming = Url::parse(
            "https://example-hoster.local/download?url=https://storage.example.org/files/archive.zip",
        )
        .unwrap();

        assert!(plugin.can_handle(&incoming));

        let resolved = plugin.resolve(incoming).await.unwrap();
        assert_eq!(
            resolved.url.as_str(),
            "https://storage.example.org/files/archive.zip"
        );
        assert_eq!(resolved.filename.as_deref(), Some("archive.zip"));
        assert!(resolved.supports_ranges);
        assert_eq!(
            resolved.additional_headers.get("Referer").map(|s| s.as_str()),
            Some("https://example-hoster.local/download?url=https://storage.example.org/files/archive.zip")
        );
    }

    #[tokio::test]
    async fn test_redirect_target_param() {
        let plugin = ExampleRedirectHosterPlugin::new();
        let incoming =
            Url::parse("https://redirector.local/get?target=https://cdn.test.net/video.mp4")
                .unwrap();

        assert!(plugin.can_handle(&incoming));

        let resolved = plugin.resolve(incoming).await.unwrap();
        assert_eq!(resolved.url.as_str(), "https://cdn.test.net/video.mp4");
        assert_eq!(resolved.filename.as_deref(), Some("video.mp4"));
    }

    #[tokio::test]
    async fn test_redirect_path_based() {
        let plugin = ExampleRedirectHosterPlugin::new();
        let incoming =
            Url::parse("https://hoster.example.com/dl/https://origin.example.com/data.tar.gz")
                .unwrap();

        assert!(plugin.can_handle(&incoming));

        let resolved = plugin.resolve(incoming).await.unwrap();
        assert_eq!(
            resolved.url.as_str(),
            "https://origin.example.com/data.tar.gz"
        );
        assert_eq!(resolved.filename.as_deref(), Some("data.tar.gz"));
    }

    #[tokio::test]
    async fn test_unsupported_host_or_missing_target() {
        let plugin = ExampleRedirectHosterPlugin::new();
        let external = Url::parse("https://other-domain.org/file.zip").unwrap();
        assert!(!plugin.can_handle(&external));

        let invalid_target = Url::parse("https://example-hoster.local/dl/not-a-valid-url").unwrap();
        let res = plugin.resolve(invalid_target).await;
        assert!(res.is_err());
    }
}
