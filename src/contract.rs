//! Stabile Verträge zwischen den Bereichen.
//!
//! - Vertrag A (`Transport`): HTTP-Zugriff, den `net` implementiert und `core` konsumiert.
//! - Vertrag B (`JobEvent`/`JobCommand`): Ereignis- und Steuerkanal zwischen Job und `ui`.
//!
//! Änderungen hier brauchen einen `HANDOFF.md`-Eintrag und Absprache mit dem Nutzer.
//! `MockTransport` und `FakeJob` erlauben, `core` und `ui` unabhängig voneinander zu testen.

#![allow(dead_code)]

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

use crate::download::{JobStatus, PartProgress};

// ───────────────────────── Vertrag A: Transport ─────────────────────────

/// Byte-Bereich einer Anfrage (inklusive Grenzen, wie `Range: bytes=start-end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    /// `None` = bis zum Ende.
    pub end: Option<u64>,
}

/// Ergebnis eines HEAD bzw. die Kopfdaten einer GET-Antwort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    /// URL nach Redirects.
    pub final_url: Url,
    /// Länge des Bodys dieser Antwort; bei Range-Antworten die Teillänge.
    pub content_length: Option<u64>,
    /// Gesamtgröße der Ressource (aus `Content-Range` bzw. `Content-Length`).
    pub total_size: Option<u64>,
    pub accepts_ranges: bool,
    pub filename: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub type ByteStream = BoxStream<'static, Result<Bytes>>;

/// Antwort einer GET-Anfrage: Kopfdaten plus Body als Stream.
pub struct Response {
    pub head: ResponseHead,
    pub body: ByteStream,
}

/// Transport-Schicht (z. B. HTTP über Tor).
///
/// `isolation` gruppiert Anfragen: gleiche ID darf einen Circuit teilen,
/// verschiedene IDs dürfen **nie** einen Circuit teilen (Stream-Isolation pro Part).
#[async_trait]
pub trait Transport: Send + Sync {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead>;
    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response>;
}

/// Aufgezeichnete Anfrage des `MockTransport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedRequest {
    Head {
        url: Url,
        isolation: u64,
    },
    Get {
        url: Url,
        range: Option<ByteRange>,
        isolation: u64,
    },
}

/// Eine vom `MockTransport` bediente Ressource.
#[derive(Debug, Clone)]
pub struct MockResource {
    pub data: Bytes,
    pub accepts_ranges: bool,
    pub filename: Option<String>,
    pub etag: Option<String>,
}

impl MockResource {
    pub fn new(data: impl Into<Bytes>) -> Self {
        Self {
            data: data.into(),
            accepts_ranges: true,
            filename: None,
            etag: None,
        }
    }
}

/// In-Memory-Transport für Tests: Ranges, Chunking, Fehlerinjektion, Aufzeichnung.
#[derive(Clone, Default)]
pub struct MockTransport {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    resources: HashMap<Url, MockResource>,
    requests: Vec<RecordedRequest>,
    /// Die nächsten n `get`-Aufrufe schlagen fehl.
    fail_next_gets: usize,
    /// Body in Stücken dieser Größe ausliefern (0 = ein Stück).
    chunk_size: usize,
    /// Body bricht nach so vielen Bytes mit Fehler ab (einmalig).
    truncate_after: Option<usize>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_resource(self, url: &str, res: MockResource) -> Self {
        let url = Url::parse(url).expect("gültige Test-URL");
        self.inner.lock().unwrap().resources.insert(url, res);
        self
    }

    pub fn with_chunk_size(self, n: usize) -> Self {
        self.inner.lock().unwrap().chunk_size = n;
        self
    }

    pub fn fail_next_gets(&self, n: usize) {
        self.inner.lock().unwrap().fail_next_gets = n;
    }

    pub fn truncate_next_body_after(&self, bytes: usize) {
        self.inner.lock().unwrap().truncate_after = Some(bytes);
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.lock().unwrap().requests.clone()
    }

    fn head_for(url: &Url, res: &MockResource, status: u16, len: u64) -> ResponseHead {
        ResponseHead {
            status,
            final_url: url.clone(),
            content_length: Some(len),
            total_size: Some(res.data.len() as u64),
            accepts_ranges: res.accepts_ranges,
            filename: res.filename.clone(),
            etag: res.etag.clone(),
            last_modified: None,
        }
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        let mut st = self.inner.lock().unwrap();
        st.requests.push(RecordedRequest::Head {
            url: url.clone(),
            isolation,
        });
        let res = st.resources.get(url).ok_or_else(|| anyhow!("404: {url}"))?;
        Ok(Self::head_for(url, res, 200, res.data.len() as u64))
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        let mut st = self.inner.lock().unwrap();
        st.requests.push(RecordedRequest::Get {
            url: url.clone(),
            range,
            isolation,
        });
        if st.fail_next_gets > 0 {
            st.fail_next_gets -= 1;
            return Err(anyhow!("injizierter Fehler"));
        }
        let res = st
            .resources
            .get(url)
            .ok_or_else(|| anyhow!("404: {url}"))?
            .clone();
        let total = res.data.len() as u64;

        // Ohne Range-Unterstützung ignoriert der Server den Header (200, ganzer Body).
        let (status, slice) = match range {
            Some(r) if res.accepts_ranges => {
                if r.start >= total {
                    return Err(anyhow!("416: Range außerhalb"));
                }
                let end = r.end.map_or(total, |e| (e + 1).min(total));
                (206, res.data.slice(r.start as usize..end as usize))
            }
            _ => (200, res.data.clone()),
        };

        let head = Self::head_for(url, &res, status, slice.len() as u64);
        let truncate = st.truncate_after.take();
        let chunk = if st.chunk_size == 0 {
            slice.len().max(1)
        } else {
            st.chunk_size
        };
        drop(st);

        let mut items: Vec<Result<Bytes>> = Vec::new();
        let limit = truncate.unwrap_or(slice.len()).min(slice.len());
        let mut pos = 0;
        while pos < limit {
            let n = chunk.min(limit - pos);
            items.push(Ok(slice.slice(pos..pos + n)));
            pos += n;
        }
        if truncate.is_some() {
            items.push(Err(anyhow!("Verbindung abgebrochen")));
        }
        Ok(Response {
            head,
            body: Box::pin(stream::iter(items)),
        })
    }
}

// ───────────────────────── Vertrag B: Job-Ereignisse ─────────────────────────

pub type JobId = Uuid;

/// Job → UI. Jeder Job sendet seine Ereignisse über einen `mpsc`-Kanal.
#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    /// Status hat gewechselt.
    Status { id: JobId, status: JobStatus },
    /// Größe/Dateiname sind bekannt (nach HEAD).
    Resolved {
        id: JobId,
        filename: String,
        total_bytes: Option<u64>,
    },
    /// Fortschritt, gedrosselt gesendet (nicht pro Chunk).
    Progress {
        id: JobId,
        bytes_done: u64,
        parts: Vec<PartProgress>,
    },
    /// Aktuelle Geschwindigkeit in Bytes/s.
    Speed { id: JobId, bytes_per_sec: u64 },
    /// Endzustand erreicht; danach kommen keine Ereignisse mehr.
    Finished {
        id: JobId,
        result: Result<(), String>,
    },
}

/// UI → Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCommand {
    Pause,
    Resume,
    Cancel,
}

/// Skriptgesteuerter Job für UI-Tests: sendet Ereignisse, reagiert auf Kommandos.
pub struct FakeJob {
    pub id: JobId,
    pub filename: String,
    pub total_bytes: u64,
    /// Bytes pro Schritt.
    pub step: u64,
}

impl FakeJob {
    pub fn new(filename: &str, total_bytes: u64, step: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            filename: filename.into(),
            total_bytes,
            step: step.max(1),
        }
    }

    /// Startet den Job; liefert den Kommando-Sender. Ereignisse gehen an `events`.
    /// Läuft ohne Zeitverzögerung (ein Schritt pro Scheduler-Tick), Pause hält ihn an.
    pub fn spawn(
        self,
        events: mpsc::UnboundedSender<JobEvent>,
    ) -> mpsc::UnboundedSender<JobCommand> {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let id = self.id;
            let send = |e: JobEvent| {
                let _ = events.send(e);
            };
            send(JobEvent::Status {
                id,
                status: JobStatus::Resolving,
            });
            send(JobEvent::Resolved {
                id,
                filename: self.filename.clone(),
                total_bytes: Some(self.total_bytes),
            });
            send(JobEvent::Status {
                id,
                status: JobStatus::Downloading,
            });

            let mut done = 0u64;
            let mut paused = false;
            loop {
                // Kommandos zuerst; im Pausenzustand blockierend darauf warten.
                let cmd = if paused {
                    cmd_rx.recv().await
                } else {
                    cmd_rx.try_recv().ok()
                };
                match cmd {
                    Some(JobCommand::Cancel) => {
                        send(JobEvent::Status {
                            id,
                            status: JobStatus::Cancelled,
                        });
                        send(JobEvent::Finished {
                            id,
                            result: Err("abgebrochen".into()),
                        });
                        return;
                    }
                    Some(JobCommand::Pause) => {
                        paused = true;
                        continue;
                    }
                    Some(JobCommand::Resume) => {
                        paused = false;
                        continue;
                    }
                    None if paused => return, // Steuerkanal geschlossen
                    None => {}
                }

                done = (done + self.step).min(self.total_bytes);
                send(JobEvent::Progress {
                    id,
                    bytes_done: done,
                    parts: vec![],
                });
                if done >= self.total_bytes {
                    send(JobEvent::Status {
                        id,
                        status: JobStatus::Done,
                    });
                    send(JobEvent::Finished { id, result: Ok(()) });
                    return;
                }
                tokio::task::yield_now().await;
            }
        });
        cmd_tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn collect(mut body: ByteStream) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    fn transport() -> (MockTransport, Url) {
        let mut res = MockResource::new(&b"0123456789"[..]);
        res.filename = Some("zehn.bin".into());
        let t = MockTransport::new()
            .with_resource("http://x.test/f", res)
            .with_chunk_size(3);
        (t, Url::parse("http://x.test/f").unwrap())
    }

    #[tokio::test]
    async fn head_liefert_metadaten() {
        let (t, url) = transport();
        let h = t.head(&url, 0).await.unwrap();
        assert_eq!(h.total_size, Some(10));
        assert!(h.accepts_ranges);
        assert_eq!(h.filename.as_deref(), Some("zehn.bin"));
    }

    #[tokio::test]
    async fn get_voll_und_range() {
        let (t, url) = transport();
        let r = t.get(&url, None, 0).await.unwrap();
        assert_eq!(r.head.status, 200);
        assert_eq!(collect(r.body).await.unwrap(), b"0123456789");

        let r = t
            .get(
                &url,
                Some(ByteRange {
                    start: 2,
                    end: Some(5),
                }),
                1,
            )
            .await
            .unwrap();
        assert_eq!(r.head.status, 206);
        assert_eq!(r.head.content_length, Some(4));
        assert_eq!(collect(r.body).await.unwrap(), b"2345");

        let r = t
            .get(
                &url,
                Some(ByteRange {
                    start: 7,
                    end: None,
                }),
                2,
            )
            .await
            .unwrap();
        assert_eq!(collect(r.body).await.unwrap(), b"789");
    }

    #[tokio::test]
    async fn ohne_range_support_kommt_ganzer_body() {
        let mut res = MockResource::new(&b"abc"[..]);
        res.accepts_ranges = false;
        let t = MockTransport::new().with_resource("http://x.test/n", res);
        let url = Url::parse("http://x.test/n").unwrap();
        let r = t
            .get(
                &url,
                Some(ByteRange {
                    start: 1,
                    end: None,
                }),
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.head.status, 200);
        assert_eq!(collect(r.body).await.unwrap(), b"abc");
    }

    #[tokio::test]
    async fn fehlerinjektion_und_aufzeichnung() {
        let (t, url) = transport();
        t.fail_next_gets(1);
        assert!(t.get(&url, None, 7).await.is_err());
        t.truncate_next_body_after(4);
        let r = t.get(&url, None, 8).await.unwrap();
        assert!(collect(r.body).await.is_err());
        assert_eq!(
            t.requests(),
            vec![
                RecordedRequest::Get {
                    url: url.clone(),
                    range: None,
                    isolation: 7
                },
                RecordedRequest::Get {
                    url,
                    range: None,
                    isolation: 8
                },
            ]
        );
    }

    #[tokio::test]
    async fn unbekannte_url_ist_fehler() {
        let t = MockTransport::new();
        let url = Url::parse("http://x.test/none").unwrap();
        assert!(t.head(&url, 0).await.is_err());
    }

    async fn drain(rx: &mut mpsc::UnboundedReceiver<JobEvent>) -> Vec<JobEvent> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            let fin = matches!(e, JobEvent::Finished { .. });
            out.push(e);
            if fin {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn fakejob_laeuft_bis_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job = FakeJob::new("a.bin", 100, 40);
        let id = job.id;
        let _cmd = job.spawn(tx);
        let ev = drain(&mut rx).await;
        assert_eq!(
            ev.first(),
            Some(&JobEvent::Status {
                id,
                status: JobStatus::Resolving
            })
        );
        assert!(ev.contains(&JobEvent::Progress {
            id,
            bytes_done: 100,
            parts: vec![]
        }));
        assert_eq!(ev.last(), Some(&JobEvent::Finished { id, result: Ok(()) }));
    }

    #[tokio::test]
    async fn fakejob_cancel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job = FakeJob::new("a.bin", 1_000_000, 1);
        let id = job.id;
        let cmd = job.spawn(tx);
        cmd.send(JobCommand::Cancel).unwrap();
        let ev = drain(&mut rx).await;
        assert!(ev.contains(&JobEvent::Status {
            id,
            status: JobStatus::Cancelled
        }));
        assert!(matches!(
            ev.last(),
            Some(JobEvent::Finished { result: Err(_), .. })
        ));
    }

    #[tokio::test]
    async fn fakejob_pause_resume() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job = FakeJob::new("a.bin", 10, 1);
        let cmd = job.spawn(tx);
        cmd.send(JobCommand::Pause).unwrap();
        cmd.send(JobCommand::Resume).unwrap();
        let ev = drain(&mut rx).await;
        assert!(matches!(
            ev.last(),
            Some(JobEvent::Finished { result: Ok(()), .. })
        ));
    }
}
