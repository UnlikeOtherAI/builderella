//! Named tokens: the scheduler's capacity check and the worker's own
//! admission (`bella-protect`) give the same answer for the same host.
//!
//! The worker's side is computed by `bella-protect` itself and what it
//! reports is fed to the scheduler the way a heartbeat would carry it, so a
//! change to either crate's token rule fails here.

use crate::support::*;
use bella_core::job::{Priority, QueueReasonCode as Code, RetryClass};
use bella_core::machine::{
    Hardware, LocalControl, MachinePolicy, MachineProfile, MeasurementQuality, PowerState,
    PressureLevel, Telemetry, ThermalState,
};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::{AttemptId, UnixMillis};
use bella_protect::{Grant, Input, ProtectConfig};
use bella_scheduler::{MachineState, effective_capacity, schedule};

const T0: UnixMillis = UnixMillis(NOW);

/// A quiet 16-CPU, 64 GiB builder as its own protection loop sees it.
struct Worker {
    policy: MachinePolicy,
    hardware: Hardware,
    telemetry: Telemetry,
    grants: Vec<Grant>,
    config: ProtectConfig,
}

impl Worker {
    fn declaring(tokens: &[(&str, u64)]) -> Self {
        let mut policy = MachinePolicy::defaults_for(MachineProfile::DedicatedBuilder);
        policy.tokens = tokens
            .iter()
            .map(|(name, count)| (name.to_string(), *count))
            .collect();
        Self {
            policy,
            hardware: Hardware {
                logical_cpus: 16,
                physical_cpus: None,
                cpu_model: String::new(),
                memory_bytes: 64 * GIB,
                disks: Vec::new(),
                gpus: Vec::new(),
            },
            telemetry: Telemetry {
                sampled_at: T0,
                cpu_used_milli: 500,
                cpu_managed_milli: 0,
                memory_total_bytes: 64 * GIB,
                memory_available_bytes: 60 * GIB,
                memory_managed_bytes: 0,
                swap_used_bytes: 0,
                memory_pressure: PressureLevel::Normal,
                disk_total_bytes: 500 * GIB,
                disk_free_bytes: 300 * GIB,
                power: PowerState::default(),
                thermal: ThermalState::Nominal,
                quality: MeasurementQuality::Good,
            },
            grants: Vec::new(),
            config: ProtectConfig::default(),
        }
    }

    /// Journal an accepted offer of one CPU, one GiB and `count` of `token`.
    fn hold(&mut self, n: u32, token: &str, count: u64) {
        self.grants.push(Grant {
            attempt_id: AttemptId::derive(&n.to_be_bytes()),
            reserved: wants(token, count),
            memory_used_bytes: None,
            cpu_used_milli: None,
            disk_used_bytes: None,
            started: false,
            priority: Priority::Normal,
            retry_class: RetryClass::Pure,
            started_at: None,
            suspended: false,
        });
    }

    fn input(&self) -> Input<'_> {
        Input {
            policy: &self.policy,
            hardware: &self.hardware,
            telemetry: Some(&self.telemetry),
            now: T0,
            grants: &self.grants,
            control: LocalControl::default(),
            config: &self.config,
        }
    }

    /// Would the worker accept an offer for `request` right now?
    fn admits(&self, request: &ResourceVector) -> bool {
        bella_protect::admit(&self.input(), request).is_ok()
    }

    /// The machine as the scheduler sees it after this worker's heartbeat:
    /// the ceiling comes from the controller's copy of the policy, the
    /// budget, flags and reservations from the worker.
    fn seen_with(&self, controller_policy: &MachinePolicy) -> MachineState {
        let budget = bella_protect::budget(&self.input());
        let mut seen = machine("worker", 16, 64);
        seen.policy = controller_policy.clone();
        seen.managed_ceiling = controller_policy.managed_ceiling(&self.hardware);
        seen.effective_budget = Some(budget.ceiling);
        seen.admitting = budget.admitting;
        seen.flags = budget.flags;
        seen.reserved = self.grants.iter().fold(ResourceVector::ZERO, |sum, grant| {
            sum.saturating_add(&grant.reserved)
        });
        seen.held_attempts = self.grants.len() as u32;
        seen
    }

    fn seen(&self) -> MachineState {
        self.seen_with(&self.policy)
    }
}

/// One CPU, one GiB and `count` of `token`.
fn wants(token: &str, count: u64) -> ResourceVector {
    ResourceVector::new(1000, GIB, 0).with_token(token, count)
}

/// Would the scheduler offer a job with this peak to the machine?
fn offers(seen: &MachineState, request: &ResourceVector) -> bool {
    blocker(seen, request).is_none()
}

fn blocker(
    seen: &MachineState,
    request: &ResourceVector,
) -> Option<bella_core::job::MachineBlocker> {
    let p = project("app");
    let mut s = spec(1, 1);
    s.envelope = ResourceEnvelope::fixed(request.clone());
    let queued = job("needs-token", &p, 0, s);
    let plan = schedule(&fleet(vec![seen.clone()], vec![p], vec![queued.clone()]));
    if plan.offers.len() == 1 {
        return None;
    }
    Some(blocker_on(reason_for(&plan, &queued), &seen.name).clone())
}

/// The shared scenario: the policy declares two simulators and one grant
/// holds one of them.
#[test]
fn a_held_token_leaves_the_same_room_for_the_scheduler_and_the_worker() {
    let mut worker = Worker::declaring(&[("ios-simulator", 2)]);
    worker.hold(1, "ios-simulator", 1);

    // What the worker reports: the declared count as the total, one free.
    let budget = bella_protect::budget(&worker.input());
    assert!(budget.admitting, "{:?}", budget.reasons);
    assert_eq!(budget.ceiling.token("ios-simulator"), 2);
    assert_eq!(budget.free.token("ios-simulator"), 1);

    // What the scheduler makes of it: the same total, and the same room.
    let seen = worker.seen();
    let capacity = effective_capacity(&seen.managed_ceiling, seen.effective_budget.as_ref());
    assert_eq!(capacity.token("ios-simulator"), 2);
    assert_eq!(
        capacity.token("ios-simulator") - seen.reserved.token("ios-simulator"),
        budget.free.token("ios-simulator")
    );

    for (count, fits) in [(1, true), (2, false), (3, false)] {
        let request = wants("ios-simulator", count);
        assert_eq!(worker.admits(&request), fits, "worker, {count}");
        assert_eq!(offers(&seen, &request), fits, "scheduler, {count}");
    }
    let found = blocker(&seen, &wants("ios-simulator", 2)).expect("two do not fit");
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert_eq!(
        found.message,
        "needs 2 `ios-simulator` tokens; 1 free (1 of 2 reserved)"
    );

    // The second simulator goes, and both sides are full.
    worker.hold(2, "ios-simulator", 1);
    let seen = worker.seen();
    let request = wants("ios-simulator", 1);
    assert!(!worker.admits(&request));
    assert!(!offers(&seen, &request));
    // Work that needs no simulator is unaffected on both sides.
    let plain = ResourceVector::new(1000, GIB, 0);
    assert!(worker.admits(&plain));
    assert!(offers(&seen, &plain));
}

/// The controller's policy revision declares a token before the worker has
/// applied it. The worker's budget does not name the token and the worker
/// would refuse it, so the scheduler does not offer it either.
#[test]
fn a_token_the_worker_does_not_report_is_not_offered() {
    let worker = Worker::declaring(&[("ios-simulator", 2)]);
    let mut newer = worker.policy.clone();
    newer.revision += 1;
    newer.tokens.insert("gpu".to_string(), 1);

    let seen = worker.seen_with(&newer);
    assert_eq!(seen.managed_ceiling.token("gpu"), 1);
    assert_eq!(
        seen.effective_budget.as_ref().map(|b| b.token("gpu")),
        Some(0)
    );
    let request = wants("gpu", 1);
    assert!(!worker.admits(&request));
    let found = blocker(&seen, &request).expect("the worker does not grant a gpu yet");
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert!(!found.permanent, "the worker will apply the policy");
    assert!(
        found
            .message
            .contains("the worker currently grants 0 of its 1 ceiling"),
        "{}",
        found.message
    );
    // The token both sides know is offered as before.
    assert!(worker.admits(&wants("ios-simulator", 2)));
    assert!(offers(&seen, &wants("ios-simulator", 2)));

    // Once the worker runs the new policy, both accept.
    let mut updated = worker;
    updated.policy = newer.clone();
    assert!(updated.admits(&request));
    assert!(offers(&updated.seen_with(&newer), &request));
}

/// The other way round the controller's policy decides: a token only the
/// worker declares is never offered, whatever the budget says.
#[test]
fn a_token_the_controller_policy_does_not_declare_is_never_offered() {
    let worker = Worker::declaring(&[("ios-simulator", 2), ("gpu", 1)]);
    let mut older = worker.policy.clone();
    older.tokens.remove("gpu");

    let seen = worker.seen_with(&older);
    assert_eq!(
        seen.effective_budget.as_ref().map(|b| b.token("gpu")),
        Some(1)
    );
    let found = blocker(&seen, &wants("gpu", 1)).expect("the ceiling has no gpu");
    assert_eq!(found.code, Code::MachineTooSmall);
    assert!(found.permanent);
}
