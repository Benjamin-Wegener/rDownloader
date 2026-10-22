//! HLS/DASH segment downloads over the contract transport (and therefore Tor).
//!
//! [`SegmentDownloader`] resolves a master playlist or MPD, downloads every
//! fragment with bounded parallelism and concatenates them into one file. Each
//! fragment uses its own isolation id, so concurrent fragments never share a
//! Tor circuit.
//!
//! ```ignore
//! let downloader = SegmentDownloader::new(transport).with_parallelism(4);
//! let report = downloader.download(&url, Path::new("video.ts")).await?;
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use super::dash::{self, DashVariant};
use super::hls::{self, HlsPlaylist};
use crate::contract::{ByteRange, Transport};
use crate::t;

/// Manifests larger than this are rejected (no legitimate playlist is bigger).
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

/// Streaming format of a resolved plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Hls,
    Dash,
}

/// One fragment (or the init section) to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRef {
    pub url: Url,
    pub byte_range: Option<ByteRange>,
}

/// Ordered list of fragments that make up one stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentPlan {
    pub kind: SegmentKind,
    /// Chosen variant/representation id, for logging.
    pub selected: Option<String>,
    pub init: Option<SegmentRef>,
    pub segments: Vec<SegmentRef>,
    pub duration_secs: Option<f64>,
}

/// Outcome of a finished segment download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SegmentReport {
    pub segments: usize,
    pub bytes: u64,
}

/// Downloads HLS/DASH streams through any [`Transport`].
pub struct SegmentDownloader {
    transport: Arc<dyn Transport>,
    parallelism: usize,
    retries: u32,
    isolation: u64,
}

impl SegmentDownloader {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            parallelism: 4,
            retries: 3,
            isolation: 0,
        }
    }

    /// Concurrent fragments; clamped to 1..=16.
    pub fn with_parallelism(mut self, parallelism: usize) -> Self {
        self.parallelism = parallelism.clamp(1, 16);
        self
    }

    /// Additional attempts per fragment after the first failure.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries.min(10);
        self
    }

    /// Base isolation id; every fragment gets a distinct id derived from it.
    pub fn with_isolation(mut self, isolation: u64) -> Self {
        self.isolation = isolation;
        self
    }

    /// Fetches and parses the manifest, including the chosen variant of a
    /// master playlist.
    pub async fn resolve(&self, url: &Url) -> Result<SegmentPlan> {
        let (final_url, body) = self.fetch_manifest(url, self.isolation).await?;

        if let Some(playlist) = hls::parse(&final_url, &body)? {
            return match playlist {
                HlsPlaylist::Media(media) => {
                    if media.live {
                        warn!("HLS playlist has no ENDLIST; downloading the listed snapshot");
                    }
                    Ok(plan_from_media(final_url, media))
                }
                HlsPlaylist::Master(variants) => {
                    let best = HlsPlaylist::best_variant(&variants)
                        .ok_or_else(|| anyhow::anyhow!(t!("net.segment.empty")))?;
                    debug!(variant = %best.url, "selected HLS variant");
                    let (variant_url, text) = self
                        .fetch_manifest(&best.url, self.isolation.wrapping_add(1))
                        .await?;
                    match hls::parse(&variant_url, &text)? {
                        Some(HlsPlaylist::Media(media)) => {
                            if media.live {
                                warn!(
                                    "HLS playlist has no ENDLIST; downloading the listed snapshot"
                                );
                            }
                            Ok(plan_from_media(variant_url, media))
                        }
                        _ => bail!(t!(
                            "net.segment.invalid",
                            detail = "variant is not a media playlist"
                        )),
                    }
                }
            };
        }

        if let Some(variants) = dash::parse(&final_url, &body)? {
            let best = DashVariant::best(&variants)
                .ok_or_else(|| anyhow::anyhow!(t!("net.segment.empty")))?;
            debug!(representation = %best.id, "selected DASH representation");
            return Ok(best.plan.clone());
        }

        bail!(t!("net.segment.manifest"));
    }

    /// Resolves `url` and writes all fragments, in order, to `output`.
    pub async fn download(&self, url: &Url, output: &Path) -> Result<SegmentReport> {
        let plan = self.resolve(url).await?;
        self.download_plan(&plan, output).await
    }

    /// Downloads an already resolved plan to `output`.
    pub async fn download_plan(&self, plan: &SegmentPlan, output: &Path) -> Result<SegmentReport> {
        if plan.segments.is_empty() {
            bail!(t!("net.segment.empty"));
        }
        let parent = match output.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        tokio::fs::create_dir_all(&parent)
            .await
            .with_context(|| t!("net.segment.write", path = parent.display().to_string()))?;
        let work = parent.join(format!(".rdl-segments-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&work)
            .await
            .with_context(|| t!("net.segment.write", path = work.display().to_string()))?;

        let result = self.download_parts(plan, &work).await;
        match result {
            Ok((paths, report)) => {
                let merged = merge_parts(&paths, output).await;
                let _ = tokio::fs::remove_dir_all(&work).await;
                merged.map(|()| report)
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&work).await;
                Err(error)
            }
        }
    }

    async fn download_parts(
        &self,
        plan: &SegmentPlan,
        work: &Path,
    ) -> Result<(Vec<PathBuf>, SegmentReport)> {
        let mut paths = Vec::with_capacity(plan.segments.len() + 1);
        let mut report = SegmentReport {
            segments: plan.segments.len(),
            bytes: 0,
        };

        if let Some(init) = &plan.init {
            let path = work.join("000000.init");
            let isolation = self.isolation.wrapping_add(10_000);
            report.bytes += self
                .fetch_with_retries(init, &path, isolation)
                .await
                .context(t!("net.segment.init"))?;
            paths.push(path);
        }

        let semaphore = Arc::new(Semaphore::new(self.parallelism));
        let mut tasks = FuturesUnordered::new();
        let total = plan.segments.len();
        for (index, segment) in plan.segments.iter().enumerate() {
            let transport = self.transport.clone();
            let semaphore = semaphore.clone();
            let path = work.join(format!("{:06}.seg", index + 1));
            let isolation = self
                .isolation
                .wrapping_add(10_001)
                .wrapping_add(index as u64);
            let retries = self.retries;
            let segment = segment.clone();
            tasks.push(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|error| anyhow::anyhow!("semaphore closed: {error}"))?;
                let bytes =
                    fetch_with_retries(transport.as_ref(), &segment, &path, isolation, retries)
                        .await
                        .with_context(|| {
                            t!("net.segment.segment", index = index + 1, total = total)
                        })?;
                Ok::<(usize, PathBuf, u64), anyhow::Error>((index, path, bytes))
            });
        }

        let mut segment_paths = vec![None; total];
        while let Some(result) = tasks.next().await {
            let (index, path, bytes) = result?;
            segment_paths[index] = Some(path);
            report.bytes += bytes;
        }
        for path in segment_paths {
            paths.push(path.ok_or_else(|| anyhow::anyhow!("segment missing"))?);
        }
        Ok((paths, report))
    }

    async fn fetch_with_retries(
        &self,
        segment: &SegmentRef,
        path: &Path,
        isolation: u64,
    ) -> Result<u64> {
        let retries = self.retries;
        fetch_with_retries(self.transport.as_ref(), segment, path, isolation, retries).await
    }

    async fn fetch_manifest(&self, url: &Url, isolation: u64) -> Result<(Url, String)> {
        let mut response = self
            .transport
            .get(url, None, isolation)
            .await
            .with_context(|| t!("net.segment.fetch", url = url.to_string()))?;
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next().await {
            let chunk = chunk.with_context(|| t!("net.segment.read"))?;
            if body.len() + chunk.len() > MAX_MANIFEST_BYTES {
                bail!(t!("net.segment.invalid", detail = "manifest too large"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((
            response.head.final_url,
            String::from_utf8_lossy(&body).into_owned(),
        ))
    }
}

fn plan_from_media(url: Url, media: hls::HlsMedia) -> SegmentPlan {
    SegmentPlan {
        kind: SegmentKind::Hls,
        selected: Some(url.to_string()),
        init: media.init,
        segments: media.segments,
        duration_secs: media.duration_secs,
    }
}

async fn fetch_with_retries(
    transport: &dyn Transport,
    segment: &SegmentRef,
    path: &Path,
    isolation: u64,
    retries: u32,
) -> Result<u64> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // Every retry gets a fresh circuit: the previous one may be dead.
        let attempt_isolation = isolation.wrapping_add(u64::from(attempt - 1) * 1_000_000);
        match fetch_segment(transport, segment, path, attempt_isolation).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) if attempt <= retries => {
                debug!(url = %segment.url, attempt, error = %error, "segment failed, retrying");
                tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn fetch_segment(
    transport: &dyn Transport,
    segment: &SegmentRef,
    path: &Path,
    isolation: u64,
) -> Result<u64> {
    let mut response = transport
        .get(&segment.url, segment.byte_range, isolation)
        .await?;
    if segment.byte_range.is_some() && response.head.status != 206 {
        bail!(t!(
            "net.segment.range_ignored",
            url = segment.url.to_string()
        ));
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| t!("net.segment.write", path = path.display().to_string()))?;
    let mut written = 0u64;
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        written += chunk.len() as u64;
        file.write_all(&chunk)
            .await
            .with_context(|| t!("net.segment.write", path = path.display().to_string()))?;
    }
    file.flush()
        .await
        .with_context(|| t!("net.segment.write", path = path.display().to_string()))?;
    Ok(written)
}

async fn merge_parts(paths: &[PathBuf], output: &Path) -> Result<()> {
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(t!("net.segment.merge", path = output.display().to_string()))
        })?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".{file_name}.rdl-merge-{}", Uuid::new_v4()));

    let result = write_merged(paths, &temporary).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return result;
    }
    let renamed = tokio::fs::rename(&temporary, output)
        .await
        .with_context(|| t!("net.segment.merge", path = output.display().to_string()));
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    renamed
}

async fn write_merged(paths: &[PathBuf], temporary: &Path) -> Result<()> {
    let mut merged = tokio::fs::File::create(temporary)
        .await
        .with_context(|| t!("net.segment.merge", path = temporary.display().to_string()))?;
    for path in paths {
        let mut part = tokio::fs::File::open(path)
            .await
            .with_context(|| t!("net.segment.merge", path = path.display().to_string()))?;
        tokio::io::copy(&mut part, &mut merged)
            .await
            .with_context(|| t!("net.segment.merge", path = temporary.display().to_string()))?;
    }
    merged
        .flush()
        .await
        .with_context(|| t!("net.segment.merge", path = temporary.display().to_string()))?;
    Ok(())
}

/// Cheap check for callers that want to route manifest URLs here.
pub fn looks_like_manifest(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        MockResource, MockTransport, RecordedRequest, Response, ResponseHead, Transport,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdl-segment-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const MASTER: &str = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000
high.m3u8
";

    const MEDIA: &str = "\
#EXTM3U
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:2,
0.ts
#EXTINF:2,
1.ts
#EXTINF:2,
2.ts
#EXT-X-ENDLIST
";

    struct FlakyTransport {
        inner: Arc<MockTransport>,
        remaining_ts_failures: AtomicUsize,
    }

    impl FlakyTransport {
        fn new(inner: Arc<MockTransport>, failures: usize) -> Self {
            Self {
                inner,
                remaining_ts_failures: AtomicUsize::new(failures),
            }
        }
    }

    #[async_trait]
    impl Transport for FlakyTransport {
        async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
            self.inner.head(url, isolation).await
        }

        async fn get(
            &self,
            url: &Url,
            range: Option<ByteRange>,
            isolation: u64,
        ) -> Result<Response> {
            let is_fragment = url.path().ends_with(".ts");
            if is_fragment && self.remaining_ts_failures.load(Ordering::SeqCst) > 0 {
                self.remaining_ts_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("injected fragment failure"));
            }
            self.inner.get(url, range, isolation).await
        }
    }

    fn hls_transport() -> Arc<MockTransport> {
        Arc::new(
            MockTransport::new()
                .with_chunk_size(3)
                .with_resource(
                    "https://cdn.test/hls/master.m3u8",
                    MockResource::new(MASTER),
                )
                .with_resource("https://cdn.test/hls/high.m3u8", MockResource::new(MEDIA))
                .with_resource("https://cdn.test/hls/init.mp4", MockResource::new("INIT"))
                .with_resource("https://cdn.test/hls/0.ts", MockResource::new("AAA"))
                .with_resource("https://cdn.test/hls/1.ts", MockResource::new("BBB"))
                .with_resource("https://cdn.test/hls/2.ts", MockResource::new("CCC")),
        )
    }

    #[tokio::test]
    async fn resolves_master_and_downloads_in_order() {
        let transport = hls_transport();
        let downloader = SegmentDownloader::new(transport.clone()).with_parallelism(3);
        let dir = temp_dir();
        let output = dir.join("video.ts");
        let url = Url::parse("https://cdn.test/hls/master.m3u8").unwrap();

        let report = downloader.download(&url, &output).await.unwrap();

        assert_eq!(report.segments, 3);
        assert_eq!(report.bytes, 13);
        assert_eq!(std::fs::read(&output).unwrap(), b"INITAAABBBCCC");
        assert!(!dir.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rdl-segments-")
        }));

        let requests = transport.requests();
        let gets: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| matches!(request, RecordedRequest::Get { .. }))
            .collect();
        assert_eq!(gets.len(), 6);
        let mut isolations = Vec::new();
        for request in gets {
            if let RecordedRequest::Get { url, isolation, .. } = request {
                if url.path().ends_with(".ts") {
                    isolations.push(*isolation);
                }
            }
        }
        isolations.sort_unstable();
        isolations.dedup();
        assert_eq!(isolations.len(), 3);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn retries_a_failed_fragment() {
        let transport = Arc::new(FlakyTransport::new(hls_transport(), 1));
        let downloader = SegmentDownloader::new(transport)
            .with_parallelism(1)
            .with_retries(2);
        let dir = temp_dir();
        let output = dir.join("video.ts");
        let url = Url::parse("https://cdn.test/hls/high.m3u8").unwrap();

        downloader.download(&url, &output).await.unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"INITAAABBBCCC");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn gives_up_after_retries_and_leaves_no_work_dir() {
        let transport = Arc::new(FlakyTransport::new(hls_transport(), 100));
        let downloader = SegmentDownloader::new(transport)
            .with_parallelism(1)
            .with_retries(0);
        let dir = temp_dir();
        let output = dir.join("video.ts");
        let url = Url::parse("https://cdn.test/hls/high.m3u8").unwrap();

        assert!(downloader.download(&url, &output).await.is_err());
        assert!(!output.exists());
        assert!(!dir.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rdl-segments-")
        }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn resolves_and_downloads_dash() {
        let mpd = r#"<MPD mediaPresentationDuration="PT4S">
  <Period><AdaptationSet><Representation id="v" bandwidth="9">
    <BaseURL>https://cdn.test/dash/</BaseURL>
    <SegmentTemplate timescale="1" duration="2" initialization="i.mp4" media="s$Number$.m4s"/>
  </Representation></AdaptationSet></Period>
</MPD>"#;
        let transport = Arc::new(
            MockTransport::new()
                .with_resource("https://cdn.test/dash/m.mpd", MockResource::new(mpd))
                .with_resource("https://cdn.test/dash/i.mp4", MockResource::new("I"))
                .with_resource("https://cdn.test/dash/s1.m4s", MockResource::new("1"))
                .with_resource("https://cdn.test/dash/s2.m4s", MockResource::new("2")),
        );
        let downloader = SegmentDownloader::new(transport);
        let dir = temp_dir();
        let output = dir.join("movie.mp4");

        let report = downloader
            .download(&Url::parse("https://cdn.test/dash/m.mpd").unwrap(), &output)
            .await
            .unwrap();
        assert_eq!(report.segments, 2);
        assert_eq!(std::fs::read(&output).unwrap(), b"I12");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_unknown_manifests() {
        let transport = Arc::new(
            MockTransport::new()
                .with_resource("https://cdn.test/file.txt", MockResource::new("hello")),
        );
        let downloader = SegmentDownloader::new(transport);
        let error = downloader
            .resolve(&Url::parse("https://cdn.test/file.txt").unwrap())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("manifest"));
    }

    #[test]
    fn manifest_detection_by_extension() {
        assert!(looks_like_manifest(
            &Url::parse("https://cdn.test/a/index.m3u8?x=1").unwrap()
        ));
        assert!(looks_like_manifest(
            &Url::parse("https://cdn.test/a/manifest.MPD").unwrap()
        ));
        assert!(!looks_like_manifest(
            &Url::parse("https://cdn.test/a/video.mp4").unwrap()
        ));
    }

    #[tokio::test]
    async fn rejects_a_server_that_ignores_ranges() {
        let playlist = "#EXTM3U\n#EXT-X-BYTERANGE:3\nseg.ts\n#EXT-X-ENDLIST\n";
        let mut resource = MockResource::new("ABCDE");
        resource.accepts_ranges = false;
        let transport = Arc::new(
            MockTransport::new()
                .with_resource("https://cdn.test/r/p.m3u8", MockResource::new(playlist))
                .with_resource("https://cdn.test/r/seg.ts", resource),
        );
        let downloader = SegmentDownloader::new(transport).with_retries(0);
        let dir = temp_dir();
        let output = dir.join("video.ts");
        let error = downloader
            .download(&Url::parse("https://cdn.test/r/p.m3u8").unwrap(), &output)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("range"));
        assert!(!output.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn report_is_default_empty() {
        assert_eq!(
            SegmentReport::default(),
            SegmentReport {
                segments: 0,
                bytes: 0
            }
        );
    }
}
