use crate::common::{boot, capacity, fixture, heartbeat, inventory, vector};
use bella_core::ErrorCode;
use bella_core::machine::{
    AvailabilityFlag, LocalControl, MachineLifecycle, MachinePolicy, MachineProfile, PressureLevel,
    Telemetry,
};
use bella_core::platform::{Capability, CapabilityState};
use bella_core::protocol::{AttemptReport, LocalAttemptState, UsageSummary};
use bella_core::resources::ResourceVector;
use bella_ledger::{LedgerError, NewMachine};

#[test]
fn enrollment_assigns_an_id_the_profile_defaults_and_a_unique_name() {
    let f = fixture();
    let new = |token_hash: &'static str| {
        f.ledger
            .enroll_machine(&NewMachine {
                name: "studio",
                token_hash,
                boot_id: &boot(1),
                inventory: &inventory("studio", 10, 32),
                profile: MachineProfile::ProtectedWorkstation,
            })
            .unwrap()
    };
    let first = new("hash-one");
    let second = new("hash-two");
    assert_ne!(first.machine_id, second.machine_id);
    assert_eq!(first.name, "studio");
    assert_eq!(second.name, "studio-2");
    assert_eq!(
        first.policy,
        MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation)
    );

    let view = f.ledger.machine_view(&first.machine_id).unwrap();
    assert_eq!(view.lifecycle, MachineLifecycle::Ready);
    assert!(!view.connected);
    assert_eq!(view.reserved, ResourceVector::ZERO);
    assert_eq!(view.managed_ceiling.cpu_milli, 8000);
    assert!(view.last_seen_at.is_none());
    let status = f.ledger.machine_status(&first.machine_id).unwrap();
    assert!(
        !status.reconciled,
        "a machine reconciles before it gets work"
    );
    assert_eq!(status.boot_id, Some(boot(1)));
    assert_eq!(f.ledger.machine_views().unwrap().len(), 2);
}

#[test]
fn inventory_revision_increases_only_when_the_content_changes() {
    let f = fixture();
    let machine = f.enroll("builder", 8, 16);
    let mut inv = inventory("builder", 8, 16);
    // The same content with any revision number changes nothing.
    inv.revision = 40;
    assert_eq!(f.ledger.update_inventory(&machine, &inv).unwrap(), 1);
    assert_eq!(f.ledger.update_inventory(&machine, &inv).unwrap(), 1);

    inv.capabilities.push(Capability {
        name: "xcode".into(),
        version: Some("16.2".into()),
        state: CapabilityState::Installed,
        attributes: Default::default(),
    });
    assert_eq!(f.ledger.update_inventory(&machine, &inv).unwrap(), 2);
    assert_eq!(f.ledger.update_inventory(&machine, &inv).unwrap(), 2);
    let view = f.ledger.machine_view(&machine).unwrap();
    assert_eq!(view.inventory.revision, 2);
    assert_eq!(view.inventory.capabilities.len(), 1);
}

#[test]
fn the_ledger_assigns_policy_revisions() {
    let f = fixture();
    let machine = f.enroll("builder", 8, 16);
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.revision = 99;
    // Same content: nothing changes, whatever revision the caller sent.
    assert_eq!(
        f.ledger
            .replace_machine_policy(&f.admin, &machine, &policy)
            .unwrap()
            .revision,
        1
    );
    policy.max_concurrent_attempts = Some(2);
    let second = f
        .ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    assert_eq!(second.revision, 2);
    policy.accept_builds = false;
    policy.revision = 0;
    let third = f
        .ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    assert_eq!(third.revision, 3);
    assert_eq!(f.ledger.machine_view(&machine).unwrap().policy, third);
}

#[test]
fn renaming_keeps_names_unique() {
    let f = fixture();
    let first = f.enroll("alpha", 8, 16);
    let second = f.enroll("beta", 8, 16);
    f.ledger.rename_machine(&second, "gamma").unwrap();
    assert_eq!(f.ledger.machine_view(&second).unwrap().name, "gamma");
    assert!(matches!(
        f.ledger.rename_machine(&second, "alpha"),
        Err(LedgerError::Conflict(_))
    ));
    assert!(matches!(
        f.ledger.rename_machine(&second, "  "),
        Err(LedgerError::Invalid(_))
    ));
    // A retired machine frees its name.
    f.ledger.retire_machine(&first).unwrap();
    f.ledger.rename_machine(&second, "alpha").unwrap();
}

#[test]
fn a_heartbeat_updates_what_the_machine_view_shows() {
    let f = fixture();
    let machine = f.machine("builder");
    f.advance_secs(5);
    let mut hb = heartbeat(&machine, &boot(1), 1, 1);
    hb.telemetry = Telemetry {
        sampled_at: f.now(),
        cpu_used_milli: 3200,
        memory_total_bytes: 64 << 30,
        memory_available_bytes: 40 << 30,
        memory_pressure: PressureLevel::Normal,
        ..Telemetry::default()
    };
    hb.effective_budget = vector(6000, 12);
    hb.flags = vec![AvailabilityFlag::OnBattery, AvailabilityFlag::DiskLow];
    hb.local_control = LocalControl {
        paused: true,
        draining: false,
        boost_until: None,
    };
    let outcome = f.ledger.record_heartbeat(&hb).unwrap();
    assert!(!outcome.needs_reconcile);
    assert!(!outcome.policy_stale);
    assert_eq!(outcome.policy_revision, 1);

    let status = f.ledger.machine_status(&machine).unwrap();
    assert!(status.admitting);
    assert_eq!(status.view.last_seen_at, Some(f.now()));
    assert_eq!(status.view.telemetry.as_ref().unwrap().cpu_used_milli, 3200);
    assert_eq!(status.view.effective_budget, Some(vector(6000, 12)));
    assert_eq!(
        status.view.flags,
        vec![AvailabilityFlag::OnBattery, AvailabilityFlag::DiskLow]
    );
    assert!(status.view.local_control.paused);

    // A worker running an old policy revision is reported.
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.low_priority = true;
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    let outcome = f
        .ledger
        .record_heartbeat(&heartbeat(&machine, &boot(1), 1, 2))
        .unwrap();
    assert!(outcome.policy_stale);
    assert_eq!(outcome.policy_revision, 2);
}

#[test]
fn a_heartbeat_reports_usage_without_shrinking_the_reservation() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    let mut hb = heartbeat(&machine, &boot(1), 1, 1);
    hb.attempts = vec![AttemptReport {
        attempt: attempt.clone(),
        job_id: job.clone(),
        state: LocalAttemptState::Running,
        last_event_seq: 0,
        reserved: vector(1000, 1),
        usage: Some(UsageSummary {
            memory_current_bytes: 1 << 20,
            ..UsageSummary::default()
        }),
    }];
    f.ledger.record_heartbeat(&hb).unwrap();
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert_eq!(record.usage.unwrap().memory_current_bytes, 1 << 20);
    assert_eq!(f.held(&machine), vector(1000, 1));
}

#[test]
fn a_heartbeat_from_another_boot_or_an_old_connection_is_not_trusted() {
    let f = fixture();
    let machine = f.machine("builder");
    f.ledger
        .record_heartbeat(&heartbeat(&machine, &boot(1), 1, 5))
        .unwrap();

    // A sequence that goes backwards needs a full reconcile.
    let regressed = f
        .ledger
        .record_heartbeat(&heartbeat(&machine, &boot(1), 1, 3))
        .unwrap();
    assert!(regressed.needs_reconcile);

    // A different boot needs one too, and the recorded boot is not replaced.
    let rebooted = f
        .ledger
        .record_heartbeat(&heartbeat(&machine, &boot(2), 1, 6))
        .unwrap();
    assert!(rebooted.needs_reconcile);
    assert_eq!(
        f.ledger.machine_status(&machine).unwrap().boot_id,
        Some(boot(1))
    );

    // A newer connection has taken over; the old one is refused.
    f.ledger.machine_connected(&machine, 2).unwrap();
    let error = f
        .ledger
        .record_heartbeat(&heartbeat(&machine, &boot(1), 1, 7))
        .unwrap_err();
    assert!(matches!(
        error,
        LedgerError::StaleConnection {
            got: 1,
            current: 2,
            ..
        }
    ));
    assert_eq!(error.code(), ErrorCode::StaleGeneration);
    assert!(matches!(
        f.ledger.machine_connected(&machine, 1),
        Err(LedgerError::StaleConnection { .. })
    ));
}

#[test]
fn a_reconnected_machine_gets_no_offer_until_it_reconciles() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    assert!(f.ledger.machine_status(&machine).unwrap().reconciled);

    // The close of an older connection changes nothing.
    assert!(!f.ledger.machine_disconnected(&machine, 0).unwrap());
    assert!(f.ledger.machine_view(&machine).unwrap().connected);

    assert!(f.ledger.machine_disconnected(&machine, 1).unwrap());
    assert!(!f.ledger.machine_disconnected(&machine, 1).unwrap());
    let status = f.ledger.machine_status(&machine).unwrap();
    assert!(!status.view.connected);
    assert!(!status.reconciled);

    f.ledger.machine_connected(&machine, 2).unwrap();
    let error = f.try_offer(&job, &machine, &vector(1000, 1)).unwrap_err();
    assert!(matches!(error, LedgerError::NotReconciled(_)));
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(
        f.ledger
            .record_heartbeat(&heartbeat(&machine, &boot(1), 2, 1))
            .unwrap()
            .needs_reconcile
    );

    f.ledger.reconcile(&machine, &boot(1), &[]).unwrap();
    f.try_offer(&job, &machine, &vector(1000, 1)).unwrap();
}

#[test]
fn controller_conditions_are_set_and_cleared_once() {
    let f = fixture();
    let machine = f.machine("builder");
    let mut hb = heartbeat(&machine, &boot(1), 1, 1);
    hb.flags = vec![AvailabilityFlag::OnBattery];
    f.ledger.record_heartbeat(&hb).unwrap();

    assert!(
        f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::Stale, true)
            .unwrap()
    );
    assert!(
        !f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::Stale, true)
            .unwrap()
    );
    assert!(
        f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::Draining, true)
            .unwrap()
    );
    assert_eq!(
        f.ledger.machine_view(&machine).unwrap().flags,
        vec![
            AvailabilityFlag::Draining,
            AvailabilityFlag::OnBattery,
            AvailabilityFlag::Stale
        ]
    );
    assert!(
        f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::Stale, false)
            .unwrap()
    );
    assert!(
        !f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::Stale, false)
            .unwrap()
    );
    // The worker's own flag is not the controller's to clear.
    assert!(
        !f.ledger
            .set_machine_condition(&machine, AvailabilityFlag::OnBattery, false)
            .unwrap()
    );
    assert_eq!(
        f.ledger.machine_view(&machine).unwrap().flags,
        vec![AvailabilityFlag::Draining, AvailabilityFlag::OnBattery]
    );

    let kinds: Vec<String> = f
        .ledger
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.entity_id == machine.as_str())
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "machine.enrolled",
            "machine.connected",
            "machine.reconciled",
            "machine.flags_changed",
            "machine.stale",
            "machine.draining",
            "machine.stale_cleared"
        ]
    );
}

#[test]
fn retiring_a_machine_stops_offers_but_releases_nothing() {
    let f = fixture();
    let machine = f.machine("builder");
    let running = f.submit("app");
    let waiting = f.submit("app");
    f.start(&running, &machine);

    assert!(f.ledger.retire_machine(&machine).unwrap());
    assert!(!f.ledger.retire_machine(&machine).unwrap());
    let view = f.ledger.machine_view(&machine).unwrap();
    assert_eq!(view.lifecycle, MachineLifecycle::Retired);
    assert!(!view.connected);
    assert_eq!(
        view.reserved,
        vector(1000, 1),
        "the reservation is still held"
    );
    assert_eq!(view.attempts.len(), 1);

    let error = f
        .try_offer(&waiting, &machine, &vector(1000, 1))
        .unwrap_err();
    assert!(matches!(error, LedgerError::PolicyDenied(_)));
    assert!(matches!(
        f.ledger.machine_connected(&machine, 2),
        Err(LedgerError::Unauthorized(_))
    ));

    let other = f.machine("spare");
    assert!(f.ledger.revoke_machine(&other).unwrap());
    assert_eq!(
        f.ledger.machine_view(&other).unwrap().lifecycle,
        MachineLifecycle::Revoked
    );
}

#[test]
fn reserved_is_the_sum_of_held_reservations() {
    let f = fixture();
    let machine = f.machine("builder");
    let jobs: Vec<_> = (0..3).map(|_| f.submit("app")).collect();
    let sizes = [
        vector(1000, 2),
        vector(2000, 3).with_token("gpu", 1),
        vector(500, 1),
    ];
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.tokens.insert("gpu".into(), 2);
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    let cap = capacity().with_token("gpu", 2);
    for (job, size) in jobs.iter().zip(&sizes) {
        f.ledger
            .create_offer(job, &machine, size, &cap, None, f.expiry())
            .unwrap();
    }
    let view = f.ledger.machine_view(&machine).unwrap();
    assert_eq!(view.reserved, vector(3500, 6).with_token("gpu", 1));
    assert_eq!(view.attempts.len(), 3);
    assert_eq!(view.attempts[0].project_name, "app");
    assert_eq!(f.ledger.machine_attempts(&machine).unwrap().len(), 3);
    let app = f.ledger.project_by_name("app").unwrap();
    assert_eq!(
        f.ledger.project_reserved(&app.id).unwrap(),
        vector(3500, 6).with_token("gpu", 1)
    );
}
