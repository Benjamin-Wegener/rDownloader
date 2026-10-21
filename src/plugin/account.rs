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

#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: Secret,
    /// Host the cookie applies to (`example.com` also covers `dl.example.com`).
    pub domain: String,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cookie {{ name: {:?}, domain: {:?}, value: *** }}",
            self.name, self.domain
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
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| host == c.domain || host.ends_with(&format!(".{}", c.domain)))
            .map(|c| format!("{}={}", c.name, c.value.expose()))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }
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
}

#[async_trait]
impl AccountPlugin for CookieImportAccountPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "cookie-import".into(),
            name: "Cookie Import".into(),
            version: "0.1.0".into(),
            author: "rdownloader".into(),
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
