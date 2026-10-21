//! Plugin permission model and capability declarations.
//!
//! Enforces least-privilege sandboxing for plugins:
//! - Domain-scoped network access (contract A Transport only)
//! - Directory-scoped filesystem access
//! - Environment/secret access permissions

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use url::Url;

/// Granular permissions granted to a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PluginPermissions {
    /// Network domains the plugin is allowed to send requests to.
    /// Wildcard "*" allows any domain (requires explicit user confirmation).
    pub network_domains: HashSet<String>,

    /// Filesystem directory paths the plugin can read from.
    pub read_paths: HashSet<PathBuf>,

    /// Filesystem directory paths the plugin can write to.
    pub write_paths: HashSet<PathBuf>,

    /// Whether the plugin is allowed to access stored credentials / account sessions.
    pub allow_account_access: bool,

    /// Whether the plugin is allowed to prompt interactive Captcha dialogs in UI.
    pub allow_interactive_captcha: bool,
}

impl PluginPermissions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder method to allow network access to a domain.
    pub fn with_network_domain(mut self, domain: impl Into<String>) -> Self {
        self.network_domains.insert(domain.into());
        self
    }

    /// Builder method to grant read access to a directory.
    pub fn with_read_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_paths.insert(path.into());
        self
    }

    /// Builder method to grant write access to a directory.
    pub fn with_write_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.write_paths.insert(path.into());
        self
    }

    /// Builder method to permit account credentials access.
    pub fn with_account_access(mut self, allow: bool) -> Self {
        self.allow_account_access = allow;
        self
    }

    /// Builder method to permit interactive Captcha challenges.
    pub fn with_interactive_captcha(mut self, allow: bool) -> Self {
        self.allow_interactive_captcha = allow;
        self
    }

    /// Checks if network requests to `url` are permitted.
    pub fn check_network(&self, url: &Url) -> Result<(), PermissionDenied> {
        if self.network_domains.contains("*") {
            return Ok(());
        }

        let host = url.host_str().unwrap_or("");
        let matches = self
            .network_domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")));

        if matches {
            Ok(())
        } else {
            Err(PermissionDenied::NetworkAccessDenied(host.to_string()))
        }
    }

    /// Checks if read access to `path` is permitted.
    pub fn check_read(&self, path: &Path) -> Result<(), PermissionDenied> {
        let is_allowed = self
            .read_paths
            .iter()
            .any(|allowed| path.starts_with(allowed));

        if is_allowed {
            Ok(())
        } else {
            Err(PermissionDenied::FileReadDenied(path.to_path_buf()))
        }
    }

    /// Checks if write access to `path` is permitted.
    pub fn check_write(&self, path: &Path) -> Result<(), PermissionDenied> {
        let is_allowed = self
            .write_paths
            .iter()
            .any(|allowed| path.starts_with(allowed));

        if is_allowed {
            Ok(())
        } else {
            Err(PermissionDenied::FileWriteDenied(path.to_path_buf()))
        }
    }

    /// Checks if account access is permitted.
    pub fn check_account_access(&self) -> Result<(), PermissionDenied> {
        if self.allow_account_access {
            Ok(())
        } else {
            Err(PermissionDenied::AccountAccessDenied)
        }
    }

    /// Checks if captcha prompt is permitted.
    pub fn check_captcha_prompt(&self) -> Result<(), PermissionDenied> {
        if self.allow_interactive_captcha {
            Ok(())
        } else {
            Err(PermissionDenied::CaptchaPromptDenied)
        }
    }
}

/// Errors raised when a plugin attempts an unpermitted operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(clippy::enum_variant_names)]
pub enum PermissionDenied {
    #[error("Plugin attempted network request to unauthorized host: {0}")]
    NetworkAccessDenied(String),

    #[error("Plugin attempted unauthorized filesystem read from: {0:?}")]
    FileReadDenied(PathBuf),

    #[error("Plugin attempted unauthorized filesystem write to: {0:?}")]
    FileWriteDenied(PathBuf),

    #[error("Plugin attempted to access account credentials without permission")]
    AccountAccessDenied,

    #[error("Plugin attempted to trigger an interactive Captcha prompt without permission")]
    CaptchaPromptDenied,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_domain_whitelist() {
        let perms = PluginPermissions::new()
            .with_network_domain("example.com")
            .with_network_domain("cdn.test");

        let ok_url1 = Url::parse("https://example.com/file").unwrap();
        let ok_url2 = Url::parse("https://sub.example.com/file").unwrap();
        let ok_url3 = Url::parse("https://cdn.test/item").unwrap();
        let bad_url = Url::parse("https://evil.org/steal").unwrap();

        assert!(perms.check_network(&ok_url1).is_ok());
        assert!(perms.check_network(&ok_url2).is_ok());
        assert!(perms.check_network(&ok_url3).is_ok());
        assert_eq!(
            perms.check_network(&bad_url),
            Err(PermissionDenied::NetworkAccessDenied("evil.org".into()))
        );
    }

    #[test]
    fn network_wildcard_permission() {
        let perms = PluginPermissions::new().with_network_domain("*");
        let any_url = Url::parse("https://anything.xyz/path").unwrap();
        assert!(perms.check_network(&any_url).is_ok());
    }

    #[test]
    fn filesystem_read_write_boundaries() {
        let allowed_dir = PathBuf::from("/tmp/rdl-sandbox");
        let perms = PluginPermissions::new()
            .with_read_path(&allowed_dir)
            .with_write_path(&allowed_dir);

        let safe_file = allowed_dir.join("temp.txt");
        let unsafe_file = PathBuf::from("/etc/passwd");

        assert!(perms.check_read(&safe_file).is_ok());
        assert!(perms.check_write(&safe_file).is_ok());

        assert_eq!(
            perms.check_read(&unsafe_file),
            Err(PermissionDenied::FileReadDenied(unsafe_file.clone()))
        );
        assert_eq!(
            perms.check_write(&unsafe_file),
            Err(PermissionDenied::FileWriteDenied(unsafe_file))
        );
    }

    #[test]
    fn capability_flags() {
        let mut perms = PluginPermissions::new();
        assert_eq!(
            perms.check_account_access(),
            Err(PermissionDenied::AccountAccessDenied)
        );
        assert_eq!(
            perms.check_captcha_prompt(),
            Err(PermissionDenied::CaptchaPromptDenied)
        );

        perms = perms
            .with_account_access(true)
            .with_interactive_captcha(true);
        assert!(perms.check_account_access().is_ok());
        assert!(perms.check_captcha_prompt().is_ok());
    }
}
