//! Environment leases.
//!
//! An environment has an explicit owner, a renewable lease, an idle timeout
//! and a hard maximum lifetime. A health probe is not evidence that someone
//! still needs it, and an open socket never extends it.

use crate::job::LeaseSpec;
use crate::time::UnixMillis;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub spec: LeaseSpec,
    pub created_at: UnixMillis,
    /// Last real interaction or explicit keepalive.
    pub last_activity_at: UnixMillis,
    /// End of the current renewable lease.
    pub lease_until: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEnd {
    IdleTimeout,
    LeaseExpired,
    MaxLifetime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RenewError {
    /// The environment already reached its absolute maximum lifetime.
    MaxLifetimeReached {
        hard_deadline: UnixMillis,
    },
    AlreadyExpired {
        reason: LeaseEnd,
    },
}

impl Lease {
    pub fn start(spec: LeaseSpec, now: UnixMillis) -> Self {
        let mut lease = Self {
            spec,
            created_at: now,
            last_activity_at: now,
            lease_until: now,
        };
        lease.lease_until = lease.clamp(now.plus_secs(spec.lease_secs));
        lease
    }

    /// The absolute deadline no renewal can pass.
    pub fn hard_deadline(&self) -> UnixMillis {
        self.created_at.plus_secs(self.spec.max_lifetime_secs)
    }

    fn clamp(&self, t: UnixMillis) -> UnixMillis {
        t.min(self.hard_deadline())
    }

    /// The moment this environment ends if nothing else happens.
    pub fn effective_deadline(&self) -> UnixMillis {
        let idle = self.last_activity_at.plus_secs(self.spec.idle_timeout_secs);
        self.lease_until.min(idle).min(self.hard_deadline())
    }

    /// Why the lease has ended as of `now`, if it has.
    pub fn ended(&self, now: UnixMillis) -> Option<LeaseEnd> {
        if now >= self.hard_deadline() {
            Some(LeaseEnd::MaxLifetime)
        } else if now >= self.lease_until {
            Some(LeaseEnd::LeaseExpired)
        } else if now >= self.last_activity_at.plus_secs(self.spec.idle_timeout_secs) {
            Some(LeaseEnd::IdleTimeout)
        } else {
            None
        }
    }

    /// Explicit renewal by the owner. Returns the new effective deadline,
    /// which is only ever reported after it has been applied.
    pub fn renew(&mut self, now: UnixMillis) -> Result<UnixMillis, RenewError> {
        match self.ended(now) {
            Some(LeaseEnd::MaxLifetime) => Err(RenewError::MaxLifetimeReached {
                hard_deadline: self.hard_deadline(),
            }),
            Some(reason) => Err(RenewError::AlreadyExpired { reason }),
            None => {
                self.last_activity_at = now;
                self.lease_until = self.clamp(now.plus_secs(self.spec.lease_secs));
                Ok(self.effective_deadline())
            }
        }
    }

    /// Real interaction resets the idle timer but does not extend the lease.
    pub fn touch(&mut self, now: UnixMillis) {
        if self.ended(now).is_none() {
            self.last_activity_at = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LeaseSpec {
        LeaseSpec {
            idle_timeout_secs: 900,
            lease_secs: 1800,
            max_lifetime_secs: 14_400,
        }
    }

    #[test]
    fn idle_timeout_ends_an_unused_environment() {
        let lease = Lease::start(spec(), UnixMillis(0));
        assert_eq!(lease.ended(UnixMillis(899_000)), None);
        assert_eq!(
            lease.ended(UnixMillis(900_000)),
            Some(LeaseEnd::IdleTimeout)
        );
    }

    #[test]
    fn touching_does_not_extend_the_lease() {
        let mut lease = Lease::start(spec(), UnixMillis(0));
        for minute in 1..30 {
            lease.touch(UnixMillis(minute * 60_000));
        }
        assert_eq!(
            lease.ended(UnixMillis(1_800_000)),
            Some(LeaseEnd::LeaseExpired)
        );
    }

    #[test]
    fn renewal_cannot_pass_the_hard_deadline() {
        let mut lease = Lease::start(spec(), UnixMillis(0));
        let mut now = UnixMillis(0);
        while lease.ended(now).is_none() {
            now = now.plus_secs(600);
            if lease.renew(now).is_err() {
                break;
            }
        }
        assert!(now.0 >= 14_400_000);
        assert!(matches!(
            lease.renew(now),
            Err(RenewError::MaxLifetimeReached { .. })
        ));
        assert_eq!(lease.effective_deadline(), UnixMillis(14_400_000));
    }

    #[test]
    fn an_expired_lease_cannot_be_revived() {
        let mut lease = Lease::start(spec(), UnixMillis(0));
        assert!(matches!(
            lease.renew(UnixMillis(1_000_000)),
            Err(RenewError::AlreadyExpired {
                reason: LeaseEnd::IdleTimeout
            })
        ));
    }
}
