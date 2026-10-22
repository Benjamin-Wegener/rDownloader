//! Detects connections that are alive but too slow to be useful.
//!
//! Tor circuits sometimes degrade to a trickle without ever failing. Dropping such a
//! part makes the retry loop build a fresh circuit. Time spent waiting for our own
//! speed limiter or disk is *not* counted: only time spent waiting for the network.

use std::time::Duration;

/// How long (of network waiting time) speed is averaged before it is judged.
pub const WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub struct TooSlow {
    pub bytes_per_sec: u64,
    pub minimum: u64,
}

pub struct SlowGuard {
    min_bytes_per_sec: u64,
    window: Duration,
    waited: Duration,
    bytes: u64,
}

impl SlowGuard {
    /// `min_bytes_per_sec == 0` disables the check.
    pub fn new(min_bytes_per_sec: u64, window: Duration) -> Self {
        Self {
            min_bytes_per_sec,
            window,
            waited: Duration::ZERO,
            bytes: 0,
        }
    }

    /// Records a chunk of `bytes` that took `waited` of network time to arrive.
    pub fn record(&mut self, bytes: u64, waited: Duration) -> Result<(), TooSlow> {
        if self.min_bytes_per_sec == 0 {
            return Ok(());
        }
        self.bytes += bytes;
        self.waited += waited;
        if self.waited < self.window {
            return Ok(());
        }
        let bytes_per_sec = (self.bytes as f64 / self.waited.as_secs_f64()) as u64;
        self.bytes = 0;
        self.waited = Duration::ZERO;
        if bytes_per_sec < self.min_bytes_per_sec {
            return Err(TooSlow {
                bytes_per_sec,
                minimum: self.min_bytes_per_sec,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(10);

    #[test]
    fn fast_connection_passes_every_window() {
        let mut g = SlowGuard::new(1024, W);
        for _ in 0..5 {
            assert_eq!(g.record(500_000, Duration::from_secs(5)), Ok(()));
            assert_eq!(g.record(500_000, Duration::from_secs(5)), Ok(()));
        }
    }

    #[test]
    fn trickle_is_reported_after_the_window() {
        let mut g = SlowGuard::new(1024, W);
        assert_eq!(
            g.record(100, Duration::from_secs(6)),
            Ok(()),
            "window not over yet"
        );
        let err = g.record(100, Duration::from_secs(6)).unwrap_err();
        assert_eq!(
            err,
            TooSlow {
                bytes_per_sec: 16,
                minimum: 1024
            }
        );
    }

    #[test]
    fn a_good_window_resets_a_bad_start() {
        let mut g = SlowGuard::new(1024, W);
        // First window is fine, the next one starts from zero.
        assert_eq!(g.record(1_000_000, Duration::from_secs(10)), Ok(()));
        assert_eq!(g.record(10, Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn zero_minimum_disables_the_check() {
        let mut g = SlowGuard::new(0, W);
        assert_eq!(g.record(0, Duration::from_secs(3600)), Ok(()));
    }
}
