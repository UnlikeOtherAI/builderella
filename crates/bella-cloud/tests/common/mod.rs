//! Shared fixture for the integration tests: a clock, a simulated provider
//! and an in-memory database, with nothing allowed until a test says so.

#![allow(dead_code)]

use bella_cloud::{
    Actor, Allowance, Budget, CloudError, Engine, EngineConfig, Intent, IntentState, NewCapacity,
    Period, PurchaseModel, RequestMode, Scope, ScopeKind, SimProvider, intent,
};
use bella_core::api::Role;
use bella_core::job::{CloudMode, Priority};
use bella_core::{Clock, CloudIntentId, FleetId, ManualClock, PrincipalId, ProjectId, UnixMillis};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;

/// 2026-11-26T05:00:00Z.
pub const T0: UnixMillis = UnixMillis(1_795_669_200_000);
pub const MINUTE: Duration = Duration::from_secs(60);
pub const HOUR: Duration = Duration::from_secs(3600);

/// Hourly price of `linux-small` on demand, with its disk and address.
pub const SMALL_HOURLY: u64 = 209_000;
/// Hold for one hour of `linux-small` on demand with two permitted attempts:
/// ten minutes of startup, the hour, and one more startup for the retry.
pub const SMALL_HOLD: u64 = 34_834 + 209_000 + 34_834;

pub struct World {
    pub clock: ManualClock,
    pub sim: SimProvider,
    pub conn: Connection,
    pub fleet: FleetId,
    pub project: ProjectId,
    pub admin: Actor,
    pub developer: Actor,
    pub viewer: Actor,
}

pub fn actor(role: Role) -> Actor {
    Actor {
        principal: PrincipalId::generate(),
        role,
    }
}

/// An allowance that permits the usual test requests up to `limit_micros`.
pub fn allowance(scope: Scope, limit_micros: u64) -> Allowance {
    Allowance {
        scope,
        period: Period::Daily,
        currency: "USD".to_string(),
        limit_micros,
        max_hourly_micros: 50_000_000,
        max_active: 20,
        max_pending: 20,
        allowed_presets: vec![
            "linux-small".to_string(),
            "linux-large".to_string(),
            "mac-dedicated".to_string(),
        ],
        allowed_regions: vec!["sim-east".to_string()],
        allowed_purchase: vec![PurchaseModel::OnDemand, PurchaseModel::Spot],
        max_lifetime_secs: 8 * 3600,
        max_attempts: 2,
    }
}

impl World {
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        Self::with_connection(conn)
    }

    pub fn with_connection(conn: Connection) -> Self {
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        bella_cloud::migrate(&conn).unwrap();
        let clock = ManualClock::at(T0);
        let sim = SimProvider::new(Arc::new(clock.clone()));
        Self {
            clock,
            sim,
            conn,
            fleet: FleetId::generate(),
            project: ProjectId::generate(),
            admin: actor(Role::Admin),
            developer: actor(Role::Developer),
            viewer: actor(Role::Viewer),
        }
    }

    pub fn engine(&self) -> Engine<'_> {
        self.engine_with(EngineConfig::default())
    }

    pub fn engine_with(&self, config: EngineConfig) -> Engine<'_> {
        Engine::new(
            &self.conn,
            &self.sim,
            &self.clock,
            self.fleet.clone(),
            config,
        )
    }

    /// Sets a fleet and a project allowance of `limit_micros` each.
    pub fn allow(&self, limit_micros: u64) {
        self.allow_with(limit_micros, |_| {});
    }

    /// Like [`World::allow`], with a change applied to both allowances.
    pub fn allow_with(&self, limit_micros: u64, change: impl Fn(&mut Allowance)) {
        let engine = self.engine();
        for scope in [Scope::Fleet, Scope::Project(self.project.clone())] {
            let mut allowance = allowance(scope, limit_micros);
            change(&mut allowance);
            engine.set_allowance(&self.admin, &allowance).unwrap();
        }
    }

    /// A manual request by a developer for one hour of `linux-small` on demand.
    pub fn request(&self) -> NewCapacity {
        NewCapacity {
            id: CloudIntentId::generate(),
            project: self.project.clone(),
            job: None,
            covers: Vec::new(),
            project_cloud: CloudMode::Hybrid,
            job_cloud: None,
            source_lan_only: false,
            priority: Priority::Normal,
            escalated: false,
            preset: "linux-small".to_string(),
            region: "sim-east".to_string(),
            purchase: PurchaseModel::OnDemand,
            image: "image-1".to_string(),
            lifetime_secs: 3600,
            requested_by: self.developer.clone(),
            mode: RequestMode::Manual,
            retry_of: None,
        }
    }

    /// Requests and authorizes, returning the intention's identifier.
    pub fn authorized(&self, engine: &Engine<'_>, request: &NewCapacity) -> CloudIntentId {
        let intent = engine.request_capacity(request).unwrap();
        let quote = engine.quote(&intent.id).unwrap();
        engine.authorize(&intent.id, &quote).unwrap();
        intent.id
    }

    pub fn intent(&self, id: &CloudIntentId) -> Intent {
        intent::get_intent(&self.conn, id).unwrap()
    }

    pub fn state(&self, id: &CloudIntentId) -> IntentState {
        self.intent(id).state
    }

    pub fn count(&self, table: &str) -> i64 {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    pub fn budget(&self, scope: ScopeKind) -> Budget {
        self.engine()
            .budgets()
            .unwrap()
            .into_iter()
            .find(|budget| budget.scope.kind() == scope)
            .unwrap()
    }

    pub fn now(&self) -> UnixMillis {
        self.clock.now()
    }
}

/// The refusal carried by an error, or a panic naming what it was instead.
pub fn refusal(error: &CloudError) -> &bella_cloud::Refusal {
    error
        .refusal()
        .unwrap_or_else(|| panic!("expected a refusal, got {error:?}"))
}

/// The denial carried by an error, or a panic naming what it was instead.
pub fn denial(error: &CloudError) -> &bella_cloud::Denial {
    error
        .denial()
        .unwrap_or_else(|| panic!("expected a denial, got {error:?}"))
}
