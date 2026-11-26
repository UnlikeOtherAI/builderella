//! `budget`: ceilings, free capacity, availability flags and reasons.

use crate::common::{GIB, Host, T0, healthy, offered, request, running};
use bella_core::job::QueueReasonCode;
use bella_core::machine::{AvailabilityFlag, PressureLevel, ThermalState};
use bella_core::resources::ResourceVector;
use std::collections::BTreeMap;

/// The static memory ceiling of the worked example, as `bella-core`
/// computes it: 32 GiB minus a 20% foreground entitlement.
const EXAMPLE_MEMORY: u64 = 32 * GIB - 32 * GIB / 100 * 20;

#[test]
fn worked_example_ceiling_is_8_cpus_and_25_6_gib_before_live_checks() {
    let mut host = Host::workstation();
    host.telemetry = None;
    let blind = host.budget();
    assert_eq!(blind.ceiling.cpu_milli, 8000);
    assert_eq!(blind.ceiling.memory_bytes, EXAMPLE_MEMORY);
    assert_eq!(format!("{:.1}", EXAMPLE_MEMORY as f64 / GIB as f64), "25.6");
    // Without an observation the ceiling is known and nothing is on offer.
    assert!(!blind.admitting);
    assert_eq!(blind.free, ResourceVector::ZERO);
    assert!(blind.reasons[0].contains("no fresh local telemetry"));

    host.telemetry = Some(healthy(T0, 32));
    let live = host.budget();
    assert!(live.admitting, "{:?}", live.reasons);
    assert!(live.flags.is_empty());
    assert!(live.reasons.is_empty());
    assert_eq!(live.ceiling.cpu_milli, 8000);
    // 30 available − 2 floor − 0.5 guard = 27.5 GiB, above the policy ceiling.
    assert_eq!(live.ceiling.memory_bytes, EXAMPLE_MEMORY);
    // 300 free − max(15% of 500, 20) = 225 GiB.
    assert_eq!(live.ceiling.disk_bytes, 225 * GIB);
    assert_eq!(live.free, live.ceiling);
}

#[test]
fn cpu_ceiling_follows_foreground_use_and_never_borrows_the_reserve() {
    // 10 CPUs, 20% entitlement: T = 10000, F = 2000, static ceiling 8000.
    struct Row {
        external: u64,
        boost: Option<u64>,
        cap: Option<u64>,
        ceiling: u64,
    }
    let row = |external, boost, cap, ceiling| Row {
        external,
        boost,
        cap,
        ceiling,
    };
    let rows = [
        // An idle foreground does not enlarge the managed share.
        row(0, None, None, 8000),
        row(1999, None, None, 8000),
        row(2000, None, None, 8000),
        // Beyond its entitlement the foreground takes from managed work.
        row(2001, None, None, 7999),
        row(3500, None, None, 6500),
        row(10_000, None, None, 0),
        row(12_000, None, None, 0),
        // A boost holds the static ceiling and gives nothing beyond it.
        row(3500, Some(60), None, 8000),
        row(0, Some(60), None, 8000),
        row(10_000, Some(60), None, 8000),
        // A boost that has ended changes nothing.
        row(3500, Some(0), None, 6500),
        // An explicit cap is part of the static ceiling.
        row(3500, None, Some(4000), 4000),
        row(7000, None, Some(4000), 3000),
        row(7000, Some(60), Some(4000), 4000),
    ];
    for (index, case) in rows.iter().enumerate() {
        let mut host = Host::workstation();
        host.set_cpu(case.external, 1000);
        host.control.boost_until = case.boost.map(|secs| host.now.plus_secs(secs));
        host.policy.max_managed = case.cap.map(|cpu| ResourceVector::new(cpu, u64::MAX, 0));
        let budget = host.budget();
        assert_eq!(budget.ceiling.cpu_milli, case.ceiling, "row {index}");
        assert_eq!(budget.free.cpu_milli, case.ceiling, "row {index}");
        assert!(budget.admitting, "row {index}: {:?}", budget.reasons);
    }
}

#[test]
fn a_reduced_cpu_ceiling_is_explained_with_its_numbers() {
    let mut host = Host::workstation();
    host.set_cpu(3500, 0);
    assert_eq!(
        host.budget().reasons,
        vec![
            "the foreground uses 3.5 CPUs against an entitlement of 2.0 CPUs; managed CPU ceiling reduced from 8.0 to 6.5 CPUs"
        ]
    );
    host.control.boost_until = Some(host.now.plus_secs(60));
    assert_eq!(
        host.budget().reasons,
        vec![
            "boost is active: managed CPU stays at the static ceiling of 8.0 CPUs while the foreground uses 3.5 CPUs against an entitlement of 2.0 CPUs"
        ]
    );
}

#[test]
fn explicit_cap_limits_memory_and_disk_too() {
    let mut host = Host::workstation();
    host.policy.max_managed = Some(ResourceVector::new(u64::MAX, 8 * GIB, 100 * GIB));
    let budget = host.budget();
    assert_eq!(budget.ceiling.cpu_milli, 8000);
    assert_eq!(budget.ceiling.memory_bytes, 8 * GIB);
    assert_eq!(budget.ceiling.disk_bytes, 100 * GIB);
}

#[test]
fn memory_ceiling_is_what_is_charged_plus_what_is_safe_to_grant() {
    let mut host = Host::shared_workstation();
    // 10 − 2 floor − 0.5 guard = 7.5 GiB.
    host.set_available(10 * GIB);
    let empty = host.budget();
    assert_eq!(empty.ceiling.memory_bytes, 7 * GIB + GIB / 2);
    assert_eq!(empty.free.memory_bytes, 7 * GIB + GIB / 2);

    // A started grant using 3 of its 4 GiB still has 1 GiB promised.
    host.grants.push(running(1, 1000, 4 * GIB, 3 * GIB));
    let one = host.budget();
    assert_eq!(one.free.memory_bytes, 6 * GIB + GIB / 2);
    assert_eq!(one.ceiling.memory_bytes, 10 * GIB + GIB / 2);

    // An offered grant promises all of its 2 GiB.
    host.grants.push(offered(2, 1000, 2 * GIB));
    let two = host.budget();
    assert_eq!(two.free.memory_bytes, 4 * GIB + GIB / 2);
    assert_eq!(two.ceiling.memory_bytes, 10 * GIB + GIB / 2);
    assert_eq!(two.free.cpu_milli, 6000);
}

#[test]
fn disk_budget_keeps_the_floor_and_unwritten_promises() {
    let mut host = Host::shared_workstation();
    // Floor: max(15% of 500 GiB, 20 GiB) = 75 GiB.
    host.sample().disk_free_bytes = 100 * GIB;
    assert_eq!(host.budget().free.disk_bytes, 25 * GIB);

    let mut grant = offered(1, 1000, GIB);
    grant.reserved.disk_bytes = 10 * GIB;
    host.grants.push(grant);
    let promised = host.budget();
    assert_eq!(promised.free.disk_bytes, 15 * GIB);
    assert_eq!(promised.ceiling.disk_bytes, 25 * GIB);

    // 4 GiB already written has already left the free space.
    host.grants[0].started = true;
    host.grants[0].disk_used_bytes = Some(4 * GIB);
    assert_eq!(host.budget().free.disk_bytes, 19 * GIB);

    // Writing more than was reserved leaves no promise outstanding.
    host.grants[0].disk_used_bytes = Some(14 * GIB);
    assert_eq!(host.budget().free.disk_bytes, 25 * GIB);

    host.sample().disk_free_bytes = 70 * GIB;
    let low = host.budget();
    assert_eq!(low.flags, vec![AvailabilityFlag::DiskLow]);
    assert!(!low.admitting);
    assert_eq!(low.free.disk_bytes, 0);
}

#[test]
fn token_budget_is_what_the_policy_declares_minus_what_is_held() {
    let mut host = Host::shared_workstation();
    host.policy.tokens = BTreeMap::from([("ios-simulator".to_string(), 2), ("gpu".to_string(), 1)]);
    let mut grant = offered(1, 1000, GIB);
    grant.reserved = grant.reserved.with_token("ios-simulator", 1);
    host.grants.push(grant);
    let budget = host.budget();
    assert_eq!(budget.ceiling.token("ios-simulator"), 2);
    assert_eq!(budget.free.token("ios-simulator"), 1);
    assert_eq!(budget.free.token("gpu"), 1);
}

#[test]
fn availability_rules() {
    struct Row {
        name: &'static str,
        setup: fn(&mut Host),
        flags: &'static [AvailabilityFlag],
        /// `None` when the host still admits.
        refusal: Option<(QueueReasonCode, &'static str)>,
    }
    let rows = [
        Row {
            name: "builds disabled",
            setup: |h| h.policy.accept_builds = false,
            flags: &[AvailabilityFlag::BuildsDisabled],
            refusal: Some((QueueReasonCode::BuildsDisabled, "does not accept builds")),
        },
        Row {
            name: "paused",
            setup: |h| h.control.paused = true,
            flags: &[AvailabilityFlag::UserPaused],
            refusal: Some((
                QueueReasonCode::MachinePaused,
                "paused by the machine's owner",
            )),
        },
        Row {
            name: "draining",
            setup: |h| h.control.draining = true,
            flags: &[AvailabilityFlag::Draining],
            refusal: Some((QueueReasonCode::MachineDraining, "draining")),
        },
        Row {
            name: "on battery where external power is required",
            setup: |h| {
                h.policy.require_ac_power = true;
                h.sample().power.on_ac = Some(false);
                h.sample().power.battery_percent = Some(80);
            },
            flags: &[AvailabilityFlag::OnBattery],
            refusal: Some((QueueReasonCode::OnBattery, "battery at 80%")),
        },
        Row {
            name: "unknown power source with a battery counts as battery",
            setup: |h| {
                h.policy.require_ac_power = true;
                h.sample().power.battery_percent = Some(64);
            },
            flags: &[AvailabilityFlag::OnBattery],
            refusal: Some((
                QueueReasonCode::OnBattery,
                "power source unknown with a battery at 64%",
            )),
        },
        Row {
            name: "unknown power source without a battery is a desktop",
            setup: |h| h.policy.require_ac_power = true,
            flags: &[],
            refusal: None,
        },
        Row {
            name: "on external power",
            setup: |h| {
                h.policy.require_ac_power = true;
                h.sample().power.on_ac = Some(true);
                h.sample().power.battery_percent = Some(90);
            },
            flags: &[],
            refusal: None,
        },
        Row {
            name: "on battery where that is allowed",
            setup: |h| h.sample().power.on_ac = Some(false),
            flags: &[],
            refusal: None,
        },
        Row {
            name: "battery below the minimum, even while charging",
            setup: |h| {
                h.policy.min_battery_percent = Some(50);
                h.sample().power.on_ac = Some(true);
                h.sample().power.battery_percent = Some(30);
            },
            flags: &[AvailabilityFlag::OnBattery],
            refusal: Some((
                QueueReasonCode::OnBattery,
                "battery at 30%, below the policy minimum of 50%",
            )),
        },
        Row {
            name: "battery exactly at the minimum",
            setup: |h| {
                h.policy.min_battery_percent = Some(50);
                h.sample().power.battery_percent = Some(50);
            },
            flags: &[],
            refusal: None,
        },
        Row {
            name: "thermal serious",
            setup: |h| h.sample().thermal = ThermalState::Serious,
            flags: &[AvailabilityFlag::Overheated],
            refusal: Some((QueueReasonCode::Overheated, "thermal state is serious")),
        },
        Row {
            name: "thermal critical",
            setup: |h| h.sample().thermal = ThermalState::Critical,
            flags: &[AvailabilityFlag::Overheated],
            refusal: Some((QueueReasonCode::Overheated, "thermal state is critical")),
        },
        Row {
            name: "thermal fair",
            setup: |h| h.sample().thermal = ThermalState::Fair,
            flags: &[],
            refusal: None,
        },
        Row {
            name: "thermal unknown",
            setup: |h| h.sample().thermal = ThermalState::Unknown,
            flags: &[],
            refusal: None,
        },
        Row {
            name: "memory pressure warning",
            setup: |h| h.set_pressure(PressureLevel::Warning),
            flags: &[AvailabilityFlag::PressureLimited],
            refusal: Some((
                QueueReasonCode::MemoryPressure,
                "memory pressure is warning (available 30.0 GiB of 32.0 GiB)",
            )),
        },
        Row {
            name: "memory pressure critical",
            setup: |h| h.set_pressure(PressureLevel::Critical),
            flags: &[AvailabilityFlag::PressureLimited],
            refusal: Some((
                QueueReasonCode::MemoryPressure,
                "memory pressure is critical",
            )),
        },
        Row {
            name: "memory pressure not reported",
            setup: |h| h.set_pressure(PressureLevel::Unknown),
            flags: &[],
            refusal: None,
        },
        Row {
            name: "available memory below the floor",
            setup: |h| h.set_available(GIB),
            flags: &[AvailabilityFlag::PressureLimited],
            refusal: Some((
                QueueReasonCode::MemoryPressure,
                "available memory 1.0 GiB is below the 2.0 GiB floor",
            )),
        },
        Row {
            name: "disk below the floor",
            setup: |h| h.sample().disk_free_bytes = 70 * GIB,
            flags: &[AvailabilityFlag::DiskLow],
            refusal: Some((
                QueueReasonCode::DiskLow,
                "work volume has 70.0 GiB free, below the 75.0 GiB floor",
            )),
        },
        Row {
            name: "no telemetry",
            setup: |h| h.telemetry = None,
            flags: &[],
            refusal: Some((QueueReasonCode::StaleTelemetry, "no fresh local telemetry")),
        },
        Row {
            name: "old telemetry",
            setup: |h| h.now = T0.plus_secs(7),
            flags: &[],
            refusal: Some((
                QueueReasonCode::StaleTelemetry,
                "no fresh local telemetry (the last sample is 7.0 s old, limit 5.0 s)",
            )),
        },
        Row {
            name: "telemetry exactly at the age limit",
            setup: |h| h.now = T0.plus_secs(5),
            flags: &[],
            refusal: None,
        },
        Row {
            name: "telemetry stamped in the future",
            setup: |h| h.sample().sampled_at = T0.plus_secs(60),
            flags: &[],
            refusal: Some((QueueReasonCode::StaleTelemetry, "60.0 s in the future")),
        },
        Row {
            name: "concurrency limit",
            setup: |h| h.grants.push(offered(1, 1000, GIB)),
            flags: &[],
            refusal: Some((
                QueueReasonCode::MachineConcurrencyLimit,
                "1 attempt(s) hold resources; policy allows 1 at a time",
            )),
        },
    ];

    for case in rows {
        let mut host = Host::workstation();
        (case.setup)(&mut host);
        let budget = host.budget();
        let verdict = host.admit(&request(1000, GIB));
        assert_eq!(budget.flags, case.flags, "{}", case.name);
        match case.refusal {
            None => {
                assert!(budget.admitting, "{}: {:?}", case.name, budget.reasons);
                assert_eq!(verdict, Ok(()), "{}", case.name);
            }
            Some((code, needle)) => {
                assert!(!budget.admitting, "{}", case.name);
                assert!(
                    budget.reasons.iter().any(|r| r.contains(needle)),
                    "{}: {:?}",
                    case.name,
                    budget.reasons
                );
                let rejection = verdict.expect_err(case.name);
                assert_eq!(rejection.code, code, "{}", case.name);
                assert!(
                    rejection.message.contains(needle),
                    "{}: {}",
                    case.name,
                    rejection.message
                );
            }
        }
    }
}

#[test]
fn every_closed_gate_is_reported_and_flags_are_sorted_without_duplicates() {
    let mut host = Host::workstation();
    host.control.paused = true;
    host.control.draining = true;
    host.set_pressure(PressureLevel::Warning);
    host.set_available(GIB);
    let budget = host.budget();
    assert_eq!(
        budget.flags,
        vec![
            AvailabilityFlag::Draining,
            AvailabilityFlag::UserPaused,
            AvailabilityFlag::PressureLimited,
        ]
    );
    assert_eq!(budget.reasons.len(), 4, "{:?}", budget.reasons);
    // `admit` reports the first of them.
    let rejection = host.admit(&request(1000, GIB)).expect_err("closed");
    assert_eq!(rejection.code, QueueReasonCode::MachinePaused);
}
