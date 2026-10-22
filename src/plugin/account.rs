//! Account plugins: log in to a hoster and provide session cookies for premium downloads.
//!
//! Secrets are never serialized and never printed: [`Secret`] and [`Cookie`] redact
//! their values in `Debug`, and neither [`Credentials`] nor [`AccountSession`] derives
//! `Serialize`. Persisting sessions to disk is a separate, deliberate step.

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use url::Url;

use super::cookies::{CookieImportReport, ImportedCookie};
use super::PluginMeta;
use crate::contract::Transport;

/// A string that must not leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to read the value; call sites are easy to grep for.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: Secret,
}

impl Credentials {
    /// `Authorization: Basic ...` header value for these credentials.
    pub fn basic_authorization(&self) -> String {
        use base64::Engine as _;
        let raw = format!("{}:{}", self.username, self.password.expose());
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: Secret,
    /// Host the cookie applies to (`example.com` also covers `dl.example.com`).
    pub domain: String,
    /// Request path the cookie is scoped to (`/` by default).
    pub path: String,
    /// Only send over HTTPS.
    pub secure: bool,
    /// Browser-only flag; kept so imports round-trip the source data.
    pub http_only: bool,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cookie {{ name: {:?}, domain: {:?}, path: {:?}, value: *** }}",
            self.name, self.domain, self.path
        )
    }
}

/// Result of a successful login.
#[derive(Debug, Clone)]
pub struct AccountSession {
    pub cookies: Vec<Cookie>,
    pub premium: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

impl AccountSession {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    /// `Cookie` header value for `url`, or `None` if no cookie matches its host.
    pub fn cookie_header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?;
        let https = url.scheme() == "https";
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| host == c.domain || host.ends_with(&format!(".{}", c.domain)))
            .filter(|c| !c.secure || https)
            .filter(|c| path_matches(&c.path, url.path()))
            .map(|c| format!("{}={}", c.name, c.value.expose()))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }
}

/// RFC 6265 path matching (`/` matches everything, `/dl` covers `/dl/x`).
fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/')
        || request_path.as_bytes().get(cookie_path.len()).copied() == Some(b'/')
}

#[async_trait]
pub trait AccountPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn can_handle(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        self.meta()
            .domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    async fn login(&self, creds: &Credentials, transport: &dyn Transport)
        -> Result<AccountSession>;
}

/// Keeps live sessions in memory, keyed by plugin id and username.
#[derive(Default)]
pub struct AccountManager {
    sessions: HashMap<(String, String), AccountSession>,
}

impl AccountManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn login(
        &mut self,
        plugin: &dyn AccountPlugin,
        creds: &Credentials,
        transport: &dyn Transport,
    ) -> Result<()> {
        let session = plugin.login(creds, transport).await?;
        self.sessions
            .insert((plugin.meta().id, creds.username.clone()), session);
        Ok(())
    }

    /// Registers an already imported session (e.g. browser cookies) without a login round-trip.
    pub fn insert_session(&mut self, plugin_id: &str, username: &str, session: AccountSession) {
        self.sessions
            .insert((plugin_id.to_string(), username.to_string()), session);
    }

    pub fn logout(&mut self, plugin_id: &str, username: &str) -> bool {
        self.sessions
            .remove(&(plugin_id.to_string(), username.to_string()))
            .is_some()
    }

    /// Headers to add to a download of `url`; expired sessions are skipped.
    pub fn headers_for(&self, url: &Url, now: DateTime<Utc>) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        let cookie = self
            .sessions
            .values()
            .filter(|s| !s.is_expired(now))
            .find_map(|s| s.cookie_header_for(url));
        if let Some(cookie) = cookie {
            headers.insert("Cookie".to_string(), cookie);
        }
        headers
    }

    /// Drops expired sessions; returns how many were removed.
    pub fn purge_expired(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| !s.is_expired(now));
        before - self.sessions.len()
    }
}

/// Account plugin for users who already have a logged-in browser session:
/// the "password" is a raw cookie string (`a=1; b=2`) copied from the browser.
/// Works for any hoster without site-specific login code.
pub struct CookieImportAccountPlugin {
    domains: Vec<String>,
}

impl CookieImportAccountPlugin {
    pub fn new(domains: Vec<String>) -> Self {
        Self { domains }
    }

    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// Builds a session from imported browser cookies, keeping only cookies that
    /// match one of this plugin's domains (an empty domain list imports all).
    pub fn session_from_report(&self, report: &CookieImportReport) -> Result<AccountSession> {
        self.session_from_imported(&report.cookies)
    }

    /// Builds a session from already parsed cookies.
    pub fn session_from_imported(&self, imported: &[ImportedCookie]) -> Result<AccountSession> {
        let mut cookies = Vec::new();
        let mut expires_at: Option<DateTime<Utc>> = None;
        for cookie in imported
            .iter()
            .filter(|cookie| self.accepts_domain(&cookie.domain))
        {
            cookies.push(Cookie::from(cookie));
            if let Some(expiry) = cookie.expires_at {
                expires_at = Some(expires_at.map_or(expiry, |current| current.min(expiry)));
            }
        }
        if cookies.is_empty() {
            bail!("no imported cookies match the plugin domains");
        }
        Ok(AccountSession {
            cookies,
            premium: true,
            expires_at,
        })
    }

    /// Reads a Netscape cookie file or a browser cookie database and imports it.
    pub fn login_from_cookie_file(&self, path: &std::path::Path) -> Result<AccountSession> {
        let report = super::cookies::import_cookie_file(path)?;
        self.session_from_report(&report)
    }

    fn accepts_domain(&self, domain: &str) -> bool {
        self.domains.is_empty()
            || self
                .domains
                .iter()
                .any(|allowed| domain == allowed || domain.ends_with(&format!(".{allowed}")))
    }
}

impl From<&ImportedCookie> for Cookie {
    fn from(imported: &ImportedCookie) -> Self {
        Self {
            name: imported.name.clone(),
            value: imported.value.clone(),
            domain: imported.domain.clone(),
            path: imported.path.clone(),
            secure: imported.secure,
            http_only: imported.http_only,
        }
    }
}

#[async_trait]
impl AccountPlugin for CookieImportAccountPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "cookie-import".into(),
            name: "Cookie Import".into(),
            version: "0.1.0".into(),
            author: "rDownloader".into(),
            description: "Uses cookies copied from a logged-in browser session".into(),
            kind: super::PluginKind::Account,
            domains: self.domains.clone(),
        }
    }

    async fn login(
        &self,
        creds: &Credentials,
        _transport: &dyn Transport,
    ) -> Result<AccountSession> {
        let cookies: Vec<Cookie> = creds
            .password
            .expose()
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .filter(|(name, _)| !name.trim().is_empty())
            .flat_map(|(name, value)| {
                self.domains.iter().map(move |d| Cookie {
                    name: name.trim().to_string(),
                    value: Secret::new(value.trim()),
                    domain: d.clone(),
                    path: "/".into(),
                    secure: false,
                    http_only: false,
                })
            })
            .collect();
        if cookies.is_empty() {
            bail!("no cookies found in the supplied cookie string");
        }
        Ok(AccountSession {
            cookies,
            premium: true,
            expires_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::MockTransport;
    use chrono::Duration;

    fn creds(cookie_string: &str) -> Credentials {
        Credentials {
            username: "me".into(),
            password: Secret::new(cookie_string),
        }
    }

    fn plugin() -> CookieImportAccountPlugin {
        CookieImportAccountPlugin::new(vec!["hoster.test".into()])
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let c = creds("sid=topsecret");
        assert!(!format!("{c:?}").contains("topsecret"));
        let cookie = Cookie {
            name: "sid".into(),
            value: Secret::new("topsecret"),
            domain: "x".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
        };
        assert!(!format!("{cookie:?}").contains("topsecret"));
    }

    #[tokio::test]
    async fn imports_cookies() {
        let s = plugin()
            .login(&creds("sid=abc; theme = dark ;;"), &MockTransport::new())
            .await
            .unwrap();
        let url = Url::parse("https://dl.hoster.test/file").unwrap();
        assert_eq!(
            s.cookie_header_for(&url).as_deref(),
            Some("sid=abc; theme=dark")
        );
    }

    #[tokio::test]
    async fn rejects_empty_cookie_string() {
        assert!(plugin()
            .login(&creds(" ; garbage"), &MockTransport::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn cookies_only_for_matching_hosts() {
        let s = plugin()
            .login(&creds("sid=abc"), &MockTransport::new())
            .await
            .unwrap();
        assert!(s
            .cookie_header_for(&Url::parse("https://evilhoster.test/").unwrap())
            .is_none());
        assert!(s
            .cookie_header_for(&Url::parse("https://other.test/").unwrap())
            .is_none());
    }

    #[test]
    fn imported_session_filters_domains() {
        let imported = vec![
            ImportedCookie {
                name: "sid".into(),
                value: Secret::new("abc"),
                domain: "hoster.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires_at: None,
            },
            ImportedCookie {
                name: "foreign".into(),
                value: Secret::new("xyz"),
                domain: "other.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires_at: None,
            },
        ];
        let session = plugin().session_from_imported(&imported).unwrap();
        assert_eq!(session.cookies.len(), 1);
        assert_eq!(session.cookies[0].domain, "hoster.test");

        let all = CookieImportAccountPlugin::new(vec![])
            .session_from_imported(&imported)
            .unwrap();
        assert_eq!(all.cookies.len(), 2);
    }

    #[test]
    fn imported_session_uses_earliest_expiry() {
        let soon = Utc::now() + Duration::seconds(60);
        let later = Utc::now() + Duration::seconds(600);
        let imported = vec![
            ImportedCookie {
                name: "sid".into(),
                value: Secret::new("abc"),
                domain: "hoster.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires_at: Some(later),
            },
            ImportedCookie {
                name: "sid2".into(),
                value: Secret::new("def"),
                domain: "hoster.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires_at: Some(soon),
            },
        ];
        let session = plugin().session_from_imported(&imported).unwrap();
        assert_eq!(session.expires_at, Some(soon));
    }

    #[test]
    fn hits_respect_path_and_secure_flag() {
        let session = AccountSession {
            cookies: vec![
                Cookie {
                    name: "path".into(),
                    value: Secret::new("1"),
                    domain: "hoster.test".into(),
                    path: "/download".into(),
                    secure: false,
                    http_only: true,
                },
                Cookie {
                    name: "secure".into(),
                    value: Secret::new("2"),
                    domain: "hoster.test".into(),
                    path: "/".into(),
                    secure: true,
                    http_only: false,
                },
            ],
            premium: true,
            expires_at: None,
        };
        let ok = Url::parse("https://hoster.test/download/file").unwrap();
        assert_eq!(
            session.cookie_header_for(&ok).as_deref(),
            Some("path=1; secure=2")
        );
        let wrong_path = Url::parse("https://hoster.test/downloads/file").unwrap();
        assert_eq!(
            session.cookie_header_for(&wrong_path).as_deref(),
            Some("secure=2")
        );
        let plain_http = Url::parse("http://hoster.test/download/file").unwrap();
        assert_eq!(
            session.cookie_header_for(&plain_http).as_deref(),
            Some("path=1")
        );
    }

    #[test]
    fn login_from_netscape_file() {
        let dir = std::env::temp_dir().join(format!("rdl-cookies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cookies.txt");
        std::fs::write(
            &file,
            ".hoster.test\tTRUE\t/\tFALSE\t0\tsid\tabc\n.other.test\tTRUE\t/\tFALSE\t0\tx\ty\n",
        )
        .unwrap();
        let session = plugin().login_from_cookie_file(&file).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(session.cookies.len(), 1);
        assert_eq!(session.cookies[0].value.expose(), "abc");
    }

    #[test]
    fn manager_accepts_imported_session() {
        let mut mgr = AccountManager::new();
        let session = AccountSession {
            cookies: vec![Cookie {
                name: "sid".into(),
                value: Secret::new("abc"),
                domain: "hoster.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
            }],
            premium: true,
            expires_at: None,
        };
        mgr.insert_session("cookie-import", "me", session);
        let url = Url::parse("https://hoster.test/f").unwrap();
        assert_eq!(
            mgr.headers_for(&url, Utc::now())
                .get("Cookie")
                .map(String::as_str),
            Some("sid=abc")
        );
    }

    #[tokio::test]
    async fn manager_provides_headers_and_expires() {
        let mut mgr = AccountManager::new();
        let t = MockTransport::new();
        mgr.login(&plugin(), &creds("sid=abc"), &t).await.unwrap();
        let url = Url::parse("https://hoster.test/f").unwrap();
        let now = Utc::now();
        assert_eq!(
            mgr.headers_for(&url, now).get("Cookie").map(String::as_str),
            Some("sid=abc")
        );

        // Replace with an already-expired session.
        mgr.sessions
            .values_mut()
            .for_each(|s| s.expires_at = Some(now - Duration::seconds(1)));
        assert!(mgr.headers_for(&url, now).is_empty());
        assert_eq!(mgr.purge_expired(now), 1);
        assert!(!mgr.logout("cookie-import", "me"));
    }

    #[tokio::test]
    async fn logout_removes_session() {
        let mut mgr = AccountManager::new();
        mgr.login(&plugin(), &creds("sid=abc"), &MockTransport::new())
            .await
            .unwrap();
        assert!(mgr.logout("cookie-import", "me"));
        let url = Url::parse("https://hoster.test/f").unwrap();
        assert!(mgr.headers_for(&url, Utc::now()).is_empty());
    }
}
