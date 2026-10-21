use std::future::Future;
use std::time::Duration;
use tracing::warn;

/// Configuration for retry attempts with exponential backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of retries (0 = no retries, only 1 attempt).
    pub max_retries: u32,
    /// Initial delay before the first retry.
    pub initial_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
    /// Multiplier for exponential backoff (typically 2).
    pub backoff_factor: u32,
}

impl RetryPolicy {
    pub fn new(max_retries: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            initial_delay,
            max_delay,
            backoff_factor: 2,
        }
    }

    /// Calculates delay before retry attempt `attempt` (1-based: attempt 1 is 1st retry).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let exp = attempt.saturating_sub(1);
        let factor = self.backoff_factor.saturating_pow(exp);
        let calculated = self.initial_delay.saturating_mul(factor);
        calculated.min(self.max_delay)
    }

    /// Executes an async operation with exponential backoff retries.
    pub async fn retry<T, E, F, Fut>(&self, op: F) -> Result<T, E>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display + From<anyhow::Error>,
    {
        self.retry_with_cancel(tokio::sync::Notify::new(), op, |_, _, _| {})
            .await
    }

    /// Executes an async operation with exponential backoff retries, aborting early if cancel is notified.
    /// `on_retry` callback is invoked on failure with (next_attempt_number, delay, error).
    pub async fn retry_with_cancel<T, E, F, Fut, H>(
        &self,
        cancel: impl std::borrow::Borrow<tokio::sync::Notify>,
        mut op: F,
        mut on_retry: H,
    ) -> Result<T, E>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display + From<anyhow::Error>,
        H: FnMut(u32, Duration, &E),
    {
        let notify = cancel.borrow();
        let mut attempt = 0;
        loop {
            tokio::select! {
                _ = notify.notified() => {
                    return Err(E::from(anyhow::anyhow!("Operation cancelled")));
                }
                res = op(attempt) => {
                    match res {
                        Ok(val) => return Ok(val),
                        Err(err) => {
                            if attempt >= self.max_retries {
                                return Err(err);
                            }
                            attempt += 1;
                            let delay = self.delay_for_attempt(attempt);
                            warn!(
                                "Operation failed: {err}. Retrying ({attempt}/{}) after {delay:?}...",
                                self.max_retries
                            );
                            on_retry(attempt, delay, &err);
                            tokio::select! {
                                _ = notify.notified() => {
                                    return Err(E::from(anyhow::anyhow!("Operation cancelled")));
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn delay_growth_and_cap() {
        let policy = RetryPolicy::new(5, Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(policy.delay_for_attempt(0), Duration::ZERO);
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(400));
        assert_eq!(policy.delay_for_attempt(4), Duration::from_millis(500)); // capped
        assert_eq!(policy.delay_for_attempt(5), Duration::from_millis(500)); // capped
    }

    #[tokio::test]
    async fn retry_eventually_succeeds() {
        let policy = RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(5));
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let res = policy
            .retry(|att| {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if att < 2 {
                        Err(anyhow::anyhow!("temporary error"))
                    } else {
                        Ok("success")
                    }
                }
            })
            .await;

        assert_eq!(res.unwrap(), "success");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_fails_when_exhausted() {
        let policy = RetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(5));
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let res: Result<(), anyhow::Error> = policy
            .retry(|_| {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("permanent error"))
                }
            })
            .await;

        assert!(res.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3); // initial attempt + 2 retries
    }

    #[tokio::test]
    async fn retry_cancels_during_sleep() {
        let policy = RetryPolicy::new(3, Duration::from_secs(10), Duration::from_secs(10));
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_c = notify.clone();

        let task = tokio::spawn(async move {
            policy
                .retry_with_cancel(
                    &*notify_c,
                    |_| async { Err::<(), _>(anyhow::anyhow!("fail immediately")) },
                    |_, _, _| {},
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        notify.notify_waiters();

        let res: Result<(), anyhow::Error> = task.await.unwrap();
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("cancelled"));
    }
}
