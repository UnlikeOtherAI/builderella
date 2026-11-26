//! Bounded retries with exponential backoff and jitter.

use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant};

/// How often and for how long a request that is safe to repeat is retried.
///
/// A request is retried only after a transient failure (see
/// [`ClientError::is_transient`](crate::ClientError::is_transient)) and only
/// when repeating it cannot change the outcome: reads, object uploads and
/// lookups, job submission (which carries an idempotency key), and cancel,
/// renew and finish, which the controller applies idempotently. Anything
/// else is sent once.
///
/// Both bounds apply: at most `max_attempts` tries, and no try starts later
/// than `max_elapsed` after the first one started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total number of tries, the first one included. `0` and `1` both mean
    /// "never retry".
    pub max_attempts: u32,
    /// Upper bound of the wait before the first retry. It doubles for each
    /// further retry.
    pub initial_backoff: Duration,
    /// Largest wait between two tries.
    pub max_backoff: Duration,
    /// No retry starts once this much time has passed since the first try
    /// began.
    pub max_elapsed: Duration,
}

impl Default for RetryPolicy {
    /// Four tries, waits of up to 250 ms, 500 ms and 1 s, no retry later
    /// than 60 s. That is longer than the default request timeout, so a
    /// request whose answer never came is still repeated.
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            max_elapsed: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Every request is sent exactly once.
    pub fn disabled() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// Whether this policy ever retries.
    pub fn is_enabled(&self) -> bool {
        self.max_attempts > 1
    }

    /// The wait before retry number `retry` (0 for the first retry): a
    /// random duration between half of and the whole exponential step, so
    /// clients that failed together do not come back together.
    pub(crate) fn backoff(&self, retry: u32) -> Duration {
        jittered(exponential(self.initial_backoff, self.max_backoff, retry))
    }
}

/// `initial * 2^step`, capped at `max`.
pub(crate) fn exponential(initial: Duration, max: Duration, step: u32) -> Duration {
    let factor = 1u32.checked_shl(step).unwrap_or(u32::MAX);
    initial.saturating_mul(factor).min(max)
}

/// A random duration in `[step / 2, step]`.
pub(crate) fn jittered(step: Duration) -> Duration {
    let half = step / 2;
    let spread = step.saturating_sub(half).as_nanos();
    if spread == 0 {
        return step;
    }
    let extra = u128::from(random_u64()) % (spread + 1);
    half + Duration::from_nanos(u64::try_from(extra).unwrap_or(u64::MAX))
}

/// Unpredictable enough for jitter; not for anything secret. The standard
/// library seeds every `RandomState` from the operating system and varies
/// it per instance.
fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x9e37_79b9_7f4a_7c15);
    hasher.finish()
}

/// The retry budget of one logical request.
pub(crate) struct Attempts<'a> {
    policy: &'a RetryPolicy,
    started: Instant,
    made: u32,
}

impl<'a> Attempts<'a> {
    pub(crate) fn new(policy: &'a RetryPolicy) -> Self {
        Self {
            policy,
            started: Instant::now(),
            made: 1,
        }
    }

    /// The wait before another try, or `None` when the budget is spent.
    /// Counts the try it grants.
    pub(crate) fn next_delay(&mut self) -> Option<Duration> {
        if self.made >= self.policy.max_attempts {
            return None;
        }
        let delay = self.policy.backoff(self.made - 1);
        if self.started.elapsed().saturating_add(delay) > self.policy.max_elapsed {
            return None;
        }
        self.made += 1;
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_steps_double_and_stop_at_the_cap() {
        let initial = Duration::from_millis(100);
        let max = Duration::from_millis(450);
        assert_eq!(exponential(initial, max, 0), Duration::from_millis(100));
        assert_eq!(exponential(initial, max, 1), Duration::from_millis(200));
        assert_eq!(exponential(initial, max, 2), Duration::from_millis(400));
        assert_eq!(exponential(initial, max, 3), max);
        assert_eq!(exponential(initial, max, 40), max);
        assert_eq!(exponential(initial, max, u32::MAX), max);
    }

    #[test]
    fn jitter_stays_between_half_and_the_whole_step() {
        let step = Duration::from_millis(80);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let wait = jittered(step);
            assert!(wait >= step / 2 && wait <= step, "{wait:?}");
            seen.insert(wait);
        }
        assert!(seen.len() > 1, "jitter never varied");
        assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn attempts_are_bounded_by_count() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_elapsed: Duration::from_secs(60),
        };
        let mut attempts = Attempts::new(&policy);
        assert!(attempts.next_delay().is_some());
        assert!(attempts.next_delay().is_some());
        assert!(attempts.next_delay().is_none());
        assert!(attempts.next_delay().is_none());
    }

    #[test]
    fn attempts_are_bounded_by_elapsed_time() {
        let policy = RetryPolicy {
            max_attempts: 100,
            initial_backoff: Duration::from_secs(10),
            max_backoff: Duration::from_secs(10),
            max_elapsed: Duration::from_secs(1),
        };
        // Even the shortest jittered wait would end after the time bound.
        assert!(Attempts::new(&policy).next_delay().is_none());
    }

    #[test]
    fn a_disabled_policy_never_retries() {
        for policy in [
            RetryPolicy::disabled(),
            RetryPolicy {
                max_attempts: 0,
                ..RetryPolicy::default()
            },
        ] {
            assert!(!policy.is_enabled());
            assert!(Attempts::new(&policy).next_delay().is_none());
        }
        assert!(RetryPolicy::default().is_enabled());
    }
}
