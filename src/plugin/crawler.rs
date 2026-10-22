//! Crawler plugins: find further links on a page and feed them to the link grabber.
//!
//! A crawler only sees the transport (contract A), never the network directly, so it
//! runs through Tor like everything else and is testable with `MockTransport`.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{PluginKind, PluginMeta};
use crate::contract::Transport;

/// Upper bound for a crawled page; larger responses are truncated.
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// A link found on a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawledLink {
    pub url: Url,
    /// Visible link text, if any.
    pub title: Option<String>,
}

#[async_trait]
pub trait CrawlerPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn can_handle(&self, url: &Url) -> bool;

    /// Fetches `url` via `transport` and returns the links worth downloading.
    async fn crawl(&self, url: &Url, transport: &dyn Transport) -> Result<Vec<CrawledLink>>;
}

/// Downloads a page body (up to [`MAX_PAGE_BYTES`]) as lossy UTF-8 text.
pub async fn fetch_page(transport: &dyn Transport, url: &Url, isolation: u64) -> Result<String> {
    let resp = transport
        .get(url, None, isolation)
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !(200..300).contains(&resp.head.status) {
        bail!("unexpected status {} for {url}", resp.head.status);
    }
    let mut body = resp.body;
    let mut data = Vec::new();
    while let Some(chunk) = body.next().await {
        data.extend_from_slice(&chunk?);
        if data.len() >= MAX_PAGE_BYTES {
            data.truncate(MAX_PAGE_BYTES);
            break;
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Extracts `<a href>` links from HTML, resolved against `base`.
/// Only `http`/`https` links are kept; duplicates (ignoring fragments) are dropped.
pub fn extract_links(html: &str, base: &Url) -> Vec<CrawledLink> {
    let lower = html.to_ascii_lowercase();
    let mut links: Vec<CrawledLink> = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let start = pos + rel;
        pos = start + 2;
        // Must be `<a` followed by whitespace (not `<abbr>`, `<area>` …).
        if !lower[pos..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(tag_len) = lower[pos..].find('>') else {
            break;
        };
        let tag_end = pos + tag_len;
        let Some(href) = attr_value(&html[pos..tag_end], &lower[pos..tag_end], "href") else {
            continue;
        };
        let Ok(mut url) = base.join(&decode_entities(&href)) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        url.set_fragment(None);
        if links.iter().any(|l| l.url == url) {
            continue;
        }
        let text_end = lower[tag_end..]
            .find("</a")
            .map_or(html.len(), |i| tag_end + i);
        let title = strip_tags(&html[tag_end + 1..text_end]);
        links.push(CrawledLink {
            url,
            title: (!title.is_empty()).then_some(title),
        });
    }
    links
}

/// Reads the value of attribute `name` from the inside of a tag.
fn attr_value(tag: &str, tag_lower: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = tag_lower[from..].find(name) {
        let at = from + rel;
        from = at + name.len();
        let preceded_ok = at == 0 || tag_lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=').filter(|_| preceded_ok) else {
            continue;
        };
        let rest = rest.trim_start();
        return Some(match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?.to_string(),
            _ => rest
                .split(|c: char| c.is_ascii_whitespace())
                .next()?
                .to_string(),
        });
    }
    None
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Generic crawler: returns every http(s) link of an HTML page.
/// Meant as the reference implementation and as a fallback for the link grabber.
pub struct HtmlLinkCrawlerPlugin;

#[async_trait]
impl CrawlerPlugin for HtmlLinkCrawlerPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "html-link-crawler".into(),
            name: "HTML Link Crawler".into(),
            version: "0.1.0".into(),
            author: "rDownloader".into(),
            description: "Finds all http(s) links on an HTML page".into(),
            kind: PluginKind::Crawler,
            domains: vec!["*".into()],
        }
    }

    fn can_handle(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
    }

    async fn crawl(&self, url: &Url, transport: &dyn Transport) -> Result<Vec<CrawledLink>> {
        let html = fetch_page(transport, url, 0).await?;
        Ok(extract_links(&html, url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{MockResource, MockTransport};

    fn base() -> Url {
        Url::parse("http://x.test/dir/page.html").unwrap()
    }

    #[test]
    fn extracts_and_resolves_links() {
        let html = r#"<p><a href="/a.zip">Archive <b>A</b></a> <A HREF='b.zip#top'>B</A>
            <a class=x href=http://y.test/c.bin>C</a></p>"#;
        let links = extract_links(html, &base());
        let urls: Vec<_> = links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "http://x.test/a.zip",
                "http://x.test/dir/b.zip",
                "http://y.test/c.bin"
            ]
        );
        assert_eq!(links[0].title.as_deref(), Some("Archive A"));
    }

    #[test]
    fn skips_non_http_duplicates_and_lookalike_tags() {
        let html = r#"<a href="mailto:a@b.c">m</a><a href="javascript:void(0)">j</a>
            <abbr href="/no">x</abbr><a href="/d">1</a><a href="/d#frag">2</a><a>no href</a>"#;
        let links = extract_links(html, &base());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url.as_str(), "http://x.test/d");
    }

    #[test]
    fn decodes_entities_in_href() {
        let links = extract_links(r#"<a href="/q?a=1&amp;b=2">q</a>"#, &base());
        assert_eq!(links[0].url.as_str(), "http://x.test/q?a=1&b=2");
    }

    #[test]
    fn malformed_html_does_not_panic() {
        assert!(extract_links("<a href=\"/x", &base()).is_empty());
        assert!(extract_links("<a", &base()).is_empty());
        assert!(extract_links("", &base()).is_empty());
    }

    #[tokio::test]
    async fn crawls_through_transport() {
        let page = MockResource::new(&br#"<a href="/f1">one</a><a href="/f2">two</a>"#[..]);
        let t = MockTransport::new().with_resource("http://x.test/dir/page.html", page);
        let links = HtmlLinkCrawlerPlugin.crawl(&base(), &t).await.unwrap();
        assert_eq!(links.len(), 2);
    }

    #[tokio::test]
    async fn crawl_propagates_transport_errors() {
        let t = MockTransport::new();
        assert!(HtmlLinkCrawlerPlugin.crawl(&base(), &t).await.is_err());
    }

    #[test]
    fn only_http_urls_are_handled() {
        assert!(HtmlLinkCrawlerPlugin.can_handle(&base()));
        assert!(!HtmlLinkCrawlerPlugin.can_handle(&Url::parse("ftp://x.test/").unwrap()));
    }
}
