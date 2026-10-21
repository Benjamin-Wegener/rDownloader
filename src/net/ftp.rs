//! FTP client over Tor streams, exposed as a [`Transport`].
//!
//! Plain FTP (RFC 959) with passive mode only. Every FTP command goes through the
//! same [`StreamProvider`] as HTTP, so nothing is resolved or connected outside
//! Tor, and each part uses its own isolation group.
//!
//! Security notes:
//! - **No encryption.** User name, password and data cross the exit relay in the
//!   clear (FTPS/SFTP are separate features). A warning is logged for every login.
//! - The address a server returns for passive mode is **ignored**: the data
//!   connection always goes to the host of the URL. Otherwise a server could make
//!   the client connect to arbitrary hosts.
//! - User name, password and path must not contain line breaks, which would
//!   allow injecting extra FTP commands through a crafted URL.
//! - Replies are read with hard limits for line length and line count.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::warn;
use url::Url;

use super::StreamProvider;
use crate::contract::{ByteRange, Response, ResponseHead, Transport};
use crate::tor::{AsyncReadWrite, TorManager};

const MAX_REPLY_LINE: u64 = 8 * 1024;
const MAX_REPLY_LINES: usize = 200;
const DATA_CHUNK: usize = 16 * 1024;

/// Where and how to log in, taken from an `ftp://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub path: String,
}

fn decode(s: &str) -> Result<String> {
    Ok(percent_encoding::percent_decode_str(s)
        .decode_utf8()?
        .into_owned())
}

/// Anything that could end the current FTP command and start another one.
fn check_line_safe(what: &str, value: &str) -> Result<()> {
    if value.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        bail!("FTP {what} contains a control character");
    }
    Ok(())
}

pub fn parse_target(url: &Url) -> Result<FtpTarget> {
    if url.scheme() != "ftp" {
        bail!("not an ftp:// URL: {url}");
    }
    let host = url.host_str().context("FTP URL without host")?.to_string();
    let user = match url.username() {
        "" => "anonymous".to_string(),
        u => decode(u)?,
    };
    let pass = match url.password() {
        None => "anonymous@".to_string(),
        Some(p) => decode(p)?,
    };
    let path = decode(url.path())?;
    check_line_safe("user name", &user)?;
    check_line_safe("password", &pass)?;
    check_line_safe("path", &path)?;
    Ok(FtpTarget {
        host,
        port: url.port().unwrap_or(21),
        user,
        pass,
        path,
    })
}

/// File name for a URL path (last non-empty segment), decoded.
fn file_name(path: &str) -> Option<String> {
    path.rsplit('/').find(|s| !s.is_empty()).map(str::to_string)
}

/// Port from an `EPSV` reply: `229 Entering Extended Passive Mode (|||6446|)`.
fn parse_epsv(text: &str) -> Result<u16> {
    let inner = text
        .split('(')
        .nth(1)
        .and_then(|r| r.split(')').next())
        .context("bad EPSV reply")?;
    let delim = inner.chars().next().context("empty EPSV reply")?;
    let fields: Vec<&str> = inner.split(delim).collect();
    fields
        .get(3)
        .and_then(|p| p.parse().ok())
        .context("bad EPSV port")
}

/// Port from a `PASV` reply: `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
/// The address part is deliberately ignored.
fn parse_pasv(text: &str) -> Result<u16> {
    let inner = text
        .split('(')
        .nth(1)
        .and_then(|r| r.split(')').next())
        .context("bad PASV reply")?;
    let n: Vec<u16> = inner
        .split(',')
        .map(|x| x.trim().parse().unwrap_or(u16::MAX))
        .collect();
    if n.len() != 6 || n[4] > 255 || n[5] > 255 {
        bail!("bad PASV reply");
    }
    Ok(n[4] * 256 + n[5])
}

/// One FTP control connection.
struct Control {
    io: BufReader<Box<dyn AsyncReadWrite>>,
}

impl Control {
    async fn reply(&mut self) -> Result<(u16, String)> {
        let mut text = String::new();
        let mut code_prefix: Option<String> = None;
        for _ in 0..MAX_REPLY_LINES {
            let mut line = String::new();
            let n = (&mut self.io)
                .take(MAX_REPLY_LINE)
                .read_line(&mut line)
                .await?;
            if n == 0 {
                bail!("FTP control connection closed");
            }
            if !line.ends_with('\n') {
                bail!("FTP reply line too long");
            }
            let line = line.trim_end();
            text.push_str(line);
            text.push('\n');
            if line.len() < 4 || !line.as_bytes()[..3].iter().all(u8::is_ascii_digit) {
                continue; // continuation text of a multi-line reply
            }
            let (code, sep) = (&line[..3], line.as_bytes()[3]);
            match (&code_prefix, sep) {
                (None, b'-') => code_prefix = Some(code.to_string()),
                (None, b' ') => return Ok((code.parse()?, text)),
                (Some(p), b' ') if p == code => return Ok((code.parse()?, text)),
                _ => {}
            }
        }
        bail!("FTP reply too long")
    }

    async fn cmd(&mut self, line: &str) -> Result<(u16, String)> {
        self.io.write_all(line.as_bytes()).await?;
        self.io.write_all(b"\r\n").await?;
        self.io.flush().await?;
        self.reply().await
    }

    async fn expect(&mut self, line: &str, ok: &[u16]) -> Result<String> {
        let (code, text) = self.cmd(line).await?;
        if !ok.contains(&code) {
            // `line` may hold a password, so only the verb is reported.
            let verb = line.split(' ').next().unwrap_or("");
            bail!("FTP {verb} failed: {}", text.trim());
        }
        Ok(text)
    }
}

pub struct FtpTransport {
    provider: Arc<dyn StreamProvider>,
}

impl FtpTransport {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self { provider: tor }
    }

    pub fn with_provider(provider: Arc<dyn StreamProvider>) -> Self {
        Self { provider }
    }

    async fn login(&self, t: &FtpTarget, isolation: u64) -> Result<Control> {
        warn!(
            "FTP to {} is not encrypted: login and data are visible to the exit relay",
            t.host
        );
        let io = self.provider.connect(isolation, &t.host, t.port).await?;
        let mut c = Control {
            io: BufReader::new(io),
        };
        let (code, text) = c.reply().await?;
        if code != 220 {
            bail!("FTP server refused the connection: {}", text.trim());
        }
        let (code, text) = c.cmd(&format!("USER {}", t.user)).await?;
        match code {
            230 => {}
            331 | 332 => {
                c.expect(&format!("PASS {}", t.pass), &[230]).await?;
            }
            _ => bail!("FTP USER failed: {}", text.trim()),
        }
        c.expect("TYPE I", &[200]).await?;
        Ok(c)
    }

    async fn size(c: &mut Control, path: &str) -> Option<u64> {
        let (code, text) = c.cmd(&format!("SIZE {path}")).await.ok()?;
        (code == 213)
            .then(|| text.split_whitespace().nth(1)?.parse().ok())
            .flatten()
    }
}

#[async_trait]
impl Transport for FtpTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        let t = parse_target(url)?;
        let mut c = self.login(&t, isolation).await?;
        let size = Self::size(&mut c, &t.path).await;
        let accepts_ranges = matches!(c.cmd("REST 0").await, Ok((350, _)));
        let _ = c.cmd("QUIT").await;
        let size = size.ok_or_else(|| anyhow!("FTP server did not report the file size (SIZE)"))?;
        Ok(ResponseHead {
            status: 200,
            final_url: url.clone(),
            content_length: Some(size),
            total_size: Some(size),
            accepts_ranges,
            filename: file_name(&t.path),
            etag: None,
            last_modified: None,
        })
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        let t = parse_target(url)?;
        let mut c = self.login(&t, isolation).await?;
        let total = Self::size(&mut c, &t.path).await;

        let start = range.map_or(0, |r| r.start);
        // Bytes this response carries: up to the range end, else up to the file end.
        let want = match (range.and_then(|r| r.end), total) {
            (Some(end), Some(total)) => Some((end + 1).min(total).saturating_sub(start)),
            (Some(end), None) => Some(end + 1 - start),
            (None, Some(total)) => Some(total.saturating_sub(start)),
            (None, None) => None,
        };

        let port = match c.cmd("EPSV").await? {
            (229, text) => parse_epsv(&text)?,
            _ => parse_pasv(&c.expect("PASV", &[227]).await?)?,
        };
        if start > 0 {
            c.expect(&format!("REST {start}"), &[350])
                .await
                .context("FTP server cannot resume")?;
        }
        let data = self.provider.connect(isolation, &t.host, port).await?;
        c.expect(&format!("RETR {}", t.path), &[125, 150]).await?;

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

        // The control connection stays open (and owned by the stream) while data flows.
        let state = (data, c, want, false);
        let body = stream::unfold(state, |(mut data, ctl, remaining, done)| async move {
            if done || remaining == Some(0) {
                return None;
            }
            let cap = remaining.map_or(DATA_CHUNK, |r| DATA_CHUNK.min(r as usize));
            let mut buf = vec![0u8; cap];
            match data.read(&mut buf).await {
                Ok(0) => match remaining {
                    Some(r) if r > 0 => {
                        let err = anyhow!("FTP data connection closed early");
                        Some((Err(err), (data, ctl, remaining, true)))
                    }
                    _ => None,
                },
                Ok(n) => {
                    buf.truncate(n);
                    let left = remaining.map(|r| r - n as u64);
                    Some((Ok(Bytes::from(buf)), (data, ctl, left, false)))
                }
                Err(e) => Some((Err(e.into()), (data, ctl, remaining, true))),
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
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn parses_targets() {
        let t =
            parse_target(&Url::parse("ftp://bob:p%40ss@files.test:2121/pub/a%20b.bin").unwrap())
                .unwrap();
        assert_eq!(
            t,
            FtpTarget {
                host: "files.test".into(),
                port: 2121,
                user: "bob".into(),
                pass: "p@ss".into(),
                path: "/pub/a b.bin".into()
            }
        );
        let anon = parse_target(&Url::parse("ftp://files.test/x").unwrap()).unwrap();
        assert_eq!(
            (anon.user.as_str(), anon.pass.as_str(), anon.port),
            ("anonymous", "anonymous@", 21)
        );
        assert!(parse_target(&Url::parse("http://files.test/x").unwrap()).is_err());
    }

    #[test]
    fn command_injection_through_url_is_rejected() {
        for bad in [
            "ftp://h.test/a%0d%0aDELE%20x",
            "ftp://u%0d%0a:p@h.test/x",
            "ftp://u:p%0a@h.test/x",
            "ftp://h.test/a%00b",
        ] {
            assert!(
                parse_target(&Url::parse(bad).unwrap()).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn passive_replies() {
        assert_eq!(
            parse_epsv("229 Entering Extended Passive Mode (|||6446|)").unwrap(),
            6446
        );
        assert_eq!(
            parse_pasv("227 Entering Passive Mode (10,0,0,5,19,137)").unwrap(),
            19 * 256 + 137
        );
        assert!(parse_pasv("227 (1,2,3,4,300,1)").is_err());
        assert!(parse_pasv("227 no numbers").is_err());
        assert!(parse_epsv("229 nothing").is_err());
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
                Ok(Box::new(s) as Box<dyn AsyncReadWrite>)
            })
        }
    }

    /// Tiny FTP server for one file; records the commands it sees.
    async fn serve(
        data: Vec<u8>,
        with_size: bool,
        truncate_data_at: Option<usize>,
    ) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let ctl = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = ctl.local_addr().unwrap().port();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = ctl.accept().await else {
                    return;
                };
                let (data, log) = (data.clone(), log2.clone());
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(sock);
                    let mut r = BufReader::new(r);
                    w.write_all(b"220 hello\r\n").await.unwrap();
                    let (mut rest, mut pasv): (u64, Option<TcpListener>) = (0, None);
                    let mut line = String::new();
                    while {
                        line.clear();
                        r.read_line(&mut line).await.unwrap_or(0) > 0
                    } {
                        let cmd = line.trim().to_string();
                        log.lock().unwrap().push(cmd.clone());
                        let verb = cmd.split(' ').next().unwrap().to_ascii_uppercase();
                        let reply = match verb.as_str() {
                            "USER" => "331 need password\r\n".into(),
                            "PASS" => "230 ok\r\n".into(),
                            "TYPE" => "200 ok\r\n".into(),
                            "SIZE" if with_size => format!("213 {}\r\n", data.len()),
                            "SIZE" => "550 no size\r\n".into(),
                            "REST" => {
                                rest = cmd[5..].parse().unwrap_or(0);
                                "350 ok\r\n".into()
                            }
                            "EPSV" => {
                                let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                                let p = l.local_addr().unwrap().port();
                                pasv = Some(l);
                                format!("229 Entering Extended Passive Mode (|||{p}|)\r\n")
                            }
                            "RETR" => {
                                w.write_all(b"150 opening\r\n").await.unwrap();
                                let l = pasv.take().unwrap();
                                let (mut d, _) = l.accept().await.unwrap();
                                let body = &data[rest as usize..];
                                let body =
                                    truncate_data_at.map_or(body, |n| &body[..n.min(body.len())]);
                                d.write_all(body).await.unwrap();
                                drop(d);
                                "226 done\r\n".into()
                            }
                            "QUIT" => "221 bye\r\n".into(),
                            _ => "502 no\r\n".into(),
                        };
                        w.write_all(reply.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        (port, log)
    }

    async fn collect(r: Response) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut body = r.body;
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    fn payload() -> Vec<u8> {
        (0u8..=255).cycle().take(5000).collect()
    }

    fn transport() -> FtpTransport {
        FtpTransport::with_provider(Arc::new(TcpProvider))
    }

    #[tokio::test]
    async fn head_reports_size_and_resume_support() {
        let (port, log) = serve(payload(), true, None).await;
        let url = Url::parse(&format!("ftp://127.0.0.1:{port}/pub/data.bin")).unwrap();
        let h = transport().head(&url, 0).await.unwrap();
        assert_eq!(
            (h.total_size, h.accepts_ranges, h.filename.as_deref()),
            (Some(5000), true, Some("data.bin"))
        );
        let seen = log.lock().unwrap().clone();
        assert!(seen.contains(&"USER anonymous".to_string()), "{seen:?}");
    }

    #[tokio::test]
    async fn downloads_whole_file_and_ranges() {
        let (port, _log) = serve(payload(), true, None).await;
        let url = Url::parse(&format!("ftp://127.0.0.1:{port}/data.bin")).unwrap();
        let t = transport();

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
    }

    #[tokio::test]
    async fn early_close_of_data_connection_is_an_error() {
        let (port, _log) = serve(payload(), true, Some(1200)).await;
        let url = Url::parse(&format!("ftp://127.0.0.1:{port}/data.bin")).unwrap();
        let r = transport().get(&url, None, 0).await.unwrap();
        assert!(collect(r).await.is_err());
    }

    #[tokio::test]
    async fn head_without_size_command_fails_clearly() {
        let (port, _log) = serve(payload(), false, None).await;
        let url = Url::parse(&format!("ftp://127.0.0.1:{port}/data.bin")).unwrap();
        let err = transport().head(&url, 0).await.unwrap_err().to_string();
        assert!(err.contains("size"), "{err}");
    }

    #[tokio::test]
    async fn password_is_not_part_of_error_messages() {
        let (port, _log) = serve(payload(), true, None).await;
        // The test server accepts any password; force a failure on a later command instead.
        let url = Url::parse(&format!("ftp://u:s3cret@127.0.0.1:{port}/data.bin")).unwrap();
        let mut c = transport()
            .login(&parse_target(&url).unwrap(), 0)
            .await
            .unwrap();
        let err = c
            .expect("PASS s3cret", &[999])
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("s3cret"), "{err}");
    }
}
