//! Identity, counters and what survives closing the journal.

mod common;

use bella_core::machine::{LocalControl, MachinePolicy, MachineProfile};
use bella_core::protocol::{AttemptEvent, LocalAttemptState, RejectCode, StepState};
use bella_core::{Clock, MachineId, UnixMillis};
use bella_journal::{Enrollment, Journal, JournalError, RESERVE_BYTES, SCHEMA_VERSION};
use common::*;
use std::sync::Arc;

#[test]
fn reopening_preserves_identity_counters_control_and_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let control = LocalControl {
        paused: true,
        draining: true,
        boost_until: Some(T0.plus_secs(900)),
    };
    let held = offer(1);
    let running = offer(2);
    let before = {
        let journal = open(&path, &clock);
        assert_eq!(journal.path(), Some(path.as_path()));
        journal.recover(&boot(1)).unwrap();
        assert!(journal.set_enrollment(&enrollment()).unwrap());
        journal.set_local_control(control).unwrap();
        assert_eq!(journal.next_connection_generation().unwrap(), 1);
        assert_eq!(journal.next_connection_generation().unwrap(), 2);
        journal.observe_epoch(7).unwrap();
        journal.hold_offer(&held).unwrap();
        let attempt = run(&journal, &running, authority(T0));
        journal
            .set_step_state(&attempt.attempt_id, "build", StepState::Succeeded, Some(0))
            .unwrap();
        journal.request_cancel(&attempt, "user cancelled").unwrap();
        journal.attempts().unwrap()
    };

    let journal = open(&path, &clock);
    assert_eq!(journal.enrollment().unwrap(), Some(enrollment()));
    assert_eq!(journal.local_control().unwrap(), control);
    assert_eq!(journal.connection_generation().unwrap(), 2);
    assert_eq!(journal.next_connection_generation().unwrap(), 3);
    assert_eq!(journal.known_epoch().unwrap(), 7);
    assert!(matches!(
        journal.observe_epoch(6),
        Err(JournalError::StaleEpoch {
            known: 7,
            offered: 6
        })
    ));

    // Every field of every attempt, the full offer included.
    let after = journal.attempts().unwrap();
    assert_eq!(after, before);
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].offer, held);
    assert_eq!(after[0].state, LocalAttemptState::OfferHeld);
    assert_eq!(after[1].offer, running);
    assert_eq!(after[1].state, LocalAttemptState::Running);
    assert_eq!(after[1].authority, Some(authority(T0)));
    assert_eq!(after[1].scope, Some(scope("bella-attempt")));
    assert_eq!(after[1].workspace.as_deref(), Some("work/attempt"));
    assert_eq!(after[1].steps["build"].exit_code, Some(0));
    assert_eq!(
        after[1]
            .cancel
            .as_ref()
            .map(|cancel| cancel.reason.as_str()),
        Some("user cancelled")
    );
    assert_eq!(after[1].boot_id, boot(1));
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation().saturating_add(&reservation())
    );
    assert_eq!(journal.reports().unwrap().len(), 2);
}

#[test]
fn the_controller_epoch_never_goes_backwards() {
    let clock = clock();
    let journal = memory(&clock);
    assert_eq!(journal.known_epoch().unwrap(), 0);
    journal.observe_epoch(3).unwrap();
    journal.observe_epoch(3).unwrap();
    journal.observe_epoch(5).unwrap();
    for older in [0, 1, 4] {
        let refused = journal.observe_epoch(older).unwrap_err();
        assert!(matches!(
            refused,
            JournalError::StaleEpoch { known: 5, offered } if offered == older
        ));
        assert_eq!(refused.reject_code(), Some(RejectCode::StaleEpoch));
        assert_eq!(journal.known_epoch().unwrap(), 5);
    }
    journal.observe_epoch(u64::from(u32::MAX) + 9).unwrap();
    assert_eq!(journal.known_epoch().unwrap(), u64::from(u32::MAX) + 9);
    // A value the journal cannot store is refused, not wrapped.
    assert!(matches!(
        journal.observe_epoch(u64::MAX),
        Err(JournalError::OutOfRange(_))
    ));
    assert_eq!(journal.known_epoch().unwrap(), u64::from(u32::MAX) + 9);
}

#[test]
fn connection_generations_increase_one_by_one_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let mut seen = Vec::new();
    for _ in 0..4 {
        let journal = open(&path, &clock);
        seen.push(journal.next_connection_generation().unwrap());
        seen.push(journal.next_connection_generation().unwrap());
    }
    assert_eq!(seen, (1..=8).collect::<Vec<u64>>());
}

#[test]
fn a_failed_transaction_does_not_consume_a_connection_generation() {
    let clock = clock();
    let journal = memory(&clock);
    assert_eq!(journal.next_connection_generation().unwrap(), 1);
    let failed: Result<u64, JournalError> = journal.with_transaction(|tx| {
        let generation = tx.next_connection_generation()?;
        tx.observe_epoch(4)?;
        tx.observe_epoch(2)?;
        Ok(generation)
    });
    assert!(matches!(failed, Err(JournalError::StaleEpoch { .. })));
    assert_eq!(journal.connection_generation().unwrap(), 1);
    assert_eq!(journal.known_epoch().unwrap(), 0);
    assert_eq!(journal.next_connection_generation().unwrap(), 2);
}

#[test]
fn enrollment_is_set_once() {
    let clock = clock();
    let journal = memory(&clock);
    assert_eq!(journal.enrollment().unwrap(), None);
    assert_eq!(journal.policy().unwrap(), None);
    assert!(matches!(
        journal.set_policy(&enrollment().policy),
        Err(JournalError::NotEnrolled)
    ));

    assert!(journal.set_enrollment(&enrollment()).unwrap());
    assert!(
        !journal.set_enrollment(&enrollment()).unwrap(),
        "the same enrollment again is accepted and changes nothing"
    );
    let elsewhere = Enrollment {
        machine_id: MachineId::derive(b"another machine"),
        ..enrollment()
    };
    assert!(matches!(
        journal.set_enrollment(&elsewhere),
        Err(JournalError::AlreadyEnrolled)
    ));
    let moved = Enrollment {
        fingerprint: "00".repeat(32),
        ..enrollment()
    };
    assert!(matches!(
        journal.set_enrollment(&moved),
        Err(JournalError::AlreadyEnrolled)
    ));
    assert_eq!(journal.enrollment().unwrap(), Some(enrollment()));
}

#[test]
fn the_controller_endpoint_can_move_without_enrolling_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let new_url = "https://controller.example.net:9443";
    let new_fingerprint = "ab".repeat(32);
    let moved = Enrollment {
        controller_url: new_url.to_owned(),
        fingerprint: new_fingerprint.clone(),
        ..enrollment()
    };
    let control = LocalControl {
        paused: true,
        draining: false,
        boost_until: None,
    };
    let held = offer(1);
    let running = offer(2);

    let (attempts, reports, events) = {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        assert!(matches!(
            journal.set_controller_endpoint(new_url, &new_fingerprint),
            Err(JournalError::NotEnrolled)
        ));
        assert_eq!(journal.enrollment().unwrap(), None);

        assert!(journal.set_enrollment(&enrollment()).unwrap());
        let mut policy = enrollment().policy;
        policy.revision += 1;
        assert!(journal.set_policy(&policy).unwrap());
        journal.set_local_control(control).unwrap();
        assert_eq!(journal.next_connection_generation().unwrap(), 1);
        journal.observe_epoch(7).unwrap();
        journal.hold_offer(&held).unwrap();
        let attempt = run(&journal, &running, authority(T0));
        journal
            .set_step_state(&attempt.attempt_id, "build", StepState::Succeeded, Some(0))
            .unwrap();
        journal
            .append_event(&attempt, AttemptEvent::Preparing)
            .unwrap();
        let before = (
            journal.attempts().unwrap(),
            journal.reports().unwrap(),
            journal.unacked_events(&attempt.attempt_id).unwrap(),
        );
        assert_eq!(before.2.len(), 1);

        // The address alone, then the certificate alone: each is a change.
        assert!(
            journal
                .set_controller_endpoint(new_url, &enrollment().fingerprint)
                .unwrap()
        );
        assert!(
            journal
                .set_controller_endpoint(new_url, &new_fingerprint)
                .unwrap()
        );
        assert!(
            !journal
                .set_controller_endpoint(new_url, &new_fingerprint)
                .unwrap(),
            "the same endpoint again changes nothing"
        );
        assert_eq!(
            journal.enrollment().unwrap(),
            Some(Enrollment {
                policy: policy.clone(),
                ..moved.clone()
            })
        );
        // Enrollment stays a one-time record: the new endpoint is the
        // stored identity, the old one no longer is, and neither call
        // touches the policy.
        assert!(!journal.set_enrollment(&moved).unwrap());
        assert!(matches!(
            journal.set_enrollment(&enrollment()),
            Err(JournalError::AlreadyEnrolled)
        ));
        assert_eq!(journal.policy().unwrap(), Some(policy));

        assert_eq!(
            (
                journal.attempts().unwrap(),
                journal.reports().unwrap(),
                journal.unacked_events(&attempt.attempt_id).unwrap(),
            ),
            before
        );
        before
    };

    // After a restart the worker reads the new endpoint and nothing else
    // has moved.
    let journal = open(&path, &clock);
    let stored = journal.enrollment().unwrap().unwrap();
    assert_eq!(stored.controller_url, new_url);
    assert_eq!(stored.fingerprint, new_fingerprint);
    assert_eq!(stored.fleet_id, enrollment().fleet_id);
    assert_eq!(stored.machine_id, enrollment().machine_id);
    assert_eq!(stored.policy.revision, enrollment().policy.revision + 1);
    assert_eq!(journal.local_control().unwrap(), control);
    assert_eq!(journal.connection_generation().unwrap(), 1);
    assert_eq!(journal.known_epoch().unwrap(), 7);
    assert_eq!(journal.attempts().unwrap(), attempts);
    assert_eq!(journal.attempts().unwrap().len(), 2);
    assert_eq!(journal.reports().unwrap(), reports);
    assert_eq!(
        journal
            .unacked_events(&attempts[1].offer.attempt.attempt_id)
            .unwrap(),
        events
    );
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation().saturating_add(&reservation())
    );
}

/// An endpoint change shares a transaction with whatever else the caller
/// records, and is undone with it.
#[test]
fn an_endpoint_change_rolls_back_with_its_transaction() {
    let clock = clock();
    let journal = memory(&clock);
    journal.set_enrollment(&enrollment()).unwrap();
    let failed: Result<(), JournalError> = journal.with_transaction(|tx| {
        assert!(tx.set_controller_endpoint("https://other.example:1", "cd")?);
        assert_eq!(
            tx.enrollment()?.map(|e| e.controller_url),
            Some("https://other.example:1".to_owned())
        );
        tx.observe_epoch(3)?;
        Err(JournalError::NotEnrolled)
    });
    assert!(failed.is_err());
    assert_eq!(journal.enrollment().unwrap(), Some(enrollment()));
    assert_eq!(journal.known_epoch().unwrap(), 0);
}

#[test]
fn the_policy_revision_only_moves_forwards() {
    let clock = clock();
    let journal = memory(&clock);
    journal.set_enrollment(&enrollment()).unwrap();
    let first = enrollment().policy;
    assert_eq!(first.revision, 1);
    assert!(!journal.set_policy(&first).unwrap());

    let mut third = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
    third.revision = 3;
    assert!(journal.set_policy(&third).unwrap());
    assert_eq!(journal.policy().unwrap(), Some(third.clone()));
    // The enrollment is read with the policy that is in force now.
    assert_eq!(journal.enrollment().unwrap().unwrap().policy, third);

    let mut second = first.clone();
    second.revision = 2;
    let stale = journal.set_policy(&second).unwrap_err();
    assert!(matches!(
        stale,
        JournalError::StalePolicyRevision {
            known: 3,
            offered: 2
        }
    ));
    assert_eq!(
        stale.reject_code(),
        Some(RejectCode::PolicyRevisionMismatch)
    );
    // One revision never names two policies.
    let mut same_number = third.clone();
    same_number.accept_builds = false;
    assert!(matches!(
        journal.set_policy(&same_number),
        Err(JournalError::StalePolicyRevision {
            known: 3,
            offered: 3
        })
    ));
    assert_eq!(journal.policy().unwrap(), Some(third));

    // Re-running enrollment with the original policy does not roll it back.
    assert!(!journal.set_enrollment(&enrollment()).unwrap());
    assert_eq!(journal.policy().unwrap().unwrap().revision, 3);
}

#[test]
fn local_control_defaults_to_running_and_is_replaced_as_a_whole() {
    let clock = clock();
    let journal = memory(&clock);
    assert_eq!(journal.local_control().unwrap(), LocalControl::default());
    let paused = LocalControl {
        paused: true,
        ..LocalControl::default()
    };
    journal.set_local_control(paused).unwrap();
    assert_eq!(journal.local_control().unwrap(), paused);
    journal.set_local_control(LocalControl::default()).unwrap();
    assert_eq!(journal.local_control().unwrap(), LocalControl::default());
}

#[test]
fn a_tool_can_set_pause_and_drain_while_the_worker_has_the_journal_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let worker = open(&path, &clock);
    worker.recover(&boot(1)).unwrap();
    let attempt = run(&worker, &offer(1), authority(T0));

    let draining = LocalControl {
        paused: false,
        draining: true,
        boost_until: None,
    };
    {
        let tool = Journal::open_existing(&path, Arc::new(clock.clone())).unwrap();
        assert!(tool.reserve().is_none());
        tool.set_local_control(draining).unwrap();
        // The tool sees the worker's attempts but cannot touch them.
        assert_eq!(tool.reports().unwrap().len(), 1);
        assert!(matches!(
            tool.set_state(&attempt.attempt_id, LocalAttemptState::Cleaning),
            Err(JournalError::NotRecovered)
        ));
    }
    // The running worker picks the change up on its next read.
    assert_eq!(worker.local_control().unwrap(), draining);
    assert_eq!(state(&worker, &attempt), LocalAttemptState::Running);
    worker
        .set_state(&attempt.attempt_id, LocalAttemptState::Collecting)
        .unwrap();
}

#[test]
fn open_existing_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    assert!(Journal::open_existing(&path, Arc::new(clock())).is_err());
    assert!(!path.exists(), "a mistyped path must not create a journal");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_newer_schema_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    {
        let journal = open(&path, &clock);
        journal.observe_epoch(11).unwrap();
    }
    let newer = SCHEMA_VERSION + 1;
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(&format!("PRAGMA user_version = {newer}"))
            .unwrap();
    }
    for attempt in [
        Journal::open(&path, Arc::new(clock.clone())),
        Journal::open_existing(&path, Arc::new(clock.clone())),
    ] {
        match attempt {
            Err(JournalError::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (newer, SCHEMA_VERSION));
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }
    // The refusal changed nothing: the newer build's data is intact.
    let raw = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, newer);
    let epoch: i64 = raw
        .query_row("SELECT known_epoch FROM identity", [], |row| row.get(0))
        .unwrap();
    assert_eq!(epoch, 11);
}

#[test]
fn a_database_that_is_not_a_journal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    // One that numbers its own schema, and one that looks unversioned.
    for (name, version) in [("ledger.db", 1), ("notes.db", 0)] {
        let path = dir.path().join(name);
        {
            let raw = rusqlite::Connection::open(&path).unwrap();
            raw.execute_batch(&format!(
                "CREATE TABLE jobs (id TEXT PRIMARY KEY); PRAGMA user_version = {version};"
            ))
            .unwrap();
        }
        assert!(
            matches!(
                Journal::open(&path, Arc::new(clock())),
                Err(JournalError::NotAJournal)
            ),
            "{name}"
        );
        // Refused without a trace: no journal tables, and not even the
        // journal mode of the foreign database was changed.
        let raw = rusqlite::Connection::open(&path).unwrap();
        let tables: Vec<String> = raw
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(tables, vec!["jobs".to_owned()], "{name}");
        let mode: String = raw
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "{name}");
        let application_id: i64 = raw
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, 0, "{name}");
        assert!(!dir.path().join(format!("{name}.reserve")).exists());
    }
}

#[test]
fn the_reserve_file_is_created_released_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let journal = open(&path, &clock);
    journal.recover(&boot(1)).unwrap();

    let reserve = journal.reserve().expect("a file journal has a reserve");
    let reserve_path = dir.path().join("journal.db.reserve");
    assert_eq!(reserve.path(), reserve_path);
    assert_eq!(reserve.size(), RESERVE_BYTES);
    assert!(journal.reserve_present());
    assert_eq!(
        std::fs::metadata(&reserve_path).unwrap().len(),
        RESERVE_BYTES
    );

    // Disk full: give the space back and keep journaling the cleanup.
    assert!(journal.release_reserve().unwrap());
    assert!(!reserve_path.exists());
    assert!(!journal.reserve_present());
    assert!(!journal.release_reserve().unwrap());
    let attempt = run(&journal, &offer(1), authority(T0));
    journal
        .set_state(&attempt.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal.release(&attempt, proof()).unwrap();

    assert!(journal.restore_reserve().unwrap());
    assert!(journal.reserve_present());
    assert!(!journal.restore_reserve().unwrap());
    assert_eq!(
        std::fs::metadata(&reserve_path).unwrap().len(),
        RESERVE_BYTES
    );

    // A released reserve comes back the next time the worker opens the
    // journal, and a tool never recreates it behind the worker's back.
    journal.release_reserve().unwrap();
    {
        let tool = Journal::open_existing(&path, Arc::new(clock.clone())).unwrap();
        assert!(!tool.release_reserve().unwrap());
        assert!(!tool.restore_reserve().unwrap());
    }
    assert!(!reserve_path.exists());
    drop(journal);
    let journal = open(&path, &clock);
    assert!(journal.reserve_present());
}

#[test]
fn an_in_memory_journal_has_no_reserve() {
    let clock = clock();
    let journal = memory(&clock);
    assert!(journal.path().is_none());
    assert!(journal.reserve().is_none());
    assert!(!journal.reserve_present());
    assert!(!journal.release_reserve().unwrap());
    assert!(!journal.restore_reserve().unwrap());
}

#[test]
fn timestamps_come_from_the_injected_clock() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    clock.set(UnixMillis(T0.0 + 5_000));
    journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap();
    let record = journal.attempt(&offer.attempt.attempt_id).unwrap().unwrap();
    assert_eq!(record.created_at, T0);
    assert_eq!(record.authorized_at, Some(clock.now()));
    assert_eq!(record.updated_at, clock.now());
    assert_eq!(record.released_at, None);
    let seen = journal.with_transaction(|tx| Ok(tx.now())).unwrap();
    assert_eq!(seen, clock.now());
}
