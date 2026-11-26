//! One test per acceptance scenario assigned to this crate, each against the
//! simulated provider with the relevant fault switched on.

mod common;

use bella_cloud::{
    Allowance, CloudError, Denial, DenyReason, Engine, EngineConfig, FailureKind, IntentState,
    NewCapacity, Provider, ProvisionOutcome, ProvisionStep, PurchaseModel, ReconcileOutcome,
    Refusal, Registration, RequestMode, ResourceKind, RetryBlocker, Scope, ScopeKind, SimProvider,
};
use bella_core::job::{CloudMode, Outcome, Priority};
use bella_core::{CloudIntentId, FleetId, JobId, ManualClock};
use common::{HOUR, MINUTE, SMALL_HOLD, T0, World, allowance, denial, refusal};
use rusqlite::Connection;
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn kinds(world: &World) -> Vec<ResourceKind> {
    world.sim.snapshot().iter().map(|r| r.kind).collect()
}

fn instances(world: &World) -> usize {
    kinds(world)
        .iter()
        .filter(|kind| **kind == ResourceKind::Instance)
        .count()
}

/// A48: capacity is refused while the provider-side janitor is missing or
/// stale, the refusal says how to repair it, and nothing is created.
#[test]
fn a48_no_capacity_without_a_healthy_janitor() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();

    world.sim.set_janitor(false, None);
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert_eq!(
        refusal(&error),
        &Refusal::JanitorNotInstalled {
            provider: "sim".to_string()
        }
    );
    assert!(error.to_string().contains("To repair: install the janitor"));

    world.sim.set_janitor(true, None);
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert!(matches!(
        refusal(&error),
        Refusal::JanitorNeverSucceeded { .. }
    ));
    assert!(error.to_string().contains("To repair"));

    world.sim.set_janitor(true, Some(world.now()));
    world.clock.advance(31 * MINUTE);
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert_eq!(
        refusal(&error),
        &Refusal::JanitorStale {
            provider: "sim".to_string(),
            age_secs: 31 * 60,
            max_age_secs: 30 * 60,
        }
    );
    assert!(error.to_string().contains("To repair"));

    world.sim.faults(|f| f.unavailable = true);
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert!(matches!(
        refusal(&error),
        Refusal::JanitorStatusUnavailable { .. }
    ));
    world.sim.faults(|f| f.unavailable = false);

    // Nothing was written and the provider was never asked to create.
    assert_eq!(world.count("cloud_intents"), 0);
    assert_eq!(world.count("cloud_money"), 0);
    assert!(world.sim.create_calls().is_empty());

    // Once the janitor has run, the same request is accepted.
    world.sim.tick();
    let id = world.authorized(&engine, &world.request());
    assert_eq!(world.state(&id), IntentState::Authorized);

    // The gate is checked again before the create call: a janitor that went
    // stale after authorization still blocks provisioning.
    world.clock.advance(31 * MINUTE);
    let error = engine.provision(&id, "boot").unwrap_err();
    assert!(matches!(refusal(&error), Refusal::JanitorStale { .. }));
    assert_eq!(world.state(&id), IntentState::Authorized);
    assert!(world.sim.create_calls().is_empty());

    world.sim.tick();
    assert_eq!(
        engine.provision(&id, "boot").unwrap(),
        ProvisionOutcome::Created
    );
}

/// A49: a create whose response is lost leaves exactly one instance and one
/// budget commitment, even when the provider ignores idempotency tokens.
#[test]
fn a49_lost_create_response_yields_one_instance_and_one_hold() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| {
        f.lose_create_response = 1;
        f.ignore_idempotency_token = true;
    });
    let id = world.authorized(&engine, &world.request());

    let outcome = engine.provision(&id, "boot-1").unwrap();
    assert!(matches!(outcome, ProvisionOutcome::Unknown { .. }));
    assert_eq!(world.state(&id), IntentState::Unknown);
    assert_eq!(world.budget(ScopeKind::Project).held_micros, SMALL_HOLD);

    // Asking again while the outcome is unknown issues no second create.
    for _ in 0..3 {
        assert_eq!(
            engine.provision(&id, "boot-1").unwrap(),
            ProvisionOutcome::NoAction {
                state: IntentState::Unknown
            }
        );
    }
    assert_eq!(world.sim.create_calls().len(), 1);

    // The listing shows the instance the lost response was about.
    assert!(matches!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::Adopted { .. }
    ));
    assert_eq!(world.state(&id), IntentState::Bootstrapping);
    assert_eq!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::NotUncertain {
            state: IntentState::Bootstrapping
        }
    );
    assert_eq!(instances(&world), 1);
    assert_eq!(world.sim.create_calls().len(), 1);
    assert_eq!(world.count("cloud_money"), 1);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, SMALL_HOLD);
    assert_eq!(world.budget(ScopeKind::Fleet).active, 1);
    assert_eq!(
        engine.on_worker_registered("boot-1").unwrap(),
        Registration::Admit { intent: id.clone() }
    );
}

/// A49: a listing that lags behind creation is not taken as proof that
/// nothing was created.
#[test]
fn a49_a_lagging_listing_does_not_cause_a_second_create() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| {
        f.lose_create_response = 1;
        f.ignore_idempotency_token = true;
        f.listing_lag_secs = 60;
    });
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();

    // Inside the settling period an empty listing concludes nothing.
    assert!(matches!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::StillUnknown { .. }
    ));
    assert_eq!(world.state(&id), IntentState::Unknown);
    assert_eq!(
        engine.provision(&id, "boot").unwrap(),
        ProvisionOutcome::NoAction {
            state: IntentState::Unknown
        }
    );

    world.clock.advance(Duration::from_secs(61));
    assert!(matches!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::Adopted { .. }
    ));
    assert_eq!(world.sim.create_calls().len(), 1);
    assert_eq!(instances(&world), 1);
    assert_eq!(world.count("cloud_money"), 1);
}

/// A49: when the provider positively reports nothing after the settling
/// period, the retry reuses the same token and the same hold.
#[test]
fn a49_a_create_that_never_happened_is_retried_with_the_same_token() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world
        .sim
        .faults(|f| f.fail_create.push_back(bella_cloud::ProviderError::Timeout));
    let id = world.authorized(&engine, &world.request());
    let hold = engine.money(&id).unwrap().hold.unwrap();

    assert!(matches!(
        engine.provision(&id, "boot-1").unwrap(),
        ProvisionOutcome::Unknown { .. }
    ));
    assert!(matches!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::StillUnknown { .. }
    ));
    world.clock.advance(Duration::from_secs(120));
    assert_eq!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::RetryPermitted {
            attempts_used: 1,
            attempts_allowed: 2
        }
    );
    assert_eq!(world.state(&id), IntentState::Authorized);
    assert_eq!(engine.money(&id).unwrap().hold.unwrap(), hold);

    assert_eq!(
        engine.provision(&id, "boot-2").unwrap(),
        ProvisionOutcome::Created
    );
    let calls = world.sim.create_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.intent == id));
    assert_eq!(instances(&world), 1);
    assert_eq!(world.count("cloud_money"), 1);
    assert_eq!(world.budget(ScopeKind::Project).held_micros, SMALL_HOLD);

    // The replaced bootstrap string no longer admits anyone.
    assert!(matches!(
        engine.on_worker_registered("boot-1").unwrap(),
        Registration::Deny {
            reason: DenyReason::UnknownToken,
            ..
        }
    ));
}

/// A50, modelled: the coordinator dies after the provider created the
/// instance. The provider's own expiry removes it, and a new engine
/// reconciles the intention and settles the hold.
#[test]
fn a50_provider_expiry_and_a_new_engine_clean_up_after_a_dead_coordinator() {
    let world = World::new();
    world.allow(10_000_000);
    let id = {
        let engine = world.engine();
        let id = world.authorized(&engine, &world.request());
        let ProvisionStep::Call(request) = engine.begin_provision(&id, "boot").unwrap() else {
            panic!("an authorized intention must get a create call");
        };
        // The provider acts on the call. The coordinator dies before it can
        // record the answer.
        world.sim.create(&request).unwrap();
        id
    };
    assert_eq!(world.state(&id), IntentState::Provisioning);
    assert_eq!(world.sim.snapshot().len(), 3);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, SMALL_HOLD);

    // Nobody is watching. The expiry passes and the janitor runs on its own.
    world.clock.advance(2 * HOUR);
    assert_eq!(world.sim.tick().len(), 3);
    assert!(world.sim.snapshot().is_empty());

    let engine = world.engine();
    let report = engine.sweep().unwrap();
    assert_eq!(report.changes.len(), 1);
    assert_eq!(report.changes[0].to, IntentState::Terminated);
    let intent = world.intent(&id);
    assert_eq!(intent.state, IntentState::Terminated);
    assert!(intent.settled_at.is_some());

    // The hold is released. What ran unobserved is charged conservatively:
    // the quoted rate from the create call to the expiry (70 minutes).
    let money = engine.money(&id).unwrap();
    assert!(money.hold.unwrap().released_at.is_some());
    assert_eq!(money.accrued.micros, 243_834);
    assert!(
        money
            .accrued
            .assumptions
            .iter()
            .any(|note| note.contains("after the expiry"))
    );
    assert_eq!(money.actual, None);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);
    assert!(report.exposure.intents.is_empty());
    assert_eq!(report.exposure.held_micros, 0);

    // Nothing is left to do.
    let again = engine.sweep().unwrap();
    assert!(again.changes.is_empty() && again.problems.is_empty());
    assert_eq!(world.sim.create_calls().len(), 1);
}

/// A50, modelled: the same when the coordinator died after recording the
/// instance. The tracked resources are charged up to the expiry.
#[test]
fn a50_a_recorded_instance_removed_by_the_provider_is_settled_too() {
    let world = World::new();
    world.allow(10_000_000);
    let id = {
        let engine = world.engine();
        let id = world.authorized(&engine, &world.request());
        assert_eq!(
            engine.provision(&id, "boot").unwrap(),
            ProvisionOutcome::Created
        );
        id
    };
    world.clock.advance(2 * HOUR);
    world.sim.tick();

    let engine = world.engine();
    engine.sweep().unwrap();
    let intent = world.intent(&id);
    assert_eq!(intent.state, IntentState::Terminated);
    assert!(intent.settled_at.is_some());
    // Instance, disk and address for the 70 minutes up to the expiry.
    assert_eq!(
        engine.money(&id).unwrap().accrued.micros,
        233_334 + 4_667 + 5_834
    );
    assert_eq!(world.budget(ScopeKind::Project).held_micros, 0);
}

/// A51: a worker whose intention was cancelled, has expired or is unknown
/// is denied, and its resources are scheduled for deletion.
#[test]
fn a51_a_worker_of_a_cancelled_expired_or_unknown_intention_is_denied() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();

    // Unknown: no intention issued this string.
    assert_eq!(
        engine.on_worker_registered("never-issued").unwrap(),
        Registration::Deny {
            reason: DenyReason::UnknownToken,
            intent: None,
            deletion_scheduled: false,
        }
    );

    // Cancelled while the provider cannot be reached, so the instance is
    // still running when its worker calls in.
    let cancelled = world.authorized(&engine, &world.request());
    engine.provision(&cancelled, "boot-cancelled").unwrap();
    world.sim.faults(|f| f.unavailable = true);
    let termination = engine.cancel(&cancelled, "no longer needed").unwrap();
    assert!(!termination.complete());
    world.sim.faults(|f| f.unavailable = false);
    assert_eq!(instances(&world), 1);
    assert_eq!(
        engine.on_worker_registered("boot-cancelled").unwrap(),
        Registration::Deny {
            reason: DenyReason::Cancelled,
            intent: Some(cancelled.clone()),
            deletion_scheduled: true,
        }
    );
    assert_eq!(world.state(&cancelled), IntentState::Terminating);
    engine.sweep().unwrap();
    assert_eq!(world.state(&cancelled), IntentState::Terminated);
    assert!(world.sim.snapshot().is_empty());

    // Expired before the worker registered. The denial itself moves the
    // intention to termination.
    let expired = world.authorized(&engine, &world.request());
    engine.provision(&expired, "boot-expired").unwrap();
    world.clock.advance(71 * MINUTE);
    assert_eq!(
        engine.on_worker_registered("boot-expired").unwrap(),
        Registration::Deny {
            reason: DenyReason::Expired,
            intent: Some(expired.clone()),
            deletion_scheduled: true,
        }
    );
    assert_eq!(world.state(&expired), IntentState::Terminating);
    assert_eq!(instances(&world), 1);
    engine.sweep().unwrap();
    assert_eq!(world.state(&expired), IntentState::Terminated);
    assert!(world.sim.snapshot().is_empty());
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);

    // A string is good for one worker only.
    world.sim.tick();
    let live = world.authorized(&engine, &world.request());
    engine.provision(&live, "boot-live").unwrap();
    assert_eq!(
        engine.on_worker_registered("boot-live").unwrap(),
        Registration::Admit {
            intent: live.clone()
        }
    );
    assert_eq!(world.state(&live), IntentState::Ready);
    assert_eq!(
        engine.on_worker_registered("boot-live").unwrap(),
        Registration::Deny {
            reason: DenyReason::TokenAlreadyUsed,
            intent: Some(live.clone()),
            deletion_scheduled: false,
        }
    );
    assert_eq!(world.state(&live), IntentState::Ready);
}

/// A51: a worker whose create outcome has not been reconciled is not
/// admitted on the strength of its string alone.
#[test]
fn a51_a_worker_is_not_admitted_before_its_instance_is_reconciled() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| f.lose_create_response = 1);
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();
    assert_eq!(
        engine.on_worker_registered("boot").unwrap(),
        Registration::Deny {
            reason: DenyReason::ProviderStateUncertain,
            intent: Some(id.clone()),
            deletion_scheduled: false,
        }
    );
    engine.reconcile(&id).unwrap();
    assert_eq!(
        engine.on_worker_registered("boot").unwrap(),
        Registration::Admit { intent: id }
    );
}

/// A52: two authorizations racing for the last of an allowance. Exactly one
/// takes the hold; the others are denied. Each thread has its own
/// connection to one database file.
#[test]
fn a52_racing_authorizations_take_the_last_of_an_allowance_once() {
    const THREADS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("controller.sqlite");
    let open = || {
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::from_secs(60)).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
            .unwrap();
        conn
    };

    let world = World::with_connection(open());
    // Room for exactly one hold at both scopes.
    world.allow(SMALL_HOLD);
    let engine = world.engine();
    let ids: Vec<CloudIntentId> = (0..THREADS)
        .map(|_| engine.request_capacity(&world.request()).unwrap().id)
        .collect();
    let quote = engine.quote(&ids[0]).unwrap();

    let barrier = Arc::new(Barrier::new(THREADS));
    let sim: &SimProvider = &world.sim;
    let clock: &ManualClock = &world.clock;
    let fleet: &FleetId = &world.fleet;
    let results: Vec<Result<_, CloudError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .iter()
            .map(|id| {
                let barrier = Arc::clone(&barrier);
                let quote = quote.clone();
                let open = &open;
                scope.spawn(move || {
                    let conn = open();
                    let engine =
                        Engine::new(&conn, sim, clock, fleet.clone(), EngineConfig::default());
                    barrier.wait();
                    engine.authorize(id, &quote)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let winners = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(winners, 1, "exactly one authorization may succeed");
    for result in &results {
        if let Err(error) = result {
            assert!(
                matches!(denial(error), Denial::BudgetExceeded { .. }),
                "{error:?}"
            );
        }
    }
    assert_eq!(world.count("cloud_money"), 1);
    let authorized = ids
        .iter()
        .filter(|id| world.state(id) == IntentState::Authorized)
        .count();
    assert_eq!(authorized, 1);
    for scope in [ScopeKind::Fleet, ScopeKind::Project] {
        let budget = world.budget(scope);
        assert_eq!(budget.held_micros, SMALL_HOLD);
        assert_eq!(budget.available_micros, 0);
        assert_eq!(budget.active, 1);
    }
}

fn spot_request(world: &World, job: &JobId) -> NewCapacity {
    NewCapacity {
        purchase: PurchaseModel::Spot,
        job: Some(job.clone()),
        ..world.request()
    }
}

/// Brings a spot intention to `Busy`.
fn busy_spot_worker(world: &World, engine: &Engine<'_>, request: &NewCapacity) -> CloudIntentId {
    let id = world.authorized(engine, request);
    let bootstrap = format!("boot-{id}");
    assert_eq!(
        engine.provision(&id, &bootstrap).unwrap(),
        ProvisionOutcome::Created
    );
    assert!(matches!(
        engine.on_worker_registered(&bootstrap).unwrap(),
        Registration::Admit { .. }
    ));
    engine.mark_busy(&id).unwrap();
    id
}

/// A53: a spot instance that vanishes fails its intention with an
/// infrastructure-interruption reason, the partial result is not promoted,
/// and a retry is permitted only while attempts, time and budget allow.
#[test]
fn a53_a_vanished_spot_instance_is_an_infrastructure_interruption() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let job = JobId::generate();
    let first = busy_spot_worker(&world, &engine, &spot_request(&world, &job));
    let original = world.intent(&first);

    world.clock.advance(5 * MINUTE);
    assert!(engine.sweep().unwrap().interruptions.is_empty());
    assert_eq!(world.sim.interrupt_spot(&first), 1);

    let report = engine.sweep().unwrap();
    assert_eq!(report.interruptions.len(), 1);
    let interruption = &report.interruptions[0];
    assert_eq!(interruption.intent, first);
    assert_eq!(interruption.job, Some(job.clone()));
    match &interruption.outcome {
        Outcome::InfraInterrupted { detail } => {
            assert!(detail.contains("infrastructure interruption"), "{detail}");
            assert!(detail.contains("spot"), "{detail}");
        }
        other => panic!("expected an infrastructure interruption, got {other:?}"),
    }
    assert!(!interruption.promote_partial_result);
    assert!(interruption.retry.permitted, "{:?}", interruption.retry);
    assert_eq!(interruption.retry.attempts_used, 1);
    assert_eq!(interruption.retry.attempts_allowed, 2);

    let failed = world.intent(&first);
    assert_eq!(failed.state, IntentState::Failed);
    assert_eq!(
        failed.failure,
        Some(FailureKind::InfrastructureInterruption)
    );
    assert!(failed.settled_at.is_some(), "nothing of it remains");
    assert_eq!(world.budget(ScopeKind::Project).held_micros, 0);

    // The retry is a new intention: same purchase model, never past the
    // original expiry, with its own authorization.
    let retry = NewCapacity {
        id: CloudIntentId::generate(),
        retry_of: Some(first.clone()),
        lifetime_secs: interruption.retry.remaining_lifetime_secs,
        ..spot_request(&world, &job)
    };
    let second = busy_spot_worker(&world, &engine, &retry);
    let replacement = world.intent(&second);
    assert_eq!(replacement.chain, first);
    assert_eq!(replacement.purchase, PurchaseModel::Spot);
    assert_eq!(replacement.expires_at, original.expires_at);
    assert_eq!(
        engine
            .money(&second)
            .unwrap()
            .hold
            .unwrap()
            .attempts_allowed,
        1
    );

    // The replacement vanishes too. The attempts are used up.
    world.clock.advance(3 * MINUTE);
    world.sim.interrupt_spot(&second);
    let report = engine.sweep().unwrap();
    assert_eq!(report.interruptions.len(), 1);
    let verdict = &report.interruptions[0].retry;
    assert!(!verdict.permitted);
    assert_eq!(
        verdict.blockers,
        vec![RetryBlocker::AttemptsExhausted {
            used: 2,
            allowed: 2
        }]
    );
    let third = NewCapacity {
        id: CloudIntentId::generate(),
        retry_of: Some(second),
        lifetime_secs: 600,
        ..spot_request(&world, &job)
    };
    let error = engine.request_capacity(&third).unwrap_err();
    assert!(matches!(refusal(&error), Refusal::RetryNotPermitted { .. }));
    assert!(
        world
            .sim
            .create_calls()
            .iter()
            .all(|call| call.purchase == PurchaseModel::Spot)
    );
}

/// A53: the retry verdict refuses on time and on budget as well.
#[test]
fn a53_a_retry_needs_time_and_budget() {
    // Too little of the lifetime is left.
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let job = JobId::generate();
    let id = busy_spot_worker(&world, &engine, &spot_request(&world, &job));
    world.clock.advance(65 * MINUTE);
    world.sim.interrupt_spot(&id);
    let report = engine.sweep().unwrap();
    assert_eq!(
        report.interruptions[0].retry.blockers,
        vec![RetryBlocker::InsufficientTime {
            remaining_secs: 300,
            required_secs: 900
        }]
    );
    assert!(!report.interruptions[0].retry.permitted);

    // The allowance no longer covers another attempt.
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let id = busy_spot_worker(&world, &engine, &spot_request(&world, &job));
    world.clock.advance(5 * MINUTE);
    world.sim.interrupt_spot(&id);
    engine
        .set_allowance(
            &world.admin,
            &allowance(Scope::Project(world.project.clone()), 1),
        )
        .unwrap();
    let report = engine.sweep().unwrap();
    let verdict = &report.interruptions[0].retry;
    assert!(!verdict.permitted);
    assert!(
        matches!(
            verdict.blockers.as_slice(),
            [RetryBlocker::Budget {
                denial: Denial::BudgetExceeded {
                    scope: ScopeKind::Project,
                    ..
                }
            }]
        ),
        "{verdict:?}"
    );
}

/// A54: spot capacity that is unavailable never silently becomes on-demand.
/// With a spot-only allowance the intention stays blocked.
#[test]
fn a54_unavailable_spot_stays_blocked_when_only_spot_is_allowed() {
    let world = World::new();
    world.allow_with(10_000_000, |allowance: &mut Allowance| {
        allowance.allowed_purchase = vec![PurchaseModel::Spot];
        allowance.max_attempts = 3;
    });
    let engine = world.engine();
    world.sim.faults(|f| f.spot_capacity_unavailable = true);
    let request = NewCapacity {
        purchase: PurchaseModel::Spot,
        ..world.request()
    };
    let id = world.authorized(&engine, &request);
    let hold = engine.money(&id).unwrap().hold.unwrap();

    for _ in 0..2 {
        let outcome = engine.provision(&id, "boot").unwrap();
        assert!(
            matches!(outcome, ProvisionOutcome::Blocked { .. }),
            "{outcome:?}"
        );
        assert_eq!(world.state(&id), IntentState::Authorized);
    }
    assert!(world.sim.snapshot().is_empty());
    assert_eq!(engine.money(&id).unwrap().hold.unwrap(), hold);
    assert_eq!(
        world.budget(ScopeKind::Project).held_micros,
        hold.amount_micros
    );

    // When spot capacity returns, the same intention gets a spot instance.
    world.sim.faults(|f| f.spot_capacity_unavailable = false);
    assert_eq!(
        engine.provision(&id, "boot").unwrap(),
        ProvisionOutcome::Created
    );
    let calls = world.sim.create_calls();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .all(|call| call.purchase == PurchaseModel::Spot)
    );
    assert_eq!(instances(&world), 1);

    // An on-demand request is not even authorized under this allowance.
    let on_demand = engine.request_capacity(&world.request()).unwrap();
    let quote = engine.quote(&on_demand.id).unwrap();
    let error = engine.authorize(&on_demand.id, &quote).unwrap_err();
    assert!(matches!(
        denial(&error),
        Denial::PurchaseNotAllowed {
            purchase: PurchaseModel::OnDemand,
            ..
        }
    ));
}

/// A54: when the allowance also permits on-demand, the spot intention
/// fails and says so; on-demand needs its own explicit request.
#[test]
fn a54_unavailable_spot_fails_instead_of_becoming_on_demand() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| f.spot_capacity_unavailable = true);
    let request = NewCapacity {
        purchase: PurchaseModel::Spot,
        ..world.request()
    };
    let id = world.authorized(&engine, &request);

    let outcome = engine.provision(&id, "boot").unwrap();
    match outcome {
        ProvisionOutcome::Failed {
            kind,
            on_demand_permitted,
            ..
        } => {
            assert_eq!(kind, FailureKind::SpotCapacityUnavailable);
            assert!(on_demand_permitted);
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    let failed = world.intent(&id);
    assert_eq!(failed.state, IntentState::Failed);
    assert_eq!(failed.failure, Some(FailureKind::SpotCapacityUnavailable));
    assert_eq!(failed.purchase, PurchaseModel::Spot);
    assert!(failed.settled_at.is_some());
    assert_eq!(world.budget(ScopeKind::Project).held_micros, 0);

    // The provider saw one spot request and created nothing.
    let calls = world.sim.create_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].purchase, PurchaseModel::Spot);
    assert!(world.sim.snapshot().is_empty());

    // A retry cannot change the purchase model either.
    let converted = NewCapacity {
        id: CloudIntentId::generate(),
        retry_of: Some(id.clone()),
        ..world.request()
    };
    let error = engine.request_capacity(&converted).unwrap_err();
    assert!(matches!(refusal(&error), Refusal::RetryNotPermitted { .. }));

    // On-demand capacity exists only through a separate request with its
    // own quote and hold.
    let explicit = world.authorized(&engine, &world.request());
    assert_ne!(explicit, id);
    assert_eq!(
        engine.provision(&explicit, "boot-on-demand").unwrap(),
        ProvisionOutcome::Created
    );
    let calls = world.sim.create_calls();
    assert_eq!(calls[1].intent, explicit);
    assert_eq!(calls[1].purchase, PurchaseModel::OnDemand);
}

/// A55: LAN-only source is never given cloud capacity. No priority,
/// escalation flag, request mode, cloud mode or role changes that.
#[test]
fn a55_lan_only_source_is_refused_whatever_the_priority() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let priorities = [
        Priority::Maintenance,
        Priority::Background,
        Priority::Normal,
        Priority::Interactive,
        Priority::Urgent,
    ];
    let mut refused = 0;
    for priority in priorities {
        for escalated in [false, true] {
            for mode in [RequestMode::Manual, RequestMode::Automatic] {
                for cloud in [
                    CloudMode::Hybrid,
                    CloudMode::AutoOverflow,
                    CloudMode::Manual,
                ] {
                    let request = NewCapacity {
                        source_lan_only: true,
                        priority,
                        escalated,
                        mode,
                        project_cloud: cloud,
                        job_cloud: Some(cloud),
                        requested_by: world.admin.clone(),
                        ..world.request()
                    };
                    let error = engine.request_capacity(&request).unwrap_err();
                    assert_eq!(refusal(&error), &Refusal::SourceLanOnly);
                    assert!(error.to_string().contains("priority and escalation"));
                    refused += 1;
                }
            }
        }
    }
    assert_eq!(refused, 60);
    assert_eq!(world.count("cloud_intents"), 0);
    assert_eq!(
        world.count("cloud_events"),
        2,
        "only the two allowance events"
    );
    assert!(world.sim.create_calls().is_empty());

    // The same request without the flag is accepted, so the flag alone decided.
    let request = NewCapacity {
        priority: Priority::Urgent,
        escalated: true,
        ..world.request()
    };
    assert!(engine.request_capacity(&request).is_ok());
}

/// A56: a deletion that leaves a disk and an address behind does not close
/// the intention. They stay visible with their cost until they are deleted,
/// and the hold stays with them.
#[test]
fn a56_leftover_disk_and_address_stay_visible_until_deleted() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();
    engine.on_worker_registered("boot").unwrap();

    world.sim.faults(|f| {
        f.leak_secondary_on_delete = true;
        f.undeletable = [ResourceKind::Disk, ResourceKind::Address].into();
    });
    world.clock.advance(10 * MINUTE);
    let termination = engine.terminate(&id, "job finished").unwrap();
    assert!(!termination.complete());
    assert_eq!(world.state(&id), IntentState::Terminating);
    let left: Vec<ResourceKind> = termination.remaining.iter().map(|r| r.kind).collect();
    assert_eq!(left, vec![ResourceKind::Address, ResourceKind::Disk]);
    assert_eq!(termination.notes.len(), 2);
    assert_eq!(instances(&world), 0);

    // The hold is still outstanding and the leftovers are priced.
    assert_eq!(world.budget(ScopeKind::Project).held_micros, SMALL_HOLD);
    let exposure = engine.exposure().unwrap();
    assert_eq!(exposure.intents.len(), 1);
    let item = &exposure.intents[0];
    assert_eq!(item.intent, id);
    assert_eq!(item.state, IntentState::Terminating);
    assert_eq!(item.hold_micros, SMALL_HOLD);
    assert_eq!(item.resources.len(), 2);
    assert_eq!(
        item.hourly_micros,
        SimProvider::DISK_HOURLY_MICROS + SimProvider::ADDRESS_HOURLY_MICROS
    );
    assert_eq!(exposure.hourly_micros, 9_000);
    let accrued_before = item.accrued.micros;
    // Ten minutes of instance, disk and address.
    assert_eq!(accrued_before, 33_334 + 667 + 834);

    // An hour later nothing has changed except the cost.
    world.clock.advance(HOUR);
    let report = engine.sweep().unwrap();
    assert!(report.changes.is_empty());
    assert!(!report.problems.is_empty());
    assert_eq!(world.state(&id), IntentState::Terminating);
    assert_eq!(
        report.exposure.intents[0].accrued.micros,
        accrued_before + 9_000
    );
    assert_eq!(world.budget(ScopeKind::Project).held_micros, SMALL_HOLD);

    // Once the provider lets go, the intention closes and the hold is released.
    world.sim.faults(|f| f.undeletable.clear());
    let report = engine.sweep().unwrap();
    assert_eq!(world.state(&id), IntentState::Terminated);
    assert!(world.sim.snapshot().is_empty());
    assert!(report.exposure.intents.is_empty());
    assert_eq!(report.exposure.hourly_micros, 0);
    assert_eq!(world.budget(ScopeKind::Project).held_micros, 0);
    assert_eq!(
        engine.money(&id).unwrap().accrued.micros,
        accrued_before + 9_000
    );
}

/// A56: a delete whose response is lost is resolved by listing, not by
/// trusting or repeating the call blindly.
#[test]
fn a56_a_lost_delete_response_is_resolved_by_the_listing() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();
    world.clock.advance(5 * MINUTE);
    engine.sweep().unwrap();

    world.sim.faults(|f| f.lose_delete_response = 1);
    let termination = engine.terminate(&id, "done").unwrap();
    assert!(termination.complete(), "{termination:?}");
    assert_eq!(world.state(&id), IntentState::Terminated);
    assert!(termination.notes.iter().any(|note| note.contains("lost")));
    assert!(world.sim.snapshot().is_empty());

    // Terminating again is harmless.
    let again = engine.terminate(&id, "done").unwrap();
    assert!(again.complete());
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);
}

/// A58: a dedicated host with a 24-hour minimum keeps its hold and stays
/// tracked after the job ends, until the provider allows its release.
#[test]
fn a58_a_minimum_commitment_keeps_the_hold_and_the_host_until_release() {
    let world = World::new();
    world.allow_with(40_000_000, |allowance: &mut Allowance| {
        allowance.max_attempts = 1;
    });
    let engine = world.engine();
    let request = NewCapacity {
        preset: "mac-dedicated".to_string(),
        ..world.request()
    };
    let intent = engine.request_capacity(&request).unwrap();
    let quote = engine.quote(&intent.id).unwrap();
    assert_eq!(quote.minimum_commitment_micros, 24 * 1_200_000);
    let hold = engine.authorize(&intent.id, &quote).unwrap();
    // Startup, one hour, and the whole 24-hour minimum.
    assert_eq!(hold.breakdown.minimum_micros, 28_800_000);
    assert_eq!(hold.amount_micros, 201_500 + 1_209_000 + 28_800_000);
    let id = intent.id;

    engine.provision(&id, "boot").unwrap();
    engine.on_worker_registered("boot").unwrap();
    engine.mark_busy(&id).unwrap();
    world.clock.advance(HOUR);
    engine.mark_idle(&id).unwrap();

    // The job is over. Everything but the host goes.
    let termination = engine.terminate(&id, "job finished").unwrap();
    assert!(!termination.complete());
    assert_eq!(world.state(&id), IntentState::Terminating);
    assert_eq!(termination.remaining.len(), 1);
    assert_eq!(termination.remaining[0].kind, ResourceKind::Host);
    assert!(
        termination
            .notes
            .iter()
            .any(|note| note.contains("minimum commitment"))
    );
    assert_eq!(kinds(&world), vec![ResourceKind::Host]);
    assert_eq!(
        world.budget(ScopeKind::Project).held_micros,
        hold.amount_micros
    );

    // 23 hours in: still committed, still tracked, still holding budget,
    // even though the janitor runs and the expiry has long passed.
    world.clock.advance(22 * HOUR);
    world.sim.tick();
    let report = engine.sweep().unwrap();
    assert!(report.changes.is_empty());
    assert_eq!(world.state(&id), IntentState::Terminating);
    assert_eq!(report.exposure.intents.len(), 1);
    assert_eq!(
        report.exposure.intents[0].resources[0].kind,
        ResourceKind::Host
    );
    assert_eq!(report.exposure.hourly_micros, 1_200_000);
    assert_eq!(kinds(&world), vec![ResourceKind::Host]);
    assert_eq!(
        world.budget(ScopeKind::Project).held_micros,
        hold.amount_micros
    );

    // The committed money is not available to anyone else meanwhile.
    let second = engine
        .request_capacity(&request_with_new_id(&request))
        .unwrap();
    let quote = engine.quote(&second.id).unwrap();
    let error = engine.authorize(&second.id, &quote).unwrap_err();
    assert!(matches!(denial(&error), Denial::BudgetExceeded { .. }));

    // After 24 hours the provider accepts the release.
    world.clock.advance(HOUR + Duration::from_secs(1));
    engine.sweep().unwrap();
    let closed = world.intent(&id);
    assert_eq!(closed.state, IntentState::Terminated);
    assert!(closed.settled_at.is_some());
    assert!(world.sim.snapshot().is_empty());
    let money = engine.money(&id).unwrap();
    assert!(money.hold.unwrap().released_at.is_some());
    // 24 hours and one second of host, one hour of disk and address.
    assert_eq!(money.accrued.micros, 28_800_334 + 4_000 + 5_000);
    assert!(money.accrued.micros <= hold.amount_micros);
}

fn request_with_new_id(request: &NewCapacity) -> NewCapacity {
    NewCapacity {
        id: CloudIntentId::generate(),
        ..request.clone()
    }
}

/// The starting point of every scenario above: nothing is allowed, so
/// nothing can be requested.
#[test]
fn fixture_starts_with_nothing_allowed() {
    let world = World::new();
    assert_eq!(world.now(), T0);
    assert_eq!(world.count("cloud_allowances"), 0);
    assert!(world.engine().budgets().unwrap().is_empty());
    let error = world
        .engine()
        .request_capacity(&world.request())
        .unwrap_err();
    assert!(matches!(refusal(&error), Refusal::NoAllowance { .. }));
}
