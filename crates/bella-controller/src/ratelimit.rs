//! Rate limiting for `POST /enroll`.

use crate::config::EnrollLimits;
use bella_core::UnixMillis;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

/// Sources tracked individually. Beyond this, sources share one bucket, so
/// an attacker with many addresses cannot grow the table without bound and
/// is still held by the global limit.
const MAX_SOURCES: usize = 4096;

/// A token bucket holding at most `burst` attempts, refilled at one attempt
/// per `refill`.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    /// Thousandths of an attempt.
    milli: u64,
    updated: UnixMillis,
}

impl Bucket {
    fn full(burst: u32, now: UnixMillis) -> Self {
        Self {
            milli: u64::from(burst) * 1000,
            updated: now,
        }
    }

    fn refill(&mut self, burst: u32, refill: Duration, now: UnixMillis) {
        let elapsed = now.since(self.updated).as_millis();
        let per_attempt = refill.as_millis().max(1);
        let gained = elapsed.saturating_mul(1000) / per_attempt;
        let cap = u64::from(burst) * 1000;
        self.milli = u64::try_from(u128::from(self.milli).saturating_add(gained))
            .unwrap_or(u64::MAX)
            .min(cap);
        // A clock that steps backwards must not hand out attempts.
        if gained > 0 || now < self.updated {
            self.updated = now;
        }
    }

    fn is_full(&self, burst: u32) -> bool {
        self.milli >= u64::from(burst) * 1000
    }

    /// Seconds until one whole attempt is available, at least one.
    fn wait_secs(&self, refill: Duration) -> u64 {
        let missing = 1000u64.saturating_sub(self.milli);
        let millis = u128::from(missing) * refill.as_millis().max(1) / 1000;
        u64::try_from(millis.div_ceil(1000))
            .unwrap_or(u64::MAX)
            .max(1)
    }
}

/// The key a source address is limited by: the address itself for IPv4 and
/// the /64 prefix for IPv6, where one host commonly owns the whole prefix.
fn source_key(addr: IpAddr) -> [u8; 9] {
    let mut key = [0u8; 9];
    match addr.to_canonical() {
        IpAddr::V4(v4) => {
            key[0] = 4;
            key[1..5].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            key[0] = 6;
            key[1..9].copy_from_slice(&v6.octets()[..8]);
        }
    }
    key
}

/// Per-source and fleet-wide limits on enrollment attempts. See
/// [`EnrollLimits`] for the policy.
#[derive(Debug)]
pub(crate) struct EnrollLimiter {
    limits: EnrollLimits,
    global: Option<Bucket>,
    sources: HashMap<[u8; 9], Bucket>,
    overflow: Option<Bucket>,
}

impl EnrollLimiter {
    pub(crate) fn new(limits: EnrollLimits) -> Self {
        Self {
            limits,
            global: None,
            sources: HashMap::new(),
            overflow: None,
        }
    }

    /// Count one attempt from `source`. `Err` carries the number of seconds
    /// after which another attempt can succeed; a refused attempt consumes
    /// nothing.
    pub(crate) fn check(&mut self, source: IpAddr, now: UnixMillis) -> Result<(), u64> {
        let limits = self.limits;
        let key = source_key(source);
        if !self.sources.contains_key(&key) && self.sources.len() >= MAX_SOURCES {
            // Forget sources that are back at their full allowance; they
            // are indistinguishable from sources never seen.
            self.sources.retain(|_, bucket| {
                bucket.refill(limits.per_source_burst, limits.per_source_refill, now);
                !bucket.is_full(limits.per_source_burst)
            });
        }
        let tracked = self.sources.contains_key(&key) || self.sources.len() < MAX_SOURCES;
        let source_bucket = if tracked {
            self.sources
                .entry(key)
                .or_insert_with(|| Bucket::full(limits.per_source_burst, now))
        } else {
            self.overflow
                .get_or_insert_with(|| Bucket::full(limits.per_source_burst, now))
        };
        source_bucket.refill(limits.per_source_burst, limits.per_source_refill, now);
        if source_bucket.milli < 1000 {
            return Err(source_bucket.wait_secs(limits.per_source_refill));
        }
        let global = self
            .global
            .get_or_insert_with(|| Bucket::full(limits.global_burst, now));
        global.refill(limits.global_burst, limits.global_refill, now);
        if global.milli < 1000 {
            return Err(global.wait_secs(limits.global_refill));
        }
        global.milli -= 1000;
        source_bucket.milli -= 1000;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn limits() -> EnrollLimits {
        EnrollLimits {
            per_source_burst: 3,
            per_source_refill: Duration::from_secs(10),
            global_burst: 5,
            global_refill: Duration::from_secs(2),
        }
    }

    fn v4(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn a_source_gets_its_burst_then_one_attempt_per_refill() {
        let mut limiter = EnrollLimiter::new(limits());
        let t0 = UnixMillis(1_000_000);
        for _ in 0..3 {
            assert_eq!(limiter.check(v4(1), t0), Ok(()));
        }
        assert_eq!(limiter.check(v4(1), t0), Err(10));
        assert_eq!(limiter.check(v4(1), t0.plus_secs(9)), Err(1));
        assert_eq!(limiter.check(v4(1), t0.plus_secs(10)), Ok(()));
        assert!(limiter.check(v4(1), t0.plus_secs(10)).is_err());
        // Another source is not affected.
        assert_eq!(limiter.check(v4(2), t0.plus_secs(10)), Ok(()));
    }

    #[test]
    fn the_global_limit_holds_across_sources() {
        let mut limiter = EnrollLimiter::new(limits());
        let t0 = UnixMillis(1_000_000);
        for source in 0..5 {
            assert_eq!(limiter.check(v4(source), t0), Ok(()));
        }
        assert_eq!(limiter.check(v4(200), t0), Err(2));
        // The refused attempt did not use up the new source's allowance.
        let later = t0.plus_secs(6);
        for _ in 0..3 {
            assert_eq!(limiter.check(v4(200), later), Ok(()));
        }
    }

    #[test]
    fn ipv6_sources_share_a_bucket_per_64_prefix() {
        let mut limiter = EnrollLimiter::new(limits());
        let t0 = UnixMillis(1_000_000);
        let host = |n: u16| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, n));
        for n in 0..3 {
            assert_eq!(limiter.check(host(n), t0), Ok(()));
        }
        assert!(limiter.check(host(99), t0).is_err());
        let other = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1));
        assert_eq!(limiter.check(other, t0), Ok(()));
    }

    #[test]
    fn the_table_of_sources_is_bounded() {
        let mut limiter = EnrollLimiter::new(EnrollLimits {
            per_source_burst: 1,
            per_source_refill: Duration::from_secs(3600),
            global_burst: u32::MAX,
            global_refill: Duration::from_millis(1),
        });
        let t0 = UnixMillis(1_000_000);
        for n in 0..(MAX_SOURCES as u32 + 50) {
            let _ = limiter.check(IpAddr::V4(Ipv4Addr::from(n)), t0);
        }
        assert!(limiter.sources.len() <= MAX_SOURCES);
        // Sources beyond the table share one allowance.
        assert!(
            limiter
                .check(IpAddr::V4(Ipv4Addr::from(900_000u32)), t0)
                .is_err()
        );
    }

    #[test]
    fn a_clock_stepping_back_grants_nothing() {
        let mut limiter = EnrollLimiter::new(limits());
        let t0 = UnixMillis(1_000_000);
        for _ in 0..3 {
            limiter.check(v4(1), t0).unwrap();
        }
        assert!(limiter.check(v4(1), UnixMillis(10)).is_err());
        assert!(limiter.check(v4(1), UnixMillis(10).plus_secs(9)).is_err());
        assert!(limiter.check(v4(1), UnixMillis(10).plus_secs(10)).is_ok());
    }
}
