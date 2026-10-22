//! Picks the transport by URL scheme: `http`/`https` use the Hyper client,
//! `ftp`/`ftps`/`ftpes` the FTP client and `sftp` the SSH client. All run over
//! Tor.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use url::Url;

use super::ftp::FtpTransport;
use super::sftp::SftpTransport;
use super::TorTransport;
use crate::contract::{ByteRange, Response, ResponseHead, Transport};
use crate::tor::TorManager;

pub struct SchemeTransport {
    http: TorTransport,
    ftp: FtpTransport,
    sftp: SftpTransport,
}

impl SchemeTransport {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self {
            http: TorTransport::new(tor.clone()),
            ftp: FtpTransport::new(tor.clone()),
            sftp: SftpTransport::new(tor),
        }
    }

    fn pick(&self, url: &Url) -> &dyn Transport {
        match url.scheme() {
            "ftp" | "ftps" | "ftpes" => &self.ftp,
            "sftp" => &self.sftp,
            _ => &self.http,
        }
    }
}

#[async_trait]
impl Transport for SchemeTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        self.pick(url).head(url, isolation).await
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        self.pick(url).get(url, range, isolation).await
    }
}
