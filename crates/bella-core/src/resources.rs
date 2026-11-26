//! Resource vectors. A slot is a bundle of resources, not "one process".

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A quantity of schedulable resources.
///
/// `tokens` are named countable resources: `gpu`, `ios-simulator`,
/// `gui-session`, a service-pool quota such as `pg-connections`, or an
/// exclusive device such as `device:pixel-8`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceVector {
    /// CPU budget in thousandths of one logical CPU.
    #[serde(default)]
    pub cpu_milli: u64,
    #[serde(default)]
    pub memory_bytes: u64,
    /// Scratch/workspace allowance on the worker's work volume.
    #[serde(default)]
    pub disk_bytes: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, u64>,
}

/// The first dimension in which a request does not fit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortfall {
    /// `cpu`, `memory`, `disk` or `token:<name>`.
    pub dimension: String,
    pub requested: u64,
    pub available: u64,
}

impl ResourceVector {
    pub const ZERO: ResourceVector =
        ResourceVector { cpu_milli: 0, memory_bytes: 0, disk_bytes: 0, tokens: BTreeMap::new() };

    pub fn new(cpu_milli: u64, memory_bytes: u64, disk_bytes: u64) -> Self {
        Self { cpu_milli, memory_bytes, disk_bytes, tokens: BTreeMap::new() }
    }

    pub fn with_token(mut self, name: impl Into<String>, count: u64) -> Self {
        self.tokens.insert(name.into(), count);
        self
    }

    pub fn token(&self, name: &str) -> u64 {
        self.tokens.get(name).copied().unwrap_or(0)
    }

    pub fn is_zero(&self) -> bool {
        self.cpu_milli == 0
            && self.memory_bytes == 0
            && self.disk_bytes == 0
            && self.tokens.values().all(|v| *v == 0)
    }

    /// Component-wise sum; `None` on overflow.
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        let mut tokens = self.tokens.clone();
        for (name, count) in &other.tokens {
            let slot = tokens.entry(name.clone()).or_insert(0);
            *slot = slot.checked_add(*count)?;
        }
        Some(Self {
            cpu_milli: self.cpu_milli.checked_add(other.cpu_milli)?,
            memory_bytes: self.memory_bytes.checked_add(other.memory_bytes)?,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
            tokens,
        })
    }

    /// Component-wise sum that saturates instead of overflowing.
    pub fn saturating_add(&self, other: &Self) -> Self {
        let mut tokens = self.tokens.clone();
        for (name, count) in &other.tokens {
            let slot = tokens.entry(name.clone()).or_insert(0);
            *slot = slot.saturating_add(*count);
        }
        Self {
            cpu_milli: self.cpu_milli.saturating_add(other.cpu_milli),
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            disk_bytes: self.disk_bytes.saturating_add(other.disk_bytes),
            tokens,
        }
    }

    /// Component-wise difference floored at zero. A release can therefore
    /// never drive capacity negative.
    pub fn saturating_sub(&self, other: &Self) -> Self {
        let mut tokens = self.tokens.clone();
        for (name, count) in &other.tokens {
            if let Some(slot) = tokens.get_mut(name) {
                *slot = slot.saturating_sub(*count);
            }
        }
        Self {
            cpu_milli: self.cpu_milli.saturating_sub(other.cpu_milli),
            memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
            disk_bytes: self.disk_bytes.saturating_sub(other.disk_bytes),
            tokens,
        }
    }

    /// Component-wise maximum: the envelope of phases that do not overlap.
    pub fn max(&self, other: &Self) -> Self {
        let mut tokens = self.tokens.clone();
        for (name, count) in &other.tokens {
            let slot = tokens.entry(name.clone()).or_insert(0);
            *slot = (*slot).max(*count);
        }
        Self {
            cpu_milli: self.cpu_milli.max(other.cpu_milli),
            memory_bytes: self.memory_bytes.max(other.memory_bytes),
            disk_bytes: self.disk_bytes.max(other.disk_bytes),
            tokens,
        }
    }

    /// Component-wise minimum.
    pub fn min(&self, other: &Self) -> Self {
        let tokens = self
            .tokens
            .iter()
            .map(|(name, count)| (name.clone(), (*count).min(other.token(name))))
            .collect();
        Self {
            cpu_milli: self.cpu_milli.min(other.cpu_milli),
            memory_bytes: self.memory_bytes.min(other.memory_bytes),
            disk_bytes: self.disk_bytes.min(other.disk_bytes),
            tokens,
        }
    }

    pub fn fits_within(&self, capacity: &Self) -> bool {
        self.first_shortfall(capacity).is_none()
    }

    pub fn first_shortfall(&self, capacity: &Self) -> Option<Shortfall> {
        let dims = [
            ("cpu", self.cpu_milli, capacity.cpu_milli),
            ("memory", self.memory_bytes, capacity.memory_bytes),
            ("disk", self.disk_bytes, capacity.disk_bytes),
        ];
        for (dimension, requested, available) in dims {
            if requested > available {
                return Some(Shortfall { dimension: dimension.to_string(), requested, available });
            }
        }
        for (name, requested) in &self.tokens {
            let available = capacity.token(name);
            if *requested > available {
                return Some(Shortfall {
                    dimension: format!("token:{name}"),
                    requested: *requested,
                    available,
                });
            }
        }
        None
    }
}

/// What a job asks for over its lifetime.
///
/// The reservation is taken for `peak`. `maximum` bounds growth requests;
/// anything beyond it means a controlled restart on a larger machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceEnvelope {
    pub initial: ResourceVector,
    pub peak: ResourceVector,
    pub maximum: ResourceVector,
}

impl ResourceEnvelope {
    /// An envelope with no growth room.
    pub fn fixed(v: ResourceVector) -> Self {
        Self { initial: v.clone(), peak: v.clone(), maximum: v }
    }

    /// `initial <= peak <= maximum` in every dimension.
    pub fn is_well_formed(&self) -> bool {
        self.initial.fits_within(&self.peak) && self.peak.fits_within(&self.maximum)
    }
}

/// How strongly a limit is actually enforced. The dashboard shows this per
/// resource and executor; nothing may be presented as `Hard` on the strength
/// of a priority setting or a compiler thread count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// No control and no reliable observation.
    None,
    /// Admission policy, priority and pressure response only.
    Advisory,
    /// Bounded inside a guest whose host overhead is accounted separately.
    BoundedGuest,
    /// Kernel-enforced limit that was verified on this machine.
    Hard,
}

/// What an executor can actually enforce, as verified by commissioning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcementReport {
    pub cpu: Enforcement,
    pub memory: Enforcement,
    /// Every descendant stays in the managed scope and can be killed with it.
    pub process_tree: Enforcement,
    pub process_count: Enforcement,
    pub disk: Enforcement,
    /// The controls above were exercised by a self-test, not just detected.
    pub verified: bool,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl EnforcementReport {
    pub fn none() -> Self {
        Self {
            cpu: Enforcement::None,
            memory: Enforcement::None,
            process_tree: Enforcement::None,
            process_count: Enforcement::None,
            disk: Enforcement::None,
            verified: false,
            notes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_and_shortfall() {
        let cap = ResourceVector::new(8000, 16 << 30, 100 << 30).with_token("gpu", 1);
        let ok = ResourceVector::new(4000, 8 << 30, 1 << 30).with_token("gpu", 1);
        assert!(ok.fits_within(&cap));
        let too_much = ok.clone().with_token("gpu", 2);
        assert_eq!(too_much.first_shortfall(&cap).unwrap().dimension, "token:gpu");
        let unknown = ok.clone().with_token("ios-simulator", 1);
        assert_eq!(unknown.first_shortfall(&cap).unwrap().available, 0);
    }

    #[test]
    fn release_never_goes_negative() {
        let held = ResourceVector::new(1000, 100, 0);
        let twice = held.saturating_sub(&held).saturating_sub(&held);
        assert!(twice.is_zero());
    }

    #[test]
    fn max_is_the_envelope_of_sequential_phases() {
        let build = ResourceVector::new(8000, 4 << 30, 0);
        let test = ResourceVector::new(2000, 8 << 30, 0).with_token("gui-session", 1);
        let peak = build.max(&test);
        assert_eq!(peak, ResourceVector::new(8000, 8 << 30, 0).with_token("gui-session", 1));
    }
}
