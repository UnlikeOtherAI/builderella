//! Fleet-relative shares, in parts per billion. Integer arithmetic only.

use crate::model::MachineState;
use bella_core::resources::ResourceVector;
use std::cmp::Ordering;

pub(crate) const PPB: u64 = 1_000_000_000;

/// Sum of every machine's managed ceiling: the denominator for shares.
#[derive(Clone, Debug, Default)]
pub(crate) struct Totals {
    total: ResourceVector,
}

fn ratio(value: u64, total: u64) -> u64 {
    if value == 0 {
        0
    } else if total == 0 {
        PPB
    } else {
        let scaled = u128::from(value) * u128::from(PPB) / u128::from(total);
        u64::try_from(scaled).unwrap_or(u64::MAX)
    }
}

impl Totals {
    pub(crate) fn of(machines: &[&MachineState]) -> Self {
        let mut total = ResourceVector::ZERO;
        for machine in machines {
            total = total.saturating_add(&machine.managed_ceiling);
        }
        Self { total }
    }

    /// The largest fraction of the fleet that `v` takes in any dimension.
    pub(crate) fn dominant_share(&self, v: &ResourceVector) -> u64 {
        let mut share = ratio(v.cpu_milli, self.total.cpu_milli)
            .max(ratio(v.memory_bytes, self.total.memory_bytes))
            .max(ratio(v.disk_bytes, self.total.disk_bytes));
        for (name, count) in &v.tokens {
            share = share.max(ratio(*count, self.total.token(name)));
        }
        share
    }

    /// How much of the fleet `free` still represents in its tightest
    /// dimension. Disk is skipped when the fleet's disk is unbounded.
    pub(crate) fn headroom(&self, free: &ResourceVector) -> u64 {
        let mut dims = vec![
            (free.cpu_milli, self.total.cpu_milli),
            (free.memory_bytes, self.total.memory_bytes),
        ];
        if self.total.disk_bytes != u64::MAX {
            dims.push((free.disk_bytes, self.total.disk_bytes));
        }
        dims.into_iter()
            .filter(|(_, total)| *total > 0)
            .map(|(value, total)| ratio(value, total))
            .min()
            .unwrap_or(0)
    }
}

/// Compare `a / weight_a` with `b / weight_b` exactly.
pub(crate) fn cmp_weighted(a: u64, weight_a: u64, b: u64, weight_b: u64) -> Ordering {
    (u128::from(a) * u128::from(weight_b)).cmp(&(u128::from(b) * u128::from(weight_a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(v: ResourceVector) -> Totals {
        Totals { total: v }
    }

    #[test]
    fn dominant_share_is_the_largest_dimension() {
        let t = totals(ResourceVector::new(16_000, 64, 1000).with_token("gpu", 2));
        assert_eq!(
            t.dominant_share(&ResourceVector::new(4000, 32, 0)),
            PPB / 2
        );
        assert_eq!(
            t.dominant_share(&ResourceVector::new(0, 0, 0).with_token("gpu", 2)),
            PPB
        );
        assert_eq!(t.dominant_share(&ResourceVector::ZERO), 0);
        // A token the fleet does not declare at all counts as the whole fleet.
        assert_eq!(
            t.dominant_share(&ResourceVector::ZERO.with_token("sim", 1)),
            PPB
        );
    }

    #[test]
    fn headroom_is_the_tightest_bounded_dimension() {
        let bounded = totals(ResourceVector::new(10_000, 100, 1000));
        assert_eq!(
            bounded.headroom(&ResourceVector::new(5000, 100, 100)),
            PPB / 10
        );
        let unbounded = totals(ResourceVector::new(10_000, 100, u64::MAX));
        assert_eq!(
            unbounded.headroom(&ResourceVector::new(5000, 100, 0)),
            PPB / 2
        );
    }

    #[test]
    fn weighted_comparison_is_exact() {
        assert_eq!(cmp_weighted(10, 4, 20, 8), Ordering::Equal);
        assert_eq!(cmp_weighted(10, 4, 21, 8), Ordering::Less);
        assert_eq!(cmp_weighted(u64::MAX, 1, u64::MAX, 2), Ordering::Greater);
    }
}
