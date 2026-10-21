//! `~/.netrc` parsing for host credentials.
//!
//! Supports the standard netrc grammar: `machine`, `default`, `login`,
//! `password`, `account`, and `macdef` (macro bodies are skipped). Host names
//! are matched case-insensitively; a `default` entry is used as a fallback.
//!
//! Passwords are stored as [`Secret`] and never appear in `Debug` output or
//! logs. [`Netrc::load`] warns when a Unix netrc file is readable by other users.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::account::{Credentials, Secret};

/// One `machine`/`default` block of a netrc file.
#[derive(Debug, Clone)]
pub struct NetrcEntry {
    /// `None` for the `default` entry.
    pub machine: Option<String>,
    pub login: String,
    pub password: Secret,
    pub account: Option<String>,
}

impl NetrcEntry {
    pub fn credentials(&self) -> Credentials {
        Credentials {
            username: self.login.clone(),
            password: self.password.clone(),
        }
    }

    fn matches(&self, host: &str) -> bool {
        self.machine
            .as_deref()
            .is_some_and(|machine| machine.eq_ignore_ascii_case(host))
    }
}

/// A parsed netrc file (in declaration order).
#[derive(Debug, Clone, Default)]
pub struct Netrc {
    entries: Vec<NetrcEntry>,
}

impl Netrc {
    pub fn parse(text: &str) -> Result<Self> {
        let mut parser = Parser::new();
        for (index, raw_line) in text.lines().enumerate() {
            parser.line(raw_line, index + 1)?;
        }
        parser.finish()
    }

    pub fn load(path: &Path) -> Result<Self> {
        warn_if_world_readable(path);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading netrc file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing netrc file {}", path.display()))
    }

    /// Loads `$NETRC` or `~/.netrc`; returns `Ok(None)` when the file is absent.
    pub fn load_default() -> Result<Option<Self>> {
        let Some(path) = default_netrc_path() else {
            return Ok(None);
        };
        if !path.is_file() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }

    pub fn entries(&self) -> &[NetrcEntry] {
        &self.entries
    }

    /// Exact machine match first, then the (last declared) `default` entry.
    pub fn entry_for(&self, host: &str) -> Option<&NetrcEntry> {
        let host = normalize_host(host);
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.matches(&host))
            .or_else(|| {
                self.entries
                    .iter()
                    .rev()
                    .find(|entry| entry.machine.is_none())
            })
    }

    pub fn credentials_for(&self, host: &str) -> Option<Credentials> {
        self.entry_for(host).map(NetrcEntry::credentials)
    }
}

fn default_netrc_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NETRC") {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join(".netrc"))
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                "netrc file is readable by other users; consider chmod 600"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[derive(Clone, Copy)]
enum Field {
    Machine,
    Login,
    Password,
    Account,
    Macdef,
}

struct EntryBuilder {
    machine: Option<String>,
    is_default: bool,
    login: Option<String>,
    password: Option<Secret>,
    account: Option<String>,
}

impl EntryBuilder {
    fn new(machine: Option<String>) -> Self {
        Self {
            is_default: machine.is_none(),
            machine,
            login: None,
            password: None,
            account: None,
        }
    }

    fn finish(self, line: usize) -> Result<NetrcEntry> {
        let label = if self.is_default {
            "default".to_string()
        } else {
            format!("machine {}", self.machine.as_deref().unwrap_or(""))
        };
        let Some(login) = self.login else {
            bail!("netrc {label} (line {line}) has no login");
        };
        let Some(password) = self.password else {
            bail!("netrc {label} (line {line}) has no password");
        };
        Ok(NetrcEntry {
            machine: self.machine,
            login,
            password,
            account: self.account,
        })
    }
}

struct Parser {
    entries: Vec<NetrcEntry>,
    current: Option<EntryBuilder>,
    pending: Option<Field>,
    in_macdef: bool,
    last_line: usize,
}

impl Parser {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            pending: None,
            in_macdef: false,
            last_line: 0,
        }
    }

    fn line(&mut self, raw_line: &str, line_no: usize) -> Result<()> {
        self.last_line = line_no;
        if self.in_macdef {
            if raw_line.trim().is_empty() {
                self.in_macdef = false;
            }
            return Ok(());
        }
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(());
        }
        let tokens = tokenize(line).with_context(|| format!("netrc line {line_no}"))?;
        for token in tokens {
            if let Some(field) = self.pending.take() {
                self.take_value(field, &token, line_no)?;
                if self.in_macdef {
                    break;
                }
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "machine" => self.pending = Some(Field::Machine),
                "default" => {
                    self.close_current(line_no)?;
                    self.current = Some(EntryBuilder::new(None));
                }
                "login" => self.pending = Some(Field::Login),
                "password" => self.pending = Some(Field::Password),
                "account" => self.pending = Some(Field::Account),
                "macdef" => self.pending = Some(Field::Macdef),
                other => bail!("netrc line {line_no}: unexpected token {other:?}"),
            }
        }
        Ok(())
    }

    fn take_value(&mut self, field: Field, token: &str, line_no: usize) -> Result<()> {
        match field {
            Field::Machine => {
                self.close_current(line_no)?;
                self.current = Some(EntryBuilder::new(Some(token.to_string())));
            }
            Field::Login => self.entry_mut(line_no)?.login = Some(token.to_string()),
            Field::Password => self.entry_mut(line_no)?.password = Some(Secret::new(token)),
            Field::Account => self.entry_mut(line_no)?.account = Some(token.to_string()),
            Field::Macdef => self.in_macdef = true,
        }
        Ok(())
    }

    fn entry_mut(&mut self, line_no: usize) -> Result<&mut EntryBuilder> {
        self.current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("netrc line {line_no}: value without machine/default"))
    }

    fn close_current(&mut self, line_no: usize) -> Result<()> {
        if let Some(builder) = self.current.take() {
            let entry = builder.finish(line_no)?;
            self.entries.push(entry);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Netrc> {
        if self.pending.is_some() {
            bail!("netrc ends with a keyword missing its value");
        }
        self.close_current(self.last_line)?;
        Ok(Netrc {
            entries: self.entries,
        })
    }
}

/// Splits a netrc line into tokens, supporting quotes and backslash escapes.
fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    let mut has_token = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                has_token = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if quoted {
        bail!("unterminated quote");
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETRC: &str = r#"
# comment
machine hoster.test
  login alice
  password s3cret

default login anonymous password anon@example.com

machine other.test login bob password "p a s s" account bob-account

macdef init
  put file
  bye

"#;

    #[test]
    fn parses_entries_and_default() {
        let netrc = Netrc::parse(NETRC).unwrap();
        assert_eq!(netrc.entries().len(), 3);

        let creds = netrc.credentials_for("hoster.test").unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password.expose(), "s3cret");

        let other = netrc.entry_for("OTHER.TEST.").unwrap();
        assert_eq!(other.login, "bob");
        assert_eq!(other.password.expose(), "p a s s");
        assert_eq!(other.account.as_deref(), Some("bob-account"));

        let unknown = netrc.credentials_for("unknown.test").unwrap();
        assert_eq!(unknown.username, "anonymous");
    }

    #[test]
    fn exact_machine_beats_default() {
        let netrc =
            Netrc::parse("default login d password d\nmachine x.test login x password y").unwrap();
        assert_eq!(netrc.credentials_for("x.test").unwrap().username, "x");
        assert_eq!(netrc.credentials_for("z.test").unwrap().username, "d");
    }

    #[test]
    fn later_entry_wins() {
        let netrc = Netrc::parse(
            "machine x.test login old password a\nmachine x.test login new password b",
        )
        .unwrap();
        assert_eq!(netrc.credentials_for("x.test").unwrap().username, "new");
    }

    #[test]
    fn missing_password_is_rejected() {
        let error = Netrc::parse("machine x.test login alice").unwrap_err();
        assert!(error.to_string().contains("no password"));
    }

    #[test]
    fn dangling_keyword_is_rejected() {
        let error = Netrc::parse("machine x.test login").unwrap_err();
        assert!(error.to_string().contains("missing its value"));
    }

    #[test]
    fn unexpected_token_is_rejected() {
        let error = Netrc::parse("machine x.test passwordy nope").unwrap_err();
        assert!(error.to_string().contains("unexpected token"));
    }

    #[test]
    fn unterminated_quote_is_rejected() {
        let error = Netrc::parse("machine x.test login \"alice").unwrap_err();
        assert!(format!("{error:#}").to_lowercase().contains("quote"));
    }

    #[test]
    fn secrets_are_redacted() {
        let netrc = Netrc::parse("machine x.test login alice password topsecret").unwrap();
        assert!(!format!("{netrc:?}").contains("topsecret"));
        assert!(!format!("{:?}", netrc.entries()[0]).contains("topsecret"));
    }

    #[test]
    fn basic_authorization_header() {
        let creds = Credentials {
            username: "user".into(),
            password: Secret::new("pass"),
        };
        assert_eq!(creds.basic_authorization(), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn missing_default_file_is_not_an_error() {
        let path = std::env::temp_dir().join(format!("rdl-netrc-{}", uuid::Uuid::new_v4()));
        // Point NETRC at a non-existent path for this check only.
        let previous = std::env::var_os("NETRC");
        std::env::set_var("NETRC", &path);
        let result = Netrc::load_default();
        match previous {
            Some(value) => std::env::set_var("NETRC", value),
            None => std::env::remove_var("NETRC"),
        }
        assert!(result.unwrap().is_none());
    }
}
