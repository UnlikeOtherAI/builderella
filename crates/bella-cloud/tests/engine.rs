//! Gates, ledger arithmetic and reconciliation paths that are not tied to
//! one acceptance scenario.

mod common;

use bella_cloud::overflow::{LocalMachine, OverflowPolicy, OverflowPreset, evaluate};
use bella_cloud::{
    Allowance, Billing, CapacityPhase, CloudError, Demand, Denial, EngineConfig, FailureKind,
    IntentState, LABEL_FLEET, LABEL_INTENT, LaunchOutcome, NewCapacity, OwnedResource, Period,
    ProvisionOutcome, PurchaseModel, ReconcileOutcome, Refusal, RequestMode, ResourceKind,
    ResourceState, Scope, ScopeKind, money,
};
use bella_core::api::Role;
use bella_core::job::CloudMode;
use bella_core::platform::{Arch, Os, Platform};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::{CloudIntentId, JobId, ProjectId, UnixMillis};
use common::{HOUR, MINUTE, SMALL_HOLD, SMALL_HOURLY, World, actor, allowance, denial, refusal};
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn provisioning_is_off_until_an_administrator_sets_both_allowances() {
    let world = World::new();
    let engine = world.engine();
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert_eq!(
        refusal(&error),
        &Refusal::NoAllowance {
            scope: ScopeKind::Fleet
        }
    );

    engine
        .set_allowance(&world.admin, &allowance(Scope::Fleet, 10_000_000))
        .unwrap();
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert_eq!(
        refusal(&error),
        &Refusal::NoAllowance {
            scope: ScopeKind::Project
        }
    );

    // An allowance for another project does not help.
    engine
        .set_allowance(
            &world.admin,
            &allowance(Scope::Project(ProjectId::generate()), 10_000_000),
        )
        .unwrap();
    assert!(engine.request_capacity(&world.request()).is_err());
    assert_eq!(world.count("cloud_intents"), 0);
    assert!(world.sim.create_calls().is_empty());

    engine
        .set_allowance(
            &world.admin,
            &allowance(Scope::Project(world.project.clone()), 10_000_000),
        )
        .unwrap();
    let intent = engine.request_capacity(&world.request()).unwrap();
    assert_eq!(intent.state, IntentState::Requested);
    // A request alone holds no money and creates nothing.
    assert_eq!(world.count("cloud_money"), 0);
    assert!(world.sim.create_calls().is_empty());
}

#[test]
fn only_an_administrator_can_set_or_remove_an_allowance() {
    let world = World::new();
    let engine = world.engine();
    for outsider in [&world.developer, &world.viewer] {
        let error = engine
            .set_allowance(outsider, &allowance(Scope::Fleet, 10_000_000))
            .unwrap_err();
        assert!(matches!(error, CloudError::AdminRequired(_)), "{error:?}");
    }
    assert_eq!(world.count("cloud_allowances"), 0);

    world.allow(10_000_000);
    let tx = world.conn.unchecked_transaction().unwrap();
    let error = money::remove_allowance(
        &tx,
        &world.developer,
        &Scope::Fleet,
        Period::Daily,
        world.now(),
    )
    .unwrap_err();
    assert!(matches!(error, CloudError::AdminRequired(_)));
    assert!(
        money::remove_allowance(&tx, &world.admin, &Scope::Fleet, Period::Daily, world.now())
            .unwrap()
    );
    tx.commit().unwrap();

    // Removing the fleet allowance turns provisioning off again.
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert!(matches!(refusal(&error), Refusal::NoAllowance { .. }));
    // Both changes are on record with who made them.
    let events: i64 = world
        .conn
        .query_row(
            "SELECT COUNT(*) FROM cloud_events WHERE kind LIKE 'allowance%' AND detail LIKE ?1",
            [format!("%{}%", world.admin.principal)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 3);
}

#[test]
fn cloud_modes_and_roles_gate_requests() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let with = |project: CloudMode, job: Option<CloudMode>, mode: RequestMode| NewCapacity {
        project_cloud: project,
        job_cloud: job,
        mode,
        ..world.request()
    };
    let refused = |request: &NewCapacity| {
        let error = engine.request_capacity(request).unwrap_err();
        refusal(&error).clone()
    };

    for mode in [RequestMode::Manual, RequestMode::Automatic] {
        assert_eq!(
            refused(&with(CloudMode::ExistingOnly, None, mode)),
            Refusal::ExistingOnly
        );
        // The job's own mode can only narrow the project's.
        assert_eq!(
            refused(&with(
                CloudMode::Hybrid,
                Some(CloudMode::ExistingOnly),
                mode
            )),
            Refusal::ExistingOnly
        );
    }
    assert_eq!(
        refused(&with(CloudMode::Manual, None, RequestMode::Automatic)),
        Refusal::ManualOnly
    );
    assert_eq!(
        refused(&with(
            CloudMode::Hybrid,
            Some(CloudMode::Manual),
            RequestMode::Automatic
        )),
        Refusal::ManualOnly
    );
    assert_eq!(
        refused(&with(CloudMode::AutoOverflow, None, RequestMode::Manual)),
        Refusal::AutomaticOnly
    );

    let by_viewer = NewCapacity {
        requested_by: world.viewer.clone(),
        ..world.request()
    };
    assert_eq!(refused(&by_viewer), Refusal::Forbidden);
    assert_eq!(world.count("cloud_intents"), 0);

    for (project, mode) in [
        (CloudMode::Manual, RequestMode::Manual),
        (CloudMode::AutoOverflow, RequestMode::Automatic),
        (CloudMode::Hybrid, RequestMode::Manual),
        (CloudMode::Hybrid, RequestMode::Automatic),
    ] {
        assert!(engine.request_capacity(&with(project, None, mode)).is_ok());
    }
}

#[test]
fn requests_outside_the_provider_capabilities_are_refused() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let error = engine
        .request_capacity(&NewCapacity {
            preset: "linux-huge".to_string(),
            ..world.request()
        })
        .unwrap_err();
    assert!(matches!(refusal(&error), Refusal::UnsupportedPreset { .. }));
    let error = engine
        .request_capacity(&NewCapacity {
            region: "sim-north".to_string(),
            ..world.request()
        })
        .unwrap_err();
    assert!(matches!(refusal(&error), Refusal::UnsupportedRegion { .. }));
    let error = engine
        .request_capacity(&NewCapacity {
            preset: "mac-dedicated".to_string(),
            purchase: PurchaseModel::Spot,
            ..world.request()
        })
        .unwrap_err();
    assert!(matches!(
        refusal(&error),
        Refusal::UnsupportedPurchase { .. }
    ));
    assert_eq!(world.count("cloud_intents"), 0);
}

#[test]
fn a_provider_that_bills_real_money_is_refused_unless_explicitly_enabled() {
    let world = World::new();
    world.allow(10_000_000);
    world.sim.set_billing(Billing::Real);

    let engine = world.engine();
    let error = engine.request_capacity(&world.request()).unwrap_err();
    assert_eq!(
        refusal(&error),
        &Refusal::PaidProvisioningDisabled {
            provider: "sim".to_string()
        }
    );
    assert_eq!(world.count("cloud_intents"), 0);
    assert!(!EngineConfig::default().allow_paid_providers);

    // An intention authorized while billing was simulated cannot be
    // provisioned once the provider bills for real.
    world.sim.set_billing(Billing::Simulated);
    let id = world.authorized(&engine, &world.request());
    world.sim.set_billing(Billing::Real);
    let error = engine.provision(&id, "boot").unwrap_err();
    assert!(matches!(
        refusal(&error),
        Refusal::PaidProvisioningDisabled { .. }
    ));
    assert!(world.sim.create_calls().is_empty());

    let enabled = world.engine_with(EngineConfig {
        allow_paid_providers: true,
        ..EngineConfig::default()
    });
    assert!(enabled.request_capacity(&world.request()).is_ok());
}

#[test]
fn a_repeated_request_returns_the_same_intention_and_a_changed_one_conflicts() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let request = world.request();
    let first = engine.request_capacity(&request).unwrap();
    let second = engine.request_capacity(&request).unwrap();
    assert_eq!(first, second);
    assert_eq!(world.count("cloud_intents"), 1);

    let changed = NewCapacity {
        preset: "linux-large".to_string(),
        ..request.clone()
    };
    assert!(matches!(
        engine.request_capacity(&changed),
        Err(CloudError::Conflict(_))
    ));

    // Authorizing twice takes one hold.
    let quote = engine.quote(&first.id).unwrap();
    let hold = engine.authorize(&first.id, &quote).unwrap();
    assert_eq!(engine.authorize(&first.id, &quote).unwrap(), hold);
    assert_eq!(world.count("cloud_money"), 1);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, SMALL_HOLD);
}

#[test]
fn illegal_transitions_are_typed_errors() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let intent = engine.request_capacity(&world.request()).unwrap();
    let illegal = |result: Result<_, CloudError>, from: IntentState, to: IntentState| match result {
        Err(CloudError::IllegalTransition { from: f, to: t }) => {
            assert_eq!((f, t), (from, to));
        }
        other => panic!("expected an illegal transition, got {other:?}"),
    };

    // No create call without an authorization.
    illegal(
        engine.provision(&intent.id, "boot").map(|_| ()),
        IntentState::Requested,
        IntentState::Provisioning,
    );
    illegal(
        engine.mark_busy(&intent.id).map(|_| ()),
        IntentState::Requested,
        IntentState::Busy,
    );
    let quote = engine.quote(&intent.id).unwrap();
    engine.authorize(&intent.id, &quote).unwrap();
    illegal(
        engine.drain(&intent.id).map(|_| ()),
        IntentState::Authorized,
        IntentState::Draining,
    );
    engine.provision(&intent.id, "boot").unwrap();
    illegal(
        engine.mark_idle(&intent.id).map(|_| ()),
        IntentState::Bootstrapping,
        IntentState::Ready,
    );
    illegal(
        engine.authorize(&intent.id, &quote).map(|_| ()),
        IntentState::Bootstrapping,
        IntentState::Authorized,
    );
    assert!(world.sim.create_calls().len() == 1);

    // A closed intention stays closed.
    engine.terminate(&intent.id, "done").unwrap();
    assert_eq!(world.state(&intent.id), IntentState::Terminated);
    illegal(
        engine.drain(&intent.id).map(|_| ()),
        IntentState::Terminated,
        IntentState::Draining,
    );
    assert_eq!(
        engine.provision(&intent.id, "boot").unwrap(),
        ProvisionOutcome::NoAction {
            state: IntentState::Terminated
        }
    );
    assert!(matches!(
        engine.intent(&CloudIntentId::generate()),
        Err(CloudError::NotFound(_))
    ));
}

#[test]
fn every_transition_is_recorded_with_its_reason() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();
    engine.on_worker_registered("boot").unwrap();
    engine.mark_busy(&id).unwrap();
    engine.drain(&id).unwrap();
    engine.terminate(&id, "job finished").unwrap();

    let events = engine.events(&id).unwrap();
    let path: Vec<IntentState> = events.iter().filter_map(|event| event.to).collect();
    assert_eq!(
        path,
        vec![
            IntentState::Requested,
            IntentState::Authorized,
            IntentState::Provisioning,
            IntentState::Bootstrapping,
            IntentState::Ready,
            IntentState::Busy,
            IntentState::Draining,
            IntentState::Terminating,
            IntentState::Terminated,
        ]
    );
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
    assert!(kinds.contains(&"hold_taken") && kinds.contains(&"hold_released"));
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    let terminating = events
        .iter()
        .find(|event| event.to == Some(IntentState::Terminating))
        .unwrap();
    assert_eq!(terminating.detail["reason"], "job finished");

    let stored = world.intent(&id);
    assert_eq!(stored.requested_by, world.developer.principal);
    assert_eq!(stored.mode, RequestMode::Manual);
    assert_eq!(stored.create_attempts, 1);
    // The bootstrap string itself is never stored, only its SHA-256.
    let hash = stored.bootstrap_token_hash.unwrap();
    assert_eq!(hash.len(), 64);
    let leaked: i64 = world
        .conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM cloud_intents WHERE bootstrap_hash = 'boot') + \
             (SELECT COUNT(*) FROM cloud_events WHERE detail LIKE '%boot%')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0);
}

#[test]
fn authorization_enforces_every_allowance_limit() {
    let world = World::new();
    let engine = world.engine();
    let set = |scope: Scope, change: &dyn Fn(&mut Allowance)| {
        let mut allowance = allowance(scope, 10_000_000);
        change(&mut allowance);
        engine.set_allowance(&world.admin, &allowance).unwrap();
    };
    let fleet = || Scope::Fleet;
    let project = || Scope::Project(world.project.clone());
    set(fleet(), &|_| {});
    set(project(), &|_| {});
    let denied = |request: &NewCapacity| {
        let intent = engine.request_capacity(request).unwrap();
        let quote = engine.quote(&intent.id).unwrap();
        let error = engine.authorize(&intent.id, &quote).unwrap_err();
        // A denial leaves the intention where it was and takes no hold.
        assert_eq!(world.state(&intent.id), IntentState::Requested);
        denial(&error).clone()
    };

    set(project(), &|a| {
        a.allowed_presets = vec!["linux-large".to_string()]
    });
    assert!(matches!(
        denied(&world.request()),
        Denial::PresetNotAllowed {
            scope: ScopeKind::Project,
            ..
        }
    ));
    // An empty allow-list allows nothing.
    set(project(), &|a| a.allowed_regions = Vec::new());
    assert!(matches!(
        denied(&world.request()),
        Denial::RegionNotAllowed { .. }
    ));
    set(project(), &|a| {
        a.allowed_purchase = vec![PurchaseModel::Spot]
    });
    assert!(matches!(
        denied(&world.request()),
        Denial::PurchaseNotAllowed { .. }
    ));
    set(project(), &|a| a.max_lifetime_secs = 1800);
    assert_eq!(
        denied(&world.request()),
        Denial::LifetimeTooLong {
            scope: ScopeKind::Project,
            requested_secs: 3600,
            max_secs: 1800
        }
    );
    set(project(), &|a| a.currency = "EUR".to_string());
    assert!(matches!(
        denied(&world.request()),
        Denial::CurrencyMismatch { .. }
    ));
    set(project(), &|a| a.max_attempts = 0);
    assert_eq!(
        denied(&world.request()),
        Denial::AttemptsExhausted {
            used: 0,
            allowed: 0
        }
    );
    set(project(), &|a| a.max_hourly_micros = SMALL_HOURLY - 1);
    assert_eq!(
        denied(&world.request()),
        Denial::HourlyRateExceeded {
            scope: ScopeKind::Project,
            would_be_micros: SMALL_HOURLY,
            max_micros: SMALL_HOURLY - 1
        }
    );
    // The fleet limit binds even when the project's is generous.
    set(project(), &|_| {});
    set(fleet(), &|a| a.limit_micros = SMALL_HOLD - 1);
    assert_eq!(
        denied(&world.request()),
        Denial::BudgetExceeded {
            scope: ScopeKind::Fleet,
            period: Period::Daily,
            limit_micros: SMALL_HOLD - 1,
            held_micros: 0,
            spent_micros: 0,
            requested_micros: SMALL_HOLD
        }
    );
    assert_eq!(world.count("cloud_money"), 0);

    // Counts: one pending intention fills `max_pending = 1`.
    set(fleet(), &|_| {});
    set(project(), &|a| a.max_pending = 1);
    let first = world.authorized(&engine, &world.request());
    assert_eq!(
        denied(&world.request()),
        Denial::TooManyPending {
            scope: ScopeKind::Project,
            pending: 1,
            max: 1
        }
    );
    // Once it is usable it is no longer pending, but it is still active.
    engine.provision(&first, "boot").unwrap();
    engine.on_worker_registered("boot").unwrap();
    set(project(), &|a| {
        a.max_pending = 1;
        a.max_active = 1;
    });
    assert_eq!(
        denied(&world.request()),
        Denial::TooManyActive {
            scope: ScopeKind::Project,
            active: 1,
            max: 1
        }
    );
    set(project(), &|a| a.max_hourly_micros = 2 * SMALL_HOURLY - 1);
    assert!(matches!(
        denied(&world.request()),
        Denial::HourlyRateExceeded { .. }
    ));
    // Exactly enough for the second hold next to the first.
    set(project(), &|a| a.limit_micros = 2 * SMALL_HOLD);
    world.authorized(&engine, &world.request());
    assert_eq!(world.budget(ScopeKind::Project).available_micros, 0);
    assert_eq!(world.budget(ScopeKind::Project).held_micros, 2 * SMALL_HOLD);
}

#[test]
fn a_stale_or_mismatched_quote_is_not_accepted_and_prices_are_never_reused() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let intent = engine.request_capacity(&world.request()).unwrap();
    let quote = engine.quote(&intent.id).unwrap();
    assert_eq!(quote.hourly_micros, SMALL_HOURLY);

    world.clock.advance(Duration::from_secs(301));
    let error = engine.authorize(&intent.id, &quote).unwrap_err();
    assert_eq!(
        denial(&error),
        &Denial::QuoteStale {
            age_secs: 301,
            max_age_secs: 300
        }
    );

    // The price changed meanwhile. A fresh quote carries the new price, and
    // the hold is computed from it.
    assert!(
        world
            .sim
            .set_price("linux-small", PurchaseModel::OnDemand, 400_000)
    );
    let fresh = engine.quote(&intent.id).unwrap();
    assert_eq!(fresh.hourly_micros, 409_000);
    assert_eq!(fresh.priced_at, world.now());

    let mut other_preset = fresh.clone();
    other_preset.preset = "linux-large".to_string();
    let error = engine.authorize(&intent.id, &other_preset).unwrap_err();
    assert_eq!(denial(&error), &Denial::QuoteMismatch { field: "preset" });
    let mut other_purchase = fresh.clone();
    other_purchase.purchase = PurchaseModel::Spot;
    assert!(engine.authorize(&intent.id, &other_purchase).is_err());
    let mut future = fresh.clone();
    future.priced_at = UnixMillis(world.now().0 + 1);
    let error = engine.authorize(&intent.id, &future).unwrap_err();
    assert_eq!(denial(&error), &Denial::QuoteFromFuture);

    let hold = engine.authorize(&intent.id, &fresh).unwrap();
    assert_eq!(hold.hourly_micros, 409_000);
    // 409,000 for 10 minutes, rounded up, then the hour, then one retry startup.
    assert_eq!(hold.amount_micros, 68_167 + 409_000 + 68_167);
}

#[test]
fn holds_survive_a_period_rollover_and_settled_spend_belongs_to_its_period() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    // One intention keeps running across midnight; another is used for half
    // an hour and settled on the first day.
    let running = world.authorized(
        &engine,
        &NewCapacity {
            lifetime_secs: 8 * 3600,
            ..world.request()
        },
    );
    engine.provision(&running, "boot-running").unwrap();
    let running_hold = engine.money(&running).unwrap().hold.unwrap().amount_micros;

    let short = world.authorized(&engine, &world.request());
    engine.provision(&short, "boot-short").unwrap();
    world.clock.advance(30 * MINUTE);
    assert!(engine.terminate(&short, "done").unwrap().complete());
    // Half an hour of instance, disk and address.
    let spent = 100_000 + 2_000 + 2_500;

    let day_one = world.budget(ScopeKind::Project);
    assert_eq!(day_one.held_micros, running_hold);
    assert_eq!(day_one.spent_micros, spent);
    assert_eq!(day_one.available_micros, 10_000_000 - running_hold - spent);
    assert_eq!(day_one.window_start, UnixMillis(1_795_651_200_000));

    // 20 hours after the start it is 01:00 UTC on the next day. The spend of day one
    // is no longer counted; the hold still is.
    world.clock.advance(19 * HOUR + 30 * MINUTE);
    let day_two = world.budget(ScopeKind::Project);
    assert_eq!(day_two.window_start.0, day_one.window_end.0);
    assert_eq!(day_two.spent_micros, 0);
    assert!(day_two.held_micros >= running_hold);
    assert_eq!(day_two.active, 1);

    // A monthly allowance still counts day one's spend on day two.
    let mut monthly = allowance(Scope::Project(world.project.clone()), 10_000_000);
    monthly.period = Period::Monthly;
    engine.set_allowance(&world.admin, &monthly).unwrap();
    let month = engine
        .budgets()
        .unwrap()
        .into_iter()
        .find(|b| b.period == Period::Monthly)
        .unwrap();
    assert_eq!(month.spent_micros, spent);
    assert!(month.held_micros >= running_hold);
}

#[test]
fn hold_accrued_estimate_and_reported_actual_are_three_separate_numbers() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();

    let money = engine.money(&id).unwrap();
    assert_eq!(money.hold.as_ref().unwrap().amount_micros, SMALL_HOLD);
    assert_eq!(money.accrued.micros, 0);
    assert_eq!(money.accrued.as_of, world.now());
    assert_eq!(money.actual, None);

    world.clock.advance(30 * MINUTE);
    let money = engine.money(&id).unwrap();
    assert_eq!(money.hold.as_ref().unwrap().amount_micros, SMALL_HOLD);
    assert_eq!(money.accrued.micros, 104_500);
    assert_eq!(money.accrued.as_of, world.now());
    assert!(!money.accrued.assumptions.is_empty());

    // The provider reports a provisional figure. Nothing else moves.
    engine.record_actual(&id, 90_000, false).unwrap();
    let money = engine.money(&id).unwrap();
    assert_eq!(money.actual.as_ref().unwrap().micros, 90_000);
    assert!(!money.actual.as_ref().unwrap().is_final);
    assert_eq!(money.accrued.micros, 104_500);
    assert_eq!(money.hold.as_ref().unwrap().amount_micros, SMALL_HOLD);

    // After settlement the estimate is frozen with its timestamp.
    engine.terminate(&id, "done").unwrap();
    let settled_at = world.now();
    world.clock.advance(HOUR);
    let money = engine.money(&id).unwrap();
    assert_eq!(money.accrued.micros, 104_500);
    assert_eq!(money.accrued.as_of, settled_at);
    assert_eq!(money.hold.as_ref().unwrap().released_at, Some(settled_at));
    // A provisional actual below the estimate does not lower the spend...
    assert_eq!(world.budget(ScopeKind::Fleet).spent_micros, 104_500);
    // ...a provisional one above it raises it...
    engine.record_actual(&id, 120_000, false).unwrap();
    assert_eq!(world.budget(ScopeKind::Fleet).spent_micros, 120_000);
    // ...and a final figure replaces the estimate.
    engine.record_actual(&id, 95_000, true).unwrap();
    assert_eq!(world.budget(ScopeKind::Fleet).spent_micros, 95_000);
    assert_eq!(engine.money(&id).unwrap().accrued.micros, 104_500);

    // There is nothing to report an actual against without a hold.
    let unauthorized = engine.request_capacity(&world.request()).unwrap();
    assert!(matches!(
        engine.record_actual(&unauthorized.id, 1, true),
        Err(CloudError::NotFound(_))
    ));
}

fn stray(world: &World, id: &str, labelled: bool, intent: Option<&CloudIntentId>) -> OwnedResource {
    let mut labels = BTreeMap::new();
    if labelled {
        labels.insert(LABEL_FLEET.to_string(), world.fleet.as_str().to_string());
    }
    if let Some(intent) = intent {
        labels.insert(LABEL_INTENT.to_string(), intent.as_str().to_string());
    }
    OwnedResource {
        id: id.to_string(),
        kind: ResourceKind::Disk,
        state: ResourceState::Running,
        intent_id: intent.cloned(),
        labels,
        created_at: world.now(),
        expires_at: None,
        releasable_at: None,
        hourly_micros: 7_000,
    }
}

#[test]
fn orphans_are_reported_and_deleted_only_with_proof_of_ownership() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    // A live intention's own resources are not orphans.
    let live = world.authorized(&engine, &world.request());
    engine.provision(&live, "boot").unwrap();
    // A resource of a settled intention that the provider still has.
    let closed = world.authorized(&engine, &world.request());
    engine.terminate(&closed, "not needed").unwrap();

    world.sim.faults(|f| f.list_unlabelled = true);
    world
        .sim
        .inject_resource(stray(&world, "vol-ours", true, None));
    world
        .sim
        .inject_resource(stray(&world, "vol-unlabelled", false, None));
    world
        .sim
        .inject_resource(stray(&world, "vol-of-closed", true, Some(&closed)));

    let report = engine.sweep().unwrap();
    let find = |id: &str| report.orphans.iter().find(|o| o.resource_id == id).unwrap();
    assert_eq!(report.orphans.len(), 3);
    assert!(find("vol-ours").ownership_proven && find("vol-ours").deleted);
    assert!(find("vol-of-closed").deleted);
    assert!(find("vol-of-closed").note.contains("settled intention"));
    let foreign = find("vol-unlabelled");
    assert!(!foreign.ownership_proven && !foreign.deleted);
    assert!(foreign.note.contains("does not carry this fleet's label"));

    // The unproven one is still there, still visible, still priced.
    let left: Vec<String> = world.sim.snapshot().iter().map(|r| r.id.clone()).collect();
    assert!(left.contains(&"vol-unlabelled".to_string()));
    assert!(!left.contains(&"vol-ours".to_string()));
    assert!(!left.contains(&"vol-of-closed".to_string()));
    assert_eq!(report.exposure.orphans.len(), 1);
    assert_eq!(report.exposure.orphans[0].resource_id, "vol-unlabelled");
    assert_eq!(
        report.exposure.hourly_micros,
        SMALL_HOURLY + 7_000,
        "the live intention's resources plus the orphan"
    );
    assert_eq!(world.state(&live), IntentState::Bootstrapping);

    // Once the provider no longer lists it, it drops out.
    world.sim.faults(|f| f.list_unlabelled = false);
    let report = engine.sweep().unwrap();
    assert!(report.orphans.is_empty());
    assert!(report.exposure.orphans.is_empty());
}

#[test]
fn a_worker_that_never_registers_fails_its_intention_and_cools_the_preset_down() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| f.readiness_delay_secs = 3600);
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();

    world.clock.advance(14 * MINUTE);
    engine.sweep().unwrap();
    assert_eq!(world.state(&id), IntentState::Bootstrapping);
    let tracked = bella_cloud::intent::resources(&world.conn, &id).unwrap();
    let instance = tracked
        .iter()
        .find(|r| r.kind == ResourceKind::Instance)
        .unwrap();
    assert_eq!(instance.state, ResourceState::Pending);

    world.clock.advance(MINUTE);
    let report = engine.sweep().unwrap();
    assert!(report.interruptions.is_empty());
    let failed = world.intent(&id);
    assert_eq!(failed.state, IntentState::Failed);
    assert_eq!(failed.failure, Some(FailureKind::BootstrapTimeout));
    // The instance that never came up is deleted and the hold released.
    assert!(failed.settled_at.is_some());
    assert!(world.sim.snapshot().is_empty());
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);

    let (capacity, history) = engine.overflow_view(UnixMillis(0)).unwrap();
    assert!(capacity.is_empty());
    assert_eq!(history.launches.len(), 1);
    assert_eq!(
        history.launches[0].outcome,
        LaunchOutcome::BootstrapFailed { at: world.now() }
    );
}

#[test]
fn two_instances_for_one_token_quarantine_the_intention() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    // The first create takes effect, its response is lost, and the listing
    // hides the instance for longer than the settling period. The provider
    // also ignores the idempotency token. This is the worst case for a retry.
    world.sim.faults(|f| {
        f.lose_create_response = 1;
        f.ignore_idempotency_token = true;
        f.listing_lag_secs = 600;
    });
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot-1").unwrap();
    world.clock.advance(Duration::from_secs(120));
    assert!(matches!(
        engine.reconcile(&id).unwrap(),
        ReconcileOutcome::RetryPermitted { .. }
    ));
    assert_eq!(
        engine.provision(&id, "boot-2").unwrap(),
        ProvisionOutcome::Created
    );

    // When the listing catches up, the duplicate is visible. The engine does
    // not guess which one is real: it quarantines and keeps the hold.
    world.clock.advance(Duration::from_secs(600));
    let report = engine.sweep().unwrap();
    assert_eq!(world.state(&id), IntentState::Quarantined);
    assert_eq!(report.exposure.intents.len(), 1);
    assert_eq!(report.exposure.intents[0].resources.len(), 6);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, SMALL_HOLD);
    let report = engine.sweep().unwrap();
    assert!(report.problems.iter().any(|p| p.contains("quarantined")));

    // A person terminates it; both instances and their leftovers go.
    let termination = engine
        .terminate(&id, "resolved by an administrator")
        .unwrap();
    assert!(termination.complete(), "{termination:?}");
    assert!(world.sim.snapshot().is_empty());
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);
}

#[test]
fn a_pending_request_that_is_never_provisioned_expires_and_releases_its_hold() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let requested = engine.request_capacity(&world.request()).unwrap().id;
    let authorized = world.authorized(&engine, &world.request());
    world.clock.advance(71 * MINUTE);
    let report = engine.sweep().unwrap();
    assert_eq!(report.changes.len(), 2);
    assert_eq!(world.state(&requested), IntentState::Terminated);
    assert_eq!(world.state(&authorized), IntentState::Terminated);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);
    assert_eq!(world.budget(ScopeKind::Fleet).spent_micros, 0);
    assert!(world.sim.create_calls().is_empty());
}

#[test]
fn a_cancelled_create_in_flight_is_cleaned_up_once_it_appears() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    world.sim.faults(|f| {
        f.lose_create_response = 1;
        f.listing_lag_secs = 60;
    });
    let id = world.authorized(&engine, &world.request());
    engine.provision(&id, "boot").unwrap();

    // Cancelled while nothing is visible yet: it must not be closed on the
    // strength of an empty listing.
    let termination = engine.cancel(&id, "changed my mind").unwrap();
    assert!(!termination.complete());
    assert_eq!(world.state(&id), IntentState::Terminating);
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, SMALL_HOLD);
    assert_eq!(world.sim.snapshot().len(), 3);

    world.clock.advance(Duration::from_secs(61));
    engine.sweep().unwrap();
    assert_eq!(world.state(&id), IntentState::Terminated);
    assert!(world.sim.snapshot().is_empty());
    assert_eq!(world.budget(ScopeKind::Fleet).held_micros, 0);
}

#[test]
fn overflow_proposals_become_one_intention_however_often_they_are_submitted() {
    let world = World::new();
    world.allow(10_000_000);
    let engine = world.engine();
    let linux = Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    };
    let policy = OverflowPolicy {
        enabled: true,
        provider: "sim".to_string(),
        presets: vec![OverflowPreset {
            name: "linux-large".to_string(),
            region: "sim-east".to_string(),
            purchase: PurchaseModel::Spot,
            platform: linux,
            resources: ResourceVector::new(16_000, 64 << 30, 100 << 30),
            lifetime_secs: 3600,
        }],
        local_machines: vec![LocalMachine {
            platform: linux,
            capacity: ResourceVector::new(8_000, 16 << 30, 100 << 30),
        }],
        ..OverflowPolicy::default()
    };
    let demand: Vec<Demand> = (0..20)
        .map(|_| Demand {
            job: JobId::generate(),
            project: world.project.clone(),
            platform: linux,
            envelope: ResourceEnvelope::fixed(ResourceVector::new(500, 2 << 30, 1 << 30)),
            queued_ms: 0,
            predicted_wait_ms: Some(10 * 60 * 1000),
            deadline: None,
            cloud: CloudMode::AutoOverflow,
            source_lan_only: false,
        })
        .collect();
    let system = actor(Role::Admin);

    let (capacity, history) = engine.overflow_view(UnixMillis(0)).unwrap();
    let proposals = evaluate(&policy, &demand, &capacity, &history, world.now());
    assert_eq!(proposals.len(), 1);
    // Twenty submissions of the same proposal are one intention.
    for _ in 0..20 {
        let request =
            proposals[0].to_new_capacity(system.clone(), CloudMode::AutoOverflow, "image-1");
        engine.request_capacity(&request).unwrap();
    }
    assert_eq!(world.count("cloud_intents"), 1);
    let stored = world.intent(&proposals[0].intent_id);
    assert_eq!(stored.mode, RequestMode::Automatic);
    assert_eq!(stored.covers.len(), 20);
    assert_eq!(stored.purchase, PurchaseModel::Spot);

    // With the intention on record, the same shortage asks for nothing.
    let (capacity, history) = engine.overflow_view(UnixMillis(0)).unwrap();
    assert_eq!(capacity.len(), 1);
    assert_eq!(capacity[0].phase, CapacityPhase::Pending);
    assert!(evaluate(&policy, &demand, &capacity, &history, world.now()).is_empty());

    // The proposal still has to pass the project's cloud mode.
    let manual_project = proposals[0].to_new_capacity(system, CloudMode::Manual, "image-1");
    let error = engine
        .request_capacity(&NewCapacity {
            id: CloudIntentId::generate(),
            ..manual_project
        })
        .unwrap_err();
    assert_eq!(refusal(&error), &Refusal::ManualOnly);
}
