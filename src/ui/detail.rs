//! Detail view of one download: parts, speed, ETA and Tor circuit health.
//! Everything here is pure (no terminal access) so it can be unit-tested.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::fmt_bytes;
use crate::contract::JobId;
use crate::download::{JobProgress, JobStatus, PartProgress, PartStatus};
use crate::t;
use crate::tor::TorHealth;

/// Minimum time between two speed samples; shorter intervals are too noisy.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
/// Weight of the newest sample in the moving average.
const NEW_SAMPLE_WEIGHT: f64 = 0.3;

struct Sample {
    bytes: u64,
    at: Instant,
    /// Smoothed bytes per second.
    speed: f64,
}

/// Derives a per-job speed from progress snapshots (exponential moving average).
#[derive(Default)]
pub struct SpeedTracker {
    samples: HashMap<JobId, Sample>,
    /// Recent smoothed speeds per job, one entry per accepted sample.
    history: HashMap<JobId, VecDeque<u64>>,
}

/// How many samples the speed history keeps (about one minute).
pub const HISTORY_LEN: usize = 120;

impl SpeedTracker {
    pub fn observe(&mut self, id: JobId, bytes_done: u64, now: Instant) {
        let Some(s) = self.samples.get_mut(&id) else {
            self.samples.insert(
                id,
                Sample {
                    bytes: bytes_done,
                    at: now,
                    speed: 0.0,
                },
            );
            return;
        };
        if bytes_done < s.bytes {
            // Job restarted (e.g. after a retry from scratch): start over.
            *s = Sample {
                bytes: bytes_done,
                at: now,
                speed: 0.0,
            };
            return;
        }
        let dt = now.duration_since(s.at);
        if dt < MIN_SAMPLE_INTERVAL {
            return;
        }
        let instant = (bytes_done - s.bytes) as f64 / dt.as_secs_f64();
        s.speed = if s.speed == 0.0 {
            instant
        } else {
            (1.0 - NEW_SAMPLE_WEIGHT) * s.speed + NEW_SAMPLE_WEIGHT * instant
        };
        s.bytes = bytes_done;
        s.at = now;
        let speed = s.speed as u64;
        let h = self.history.entry(id).or_default();
        if h.len() == HISTORY_LEN {
            h.pop_front();
        }
        h.push_back(speed);
    }

    /// The recent speeds of a job, oldest first.
    pub fn history(&self, id: JobId) -> Vec<u64> {
        self.history
            .get(&id)
            .map_or_else(Vec::new, |h| h.iter().copied().collect())
    }

    /// A speed reported by the job itself (`JobEvent::Speed`) wins over the estimate.
    pub fn set_reported(&mut self, id: JobId, bytes_per_sec: u64, now: Instant) {
        let s = self.samples.entry(id).or_insert(Sample {
            bytes: 0,
            at: now,
            speed: 0.0,
        });
        s.speed = bytes_per_sec as f64;
    }

    pub fn speed(&self, id: JobId) -> u64 {
        self.samples.get(&id).map_or(0, |s| s.speed as u64)
    }

    pub fn forget(&mut self, id: JobId) {
        self.samples.remove(&id);
        self.history.remove(&id);
    }
}

/// Seconds until the download is done, or `None` if the speed is unknown.
pub fn eta_secs(remaining: u64, bytes_per_sec: u64) -> Option<u64> {
    (bytes_per_sec > 0).then(|| remaining.div_ceil(bytes_per_sec))
}

pub fn fmt_eta(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

pub fn fmt_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", fmt_bytes(bytes_per_sec))
}

pub(crate) fn job_status_label(status: &JobStatus) -> String {
    match status {
        JobStatus::Queued => t!("ui.job.queued"),
        JobStatus::Resolving => t!("ui.job.resolving"),
        JobStatus::Downloading => t!("ui.job.downloading"),
        JobStatus::Merging => t!("ui.job.merging"),
        JobStatus::Done => t!("ui.job.done"),
        JobStatus::Paused => t!("ui.job.paused"),
        JobStatus::Cancelled => t!("ui.job.cancelled"),
        JobStatus::Failed(e) => t!("ui.job.failed", error = e),
    }
}

fn part_status_label(status: &PartStatus) -> String {
    match status {
        PartStatus::Pending => t!("ui.part.pending"),
        PartStatus::Downloading => t!("ui.part.downloading"),
        PartStatus::Retrying { attempt, max } => {
            t!("ui.part.retrying", attempt = attempt, max = max)
        }
        PartStatus::Done => t!("ui.part.done"),
        PartStatus::Failed(e) => t!("ui.part.failed", error = e),
    }
}

fn part_line(p: &PartProgress) -> String {
    let pct = (p.bytes_done * 100).checked_div(p.total).unwrap_or(0);
    t!(
        "ui.detail.part_line",
        index = p.index + 1,
        done = fmt_bytes(p.bytes_done),
        total = fmt_bytes(p.total),
        pct = pct,
        status = part_status_label(&p.status)
    )
}

/// One-line summary of the Tor connection, or "unknown" without Tor (e.g. `--no-tor`).
pub fn circuit_line(health: Option<&TorHealth>) -> String {
    match health {
        None => t!("ui.detail.circuit_unknown"),
        Some(h) if h.is_healthy() => t!("ui.detail.circuit_ready"),
        Some(h) => match h.blockage() {
            Some(reason) => t!("ui.detail.circuit_blocked", reason = reason),
            None => t!(
                "ui.detail.circuit_bootstrapping",
                percent = h.bootstrap_percent()
            ),
        },
    }
}

pub fn detail_text(job: &JobProgress, bytes_per_sec: u64, health: Option<&TorHealth>) -> String {
    let active = matches!(job.status, JobStatus::Downloading);
    let eta = if active {
        eta_secs(
            job.total_bytes.saturating_sub(job.bytes_done),
            bytes_per_sec,
        )
        .map_or_else(|| t!("ui.detail.unknown"), fmt_eta)
    } else {
        t!("ui.detail.unknown")
    };
    let speed = if active {
        fmt_speed(bytes_per_sec)
    } else {
        t!("ui.detail.unknown")
    };

    let mut lines = vec![
        t!("ui.detail.url", url = &job.url),
        t!("ui.detail.status", status = job_status_label(&job.status)),
        t!("ui.detail.speed_eta", speed = speed, eta = eta),
        t!("ui.detail.circuit", circuit = circuit_line(health)),
        t!("ui.detail.parts_header", count = job.parts.len()),
    ];
    lines.extend(job.parts.iter().map(part_line));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn job(parts: Vec<PartProgress>, status: JobStatus) -> JobProgress {
        JobProgress {
            id: Uuid::new_v4(),
            url: "http://x.test/a.bin".into(),
            filename: "a.bin".into(),
            total_bytes: 1000,
            bytes_done: 400,
            parts,
            status,
        }
    }

    #[test]
    fn eta_math() {
        assert_eq!(eta_secs(1000, 100), Some(10));
        assert_eq!(eta_secs(1001, 100), Some(11));
        assert_eq!(eta_secs(1000, 0), None);
        assert_eq!(eta_secs(0, 100), Some(0));
    }

    #[test]
    fn speed_history_is_kept_and_capped() {
        let (mut tr, id, t0) = (SpeedTracker::default(), Uuid::new_v4(), Instant::now());
        for i in 0..(HISTORY_LEN as u64 + 20) {
            tr.observe(id, i * 1000, t0 + Duration::from_secs(i));
        }
        let h = tr.history(id);
        assert_eq!(h.len(), HISTORY_LEN);
        assert!(h.iter().all(|v| *v > 0));
        tr.forget(id);
        assert!(tr.history(id).is_empty());
    }

    #[test]
    fn eta_formatting() {
        assert_eq!(fmt_eta(42), "42s");
        assert_eq!(fmt_eta(185), "3m 05s");
        assert_eq!(fmt_eta(3_725), "1h 02m");
    }

    #[test]
    fn speed_is_estimated_and_smoothed() {
        let (mut tr, id, t0) = (SpeedTracker::default(), Uuid::new_v4(), Instant::now());
        tr.observe(id, 0, t0);
        tr.observe(id, 1000, t0 + Duration::from_millis(100)); // too soon: ignored
        assert_eq!(tr.speed(id), 0);
        tr.observe(id, 2000, t0 + Duration::from_secs(1));
        assert_eq!(tr.speed(id), 2000);
        tr.observe(id, 2000, t0 + Duration::from_secs(2)); // stalled: average decays
        assert_eq!(tr.speed(id), 1400);
    }

    #[test]
    fn speed_resets_when_progress_goes_backwards() {
        let (mut tr, id, t0) = (SpeedTracker::default(), Uuid::new_v4(), Instant::now());
        tr.observe(id, 5000, t0);
        tr.observe(id, 9000, t0 + Duration::from_secs(1));
        assert!(tr.speed(id) > 0);
        tr.observe(id, 0, t0 + Duration::from_secs(2));
        assert_eq!(tr.speed(id), 0);
    }

    #[test]
    fn reported_speed_wins_and_forget_clears() {
        let (mut tr, id, t0) = (SpeedTracker::default(), Uuid::new_v4(), Instant::now());
        tr.set_reported(id, 777, t0);
        assert_eq!(tr.speed(id), 777);
        tr.forget(id);
        assert_eq!(tr.speed(id), 0);
    }

    #[test]
    fn detail_shows_parts_speed_and_eta() {
        let parts = vec![
            PartProgress {
                index: 0,
                bytes_done: 500,
                total: 500,
                status: PartStatus::Done,
            },
            PartProgress {
                index: 1,
                bytes_done: 100,
                total: 500,
                status: PartStatus::Retrying { attempt: 2, max: 3 },
            },
        ];
        let text = detail_text(&job(parts, JobStatus::Downloading), 200, None);
        assert!(text.contains("http://x.test/a.bin"));
        assert!(text.contains("200 B/s"), "{text}");
        assert!(text.contains("3s"), "{text}"); // 600 remaining / 200 per second
        assert!(text.contains("Part 2"), "{text}");
        assert!(text.contains("20%"), "{text}");
        assert!(text.contains("2/3"), "{text}");
    }

    #[test]
    fn inactive_job_shows_no_speed_or_eta() {
        let text = detail_text(&job(vec![], JobStatus::Paused), 999, None);
        assert!(!text.contains("999"), "{text}");
    }

    #[test]
    fn circuit_without_tor_is_unknown() {
        assert_eq!(circuit_line(None), t!("ui.detail.circuit_unknown"));
    }
}
