//! Restart single connections that deliver much less than their siblings.
//!
//! Tor circuits sometimes degrade to a trickle without failing. When one part of a
//! multi-part download stays far below the typical part speed, only that part is
//! restarted: it resumes at its saved offset on a **new circuit** (new isolation id).
//!
//! The network must never be flooded, so every restart is rationed:
//! - at least [`Limits::min_parts`] active parts, and the median must be a healthy
//!   speed (if everything is slow, the link or the server is the limit, not a circuit);
//! - a part is slow only below [`Limits::fraction`] of the median for a whole window;
//! - nearly finished parts are left alone;
//! - per part: cool-down that doubles with every restart, at most [`Limits::max_per_part`];
//! - per job: at most one restart per [`Limits::job_gap`];
//! - globally: at most [`GLOBAL_PER_MINUTE`] restarts per minute over all jobs.
//!
//! A paused job or a paused limiter is never measured.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::info;

use super::control::JobControl;

/// Restarts per minute over all jobs together.
pub const GLOBAL_PER_MINUTE: usize = 10;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min_parts: usize,
    /// A part is slow below this fraction of the median.
    pub fraction: f64,
    /// Median speed (bytes/s) below which nothing is restarted.
    pub min_median: f64,
    /// Parts with less than this left to download are not restarted.
    pub near_done: u64,
    /// Length of the measuring window.
    pub window: Duration,
    /// Time between two measurements.
    pub tick: Duration,
    /// Cool-down before the first restart of a part; doubles per restart.
    pub cooldown: Duration,
    pub max_per_part: u32,
    /// Minimum time between two restarts in one job.
    pub job_gap: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_parts: 3,
            fraction: 0.25,
            min_median: 32.0 * 1024.0,
            near_done: 1024 * 1024,
            window: Duration::from_secs(20),
            tick: Duration::from_secs(5),
            cooldown: Duration::from_secs(60),
            max_per_part: 3,
            job_gap: Duration::from_secs(15),
        }
    }
}

/// Limits for the engine; tests use short times.
pub fn current_limits() -> Limits {
    #[cfg(test)]
    {
        Limits {
            window: Duration::from_millis(600),
            tick: Duration::from_millis(100),
            cooldown: Duration::from_millis(400),
            job_gap: Duration::from_millis(200),
            min_median: 1024.0,
            near_done: 0,
            ..Limits::default()
        }
    }
    #[cfg(not(test))]
    Limits::default()
}

/// Speed of one part over a sliding window of byte counter samples.
#[derive(Default)]
pub struct SpeedWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedWindow {
    pub fn push(&mut self, now: Instant, bytes: u64, window: Duration) {
        self.samples.push_back((now, bytes));
        // Keep one sample older than the window as the reference point.
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= window {
            self.samples.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Bytes per second, once the samples span at least `window`.
    pub fn speed(&self, window: Duration) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let span = t1.duration_since(t0);
        (span >= window).then(|| b1.saturating_sub(b0) as f64 / span.as_secs_f64())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartView {
    pub index: usize,
    /// `None` while the window is not full yet.
    pub speed: Option<f64>,
    pub remaining: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PartHistory {
    pub restarts: u32,
    pub last_restart: Option<Instant>,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The part to restart now, if any.
pub fn pick_victim(
    parts: &[PartView],
    history: &[PartHistory],
    now: Instant,
    job_last_restart: Option<Instant>,
    limits: &Limits,
) -> Option<usize> {
    if job_last_restart.is_some_and(|t| now.duration_since(t) < limits.job_gap) {
        return None;
    }
    // Only parts that still have a lot to do and a full measuring window count.
    let active: Vec<&PartView> = parts
        .iter()
        .filter(|p| p.remaining > limits.near_done && p.speed.is_some())
        .collect();
    if active.len() < limits.min_parts {
        return None;
    }
    let mut speeds: Vec<f64> = active.iter().filter_map(|p| p.speed).collect();
    let typical = median(&mut speeds);
    if typical < limits.min_median {
        return None;
    }
    active
        .into_iter()
        .filter(|p| p.speed.is_some_and(|s| s < typical * limits.fraction))
        .filter(|p| {
            let h = history.get(p.index).copied().unwrap_or_default();
            let wait = limits.cooldown * 2u32.saturating_pow(h.restarts.min(6));
            h.restarts < limits.max_per_part
                && h.last_restart.is_none_or(|t| now.duration_since(t) >= wait)
        })
        .min_by(|a, b| {
            a.speed
                .partial_cmp(&b.speed)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|p| p.index)
}

/// Rations restarts over all jobs.
#[derive(Default)]
pub struct GlobalBudget {
    taken: Mutex<VecDeque<Instant>>,
}

impl GlobalBudget {
    pub const fn new() -> Self {
        Self {
            taken: Mutex::new(VecDeque::new()),
        }
    }

    /// Takes one restart from the budget; `false` when this minute's budget is used up.
    pub fn try_take(&self, now: Instant, per_minute: usize) -> bool {
        let mut taken = self.taken.lock().unwrap();
        while taken
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            taken.pop_front();
        }
        if taken.len() >= per_minute {
            return false;
        }
        taken.push_back(now);
        true
    }
}

fn global_budget() -> &'static GlobalBudget {
    static BUDGET: OnceLock<GlobalBudget> = OnceLock::new();
    BUDGET.get_or_init(GlobalBudget::new)
}

/// What the monitor needs to know about one part of a job.
pub struct PartProbe {
    /// Bytes delivered since the job started (live counter).
    pub live: Arc<AtomicU64>,
    /// Bytes this part already had on disk when the job started.
    pub initial: u64,
    pub total: u64,
    /// Wakes the part's task: restart on a new circuit.
    pub restart: Arc<Notify>,
}

/// Watches the parts of one job until it is aborted. Does nothing for a paused job.
pub async fn monitor(probes: Vec<PartProbe>, control: Arc<JobControl>, limits: Limits) {
    let mut windows: Vec<SpeedWindow> = probes.iter().map(|_| SpeedWindow::default()).collect();
    let mut history = vec![PartHistory::default(); probes.len()];
    let mut job_last_restart: Option<Instant> = None;
    loop {
        tokio::time::sleep(limits.tick).await;
        let now = Instant::now();
        if control.is_paused() || super::limiter::global().is_paused() {
            windows.iter_mut().for_each(SpeedWindow::clear);
            continue;
        }
        let views: Vec<PartView> = probes
            .iter()
            .zip(windows.iter_mut())
            .enumerate()
            .map(|(index, (probe, window))| {
                let delivered = probe.live.load(Ordering::Relaxed);
                window.push(now, delivered, limits.window);
                PartView {
                    index,
                    speed: window.speed(limits.window),
                    remaining: probe.total.saturating_sub(probe.initial + delivered),
                }
            })
            .collect();
        if let Some(i) = pick_victim(&views, &history, now, job_last_restart, &limits) {
            if global_budget().try_take(now, GLOBAL_PER_MINUTE) {
                info!("Part {i} delivers far less than the other parts: restarting it on a new circuit");
                probes[i].restart.notify_one();
                history[i].restarts += 1;
                history[i].last_restart = Some(now);
                job_last_restart = Some(now);
                windows[i].clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            near_done: 100,
            ..Limits::default()
        }
    }

    fn view(index: usize, speed: Option<f64>, remaining: u64) -> PartView {
        PartView {
            index,
            speed,
            remaining,
        }
    }

    const MB: f64 = 1_000_000.0;

    #[test]
    fn restarts_the_one_slow_part() {
        let parts = [
            view(0, Some(MB), 1 << 20),
            view(1, Some(0.9 * MB), 1 << 20),
            view(2, Some(1000.0), 1 << 20),
            view(3, Some(1.1 * MB), 1 << 20),
        ];
        let h = vec![PartHistory::default(); 4];
        assert_eq!(
            pick_victim(&parts, &h, Instant::now(), None, &limits()),
            Some(2)
        );
    }

    #[test]
    fn nothing_to_do_when_all_parts_are_alike() {
        let parts: Vec<_> = (0..4).map(|i| view(i, Some(MB), 1 << 20)).collect();
        let h = vec![PartHistory::default(); 4];
        assert_eq!(
            pick_victim(&parts, &h, Instant::now(), None, &limits()),
            None
        );
    }

    #[test]
    fn when_everything_is_slow_nothing_is_restarted() {
        // Median below the healthy floor: the link is the bottleneck, not a circuit.
        let parts: Vec<_> = [10.0, 12.0, 1.0, 11.0]
            .iter()
            .enumerate()
            .map(|(i, s)| view(i, Some(*s), 1 << 20))
            .collect();
        let h = vec![PartHistory::default(); 4];
        assert_eq!(
            pick_victim(&parts, &h, Instant::now(), None, &limits()),
            None
        );
    }

    #[test]
    fn needs_enough_parts_and_a_full_window() {
        let two = [view(0, Some(MB), 1 << 20), view(1, Some(1.0), 1 << 20)];
        assert_eq!(
            pick_victim(
                &two,
                &[PartHistory::default(); 2],
                Instant::now(),
                None,
                &limits()
            ),
            None
        );
        let unmeasured = [
            view(0, Some(MB), 1 << 20),
            view(1, Some(MB), 1 << 20),
            view(2, None, 1 << 20),
        ];
        assert_eq!(
            pick_victim(
                &unmeasured,
                &[PartHistory::default(); 3],
                Instant::now(),
                None,
                &limits()
            ),
            None
        );
    }

    #[test]
    fn nearly_finished_parts_are_left_alone() {
        let parts = [
            view(0, Some(MB), 1 << 20),
            view(1, Some(MB), 1 << 20),
            view(2, Some(MB), 1 << 20),
            view(3, Some(1.0), 50),
        ];
        assert_eq!(
            pick_victim(
                &parts,
                &[PartHistory::default(); 4],
                Instant::now(),
                None,
                &limits()
            ),
            None
        );
    }

    #[test]
    fn cooldown_doubles_and_the_total_is_capped() {
        let l = limits();
        let parts = [
            view(0, Some(MB), 1 << 20),
            view(1, Some(MB), 1 << 20),
            view(2, Some(1.0), 1 << 20),
            view(3, Some(MB), 1 << 20),
        ];
        let t0 = Instant::now();
        let mut h = vec![PartHistory::default(); 4];
        h[2] = PartHistory {
            restarts: 1,
            last_restart: Some(t0),
        };
        assert_eq!(
            pick_victim(&parts, &h, t0 + Duration::from_secs(90), None, &l),
            None,
            "second cool-down is 120 s"
        );
        assert_eq!(
            pick_victim(&parts, &h, t0 + Duration::from_secs(125), None, &l),
            Some(2)
        );
        h[2] = PartHistory {
            restarts: 3,
            last_restart: Some(t0),
        };
        assert_eq!(
            pick_victim(&parts, &h, t0 + Duration::from_secs(10_000), None, &l),
            None,
            "at most three restarts per part"
        );
    }

    #[test]
    fn one_restart_at_a_time_per_job() {
        let parts = [
            view(0, Some(MB), 1 << 20),
            view(1, Some(MB), 1 << 20),
            view(2, Some(1.0), 1 << 20),
            view(3, Some(MB), 1 << 20),
        ];
        let h = vec![PartHistory::default(); 4];
        let now = Instant::now();
        assert_eq!(
            pick_victim(
                &parts,
                &h,
                now,
                Some(now - Duration::from_secs(5)),
                &limits()
            ),
            None
        );
        assert_eq!(
            pick_victim(
                &parts,
                &h,
                now,
                Some(now - Duration::from_secs(20)),
                &limits()
            ),
            Some(2)
        );
    }

    #[test]
    fn the_global_budget_is_rationed() {
        let b = GlobalBudget::new();
        let t = Instant::now();
        for _ in 0..3 {
            assert!(b.try_take(t, 3));
        }
        assert!(!b.try_take(t, 3), "budget used up");
        assert!(!b.try_take(t + Duration::from_secs(59), 3));
        assert!(
            b.try_take(t + Duration::from_secs(61), 3),
            "a minute later it refills"
        );
    }

    #[test]
    fn speed_window_needs_a_full_window() {
        let mut w = SpeedWindow::default();
        let t = Instant::now();
        let win = Duration::from_secs(20);
        w.push(t, 0, win);
        w.push(t + Duration::from_secs(10), 1000, win);
        assert_eq!(w.speed(win), None, "only 10 s measured");
        w.push(t + Duration::from_secs(20), 4000, win);
        assert_eq!(w.speed(win), Some(200.0));
        w.push(t + Duration::from_secs(30), 5000, win);
        assert_eq!(w.speed(win), Some(200.0), "20 s window: 1000 -> 5000");
        w.clear();
        assert_eq!(w.speed(win), None);
    }
}
