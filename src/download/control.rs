//! Pause, resume and cancel of running downloads.
//!
//! Every job has one [`JobControl`], found by the job id. The queue (window, terminal
//! UI, RPC) only sends commands to it; the engine obeys it: a paused job stops reading
//! between two chunks (connections stay open, the data already on disk is kept), a
//! cancelled job drops its connections. The status shown in the queue follows the
//! engine, never the other way round.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{watch, Notify};
use uuid::Uuid;

/// Why a job was interrupted from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// Connections closed, partial data kept: the download can be resumed later.
    Stop,
    /// Gone for good: the partial data is deleted.
    Cancel,
}

pub struct JobControl {
    paused: watch::Sender<bool>,
    cancelled: AtomicBool,
    stopped: AtomicBool,
    /// Notified on cancel; the retry loops of the parts listen to it.
    pub cancel: Arc<Notify>,
}

impl JobControl {
    fn new() -> Self {
        Self {
            paused: watch::channel(false).0,
            cancelled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            cancel: Arc::new(Notify::new()),
        }
    }

    pub fn pause(&self) {
        self.paused.send_replace(true);
    }

    pub fn resume(&self) {
        self.paused.send_replace(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Closes the connections and keeps the partial data (resumable later).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
        self.resume();
        self.cancel.notify_waiters();
    }

    /// How the job was interrupted, if it was. Cancel wins over stop.
    pub fn interrupt(&self) -> Option<Interrupt> {
        if !self.is_cancelled() {
            return None;
        }
        // `stop` also sets `cancelled`; a later `cancel` clears the stop mark.
        Some(if self.stopped.load(Ordering::SeqCst) {
            Interrupt::Stop
        } else {
            Interrupt::Cancel
        })
    }

    pub fn cancel(&self) {
        self.stopped.store(false, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
        // A paused job must wake up to notice the cancellation.
        self.resume();
        self.cancel.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Returns at once while running; waits while paused. Also returns on cancel.
    pub async fn wait_unpaused(&self) {
        let mut rx = self.paused.subscribe();
        while *rx.borrow_and_update() && !self.is_cancelled() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

fn registry() -> &'static Mutex<HashMap<Uuid, Arc<JobControl>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Uuid, Arc<JobControl>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// The control of job `id`; created on first use.
pub fn get_or_create(id: Uuid) -> Arc<JobControl> {
    registry()
        .lock()
        .unwrap()
        .entry(id)
        .or_insert_with(|| Arc::new(JobControl::new()))
        .clone()
}

pub fn get(id: Uuid) -> Option<Arc<JobControl>> {
    registry().lock().unwrap().get(&id).cloned()
}

/// Forget a finished job.
pub fn remove(id: Uuid) {
    registry().lock().unwrap().remove(&id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn running_job_does_not_wait() {
        let c = JobControl::new();
        tokio::time::timeout(Duration::from_millis(100), c.wait_unpaused())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn paused_job_waits_until_resumed() {
        let c = Arc::new(JobControl::new());
        c.pause();
        assert!(c.is_paused());
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.wait_unpaused().await })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!waiter.is_finished(), "must stay blocked while paused");
        c.resume();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_wakes_a_paused_job() {
        let c = Arc::new(JobControl::new());
        c.pause();
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.wait_unpaused().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        c.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(c.is_cancelled());
    }

    #[test]
    fn registry_returns_the_same_control() {
        let id = Uuid::new_v4();
        assert!(get(id).is_none());
        let a = get_or_create(id);
        let b = get_or_create(id);
        assert!(Arc::ptr_eq(&a, &b));
        a.pause();
        assert!(get(id).unwrap().is_paused());
        remove(id);
        assert!(get(id).is_none());
    }

    #[test]
    fn stop_and_cancel_are_told_apart() {
        let c = JobControl::new();
        assert_eq!(c.interrupt(), None);
        c.stop();
        assert_eq!(c.interrupt(), Some(Interrupt::Stop));
        assert!(c.is_cancelled(), "a stopped job ends like a cancelled one");
        c.cancel();
        assert_eq!(c.interrupt(), Some(Interrupt::Cancel), "cancel wins");
        let d = JobControl::new();
        d.cancel();
        assert_eq!(d.interrupt(), Some(Interrupt::Cancel));
    }
}
