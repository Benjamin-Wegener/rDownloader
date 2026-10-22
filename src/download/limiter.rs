//! Global bandwidth limit (token bucket shared by all parts of all jobs).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Smallest burst allowance, so tiny rates still let a normal read chunk through.
const MIN_BURST_BYTES: f64 = 16.0 * 1024.0;

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    /// While set, `acquire` waits (a timetable "pause" window).
    paused: std::sync::atomic::AtomicBool,
    /// Bytes per second; 0 = unlimited.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            paused: std::sync::atomic::AtomicBool::new(false),
            rate: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket {
                tokens: burst(bytes_per_sec),
                last: Instant::now(),
            }),
        }
    }

    /// Changes the limit at runtime (0 = unlimited).
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Waits until `n` bytes may be transferred. Callers that arrive while the bucket
    /// is empty queue up behind each other (the bucket goes into debt), so the
    /// long-run throughput never exceeds the rate no matter how many parts run.
    pub async fn acquire(&self, n: u64) {
        while self.is_paused() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let rate = self.rate();
        if rate == 0 || n == 0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock().await;
            let now = Instant::now();
            let refill = now.duration_since(b.last).as_secs_f64() * rate as f64;
            b.tokens = (b.tokens + refill).min(burst(rate));
            b.last = now;
            b.tokens -= n as f64;
            if b.tokens >= 0.0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(-b.tokens / rate as f64)
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

fn burst(rate: u64) -> f64 {
    (rate as f64 / 4.0).max(MIN_BURST_BYTES)
}

/// Process-wide limiter, initialised from `max_speed_kib` in the config.
pub fn global() -> Arc<RateLimiter> {
    static GLOBAL: OnceLock<Arc<RateLimiter>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| {
            Arc::new(RateLimiter::new(
                crate::config::config().max_speed_kib.saturating_mul(1024),
            ))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::new(0);
        let t = Instant::now();
        l.acquire(1 << 30).await;
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn throttles_to_the_rate() {
        // 100 KiB/s: burst is 25 KiB, so 75 KiB more must take about 0.75 s.
        let l = RateLimiter::new(100 * 1024);
        let t = Instant::now();
        for _ in 0..25 {
            l.acquire(4 * 1024).await; // 100 KiB in total
        }
        let e = t.elapsed();
        assert!(e >= Duration::from_millis(600), "too fast: {e:?}");
        assert!(e < Duration::from_millis(1500), "too slow: {e:?}");
    }

    #[tokio::test]
    async fn parallel_callers_share_one_budget() {
        let l = Arc::new(RateLimiter::new(100 * 1024));
        let t = Instant::now();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let l = l.clone();
                tokio::spawn(async move {
                    for _ in 0..5 {
                        l.acquire(4 * 1024).await;
                    }
                })
            })
            .collect();
        for h in tasks {
            h.await.unwrap();
        }
        // 80 KiB total, 25 KiB burst => at least ~0.5 s regardless of parallelism.
        assert!(
            t.elapsed() >= Duration::from_millis(450),
            "{:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn rate_can_be_changed_at_runtime() {
        let l = RateLimiter::new(1024);
        l.set_rate(0);
        assert_eq!(l.rate(), 0);
        let t = Instant::now();
        l.acquire(1 << 20).await;
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn pausing_blocks_until_resumed() {
        let l = Arc::new(RateLimiter::new(0));
        l.set_paused(true);
        let l2 = l.clone();
        let waiter = tokio::spawn(async move { l2.acquire(1).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiter.is_finished(), "must wait while paused");
        l.set_paused(false);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}
