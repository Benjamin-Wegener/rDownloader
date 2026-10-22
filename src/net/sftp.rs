//! SFTP client over Tor streams, exposed as a [`Transport`].
//!
//! Every connection goes through the same [`StreamProvider`] as HTTP, so the
//! host name is resolved and the connection is opened inside Tor. The SSH
//! handshake, password/key authentication and the SFTP subsystem run on top of
//! that stream.
//!
//! Security notes:
//! - **Host keys are mandatory.** A connection is refused when the server key is
//!   missing from `~/.ssh/known_hosts` or differs from the recorded one, so a
//!   Tor exit cannot impersonate the server. The file is read with
//!   [`russh::keys::check_known_hosts_path`]; host wildcards and revoked keys
//!   are not supported and therefore count as unknown (fail closed).
//! - Passwords in the URL are not logged. Error messages name the verb, the
//!   host and the user, never the secret.
//! - Unencrypted private keys from `~/.ssh` are used when the URL carries no
//!   password; encrypted keys are skipped because there is no passphrase
//!   prompt in a download job.
//! - Random access is native to SFTP, so byte ranges (resume, multi-part)
//!   seek on the server instead of re-reading the file.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use russh::client::{self, Handle};
use russh::keys::{
    check_known_hosts_path, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;
use url::Url;

use super::StreamProvider;
use crate::contract::{ByteRange, Response, ResponseHead, Transport};
use crate::t;
use crate::tor::TorManager;

const DATA_CHUNK: usize = 64 * 1024;
const KEY_FILES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];

/// Where and how to log in, taken from an `sftp://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub path: String,
}

fn decode(s: &str) -> Result<String> {
    Ok(percent_encoding::percent_decode_str(s)
        .decode_utf8()?
        .into_owned())
}

fn check_path_safe(value: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("SFTP path contains a NUL byte");
    }
    Ok(())
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

pub fn parse_target(url: &Url) -> Result<SftpTarget> {
    if url.scheme() != "sftp" {
        bail!(t!("net.error.sftp_target", url = url.as_str()));
    }
    let host = url
        .host_str()
        .context(t!("net.error.url_without_host"))?
        .to_string();
    let user = match url.username() {
        "" => default_user(),
        u => decode(u)?,
    };
    if user.is_empty() {
        bail!(t!("net.error.sftp_no_user"));
    }
    let password = match url.password() {
        Some(p) => Some(decode(p)?),
        None => None,
    };
    let path = decode(url.path())?;
    check_path_safe(&path)?;
    Ok(SftpTarget {
        host,
        port: url.port().unwrap_or(22),
        user,
        password,
        path,
    })
}

/// File name for a URL path (last non-empty segment), decoded.
fn file_name(path: &str) -> Option<String> {
    path.rsplit('/').find(|s| !s.is_empty()).map(str::to_string)
}

/// `~/.ssh/known_hosts` (or `.ssh/known_hosts` when the home directory is unknown).
pub fn default_known_hosts() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("known_hosts")
}

/// Failure while establishing the SSH connection.
#[derive(Debug, thiserror::Error)]
enum SshClientError {
    /// The server key is missing from known_hosts or does not match. Kept
    /// separate so the message reaches the user without a generic wrapper.
    #[error("{0}")]
    HostKey(String),
    #[error(transparent)]
    Transport(#[from] russh::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// SSH client handler that enforces the known-hosts entry for this target.
struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl client::Handler for HostKeyCheck {
    type Error = SshClientError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            return Err(SshClientError::HostKey(t!(
                "net.error.sftp_certificate",
                host = &self.host,
                port = self.port
            )));
        };
        match check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Ok(false) => Err(SshClientError::HostKey(t!(
                "net.error.sftp_unknown_host_key",
                host = &self.host,
                port = self.port,
                path = self.known_hosts.display()
            ))),
            Err(error) => Err(SshClientError::HostKey(t!(
                "net.error.sftp_changed_host_key",
                host = &self.host,
                port = self.port,
                detail = error
            ))),
        }
    }
}

pub struct SftpTransport {
    provider: Arc<dyn StreamProvider>,
    known_hosts: PathBuf,
}

impl SftpTransport {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self::with_provider(tor, default_known_hosts())
    }

    pub fn with_provider(provider: Arc<dyn StreamProvider>, known_hosts: PathBuf) -> Self {
        Self {
            provider,
            known_hosts,
        }
    }

    async fn connect(&self, t: &SftpTarget, isolation: u64) -> Result<Handle<HostKeyCheck>> {
        let stream = self
            .provider
            .connect(isolation, &t.host, t.port)
            .await
            .with_context(|| t!("net.error.stream", host = &t.host, port = t.port))?;
        let handler = HostKeyCheck {
            host: t.host.clone(),
            port: t.port,
            known_hosts: self.known_hosts.clone(),
        };
        let session = client::connect_stream(Arc::new(client::Config::default()), stream, handler)
            .await
            .map_err(|error| match error {
                SshClientError::HostKey(message) => anyhow::anyhow!(message),
                SshClientError::Transport(error) => anyhow::Error::new(error).context(t!(
                    "net.error.sftp_connect",
                    host = &t.host,
                    port = t.port
                )),
                SshClientError::Other(error) => error,
            })?;
        Ok(session)
    }

    async fn authenticate(&self, session: &mut Handle<HostKeyCheck>, t: &SftpTarget) -> Result<()> {
        if let Some(password) = &t.password {
            let result = session
                .authenticate_password(&t.user, password.as_str())
                .await
                .context(t!("net.error.sftp_auth_transport"))?;
            if result.success() {
                return Ok(());
            }
            bail!(t!("net.error.sftp_auth", user = &t.user, host = &t.host));
        }

        let known_hosts = self.known_hosts.clone();
        let mut tried = false;
        for path in key_paths() {
            if !path.exists() {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| t!("net.error.sftp_key_read", path = path.display()))?;
            let key = match PrivateKey::from_openssh(&contents) {
                Ok(key) if !key.is_encrypted() => key,
                Ok(_) => {
                    debug!("Skipping encrypted SSH key {}", path.display());
                    continue;
                }
                Err(error) => {
                    debug!("Skipping SSH key {}: {error}", path.display());
                    continue;
                }
            };
            tried = true;
            let result = session
                .authenticate_publickey(&t.user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
                .await
                .context(t!("net.error.sftp_auth_transport"))?;
            if result.success() {
                debug!(
                    "Authenticated {}@{} with {} (known hosts: {})",
                    t.user,
                    t.host,
                    path.display(),
                    known_hosts.display()
                );
                return Ok(());
            }
        }
        if tried {
            bail!(t!("net.error.sftp_auth", user = &t.user, host = &t.host));
        }
        bail!(t!(
            "net.error.sftp_no_credentials",
            user = &t.user,
            host = &t.host
        ))
    }

    async fn session(
        &self,
        t: &SftpTarget,
        isolation: u64,
    ) -> Result<(Handle<HostKeyCheck>, SftpSession)> {
        let mut session = self.connect(t, isolation).await?;
        self.authenticate(&mut session, t).await?;
        let channel = session
            .channel_open_session()
            .await
            .context(t!("net.error.sftp_channel"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context(t!("net.error.sftp_subsystem"))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .context(t!("net.error.sftp_session", host = &t.host))?;
        Ok((session, sftp))
    }

    async fn size(sftp: &SftpSession, path: &str) -> Result<u64> {
        let metadata = sftp
            .metadata(path)
            .await
            .with_context(|| t!("net.error.sftp_stat", path = path))?;
        metadata
            .size
            .context(t!("net.error.sftp_size", path = path))
    }
}

fn key_paths() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    KEY_FILES
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .collect()
}

#[async_trait]
impl Transport for SftpTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        let t = parse_target(url)?;
        let (_session, sftp) = self.session(&t, isolation).await?;
        let size = Self::size(&sftp, &t.path).await?;
        Ok(ResponseHead {
            status: 200,
            final_url: url.clone(),
            content_length: Some(size),
            total_size: Some(size),
            accepts_ranges: true,
            filename: file_name(&t.path),
            etag: None,
            last_modified: None,
        })
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        let t = parse_target(url)?;
        let (session, sftp) = self.session(&t, isolation).await?;
        let total = Self::size(&sftp, &t.path).await.ok();

        let start = range.map_or(0, |r| r.start);
        let want = match (range.and_then(|r| r.end), total) {
            (Some(end), Some(total)) => Some((end + 1).min(total).saturating_sub(start)),
            (Some(end), None) => Some(end + 1 - start),
            (None, Some(total)) => Some(total.saturating_sub(start)),
            (None, None) => None,
        };

        let mut file = sftp
            .open_with_flags(t.path.as_str(), OpenFlags::READ)
            .await
            .with_context(|| t!("net.error.sftp_open", path = &t.path))?;
        if start > 0 {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .with_context(|| t!("net.error.sftp_seek", path = &t.path))?;
        }

        let head = ResponseHead {
            status: if range.is_some() { 206 } else { 200 },
            final_url: url.clone(),
            content_length: want,
            total_size: total,
            accepts_ranges: true,
            filename: file_name(&t.path),
            etag: None,
            last_modified: None,
        };

        let state = (file, session, want, false);
        let body = stream::unfold(state, |(mut file, session, remaining, done)| async move {
            if done || remaining == Some(0) {
                return None;
            }
            let cap = remaining.map_or(DATA_CHUNK, |r| DATA_CHUNK.min(r as usize));
            let mut buf = vec![0u8; cap];
            match file.read(&mut buf).await {
                Ok(0) => match remaining {
                    Some(r) if r > 0 => Some((
                        Err(anyhow::anyhow!(t!("net.error.sftp_closed_early"))),
                        (file, session, remaining, true),
                    )),
                    _ => None,
                },
                Ok(n) => {
                    buf.truncate(n);
                    let left = remaining.map(|r| r - n as u64);
                    Some((Ok(Bytes::from(buf)), (file, session, left, false)))
                }
                Err(error) => Some((Err(error.into()), (file, session, remaining, true))),
            }
        });
        Ok(Response {
            head,
            body: Box::pin(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::ConnectFuture;
    use futures::StreamExt;
    use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session};
    use russh::ChannelId;
    use russh_sftp::protocol::{Attrs, Data, FileAttributes, Handle, Status, StatusCode};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    #[test]
    fn parses_targets() {
        let t =
            parse_target(&Url::parse("sftp://bob:p%40ss@files.test:2222/pub/a%20b.bin").unwrap())
                .unwrap();
        assert_eq!(
            t,
            SftpTarget {
                host: "files.test".into(),
                port: 2222,
                user: "bob".into(),
                password: Some("p@ss".into()),
                path: "/pub/a b.bin".into()
            }
        );

        let t = parse_target(&Url::parse("sftp://files.test/x").unwrap()).unwrap();
        assert_eq!(t.port, 22);
        assert_eq!(t.password, None);
        assert!(!t.user.is_empty(), "user defaults to the local account");

        assert!(parse_target(&Url::parse("ftp://files.test/x").unwrap()).is_err());
        assert!(parse_target(&Url::parse("sftp://files.test/a%00b").unwrap()).is_err());
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("/pub/a.bin").as_deref(), Some("a.bin"));
        assert_eq!(file_name("/dir/").as_deref(), Some("dir"));
        assert_eq!(file_name("/").as_deref(), None);
    }

    struct TcpProvider;
    impl StreamProvider for TcpProvider {
        fn connect<'a>(&'a self, _isolation: u64, host: &'a str, port: u16) -> ConnectFuture<'a> {
            Box::pin(async move {
                let s = TcpStream::connect((host, port)).await?;
                Ok(Box::new(s) as Box<dyn crate::tor::AsyncReadWrite>)
            })
        }
    }

    #[derive(Clone)]
    struct TestServer {
        data: Arc<Vec<u8>>,
    }

    impl russh::server::Server for TestServer {
        type Handler = SshSession;

        fn new_client(&mut self, _: Option<SocketAddr>) -> SshSession {
            SshSession {
                data: self.data.clone(),
                channels: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    struct SshSession {
        data: Arc<Vec<u8>>,
        channels: Arc<Mutex<HashMap<ChannelId, russh::Channel<Msg>>>>,
    }

    impl russh::server::Handler for SshSession {
        type Error = anyhow::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            _password: &str,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: russh::Channel<Msg>,
            reply: ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel_id: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                session.channel_failure(channel_id)?;
                return Ok(());
            }
            let channel = self.channels.lock().await.remove(&channel_id).unwrap();
            session.channel_success(channel_id)?;
            russh_sftp::server::run(
                channel.into_stream(),
                TestSftp {
                    data: self.data.clone(),
                },
            )
            .await;
            Ok(())
        }
    }

    struct TestSftp {
        data: Arc<Vec<u8>>,
    }

    impl russh_sftp::server::Handler for TestSftp {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            _pflags: russh_sftp::protocol::OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            Ok(Handle {
                id,
                handle: filename,
            })
        }

        async fn read(
            &mut self,
            id: u32,
            _handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let offset = offset as usize;
            if offset >= self.data.len() {
                return Err(StatusCode::Eof);
            }
            let end = (offset + len as usize).min(self.data.len());
            Ok(Data {
                id,
                data: self.data[offset..end].to_vec(),
            })
        }

        async fn stat(&mut self, id: u32, _path: String) -> Result<Attrs, Self::Error> {
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(self.data.len() as u64),
                    permissions: Some(0o100644),
                    ..FileAttributes::default()
                },
            })
        }

        async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: "Ok".into(),
                language_tag: "en-US".into(),
            })
        }
    }

    async fn serve(data: Vec<u8>) -> (u16, PathBuf) {
        let key = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let known_hosts =
            std::env::temp_dir().join(format!("rdl-known-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::write(&known_hosts, format!("[127.0.0.1]:{port} {public}\n")).unwrap();

        let config = Arc::new(russh::server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![key],
            ..Default::default()
        });
        let mut server = TestServer {
            data: Arc::new(data),
        };
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        (port, known_hosts)
    }

    fn payload() -> Vec<u8> {
        (0u8..=255).cycle().take(5000).collect()
    }

    async fn collect(r: Response) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut body = r.body;
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn downloads_whole_file_and_ranges() {
        let (port, known_hosts) = serve(payload()).await;
        let url = Url::parse(&format!("sftp://bob:secret@127.0.0.1:{port}/data.bin")).unwrap();
        let t = SftpTransport::with_provider(Arc::new(TcpProvider), known_hosts.clone());

        let head = t.head(&url, 0).await.unwrap();
        assert_eq!(
            (
                head.total_size,
                head.accepts_ranges,
                head.filename.as_deref()
            ),
            (Some(5000), true, Some("data.bin"))
        );

        let full = t.get(&url, None, 0).await.unwrap();
        assert_eq!(
            (full.head.status, full.head.content_length),
            (200, Some(5000))
        );
        assert_eq!(collect(full).await.unwrap(), payload());

        let part = t
            .get(
                &url,
                Some(ByteRange {
                    start: 1000,
                    end: Some(1999),
                }),
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            (part.head.status, part.head.content_length),
            (206, Some(1000))
        );
        assert_eq!(collect(part).await.unwrap(), payload()[1000..2000]);

        let tail = t
            .get(
                &url,
                Some(ByteRange {
                    start: 4000,
                    end: None,
                }),
                2,
            )
            .await
            .unwrap();
        assert_eq!(collect(tail).await.unwrap(), payload()[4000..]);

        let _ = std::fs::remove_file(known_hosts);
    }

    #[tokio::test]
    async fn unknown_host_key_is_refused() {
        let (port, known_hosts) = serve(payload()).await;
        std::fs::write(&known_hosts, "").unwrap();
        let url = Url::parse(&format!("sftp://bob:secret@127.0.0.1:{port}/data.bin")).unwrap();
        let t = SftpTransport::with_provider(Arc::new(TcpProvider), known_hosts.clone());
        let error = t.head(&url, 0).await.unwrap_err().to_string();
        assert!(error.contains("Unknown SSH host key"), "{error}");
        let _ = std::fs::remove_file(known_hosts);
    }

    #[tokio::test]
    async fn changed_host_key_is_refused() {
        let (port, known_hosts) = serve(payload()).await;
        let other = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        std::fs::write(
            &known_hosts,
            format!(
                "[127.0.0.1]:{port} {}\n",
                other.public_key().to_openssh().unwrap()
            ),
        )
        .unwrap();
        let url = Url::parse(&format!("sftp://bob:secret@127.0.0.1:{port}/data.bin")).unwrap();
        let t = SftpTransport::with_provider(Arc::new(TcpProvider), known_hosts.clone());
        let error = t.head(&url, 0).await.unwrap_err().to_string();
        assert!(error.contains("does not match"), "{error}");
        let _ = std::fs::remove_file(known_hosts);
    }
}
