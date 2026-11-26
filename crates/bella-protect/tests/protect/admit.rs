//! `admit`: the two checks of §4 in every dimension, and what a refusal says.

use crate::common::{GIB, Host, MIB, offered, request, running};
use bella_core::job::QueueReasonCode;
use bella_core::machine::MachineProfile;
use bella_core::resources::ResourceVector;
use std::collections::BTreeMap;

const EXAMPLE_MEMORY: u64 = 32 * GIB - 32 * GIB / 100 * 20;

/// A workstation with two grants: 5 CPUs, 16 GiB, 60 GiB of disk and one
/// simulator held out of 8 CPUs, 25.6 GiB, 100 GiB and two simulators.
fn busy_host() -> Host {
    let mut host = Host::shared_workstation();
    host.policy.tokens = BTreeMap::from([("ios-simulator".to_string(), 2)]);
    host.policy.max_managed = Some(ResourceVector::new(u64::MAX, u64::MAX, 100 * GIB));
    let mut waiting = offered(1, 3000, 10 * GIB);
    waiting.reserved.disk_bytes = 40 * GIB;
    waiting.reserved = waiting.reserved.with_token("ios-simulator", 1);
    let mut active = running(2, 2000, 6 * GIB, GIB);
    active.reserved.disk_bytes = 20 * GIB;
    host.grants = vec![waiting, active];
    host
}

#[test]
fn policy_capacity_holds_in_every_dimension() {
    let fits = |cpu, memory, disk: u64, simulators: u64| {
        let mut q = ResourceVector::new(cpu, memory, disk);
        if simulators > 0 {
            q = q.with_token("ios-simulator", simulators);
        }
        q
    };
    let memory_left = EXAMPLE_MEMORY - 16 * GIB;
    let rows: [(ResourceVector, Option<&str>); 8] = [
        // CPU: 5 reserved of 8.
        (fits(3000, GIB, 0, 0), None),
        (
            fits(3001, GIB, 0, 0),
            Some("needs 3.0 CPUs; 3.0 CPUs are unreserved (ceiling 8.0, reserved 5.0)"),
        ),
        // Memory: 16 GiB reserved of 25.6, to the byte.
        (fits(1000, memory_left, 0, 0), None),
        (
            fits(1000, memory_left + 1, 0, 0),
            Some(
                "needs 9.6 GiB of memory; 9.6 GiB is unreserved (ceiling 25.6, charged to grants 16.0)",
            ),
        ),
        // Disk: 60 GiB reserved of the 100 GiB cap.
        (fits(1000, GIB, 40 * GIB, 0), None),
        (
            fits(1000, GIB, 40 * GIB + 1, 0),
            Some(
                "needs 40.0 GiB of scratch disk; 40.0 GiB is unreserved (ceiling 100.0, reserved 60.0)",
            ),
        ),
        // Tokens: one simulator held of two.
        (fits(1000, GIB, 0, 1), None),
        (
            fits(1000, GIB, 0, 2),
            Some("needs 2 of token `ios-simulator`; 1 free (declared 2, reserved 1)"),
        ),
    ];
    let host = busy_host();
    for (index, (q, refusal)) in rows.iter().enumerate() {
        match (host.admit(q), refusal) {
            (Ok(()), None) => {}
            (Err(rejection), Some(message)) => {
                assert_eq!(
                    rejection.code,
                    QueueReasonCode::WaitingForCapacity,
                    "row {index}"
                );
                assert_eq!(rejection.message, *message, "row {index}");
            }
            (verdict, _) => panic!("row {index}: unexpected {verdict:?}"),
        }
    }
}

#[test]
fn a_request_the_machine_could_never_grant_is_machine_too_small() {
    let rows: [(ResourceVector, &str); 5] = [
        (
            request(9000, GIB),
            "needs 9.0 CPUs; this machine's policy ceiling for managed work is 8.0 CPUs",
        ),
        (
            request(1000, 26 * GIB),
            "needs 26.0 GiB of memory; this machine's policy ceiling for managed work is 25.6 GiB",
        ),
        (
            ResourceVector::new(1000, GIB, 101 * GIB),
            "needs 101.0 GiB of scratch disk; this machine's policy ceiling for managed work is 100.0 GiB",
        ),
        (
            request(1000, GIB).with_token("gpu", 1),
            "needs 1 of token `gpu`, which this machine does not declare (declared: `ios-simulator`)",
        ),
        (
            request(1000, GIB).with_token("ios-simulator", 3),
            "needs 3 of token `ios-simulator`; this machine declares 2",
        ),
    ];
    let mut host = busy_host();
    host.grants.clear();
    for (q, message) in &rows {
        let rejection = host.admit(q).expect_err(message);
        assert_eq!(rejection.code, QueueReasonCode::MachineTooSmall);
        assert_eq!(rejection.message, *message);
    }
    // Asking for none of an undeclared token asks for nothing.
    assert_eq!(host.admit(&request(1000, GIB).with_token("gpu", 0)), Ok(()));

    // "Never on this machine" is said before a reason that waiting fixes,
    // and after "this machine takes no builds".
    host.control.paused = true;
    let paused = host.admit(&request(9000, GIB)).expect_err("too big");
    assert_eq!(paused.code, QueueReasonCode::MachineTooSmall);
    host.policy.accept_builds = false;
    let disabled = host.admit(&request(9000, GIB)).expect_err("disabled");
    assert_eq!(disabled.code, QueueReasonCode::BuildsDisabled);
}

#[test]
fn a_machine_without_tokens_says_so() {
    let host = Host::workstation();
    let rejection = host
        .admit(&request(1000, GIB).with_token("gpu", 1))
        .expect_err("undeclared");
    assert_eq!(
        rejection.message,
        "needs 1 of token `gpu`, which this machine does not declare (declared: none)"
    );
}

#[test]
fn immediate_safety_refusal_carries_the_numbers() {
    // 64 GiB: policy ceiling 51.2 GiB, so only immediate safety can refuse.
    let mut host = Host::with(MachineProfile::ProtectedWorkstation, 16, 64);
    host.policy.max_concurrent_attempts = None;
    host.set_available(14 * GIB + 205 * MIB);
    // 2.5 of 4 GiB in use: 1.5 GiB still promised.
    host.grants = vec![running(1, 1000, 4 * GIB, 2 * GIB + 512 * MIB)];

    let rejection = host.admit(&request(1000, 18 * GIB)).expect_err("unsafe");
    assert_eq!(rejection.code, QueueReasonCode::ForegroundProtection);
    assert_eq!(
        rejection.message,
        "needs 18.0 GiB; 10.2 GiB can be granted safely now (available 14.2, floor 2.0, guard 0.5, promised 1.5)"
    );
    assert_eq!(rejection.to_string(), rejection.message);

    // 14.2 − 2 − 0.5 − 1.5 = 10.2 GiB, to the byte.
    let safe = 10 * GIB + 205 * MIB;
    assert_eq!(host.admit(&request(1000, safe)), Ok(()));
    assert!(host.admit(&request(1000, safe + 1)).is_err());
    assert_eq!(host.budget().free.memory_bytes, safe);
}

#[test]
fn a_started_grant_with_unknown_use_keeps_its_whole_promise() {
    let mut host = Host::shared_workstation();
    host.set_available(10 * GIB);
    let mut unknown = running(1, 1000, 4 * GIB, 0);
    unknown.memory_used_bytes = None;
    host.grants = vec![unknown];
    // 10 − 2 − 0.5 − 4 = 3.5 GiB.
    assert_eq!(host.admit(&request(1000, 3 * GIB + GIB / 2)), Ok(()));
    assert!(host.admit(&request(1000, 3 * GIB + GIB / 2 + 1)).is_err());
}

#[test]
fn foreground_cpu_use_beyond_its_entitlement_shrinks_what_can_be_admitted() {
    // T = 10, F = 2, static ceiling 8. The foreground uses 5: ceiling 5.
    let mut host = Host::shared_workstation();
    host.set_cpu(5000, 2000);
    host.grants = vec![running(1, 2000, 2 * GIB, GIB)];

    assert_eq!(host.admit(&request(3000, GIB)), Ok(()));
    let rejection = host.admit(&request(3500, GIB)).expect_err("contracted");
    assert_eq!(rejection.code, QueueReasonCode::ForegroundProtection);
    assert_eq!(
        rejection.message,
        "needs 3.5 CPUs; 3.0 CPUs can be granted now (managed ceiling 5.0 of a static 8.0, because the foreground uses 5.0 against a 2.0 entitlement; reserved 2.0)"
    );
    // More than the static ceiling has room for is plain lack of capacity.
    let rejection = host.admit(&request(6500, GIB)).expect_err("full");
    assert_eq!(rejection.code, QueueReasonCode::WaitingForCapacity);

    // A boost restores the static ceiling and nothing beyond it.
    host.control.boost_until = Some(host.now.plus_secs(60));
    assert_eq!(host.admit(&request(3500, GIB)), Ok(()));
    assert_eq!(host.admit(&request(6000, GIB)), Ok(()));
    assert!(host.admit(&request(6001, GIB)).is_err());
}

#[test]
fn an_idle_foreground_does_not_lend_its_reserve() {
    let mut host = Host::shared_workstation();
    host.set_cpu(0, 0);
    assert_eq!(host.admit(&request(8000, GIB)), Ok(()));
    let rejection = host.admit(&request(8001, GIB)).expect_err("reserve");
    assert_eq!(rejection.code, QueueReasonCode::MachineTooSmall);
    // With and without a boost.
    host.control.boost_until = Some(host.now.plus_secs(60));
    assert_eq!(host.admit(&request(8000, GIB)), Ok(()));
    assert!(host.admit(&request(8001, GIB)).is_err());
}

#[test]
fn live_disk_check_keeps_the_floor_and_unwritten_promises() {
    let mut host = Host::shared_workstation();
    // Floor 75 GiB of a 500 GiB volume.
    host.sample().disk_free_bytes = 100 * GIB;
    let disk = |bytes| ResourceVector::new(1000, GIB, bytes);
    assert_eq!(host.admit(&disk(25 * GIB)), Ok(()));
    let rejection = host.admit(&disk(26 * GIB)).expect_err("floor");
    assert_eq!(rejection.code, QueueReasonCode::DiskLow);
    assert_eq!(
        rejection.message,
        "needs 26.0 GiB of scratch disk; 25.0 GiB can be granted now (free 100.0, floor 75.0, promised 0.0)"
    );

    let mut waiting = offered(1, 1000, GIB);
    waiting.reserved.disk_bytes = 10 * GIB;
    host.grants = vec![waiting];
    assert_eq!(host.admit(&disk(15 * GIB)), Ok(()));
    let rejection = host.admit(&disk(15 * GIB + 1)).expect_err("promised");
    assert!(
        rejection.message.contains("promised 10.0"),
        "{}",
        rejection.message
    );
}

#[test]
fn an_empty_request_passes_only_while_the_host_admits() {
    let mut host = Host::workstation();
    assert_eq!(host.admit(&ResourceVector::ZERO), Ok(()));
    host.control.paused = true;
    assert!(host.admit(&ResourceVector::ZERO).is_err());
}
