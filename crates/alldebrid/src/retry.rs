//! Retry policy for transient failures.

use std::time::Duration;

/// How transient failures (network errors, HTTP 429/502/503/504, `NO_SERVER`)
/// are retried. Delays grow exponentially: `base_delay * 2^attempt`, capped at
/// `max_delay`. A `Retry-After` header always takes precedence.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Retries after the first attempt. `0` disables retrying.
    pub max_retries: u32,
    /// Delay before the first retry.
    pub base_delay: Duration,
    /// Upper bound for a single delay.
    pub max_delay: Duration,
    /// Randomize each delay between 50% and 100% of its value.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Never retry.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    /// Delay before retry number `attempt` (0-based).
    pub(crate) fn delay(&self, attempt: u32) -> Duration {
        let exp = self
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt.min(16)));
        let d = exp.min(self.max_delay);
        if self.jitter {
            d.mul_f64(0.5 + fastrand::f64() * 0.5)
        } else {
            d
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_and_capped() {
        let p = RetryPolicy {
            jitter: false,
            ..RetryPolicy::default()
        };
        assert_eq!(p.delay(0), Duration::from_millis(500));
        assert_eq!(p.delay(1), Duration::from_secs(1));
        assert_eq!(p.delay(2), Duration::from_secs(2));
        assert_eq!(p.delay(10), Duration::from_secs(10));
        assert_eq!(p.delay(u32::MAX), Duration::from_secs(10));
    }

    #[test]
    fn jitter_stays_in_range() {
        let p = RetryPolicy::default();
        for _ in 0..100 {
            let d = p.delay(1);
            assert!(d >= Duration::from_millis(500) && d <= Duration::from_secs(1));
        }
    }
}
