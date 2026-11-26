use crate::common::{Fixture, exited, fixture, released, spec, vector};
use bella_core::api::EnvironmentState;
use bella_core::ids::{EnvironmentId, JobId, MachineId};
use bella_core::job::{JobPhase, LeaseSpec, Outcome};
use bella_core::lease::LeaseEnd;
use bella_core::protocol::{AttemptEvent, AttemptRef, Endpoint};
use bella_core::resources::ResourceVector;
use bella_ledger::LedgerError;

fn endpoints() -> Vec<Endpoint> {
    vec![Endpoint {
        step: "server".into(),
        host: "127.0.0.1".into(),
        port: 8080,
    }]
}

fn ready() -> AttemptEvent {
    AttemptEvent::EnvironmentReady {
        endpoints: endpoints(),
    }
}

/// A started attempt whose services are up, with a 15 min idle timeout, a
/// 30 min lease and a 4 h maximum lifetime.
fn serving(f: &Fixture) -> (MachineId, JobId, AttemptRef, EnvironmentId) {
    let machine = f.machine("builder");
    let mut with_environment = spec();
    with_environment.environment = Some(LeaseSpec {
        idle_timeout_secs: 900,
        lease_secs: 1800,
        max_lifetime_secs: 14_400,
    });
    let job = f.submit_spec("app", with_environment);
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing, ready()]);
    let environment = f.ledger.job(&job).unwrap().environment_id.unwrap();
    (machine, job, attempt, environment)
}

#[test]
fn an_environment_is_created_when_services_are_ready() {
    let f = fixture();
    let (machine, job, attempt, id) = serving(&f);
    let view = f.ledger.environment(&id).unwrap();
    assert_eq!(view.state, EnvironmentState::Ready);
    assert_eq!(view.job_id, job);
    assert_eq!(view.attempt_id, attempt.attempt_id);
    assert_eq!(view.machine_id, machine);
    assert_eq!(view.machine_name, "builder");
    assert_eq!(view.endpoints, endpoints());
    assert_eq!(view.reserved, vector(1000, 1));
    assert_eq!(view.lease.created_at, f.now());
    assert_eq!(view.effective_deadline, f.now().plus_secs(900));
    assert!(view.ended_reason.is_none());
    assert_eq!(f.ledger.job_environment(&job).unwrap(), Some(view.clone()));
    assert_eq!(f.ledger.environments(false).unwrap(), [view]);
    assert_eq!(
        f.ledger
            .fleet_overview("0.1.0", false)
            .unwrap()
            .environments,
        1
    );
    assert!(
        f.job_event_kinds(&job)
            .contains(&"environment.ready".to_string())
    );
}

#[test]
fn a_job_that_asked_for_no_environment_gets_none() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing, ready()]);
    assert!(f.ledger.job(&job).unwrap().environment_id.is_none());
    assert!(f.ledger.job_environment(&job).unwrap().is_none());
    assert!(f.ledger.environments(true).unwrap().is_empty());
    assert!(matches!(
        f.ledger.environment(&EnvironmentId::generate()),
        Err(LedgerError::NotFound(_))
    ));
}

#[test]
fn use_resets_the_idle_timer_but_only_renewal_extends_the_lease() {
    let f = fixture();
    let (_, _, _, id) = serving(&f);
    let created = f.now();

    // Touching every ten minutes keeps it from idling out...
    for _ in 0..2 {
        f.advance_secs(600);
        let view = f.ledger.touch_environment(&id).unwrap();
        assert_eq!(view.lease.last_activity_at, f.now());
    }
    // ...but the lease itself still ends 30 minutes after it started.
    let view = f.ledger.environment(&id).unwrap();
    assert_eq!(view.effective_deadline, created.plus_secs(1800));
    assert!(f.ledger.expired_environments(f.now()).unwrap().is_empty());

    // Renewal starts a new lease from now.
    let renewed = f.ledger.renew_environment(&id).unwrap();
    assert_eq!(renewed.lease.lease_until, f.now().plus_secs(1800));
    assert_eq!(renewed.effective_deadline, f.now().plus_secs(900));
    assert_eq!(f.ledger.environment(&id).unwrap(), renewed);
}

#[test]
fn an_idle_environment_expires_and_cannot_be_revived() {
    let f = fixture();
    let (_, _, attempt, id) = serving(&f);
    f.advance_secs(899);
    assert!(f.ledger.expired_environments(f.now()).unwrap().is_empty());
    f.advance_secs(1);
    let expired = f.ledger.expired_environments(f.now()).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].environment.id, id);
    assert_eq!(expired[0].reason, LeaseEnd::IdleTimeout);

    // Listing it changes nothing, and an ended lease is never renewed.
    assert_eq!(
        f.ledger.environment(&id).unwrap().state,
        EnvironmentState::Ready
    );
    assert!(matches!(
        f.ledger.renew_environment(&id),
        Err(LedgerError::InvalidState(_))
    ));
    let touched = f.ledger.touch_environment(&id).unwrap();
    assert_ne!(touched.lease.last_activity_at, f.now());

    let finish = f
        .ledger
        .request_environment_finish(&id, "idle timeout")
        .unwrap();
    assert_eq!(finish.environment.state, EnvironmentState::Finishing);
    assert_eq!(
        finish.environment.ended_reason.as_deref(),
        Some("idle timeout")
    );
    assert_eq!(finish.target.unwrap().attempt, attempt);
    assert!(f.ledger.expired_environments(f.now()).unwrap().is_empty());
}

#[test]
fn renewal_cannot_pass_the_maximum_lifetime() {
    let f = fixture();
    let (_, _, _, id) = serving(&f);
    let created = f.now();
    let mut renewals = 0;
    loop {
        f.advance_secs(600);
        match f.ledger.renew_environment(&id) {
            Ok(view) => {
                renewals += 1;
                assert!(view.effective_deadline <= created.plus_secs(14_400));
            }
            Err(error) => {
                assert!(matches!(error, LedgerError::InvalidState(_)));
                break;
            }
        }
    }
    assert_eq!(
        renewals, 23,
        "ten-minute renewals for just under four hours"
    );
    let expired = f.ledger.expired_environments(f.now()).unwrap();
    assert_eq!(expired[0].reason, LeaseEnd::MaxLifetime);
}

#[test]
fn finishing_keeps_the_resources_until_the_attempt_is_released() {
    let f = fixture();
    let (machine, job, attempt, id) = serving(&f);

    let first = f
        .ledger
        .request_environment_finish(&id, "done testing")
        .unwrap();
    assert_eq!(first.environment.state, EnvironmentState::Finishing);
    let events = f.ledger.last_event_seq().unwrap();
    let second = f.ledger.request_environment_finish(&id, "again").unwrap();
    assert_eq!(second.environment, first.environment);
    assert_eq!(
        f.ledger.last_event_seq().unwrap(),
        events,
        "asking twice is a no-op"
    );
    assert!(matches!(
        f.ledger.renew_environment(&id),
        Err(LedgerError::InvalidState(_))
    ));
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert_eq!(f.ledger.environment(&id).unwrap().reserved, vector(1000, 1));

    f.events(&attempt, 3, vec![exited(Outcome::Succeeded), released()]);
    let ended = f.ledger.environment(&id).unwrap();
    assert_eq!(ended.state, EnvironmentState::Ended);
    assert_eq!(ended.ended_reason.as_deref(), Some("done testing"));
    assert_eq!(ended.reserved, ResourceVector::ZERO);
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Finished);
    assert!(f.ledger.environments(false).unwrap().is_empty());
    assert_eq!(f.ledger.environments(true).unwrap().len(), 1);
    assert_eq!(
        f.ledger
            .fleet_overview("0.1.0", false)
            .unwrap()
            .environments,
        0
    );
    let finish = f.ledger.request_environment_finish(&id, "late").unwrap();
    assert!(finish.target.is_none());
    let kinds = f.job_event_kinds(&job);
    let position = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(position("environment.finishing") < position("environment.ended"));
    assert!(position("environment.ended") < position("job.finished"));
}

#[test]
fn an_exit_or_a_quarantine_carries_over_to_the_environment() {
    let f = fixture();
    let (_, _, attempt, id) = serving(&f);
    f.events(&attempt, 3, vec![exited(Outcome::Succeeded)]);
    assert_eq!(
        f.ledger.environment(&id).unwrap().state,
        EnvironmentState::Finishing
    );
    f.events(
        &attempt,
        4,
        vec![AttemptEvent::Quarantined {
            reason: "the server would not stop".into(),
        }],
    );
    let view = f.ledger.environment(&id).unwrap();
    assert_eq!(view.state, EnvironmentState::Quarantined);
    assert_eq!(
        view.reserved,
        vector(1000, 1),
        "still counted against the machine"
    );

    let ended = f
        .ledger
        .mark_environment_ended(&id, "operator cleaned up")
        .unwrap();
    assert_eq!(ended.state, EnvironmentState::Ended);
    assert_eq!(
        f.ledger.mark_environment_ended(&id, "again").unwrap(),
        ended
    );
}
