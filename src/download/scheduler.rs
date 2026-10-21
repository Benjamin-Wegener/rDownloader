//! Job scheduler: limits how many jobs run at the same time, overall and per host.
//!
//! Hosters throttle or ban clients that open too many connections, so jobs for
//! the same host wait for each other while jobs for other hosts keep going.
//! Waiting jobs are served in arrival order.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Held while a job runs; dropping it lets the next waiting job start.
#[derive(Debug)]
pub struct JobPermit {
    // Field order = drop order: free the host slot before the global one.
    _host: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

pub struct Scheduler {
    global: Arc<Semaphore>,
    per_host_limit: usize,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// `0` means unlimited.
fn permits(limit: usize) -> usize {
    if limit == 0 {
        Semaphore::MAX_PERMITS
    } else {
        limit.min(Semaphore::MAX_PERMITS)
    }
}

/// Lower-cased host, so `Example.COM` and `example.com` share one limit.
pub fn host_key(url: &url::Url) -> String {
    url.host_str().unwrap_or("").to_ascii_lowercase()
}

impl Scheduler {
    pub fn new(max_concurrent_jobs: usize, max_jobs_per_host: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(permits(max_concurrent_jobs))),
            per_host_limit: permits(max_jobs_per_host),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Waits until a job for `host` may start.
    ///
    /// The host slot is taken first: a job that waits for its busy host must not
    /// occupy a global slot that a job for another host could use.
    pub async fn acquire(&self, host: &str) -> JobPermit {
        let host_sem = {
            let mut hosts = self.hosts.lock().unwrap();
            hosts
                .entry(host.to_ascii_lowercase())
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_host_limit)))
                .clone()
        };
        // The semaphores are never closed, so acquiring cannot fail.
        let _host = host_sem
            .acquire_owned()
            .await
            .expect("host semaphore closed");
        let _global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .expect("global semaphore closed");
        JobPermit { _host, _global }
    }

    /// Jobs that could start right now without waiting (global limit only).
    pub fn free_slots(&self) -> usize {
        self.global.available_permits()
    }
}

/// Process-wide scheduler, configured from `max_concurrent_jobs` / `max_jobs_per_host`.
pub fn global() -> Arc<Scheduler> {
    static GLOBAL: OnceLock<Arc<Scheduler>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| {
            let cfg = crate::config::config();
            Arc::new(Scheduler::new(
                cfg.max_concurrent_jobs,
                cfg.max_jobs_per_host,
            ))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    const SHORT: Duration = Duration::from_millis(80);

    #[tokio::test]
    async fn global_limit_blocks_until_a_job_finishes() {
        let s = Scheduler::new(2, 0);
        let a = s.acquire("a.test").await;
        let _b = s.acquire("b.test").await;
        assert!(
            timeout(SHORT, s.acquire("c.test")).await.is_err(),
            "third job must wait"
        );
        drop(a);
        assert!(timeout(SHORT, s.acquire("c.test")).await.is_ok());
    }

    #[tokio::test]
    async fn same_host_waits_other_hosts_continue() {
        let s = Scheduler::new(0, 1);
        let a = s.acquire("hoster.test").await;
        assert!(timeout(SHORT, s.acquire("hoster.test")).await.is_err());
        assert!(timeout(SHORT, s.acquire("other.test")).await.is_ok());
        drop(a);
        assert!(timeout(SHORT, s.acquire("hoster.test")).await.is_ok());
    }

    #[tokio::test]
    async fn host_names_are_case_insensitive() {
        let s = Scheduler::new(0, 1);
        let _a = s.acquire("Hoster.TEST").await;
        assert!(timeout(SHORT, s.acquire("hoster.test")).await.is_err());
    }

    #[tokio::test]
    async fn waiting_for_a_busy_host_does_not_hold_a_global_slot() {
        let s = Arc::new(Scheduler::new(2, 1));
        let _a = s.acquire("hoster.test").await;
        let waiter = {
            let s = s.clone();
            tokio::spawn(async move { s.acquire("hoster.test").await })
        };
        tokio::time::sleep(SHORT).await;
        assert_eq!(
            s.free_slots(),
            1,
            "the waiting job must not consume a global slot"
        );
        assert!(timeout(SHORT, s.acquire("other.test")).await.is_ok());
        waiter.abort();
    }

    #[tokio::test]
    async fn zero_means_unlimited() {
        let s = Scheduler::new(0, 0);
        let mut held = Vec::new();
        for _ in 0..50 {
            held.push(
                timeout(SHORT, s.acquire("h.test"))
                    .await
                    .expect("never blocks"),
            );
        }
    }

    #[tokio::test]
    async fn waiters_are_served_in_arrival_order() {
        let s = Arc::new(Scheduler::new(1, 0));
        let first = s.acquire("h.test").await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..3 {
            let (s, order) = (s.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let _p = s.acquire("h.test").await;
                order.lock().unwrap().push(n);
            }));
            tokio::time::sleep(Duration::from_millis(20)).await; // fix arrival order
        }
        drop(first);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn host_key_lowercases() {
        let u = url::Url::parse("https://Dl.Example.COM/f").unwrap();
        assert_eq!(host_key(&u), "dl.example.com");
    }
}
