use crate::common::{START, boot, capacity, exited, fixture, vector, with_principals};
use bella_core::api::{EntityKind, Role};
use bella_core::job::{JobPhase, Outcome};
use bella_core::protocol::{AttemptEvent, AttemptReport, LocalAttemptState, ReconcileAction};
use bella_core::{ManualClock, UnixMillis};
use bella_ledger::rusqlite::{self, OptionalExtension};
use bella_ledger::{Authenticated, Ledger, LedgerError};
use std::path::Path;
use std::sync::Arc;

fn open(path: &Path, clock: &ManualClock) -> Ledger {
    Ledger::open(path, Arc::new(clock.clone())).unwrap()
}

/// A restarted controller sees everything the previous one committed,
/// under a higher epoch, and reconciles before it offers work.
#[test]
fn reopening_preserves_everything_and_bumps_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let clock = ManualClock::at(START);

    let f = with_principals(open(&path, &clock), clock.clone());
    assert_eq!(f.ledger.epoch(), 1);
    let fleet = f.ledger.fleet_id().clone();
    let machine = f.machine("builder");
    let queued = f.submit("app");
    let offered = f.submit("app");
    let running = f.submit("app");
    let finished = f.submit("lib");
    let pending = f.offer(&offered, &machine);
    f.accept(&pending);
    let attempt = f.start(&running, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);
    let done = f.start(&finished, &machine);
    f.finish(&done, Outcome::Succeeded);
    let keyed = f
        .ledger
        .submit_job(
            &f.dev,
            Some("key"),
            &f.request("app", crate::common::spec()),
        )
        .unwrap();

    let jobs_before: Vec<_> = [&queued, &offered, &running, &finished, &keyed.id]
        .into_iter()
        .map(|job| f.ledger.job(job).unwrap())
        .collect();
    let machine_before = f.ledger.machine_view(&machine).unwrap();
    let events_before = f.ledger.events_after(0, 10_000).unwrap();
    let (admin, dev) = (f.admin.clone(), f.dev.clone());
    let request = f.request("app", crate::common::spec());
    drop(f);

    clock.advance(std::time::Duration::from_secs(120));
    let ledger = open(&path, &clock);
    assert_eq!(ledger.epoch(), 2, "the epoch is bumped on every open");
    assert_eq!(*ledger.fleet_id(), fleet, "the fleet id is created once");

    // Everything committed before is still there.
    for before in &jobs_before {
        assert_eq!(&ledger.job(&before.id).unwrap(), before);
    }
    // Reservations survive the restart; connections do not.
    let machine_after = ledger.machine_view(&machine).unwrap();
    assert!(machine_before.connected && !machine_after.connected);
    assert_eq!(
        machine_after,
        bella_core::api::MachineView {
            connected: false,
            ..machine_before
        }
    );
    assert_eq!(machine_after.reserved, vector(2000, 2));
    let events_after = ledger.events_after(0, 10_000).unwrap();
    assert_eq!(&events_after[..events_before.len()], &events_before[..]);
    let started: Vec<_> = events_after[events_before.len()..].iter().collect();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].kind, "machine.disconnected");
    assert_eq!(started[1].entity_id, machine.as_str());
    assert_eq!(started[0].kind, "fleet.controller_started");
    assert_eq!(started[0].entity, EntityKind::Fleet);
    assert_eq!(started[0].data["epoch"], 2);
    assert_eq!(started[0].at, UnixMillis(START.0 + 120_000));

    // Identity and idempotency survive too.
    assert!(matches!(
        ledger.authenticate("hash-of-admin-token").unwrap(),
        Authenticated::Principal(p) if p.id == admin && p.role == Role::Admin
    ));
    assert!(matches!(
        ledger.authenticate("hash-of-builder-token").unwrap(),
        Authenticated::Machine { id, .. } if id == machine
    ));
    assert_eq!(
        ledger.submit_job(&dev, Some("key"), &request).unwrap().id,
        keyed.id
    );

    // Commands minted by the previous controller are stale.
    assert!(matches!(
        ledger.commit_start(&pending, 1),
        Err(LedgerError::StaleEpoch { got: 1, current: 2 })
    ));
    assert!(matches!(
        ledger.renew_authority(&attempt, 1),
        Err(LedgerError::StaleEpoch { .. })
    ));
    // The machine reconciled with the previous controller, not this one.
    assert!(!ledger.machine_status(&machine).unwrap().reconciled);
    let expires = ledger.now().plus_secs(15);
    assert!(matches!(
        ledger.create_offer(
            &queued,
            &machine,
            &vector(1000, 1),
            &capacity(),
            None,
            expires
        ),
        Err(LedgerError::NotReconciled(_))
    ));

    // The worker reconnects and reports what it still runs.
    ledger.machine_connected(&machine, 2).unwrap();
    let reports = [
        AttemptReport {
            attempt: attempt.clone(),
            job_id: running.clone(),
            state: LocalAttemptState::Running,
            last_event_seq: 1,
            reserved: vector(1000, 1),
            usage: None,
        },
        AttemptReport {
            attempt: pending.clone(),
            job_id: offered.clone(),
            state: LocalAttemptState::OfferHeld,
            last_event_seq: 0,
            reserved: vector(1000, 1),
            usage: None,
        },
    ];
    let actions: Vec<_> = ledger
        .reconcile(&machine, &boot(1), &reports)
        .unwrap()
        .into_iter()
        .map(|verdict| verdict.action)
        .collect();
    assert_eq!(
        actions,
        [ReconcileAction::Continue, ReconcileAction::Forget]
    );
    assert_eq!(ledger.job(&offered).unwrap().phase, JobPhase::Queued);
    ledger
        .create_offer(
            &queued,
            &machine,
            &vector(1000, 1),
            &capacity(),
            None,
            expires,
        )
        .unwrap();

    // The surviving attempt carries on under the new epoch.
    let event = bella_core::protocol::AttemptEventMsg {
        attempt: attempt.clone(),
        seq: 2,
        at: ledger.now(),
        event: exited(Outcome::Succeeded),
    };
    ledger.record_attempt_event(&event).unwrap();
    assert_eq!(
        ledger.job(&running).unwrap().outcome,
        Some(Outcome::Succeeded)
    );
    ledger.renew_authority(&attempt, 2).unwrap();

    // A third open bumps again.
    drop(ledger);
    assert_eq!(open(&path, &clock).epoch(), 3);
}

#[test]
fn a_file_backed_ledger_uses_wal_with_full_sync_and_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = open(&path, &ManualClock::at(START));
    let pragma = |name: &str| -> String {
        ledger
            .with_transaction(|tx| {
                let value = tx.query_row(&format!("PRAGMA {name}"), [], |row| {
                    row.get::<_, rusqlite::types::Value>(0)
                })?;
                Ok::<_, LedgerError>(value)
            })
            .map(|value: rusqlite::types::Value| match value {
                rusqlite::types::Value::Integer(n) => n.to_string(),
                rusqlite::types::Value::Text(text) => text,
                other => format!("{other:?}"),
            })
            .unwrap()
    };
    assert_eq!(pragma("journal_mode"), "wal");
    assert_eq!(pragma("synchronous"), "2", "2 is FULL");
    assert_eq!(pragma("foreign_keys"), "1");
    assert!(std::fs::exists(dir.path().join("ledger.sqlite-wal")).unwrap());

    // Foreign keys are enforced.
    let violation = ledger.with_transaction(|tx| {
        let inserted = tx.execute(
            "INSERT INTO idempotency_keys (principal_id, key, request_digest, job_id, created_at)
             VALUES ('prn_missing', 'k', 'd', 'job_missing', 0)",
            [],
        )?;
        Ok::<_, LedgerError>(inserted)
    });
    assert!(matches!(violation, Err(LedgerError::Sqlite(_))));
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let clock = ManualClock::at(START);
    let ledger = open(&path, &clock);
    ledger
        .with_transaction(|tx| {
            let updated = tx.execute(
                "UPDATE schema_versions SET version = version + 1 WHERE component = 'ledger'",
                [],
            )?;
            Ok::<_, LedgerError>(updated)
        })
        .unwrap();
    drop(ledger);

    let error = Ledger::open(&path, Arc::new(clock.clone())).unwrap_err();
    match error {
        LedgerError::SchemaTooNew {
            component,
            found,
            supported,
        } => {
            assert_eq!(component, "ledger");
            assert_eq!(found, supported + 1);
        }
        other => panic!("unexpected error {other}"),
    }
    // The refused open changed nothing: the epoch was not bumped.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let epoch: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'controller_epoch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(epoch, "1");
}

#[test]
fn components_keep_their_own_migrations_in_the_same_database() {
    let f = fixture();
    let v1 = "CREATE TABLE cloud_intents (id TEXT PRIMARY KEY, state TEXT NOT NULL);";
    let v2 = "ALTER TABLE cloud_intents ADD COLUMN budget INTEGER NOT NULL DEFAULT 0;
              CREATE INDEX cloud_intents_by_state ON cloud_intents (state);";
    assert_eq!(
        f.ledger.apply_component_migrations("cloud", &[v1]).unwrap(),
        1
    );
    // Applying the same list again does nothing; a longer list applies the rest.
    assert_eq!(
        f.ledger.apply_component_migrations("cloud", &[v1]).unwrap(),
        1
    );
    assert_eq!(
        f.ledger
            .apply_component_migrations("cloud", &[v1, v2])
            .unwrap(),
        2
    );
    assert_eq!(
        f.ledger
            .apply_component_migrations("cloud", &[v1, v2])
            .unwrap(),
        2
    );
    // Versions are tracked per component.
    assert_eq!(
        f.ledger
            .apply_component_migrations("other", &["CREATE TABLE other_things (id TEXT);"])
            .unwrap(),
        1
    );

    // A database ahead of the code is refused.
    assert!(matches!(
        f.ledger.apply_component_migrations("cloud", &[v1]),
        Err(LedgerError::SchemaTooNew {
            found: 2,
            supported: 1,
            ..
        })
    ));
    // A failing migration applies nothing, including earlier entries of the
    // same call.
    let broken = f.ledger.apply_component_migrations(
        "third",
        &["CREATE TABLE third_things (id TEXT);", "THIS IS NOT SQL;"],
    );
    assert!(matches!(broken, Err(LedgerError::Sqlite(_))));
    assert_eq!(
        f.ledger
            .apply_component_migrations("third", &["CREATE TABLE third_things (id TEXT);"])
            .unwrap(),
        1
    );
    // The ledger's own component is not available to others.
    assert!(matches!(
        f.ledger.apply_component_migrations("ledger", &[v1]),
        Err(LedgerError::Invalid(_))
    ));

    // The component reads and writes its tables in ledger transactions, and
    // its events join the same log.
    let before = f.ledger.last_event_seq().unwrap();
    let now = f.now();
    let seq = f
        .ledger
        .with_transaction(|tx| {
            tx.execute(
                "INSERT INTO cloud_intents (id, state, budget) VALUES ('cld_1', 'requested', 5)",
                [],
            )?;
            let seq = Ledger::append_event(
                tx,
                now,
                EntityKind::Cloud,
                "cld_1",
                None,
                "cloud.intent_requested",
                &serde_json::json!({ "budget": 5 }),
            )?;
            Ok::<_, LedgerError>(seq)
        })
        .unwrap();
    assert_eq!(seq, before + 1);
    let event = f.ledger.events_after(before, 10).unwrap().remove(0);
    assert_eq!(event.kind, "cloud.intent_requested");
    assert_eq!(event.entity, EntityKind::Cloud);
    assert_eq!(event.at, now);

    // An error rolls the whole transaction back, event included.
    let failed: Result<(), LedgerError> = f.ledger.with_transaction(|tx| {
        tx.execute("UPDATE cloud_intents SET state = 'launched'", [])?;
        Ledger::append_event(
            tx,
            now,
            EntityKind::Cloud,
            "cld_1",
            None,
            "cloud.intent_launched",
            &serde_json::Value::Null,
        )?;
        tx.execute(
            "INSERT INTO cloud_intents (id, state) VALUES ('cld_1', 'duplicate')",
            [],
        )?;
        Ok(())
    });
    assert!(failed.is_err());
    assert_eq!(f.ledger.last_event_seq().unwrap(), seq);
    let state: Option<String> = f
        .ledger
        .with_transaction(|tx| {
            let state = tx
                .query_row(
                    "SELECT state FROM cloud_intents WHERE id = 'cld_1'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            Ok::<_, LedgerError>(state)
        })
        .unwrap();
    assert_eq!(state.as_deref(), Some("requested"));
}

#[test]
fn an_in_memory_ledger_starts_at_epoch_one_with_its_own_fleet() {
    let a = fixture();
    let b = fixture();
    assert_eq!(a.ledger.epoch(), 1);
    assert_ne!(a.ledger.fleet_id(), b.ledger.fleet_id());
    let kinds: Vec<_> = a
        .ledger
        .events_after(0, 2)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(kinds, ["fleet.created", "fleet.controller_started"]);
    let newest = a.ledger.last_event_seq().unwrap();
    assert!(a.ledger.events_after(newest, 10).unwrap().is_empty());
}

/// Two controller processes must never both write. The one that opened
/// the database last owns it; the other is fenced out.
#[test]
fn a_superseded_ledger_can_no_longer_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let clock = ManualClock::at(START);
    let old = with_principals(open(&path, &clock), clock.clone());
    let machine = old.machine("builder");
    let job = old.submit("app");
    let attempt = old.offer(&job, &machine);
    let request = old.request("app", crate::common::spec());

    let new = open(&path, &clock);
    assert_eq!((old.ledger.epoch(), new.epoch()), (1, 2));

    // Every kind of write through the old handle is refused.
    let stale = |error: LedgerError| {
        assert!(
            matches!(error, LedgerError::StaleEpoch { got: 1, current: 2 }),
            "{error}"
        );
    };
    stale(old.ledger.submit_job(&old.dev, None, &request).unwrap_err());
    stale(old.ledger.revoke_offer(&attempt, "late").unwrap_err());
    stale(old.ledger.request_cancel(&job).unwrap_err());
    stale(old.ledger.reconcile(&machine, &boot(1), &[]).unwrap_err());
    stale(
        old.ledger
            .apply_component_migrations("cloud", &["CREATE TABLE cloud_x (id TEXT);"])
            .unwrap_err(),
    );
    stale(
        old.ledger
            .with_transaction(|tx| {
                tx.execute("DELETE FROM events", [])?;
                Ok::<_, LedgerError>(())
            })
            .unwrap_err(),
    );

    // It can still read, and what it reads is what the new owner sees.
    assert_eq!(old.ledger.job(&job).unwrap(), new.job(&job).unwrap());
    assert_eq!(new.job(&job).unwrap().phase, JobPhase::Offered);
    // The new owner writes as usual.
    new.request_cancel(&job).unwrap();
    assert_eq!(old.ledger.job(&job).unwrap().phase, JobPhase::Finished);
}
