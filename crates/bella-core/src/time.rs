//! Wall-clock timestamps and an injectable clock.
//!
//! Wall time is used for user-facing deadlines and anything that crosses
//! machines. Local elapsed intervals use `std::time::Instant` at the call
//! site. An old wall-clock timestamp is never proof that a lease is fresh.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

impl UnixMillis {
    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(d.as_millis() as u64)
    }

    pub fn plus(self, d: Duration) -> Self {
        Self(self.0.saturating_add(d.as_millis() as u64))
    }

    pub fn plus_secs(self, secs: u64) -> Self {
        Self(self.0.saturating_add(secs.saturating_mul(1000)))
    }

    pub fn minus(self, d: Duration) -> Self {
        Self(self.0.saturating_sub(d.as_millis() as u64))
    }

    /// Time elapsed since `earlier`; zero when `earlier` is in the future.
    pub fn since(self, earlier: UnixMillis) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> UnixMillis;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixMillis {
        UnixMillis::now()
    }
}

/// Test clock that only moves when told to.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn at(start: UnixMillis) -> Self {
        Self(Arc::new(AtomicU64::new(start.0)))
    }

    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
    }

    pub fn set(&self, t: UnixMillis) {
        self.0.store(t.0, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixMillis {
        UnixMillis(self.0.load(Ordering::SeqCst))
    }
}
