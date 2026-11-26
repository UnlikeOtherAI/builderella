//! Seeded random admission sequences checked against the two inequalities
//! of §4, recomputed here without the crate's own helpers.

use crate::common::{GIB, Host, MIB, offered};
use bella_core::machine::{MachineProfile, PressureLevel};
use bella_core::resources::ResourceVector;
use bella_protect::{Grant, admit, budget};

/// SplitMix64: small, seedable and good enough to vary a scenario.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..=max`.
    fn up_to(&mut self, max: u64) -> u64 {
        if max == u64::MAX {
            self.next_u64()
        } else {
            self.next_u64() % (max + 1)
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.up_to(99) < percent
    }
}

fn memory_use(grant: &Grant) -> i128 {
    if grant.started {
        i128::from(grant.memory_used_bytes.unwrap_or(0))
    } else {
        0
    }
}

fn disk_use(grant: &Grant) -> i128 {
    if grant.started {
        i128::from(grant.disk_used_bytes.unwrap_or(0))
    } else {
        0
    }
}

/// Every condition an admission must have met, from the definitions.
fn assert_admission_was_sound(host: &Host, q: &ResourceVector, context: &str) {
    let ceiling = host.policy.managed_ceiling(&host.hardware);
    let sample = host.telemetry.as_ref().expect("admitted without telemetry");
    let grants = &host.grants;

    // Policy capacity, CPU: against the static ceiling, and against what
    // the foreground leaves unless a boost is active.
    let reserved_cpu: i128 = grants
        .iter()
        .map(|g| i128::from(g.reserved.cpu_milli))
        .sum();
    let need_cpu = reserved_cpu + i128::from(q.cpu_milli);
    assert!(need_cpu <= i128::from(ceiling.cpu_milli), "{context}: cpu");
    let total = i128::from(host.hardware.logical_cpus) * 1000;
    let entitlement = total * i128::from(host.policy.foreground_cpu_percent) / 100;
    let external =
        (i128::from(sample.cpu_used_milli) - i128::from(sample.cpu_managed_milli)).max(0);
    let boosted = host
        .control
        .boost_until
        .is_some_and(|until| until > host.now);
    if !boosted {
        assert!(
            need_cpu <= total - external.max(entitlement),
            "{context}: cpu against foreground use"
        );
    }

    // Policy capacity, memory, with observed excess charged.
    let charged: i128 = grants
        .iter()
        .map(|g| i128::from(g.reserved.memory_bytes).max(memory_use(g)))
        .sum();
    assert!(
        charged + i128::from(q.memory_bytes) <= i128::from(ceiling.memory_bytes),
        "{context}: memory capacity"
    );

    // Immediate safety: A − F − G − Σ max(r_i − u_i, 0) ≥ q.memory.
    let promised: i128 = grants
        .iter()
        .map(|g| (i128::from(g.reserved.memory_bytes) - memory_use(g)).max(0))
        .sum();
    let safe = i128::from(sample.memory_available_bytes)
        - i128::from(host.policy.available_memory_floor_bytes)
        - i128::from(host.config.memory_guard_bytes)
        - promised;
    assert!(
        safe >= i128::from(q.memory_bytes),
        "{context}: immediate safety"
    );

    // Disk: capacity, then free space above the floor and the promises.
    let reserved_disk: i128 = grants
        .iter()
        .map(|g| i128::from(g.reserved.disk_bytes))
        .sum();
    assert!(
        reserved_disk + i128::from(q.disk_bytes) <= i128::from(ceiling.disk_bytes),
        "{context}: disk capacity"
    );
    let disk_promised: i128 = grants
        .iter()
        .map(|g| (i128::from(g.reserved.disk_bytes) - disk_use(g)).max(0))
        .sum();
    let disk_floor = i128::from(host.policy.disk_floor_bytes(sample.disk_total_bytes));
    assert!(
        i128::from(sample.disk_free_bytes) - disk_floor - disk_promised >= i128::from(q.disk_bytes),
        "{context}: disk floor"
    );

    // Tokens.
    for (name, count) in &q.tokens {
        let held: u64 = grants.iter().map(|g| g.reserved.token(name)).sum();
        let declared = host.policy.tokens.get(name).copied().unwrap_or(0);
        assert!(held + count <= declared, "{context}: token {name}");
    }

    // Nothing that closes admission was in effect.
    assert!(
        sample.memory_pressure < PressureLevel::Warning,
        "{context}: pressure"
    );
    if let Some(limit) = host.policy.max_concurrent_attempts {
        assert!(
            (grants.len() as u64) < u64::from(limit),
            "{context}: concurrency"
        );
    }
    for grant in grants {
        let excess = memory_use(grant) - i128::from(grant.reserved.memory_bytes);
        assert!(
            excess <= i128::from(host.config.memory_excess_tolerance_bytes),
            "{context}: admitted beside an offender"
        );
    }
}

#[test]
fn random_admission_sequences_never_break_capacity_or_immediate_safety() {
    let profiles = [
        MachineProfile::ProtectedWorkstation,
        MachineProfile::DedicatedBuilder,
        MachineProfile::PortableBuilder,
        MachineProfile::EphemeralCloud,
    ];
    let mut admitted = 0u32;
    let mut refused = 0u32;

    for seed in 0..300u64 {
        let mut rng = Rng(seed);
        let profile = profiles[rng.up_to(3) as usize];
        let logical_cpus = 2 + rng.up_to(62) as u32;
        let memory_gib = 4 + rng.up_to(252);
        let mut host = Host::with(profile, logical_cpus, memory_gib);
        host.policy.max_concurrent_attempts = if rng.chance(50) {
            None
        } else {
            Some(1 + rng.up_to(7) as u32)
        };
        host.policy.tokens.insert("sim".to_string(), rng.up_to(3));
        if rng.chance(30) {
            host.policy.max_managed = Some(ResourceVector::new(
                1000 + rng.up_to(u64::from(logical_cpus) * 1000),
                GIB + rng.up_to(memory_gib * GIB),
                rng.up_to(200 * GIB),
            ));
        }
        let ceiling = host.policy.managed_ceiling(&host.hardware);
        let total_cpu = u64::from(logical_cpus) * 1000;
        let mut next_attempt = 0u32;

        for step in 0..60 {
            let context = format!("seed {seed}, step {step}");

            // The world moves on.
            host.now = host.now.plus_secs(1);
            let now = host.now;
            let available = rng.up_to(memory_gib * GIB);
            let external = rng.up_to(total_cpu);
            let managed = rng.up_to(1000);
            let disk_free = rng.up_to(500 * GIB);
            let pressure = match rng.up_to(99) {
                0..=2 => PressureLevel::Critical,
                3..=9 => PressureLevel::Warning,
                10..=14 => PressureLevel::Unknown,
                _ => PressureLevel::Normal,
            };
            let sample = host.sample();
            sample.sampled_at = now;
            sample.memory_available_bytes = available;
            sample.cpu_used_milli = external + managed;
            sample.cpu_managed_milli = managed;
            sample.disk_free_bytes = disk_free;
            sample.memory_pressure = pressure;
            host.control.boost_until = rng.chance(10).then(|| now.plus_secs(10));

            for grant in &mut host.grants {
                if !grant.started && rng.chance(40) {
                    grant.started = true;
                    grant.started_at = Some(now);
                }
                if grant.started {
                    let reserved = grant.reserved.memory_bytes;
                    grant.memory_used_bytes = Some(if rng.chance(4) {
                        reserved + rng.up_to(reserved / 4 + 128 * MIB)
                    } else {
                        rng.up_to(reserved)
                    });
                    grant.disk_used_bytes =
                        rng.chance(70).then(|| rng.up_to(grant.reserved.disk_bytes));
                }
                grant.suspended = rng.chance(10);
            }
            if !host.grants.is_empty() && rng.chance(15) {
                let index = rng.up_to(host.grants.len() as u64 - 1) as usize;
                host.grants.remove(index);
            }

            // An offer arrives.
            let mut q = ResourceVector::new(
                rng.up_to(ceiling.cpu_milli / 3),
                rng.up_to(ceiling.memory_bytes / 3),
                rng.up_to(40 * GIB),
            );
            match rng.up_to(9) {
                0 => q = q.with_token("gpu", 1),
                1..=4 => q = q.with_token("sim", rng.up_to(2)),
                _ => {}
            }

            let input = host.input();
            let report = budget(&input);
            match admit(&input, &q) {
                Ok(()) => {
                    admitted += 1;
                    assert_admission_was_sound(&host, &q, &context);
                    assert!(report.admitting, "{context}: budget disagrees");
                    assert!(q.fits_within(&report.free), "{context}: beyond free");
                    next_attempt += 1;
                    let mut grant = offered(next_attempt, 0, 0);
                    grant.reserved = q;
                    host.grants.push(grant);
                }
                Err(rejection) => {
                    refused += 1;
                    assert!(!rejection.message.is_empty(), "{context}");
                    // The budget must not advertise what `admit` refuses.
                    assert!(
                        !(report.admitting && q.fits_within(&report.free)),
                        "{context}: budget offers what was refused: {}",
                        rejection.message
                    );
                }
            }

            // Whatever happened, reservations never exceed the ceiling.
            let reserved = host.grants.iter().fold(ResourceVector::ZERO, |sum, g| {
                sum.saturating_add(&g.reserved)
            });
            assert!(
                reserved.fits_within(&ceiling),
                "{context}: reserved {reserved:?} exceeds {ceiling:?}"
            );
        }
    }

    // The sequences exercised both outcomes, many times over.
    assert!(admitted > 300, "only {admitted} admissions");
    assert!(refused > 1000, "only {refused} refusals");
}
