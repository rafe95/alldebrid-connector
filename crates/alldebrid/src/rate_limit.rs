//! Client-side rate limiting.
//!
//! AllDebrid allows 12 requests per second and 600 requests per minute.
//! [`RateLimit`] enforces both with two token buckets shared by every clone of
//! a [`Client`](crate::Client).

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Rate limit configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimit {
    /// Maximum requests per second.
    pub per_second: u32,
    /// Maximum requests per minute.
    pub per_minute: u32,
}

impl Default for RateLimit {
    /// The limits documented by AllDebrid: 12/s and 600/min.
    fn default() -> Self {
        Self {
            per_second: 12,
            per_minute: 600,
        }
    }
}

#[derive(Debug)]
struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
}

impl Bucket {
    fn new(capacity: u32, window: Duration) -> Self {
        let capacity = f64::from(capacity.max(1));
        Self {
            capacity,
            refill_per_sec: capacity / window.as_secs_f64(),
            tokens: capacity,
        }
    }

    fn refill(&mut self, elapsed: Duration) {
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.refill_per_sec).min(self.capacity);
    }

    /// Time until one token is available (zero if available now).
    fn wait_time(&self) -> Duration {
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / self.refill_per_sec)
        }
    }
}

#[derive(Debug)]
struct State {
    buckets: [Bucket; 2],
    last: Instant,
}

/// Shared limiter. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    state: Mutex<State>,
}

impl RateLimiter {
    pub(crate) fn new(cfg: &RateLimit) -> Self {
        Self {
            state: Mutex::new(State {
                buckets: [
                    Bucket::new(cfg.per_second, Duration::from_secs(1)),
                    Bucket::new(cfg.per_minute, Duration::from_secs(60)),
                ],
                last: Instant::now(),
            }),
        }
    }

    /// Wait until a request may be sent, then consume one token.
    pub(crate) async fn acquire(&self) {
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(st.last);
                st.last = now;
                for b in &mut st.buckets {
                    b.refill(elapsed);
                }
                let wait = st
                    .buckets
                    .iter()
                    .map(Bucket::wait_time)
                    .max()
                    .unwrap_or_default();
                if wait.is_zero() {
                    for b in &mut st.buckets {
                        b.tokens -= 1.0;
                    }
                    return;
                }
                wait
            };
            tracing::trace!(?wait, "rate limit reached, waiting");
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn enforces_per_second_limit() {
        let rl = RateLimiter::new(&RateLimit {
            per_second: 3,
            per_minute: 1000,
        });
        let start = Instant::now();
        for _ in 0..3 {
            rl.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "burst of 3 is free");
        rl.acquire().await;
        let e = start.elapsed();
        assert!(
            e >= Duration::from_millis(330) && e < Duration::from_millis(400),
            "{e:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn enforces_per_minute_limit() {
        let rl = RateLimiter::new(&RateLimit {
            per_second: 1000,
            per_minute: 2,
        });
        let start = Instant::now();
        rl.acquire().await;
        rl.acquire().await;
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_secs(29));
    }
}
