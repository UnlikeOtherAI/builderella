//! The engine: every operation on cloud capacity.
//!
//! Rules every operation follows:
//!
//! * The database is written before the provider is called. A controller
//!   that dies between the two restarts with a record of what it may have
//!   done, and [`Engine::sweep`] finishes the job.
//! * No database transaction is held open across a provider call.
//! * Every operation can be repeated. Repeating it never creates a second
//!   resource or a second hold.
//! * A provider answer that leaves the outcome open (`Timeout`,
//!   `Unavailable`) is recorded as uncertainty and resolved only by a
//!   listing, never by assuming.
//! * A hold is released only when the provider positively reports that no
//!   resource of the intention remains.

use crate::convert::{add, prorate_up};
use crate::error::{CloudError, Refusal, Result};
use crate::intent::{
    self, FailureKind, Intent, IntentEvent, IntentState, NewIntent, Orphan, RequestMode,
    TrackedResource,
};
use crate::money::{
    self, Accrued, AdmissionRequest, Allowance, Budget, Denial, Hold, Money, MoneyConfig, ScopeKind,
};
use crate::overflow::{CapacityPhase, LaunchHistory, LaunchOutcome, LaunchRecord, PendingOrActive};
use crate::provider::{
    Billing, Capabilities, CreateAck, CreateRequest, LABEL_FLEET, LABEL_INTENT, OwnedResource,
    Provider, ProviderError, PurchaseModel, Quote, ResourceKind,
};
use bella_core::api::Role;
use bella_core::job::{CloudMode, Outcome, Priority};
use bella_core::resources::ResourceVector;
use bella_core::{
    Clock, CloudIntentId, Digest, FleetId, JobId, PrincipalId, ProjectId, UnixMillis,
};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

/// Where the engine gets its transactions.
///
/// `transact` runs `work` inside one write transaction, commits when it
/// returns `Ok` and rolls back when it returns `Err`. The controller
/// implements this on top of the ledger's `with_transaction`; the
/// implementations here cover a bare connection.
pub trait Database {
    fn transact(&self, work: &mut dyn FnMut(&Transaction<'_>) -> Result<()>) -> Result<()>;
}

impl Database for Connection {
    /// Uses `BEGIN IMMEDIATE`, so the write lock is taken before anything is
    /// read and two writers cannot both act on the same snapshot.
    fn transact(&self, work: &mut dyn FnMut(&Transaction<'_>) -> Result<()>) -> Result<()> {
        let tx = Transaction::new_unchecked(self, TransactionBehavior::Immediate)?;
        work(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

impl Database for Mutex<Connection> {
    fn transact(&self, work: &mut dyn FnMut(&Transaction<'_>) -> Result<()>) -> Result<()> {
        let conn = self.lock().unwrap_or_else(PoisonError::into_inner);
        Database::transact(&*conn, work)
    }
}

/// Runs `work` in one transaction of `db` and returns its value.
pub fn run<T>(db: &dyn Database, mut work: impl FnMut(&Transaction<'_>) -> Result<T>) -> Result<T> {
    let mut output = None;
    db.transact(&mut |tx: &Transaction<'_>| {
        output = Some(work(tx)?);
        Ok(())
    })?;
    output.ok_or_else(|| CloudError::Invalid("the transaction body did not run".to_string()))
}

/// Who is acting. The role comes from the controller's token check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub principal: PrincipalId,
    pub role: Role,
}

/// Engine settings. The defaults are starting values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    /// A quote older than this cannot be used to authorize.
    pub quote_max_age_secs: u64,
    /// Capacity is refused when the provider-side janitor last succeeded
    /// longer ago than this.
    pub janitor_max_staleness_secs: u64,
    /// How long after a create call an empty listing is not yet trusted.
    pub settling_secs: u64,
    /// Time a new instance is assumed to bill before it does useful work.
    /// Added to the lifetime to form the intention's expiry.
    pub startup_secs: u64,
    /// A worker that has not registered this long after creation has failed.
    pub bootstrap_timeout_secs: u64,
    /// A retry is not worth starting with less lifetime than this left.
    pub min_retry_lifetime_secs: u64,
    /// Whether a provider that bills real money may be used at all.
    /// Off by default; nothing in this crate turns it on.
    pub allow_paid_providers: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            quote_max_age_secs: 5 * 60,
            janitor_max_staleness_secs: 30 * 60,
            settling_secs: 2 * 60,
            startup_secs: 10 * 60,
            bootstrap_timeout_secs: 15 * 60,
            min_retry_lifetime_secs: 5 * 60,
            allow_paid_providers: false,
        }
    }
}

impl EngineConfig {
    /// The part of the configuration the money ledger uses.
    pub fn money(&self) -> MoneyConfig {
        MoneyConfig {
            quote_max_age_secs: self.quote_max_age_secs,
            startup_secs: self.startup_secs,
        }
    }
}

/// A request for capacity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCapacity {
    /// Chosen by the caller. Repeating a request with the same identifier
    /// returns the existing intention instead of creating another.
    pub id: CloudIntentId,
    pub project: ProjectId,
    pub job: Option<JobId>,
    /// Jobs the overflow policy counted on this capacity for.
    pub covers: Vec<JobId>,
    /// The project's cloud mode.
    pub project_cloud: CloudMode,
    /// The job's own cloud mode, when the request is for a job. Both modes
    /// must permit the request.
    pub job_cloud: Option<CloudMode>,
    /// True when the project or the job says the source may not leave the
    /// LAN. Such a request is always refused.
    pub source_lan_only: bool,
    /// Recorded for the audit trail. It never changes a refusal.
    pub priority: Priority,
    /// Recorded for the audit trail. It never changes a refusal.
    pub escalated: bool,
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub image: String,
    /// Useful lifetime wanted. The intention expires this long, plus the
    /// startup allowance, after the request.
    pub lifetime_secs: u64,
    pub requested_by: Actor,
    pub mode: RequestMode,
    /// The failed intention this one replaces, for a retry.
    pub retry_of: Option<CloudIntentId>,
}

/// What `provision` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProvisionOutcome {
    /// The provider created the instance; the intention is `Bootstrapping`.
    Created,
    /// The create call's outcome is not known. The intention is `Unknown`,
    /// the hold is kept and no second create is issued until
    /// [`Engine::reconcile`] has established what exists.
    Unknown { reason: String },
    /// Spot capacity is unavailable and spot is the only purchase model the
    /// allowance permits. The intention is back in `Authorized` with its
    /// hold; nothing was created and nothing was converted.
    Blocked { reason: String },
    /// The provider definitely created nothing. The intention is `Failed`.
    Failed {
        kind: FailureKind,
        reason: String,
        /// For a spot request that found no capacity: whether the allowance
        /// would permit a separate, explicit on-demand request. The engine
        /// never makes that request itself.
        on_demand_permitted: bool,
    },
    /// The intention was not in `Authorized`, so no create call was issued.
    NoAction { state: IntentState },
}

/// The first half of a provision, for callers that issue the provider call
/// themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionStep {
    /// The intention is now `Provisioning`. Issue exactly this create call
    /// and pass its result to [`Engine::finish_provision`].
    Call(Box<CreateRequest>),
    /// Nothing to call.
    Done(ProvisionOutcome),
}

/// What `reconcile` established about an uncertain intention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReconcileOutcome {
    /// The provider reports exactly one instance; it now belongs to the
    /// intention, which is `Bootstrapping`.
    Adopted { resource_id: String },
    /// Nothing could be established yet. The hold is kept.
    StillUnknown { reason: String },
    /// The provider positively reports nothing after the settling period.
    /// The intention is back in `Authorized`; `provision` may be called
    /// again and will use the same idempotency token.
    RetryPermitted {
        attempts_used: u32,
        attempts_allowed: u32,
    },
    /// The intention is `Failed`.
    Failed { kind: FailureKind },
    /// The provider reports more than one instance for one token. The
    /// intention is `Quarantined` and needs a person.
    Quarantined { instances: usize },
    /// The intention had been cancelled or had expired, so it was moved to
    /// termination instead of being used.
    TerminationScheduled,
    /// The intention's provider state was not uncertain.
    NotUncertain { state: IntentState },
}

string_enum! {
    /// Why a worker was not admitted.
    pub enum DenyReason {
        /// No intention issued this bootstrap string.
        UnknownToken => "unknown_token",
        /// The string was already used once.
        TokenAlreadyUsed => "token_already_used",
        /// The intention was cancelled.
        Cancelled => "cancelled",
        /// The intention expired.
        Expired => "expired",
        /// The intention failed or was terminated.
        Closed => "closed",
        /// The engine has not yet established what the provider created.
        /// The string is not consumed; the worker may try again.
        ProviderStateUncertain => "provider_state_uncertain",
        /// The intention is quarantined.
        Quarantined => "quarantined",
    }
}

/// The answer to a worker presenting its bootstrap string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Registration {
    /// The string was valid and is now used up; the intention is `Ready`.
    Admit { intent: CloudIntentId },
    Deny {
        reason: DenyReason,
        intent: Option<CloudIntentId>,
        /// The worker's resources are recorded for deletion: the intention
        /// is terminating, or is closed and the next sweep removes what it
        /// still owns.
        deletion_scheduled: bool,
    },
}

/// Where a termination stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Termination {
    pub intent: Intent,
    /// Resources that still exist or whose deletion is not yet proven.
    pub remaining: Vec<TrackedResource>,
    /// What stood in the way, in plain words.
    pub notes: Vec<String>,
}

impl Termination {
    /// No resource remains and the hold is released.
    pub fn complete(&self) -> bool {
        self.intent.settled_at.is_some()
    }
}

/// Why a retry is not permitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "blocker", rename_all = "snake_case")]
pub enum RetryBlocker {
    AttemptsExhausted {
        used: u32,
        allowed: u32,
    },
    /// Too little of the original lifetime is left.
    InsufficientTime {
        remaining_secs: u64,
        required_secs: u64,
    },
    /// The provider could not price the retry.
    QuoteUnavailable {
        detail: String,
    },
    /// The allowance would deny the retry.
    Budget {
        denial: Denial,
    },
}

/// Whether a failed intention may be replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetryVerdict {
    /// True only when attempts, time and budget all allow it.
    pub permitted: bool,
    pub attempts_used: u32,
    pub attempts_allowed: u32,
    /// Lifetime a replacement could have without outliving the original expiry.
    pub remaining_lifetime_secs: u64,
    pub blockers: Vec<RetryBlocker>,
}

/// Capacity that was lost while it may have been in use.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Interruption {
    pub intent: CloudIntentId,
    pub job: Option<JobId>,
    pub covers: Vec<JobId>,
    /// The outcome to record for work that was running: always
    /// `InfraInterrupted`.
    pub outcome: Outcome,
    /// Always false. Whatever the interrupted attempt produced is not a
    /// result; only a completed attempt sets a job's canonical result.
    pub promote_partial_result: bool,
    pub retry: RetryVerdict,
}

/// One state change made by a sweep.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StateChange {
    pub intent: CloudIntentId,
    pub from: IntentState,
    pub to: IntentState,
}

/// A provider resource a sweep found that no open intention accounts for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OrphanReport {
    pub resource_id: String,
    pub kind: ResourceKind,
    pub hourly_micros: u64,
    pub intent_label: Option<String>,
    /// The resource carries this fleet's label.
    pub ownership_proven: bool,
    /// Deleted by this sweep. Never true without `ownership_proven`.
    pub deleted: bool,
    pub note: String,
}

/// Cost that is not yet resolved for one intention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IntentExposure {
    pub intent: CloudIntentId,
    pub project: ProjectId,
    pub state: IntentState,
    pub reason: Option<String>,
    /// The outstanding hold; zero when none is held.
    pub hold_micros: u64,
    pub accrued: Accrued,
    /// What the resources that still exist cost per hour.
    pub hourly_micros: u64,
    pub resources: Vec<TrackedResource>,
}

/// Everything that still costs or may cost money.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Exposure {
    pub as_of: UnixMillis,
    pub intents: Vec<IntentExposure>,
    /// Provider resources no open intention accounts for and that are not
    /// known to be gone.
    pub orphans: Vec<Orphan>,
    pub held_micros: u64,
    pub accrued_micros: u64,
    /// Combined hourly rate of every resource listed here.
    pub hourly_micros: u64,
}

/// What one sweep found and did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SweepReport {
    pub at: UnixMillis,
    pub changes: Vec<StateChange>,
    pub interruptions: Vec<Interruption>,
    pub orphans: Vec<OrphanReport>,
    /// Things the sweep could not resolve this time.
    pub problems: Vec<String>,
    pub exposure: Exposure,
}

/// Everything that still costs or may cost money for `provider`, as of `now`.
pub fn exposure(conn: &Connection, provider: &str, now: UnixMillis) -> Result<Exposure> {
    let mut intents = Vec::new();
    let mut held_micros = 0u64;
    let mut accrued_micros = 0u64;
    let mut hourly_total = 0u64;
    for open in intent::list_unsettled(conn, provider)? {
        let hold = money::hold(conn, &open.id)?.filter(|hold| hold.released_at.is_none());
        let resources = intent::live_resources(conn, &open.id)?;
        if hold.is_none() && resources.is_empty() {
            continue;
        }
        let accrued = money::accrued(conn, &open.id, now)?;
        let mut hourly_micros = 0u64;
        for resource in &resources {
            hourly_micros = add(hourly_micros, resource.hourly_micros)?;
        }
        let hold_micros = hold.map(|hold| hold.amount_micros).unwrap_or(0);
        held_micros = add(held_micros, hold_micros)?;
        accrued_micros = add(accrued_micros, accrued.micros)?;
        hourly_total = add(hourly_total, hourly_micros)?;
        intents.push(IntentExposure {
            intent: open.id,
            project: open.project,
            state: open.state,
            reason: open.reason,
            hold_micros,
            accrued,
            hourly_micros,
            resources,
        });
    }
    let orphans = intent::open_orphans(conn, provider)?;
    for orphan in &orphans {
        hourly_total = add(hourly_total, orphan.hourly_micros)?;
    }
    Ok(Exposure {
        as_of: now,
        intents,
        orphans,
        held_micros,
        accrued_micros,
        hourly_micros: hourly_total,
    })
}

fn check_mode(cloud: CloudMode, mode: RequestMode) -> std::result::Result<(), Refusal> {
    match (cloud, mode) {
        (CloudMode::ExistingOnly, _) => Err(Refusal::ExistingOnly),
        (CloudMode::Manual, RequestMode::Automatic) => Err(Refusal::ManualOnly),
        (CloudMode::AutoOverflow, RequestMode::Manual) => Err(Refusal::AutomaticOnly),
        _ => Ok(()),
    }
}

/// Settles a final intention when nothing stands in the way. Returns
/// whether it is settled.
fn settle_if_clean(tx: &Transaction<'_>, id: &CloudIntentId, now: UnixMillis) -> Result<bool> {
    let current = intent::get_intent(tx, id)?;
    if current.settled_at.is_some() {
        return Ok(true);
    }
    if !current.state.is_final()
        || current.create_uncertain
        || !intent::live_resources(tx, id)?.is_empty()
    {
        return Ok(false);
    }
    money::settle(tx, id, now)?;
    Ok(true)
}

fn deletable(resource: &TrackedResource, now: UnixMillis) -> bool {
    !(resource.kind == ResourceKind::Host && resource.releasable_at.is_some_and(|at| now < at))
}

fn delete_order(kind: ResourceKind) -> u8 {
    match kind {
        ResourceKind::Instance => 0,
        ResourceKind::Disk => 1,
        ResourceKind::Address => 2,
        ResourceKind::Host => 3,
    }
}

fn same_request(existing: &Intent, request: &NewCapacity) -> bool {
    existing.project == request.project
        && existing.job == request.job
        && existing.preset == request.preset
        && existing.region == request.region
        && existing.purchase == request.purchase
        && existing.image == request.image
        && existing.lifetime_secs == request.lifetime_secs
        && existing.mode == request.mode
        && existing.retry_of == request.retry_of
        && existing.requested_by == request.requested_by.principal
}

/// The engine for one provider and one fleet.
pub struct Engine<'a> {
    db: &'a dyn Database,
    provider: &'a dyn Provider,
    clock: &'a dyn Clock,
    fleet: FleetId,
    config: EngineConfig,
    provider_name: String,
}

impl<'a> Engine<'a> {
    /// An engine for `provider` on behalf of `fleet`. It holds no state of
    /// its own: everything is in the database, so a new engine continues
    /// where an earlier one stopped.
    pub fn new(
        db: &'a dyn Database,
        provider: &'a dyn Provider,
        clock: &'a dyn Clock,
        fleet: FleetId,
        config: EngineConfig,
    ) -> Self {
        let provider_name = provider.capabilities().provider;
        Self {
            db,
            provider,
            clock,
            fleet,
            config,
            provider_name,
        }
    }

    fn tx<T>(&self, work: impl FnMut(&Transaction<'_>) -> Result<T>) -> Result<T> {
        run(self.db, work)
    }

    fn carries_fleet_label(&self, resource: &OwnedResource) -> bool {
        resource.labels.get(LABEL_FLEET).map(String::as_str) == Some(self.fleet.as_str())
    }

    /// The provider's listing for one intention, reduced to the resources
    /// that provably belong to it: labelled with this fleet and with this
    /// intention. Anything else an adapter returns is ignored, so nothing
    /// is adopted or deleted on a listing's word alone.
    fn owned_by(
        &self,
        id: &CloudIntentId,
    ) -> std::result::Result<Vec<OwnedResource>, ProviderError> {
        let listing = self.provider.find_by_intent(id)?;
        Ok(listing
            .into_iter()
            .filter(|resource| {
                self.carries_fleet_label(resource)
                    && resource.labels.get(LABEL_INTENT).map(String::as_str) == Some(id.as_str())
            })
            .collect())
    }

    /// The intention as stored.
    pub fn intent(&self, id: &CloudIntentId) -> Result<Intent> {
        self.tx(|tx| intent::get_intent(tx, id))
    }

    /// The intention's history, oldest first.
    pub fn events(&self, id: &CloudIntentId) -> Result<Vec<IntentEvent>> {
        self.tx(|tx| intent::events(tx, id))
    }

    /// The hold, the accrued estimate and the reported actual of an intention.
    pub fn money(&self, id: &CloudIntentId) -> Result<Money> {
        let now = self.clock.now();
        self.tx(|tx| money::money(tx, id, now))
    }

    /// Sets an allowance. Administrators only; see [`money::set_allowance`].
    pub fn set_allowance(&self, actor: &Actor, allowance: &Allowance) -> Result<()> {
        let now = self.clock.now();
        self.tx(|tx| money::set_allowance(tx, actor, allowance, now))
    }

    /// How every allowance stands right now.
    pub fn budgets(&self) -> Result<Vec<Budget>> {
        let now = self.clock.now();
        self.tx(|tx| {
            let mut budgets = Vec::new();
            for allowance in money::allowances(tx)? {
                budgets.push(money::budget(tx, &allowance, now)?);
            }
            Ok(budgets)
        })
    }

    /// Records the provider's billed figure for an intention.
    pub fn record_actual(&self, id: &CloudIntentId, micros: u64, is_final: bool) -> Result<()> {
        let now = self.clock.now();
        self.tx(|tx| money::record_actual(tx, id, micros, is_final, now))
    }

    fn paid_gate(&self, capabilities: &Capabilities) -> std::result::Result<(), Refusal> {
        if capabilities.billing == Billing::Real && !self.config.allow_paid_providers {
            return Err(Refusal::PaidProvisioningDisabled {
                provider: capabilities.provider.clone(),
            });
        }
        if !capabilities.provider_side_expiry {
            return Err(Refusal::NoProviderSideExpiry {
                provider: capabilities.provider.clone(),
            });
        }
        Ok(())
    }

    /// Refuses unless the provider-side janitor is installed and succeeded
    /// recently. Without it, a lost controller would leave resources running
    /// with nothing to remove them.
    fn janitor_gate(&self, now: UnixMillis) -> std::result::Result<(), Refusal> {
        let provider = self.provider_name.clone();
        let status = match self.provider.janitor_status() {
            Ok(status) => status,
            Err(error) => {
                return Err(Refusal::JanitorStatusUnavailable {
                    provider,
                    detail: error.to_string(),
                });
            }
        };
        if !status.installed {
            return Err(Refusal::JanitorNotInstalled { provider });
        }
        let Some(last_success_at) = status.last_success_at else {
            return Err(Refusal::JanitorNeverSucceeded { provider });
        };
        let age_secs = now.since(last_success_at).as_secs();
        if age_secs > self.config.janitor_max_staleness_secs {
            return Err(Refusal::JanitorStale {
                provider,
                age_secs,
                max_age_secs: self.config.janitor_max_staleness_secs,
            });
        }
        Ok(())
    }

    /// Records that capacity is wanted. Creates an intention in `Requested`
    /// and nothing else: no hold, no provider resource.
    ///
    /// Refused with [`CloudError::Refused`], writing nothing, when:
    ///
    /// * the source is LAN-only, whatever the priority or escalation;
    /// * the project's or the job's cloud mode does not permit the request
    ///   (`ExistingOnly` permits none, `Manual` no automatic ones,
    ///   `AutoOverflow` no manual ones);
    /// * a manual request comes from a viewer;
    /// * the provider bills real money and paid provisioning is not enabled,
    ///   or it has no provider-side expiry;
    /// * the provider does not offer the preset, region or purchase model;
    /// * the provider-side janitor is missing, has never succeeded, is stale
    ///   or cannot be asked;
    /// * no fleet allowance or no project allowance exists.
    ///
    /// Repeating a request with the same identifier returns the existing
    /// intention; the same identifier with different content is a conflict.
    pub fn request_capacity(&self, request: &NewCapacity) -> Result<Intent> {
        let now = self.clock.now();
        if request.source_lan_only {
            return Err(Refusal::SourceLanOnly.into());
        }
        check_mode(request.project_cloud, request.mode)?;
        if let Some(job_cloud) = request.job_cloud {
            check_mode(job_cloud, request.mode)?;
        }
        if request.mode == RequestMode::Manual && request.requested_by.role < Role::Developer {
            return Err(Refusal::Forbidden.into());
        }
        if request.lifetime_secs == 0 {
            return Err(CloudError::Invalid(
                "a capacity request needs a lifetime".to_string(),
            ));
        }
        let capabilities = self.provider.capabilities();
        self.paid_gate(&capabilities)?;
        let Some(preset) = capabilities.preset(&request.preset) else {
            return Err(Refusal::UnsupportedPreset {
                provider: capabilities.provider.clone(),
                preset: request.preset.clone(),
            }
            .into());
        };
        if !capabilities.regions.contains(&request.region) {
            return Err(Refusal::UnsupportedRegion {
                provider: capabilities.provider.clone(),
                region: request.region.clone(),
            }
            .into());
        }
        if !preset.purchase.contains(&request.purchase) {
            return Err(Refusal::UnsupportedPurchase {
                preset: request.preset.clone(),
                purchase: request.purchase,
            }
            .into());
        }
        self.janitor_gate(now)?;

        let wanted_expiry = now
            .plus_secs(self.config.startup_secs)
            .plus_secs(request.lifetime_secs);
        self.tx(|tx| {
            if let Some(existing) = intent::find_intent(tx, &request.id)? {
                return if same_request(&existing, request) {
                    Ok(existing)
                } else {
                    Err(CloudError::Conflict(format!(
                        "intention {} already exists with different content",
                        request.id
                    )))
                };
            }
            let applicable = money::allowances_for(tx, &request.project)?;
            for scope in [ScopeKind::Fleet, ScopeKind::Project] {
                if !applicable.iter().any(|a| a.scope.kind() == scope) {
                    return Err(Refusal::NoAllowance { scope }.into());
                }
            }
            let (chain, expires_at) = match &request.retry_of {
                None => (request.id.clone(), wanted_expiry),
                Some(prior_id) => {
                    let prior = intent::get_intent(tx, prior_id)?;
                    self.check_retry(tx, &prior, request, now)?;
                    (prior.chain.clone(), wanted_expiry.min(prior.expires_at))
                }
            };
            intent::insert_intent(
                tx,
                &NewIntent {
                    id: request.id.clone(),
                    provider: self.provider_name.clone(),
                    fleet: self.fleet.clone(),
                    project: request.project.clone(),
                    job: request.job.clone(),
                    covers: request.covers.clone(),
                    chain,
                    retry_of: request.retry_of.clone(),
                    preset: request.preset.clone(),
                    region: request.region.clone(),
                    purchase: request.purchase,
                    image: request.image.clone(),
                    lifetime_secs: request.lifetime_secs,
                    expires_at,
                    requested_by: request.requested_by.principal.clone(),
                    mode: request.mode,
                    priority: request.priority,
                    escalated: request.escalated,
                },
                now,
            )
        })
    }

    fn check_retry(
        &self,
        tx: &Transaction<'_>,
        prior: &Intent,
        request: &NewCapacity,
        now: UnixMillis,
    ) -> Result<()> {
        let refuse =
            |detail: String| -> Result<()> { Err(Refusal::RetryNotPermitted { detail }.into()) };
        if prior.state != IntentState::Failed {
            return refuse(format!(
                "intention {} is {}, only a failed intention is retried",
                prior.id, prior.state
            ));
        }
        if prior.project != request.project {
            return refuse("the retry is for a different project".to_string());
        }
        if prior.purchase != request.purchase {
            return refuse(format!(
                "the failed intention was {}; a retry keeps the purchase model, a different one is a new request",
                prior.purchase
            ));
        }
        let used = intent::chain_attempts(tx, &prior.chain)?;
        let allowed = money::max_attempts_for(tx, &prior.project)?;
        if used >= allowed {
            return refuse(format!(
                "{used} of {allowed} permitted create attempts are used"
            ));
        }
        let remaining_secs = prior.expires_at.0.saturating_sub(now.0) / 1000;
        let required_secs = self
            .config
            .startup_secs
            .saturating_add(self.config.min_retry_lifetime_secs);
        if remaining_secs < required_secs {
            return refuse(format!(
                "{remaining_secs}s remain before the original expiry, {required_secs}s are needed"
            ));
        }
        Ok(())
    }

    /// The provider's current price for an intention.
    pub fn quote(&self, id: &CloudIntentId) -> Result<Quote> {
        let current = self.intent(id)?;
        Ok(self.provider.quote(
            &current.preset,
            &current.region,
            current.purchase,
            current.lifetime_secs,
        )?)
    }

    /// Takes the budget hold and moves the intention to `Authorized`, in one
    /// transaction. See [`money::authorize`].
    pub fn authorize(&self, id: &CloudIntentId, quote: &Quote) -> Result<Hold> {
        let now = self.clock.now();
        let config = self.config.money();
        self.tx(|tx| money::authorize(tx, id, quote, &config, now))
    }

    fn create_request(&self, current: &Intent, bootstrap: &str) -> CreateRequest {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_FLEET.to_string(), self.fleet.as_str().to_string());
        labels.insert(LABEL_INTENT.to_string(), current.id.as_str().to_string());
        CreateRequest {
            intent_id: current.id.clone(),
            fleet_id: self.fleet.clone(),
            preset: current.preset.clone(),
            region: current.region.clone(),
            purchase: current.purchase,
            image: current.image.clone(),
            expires_at: current.expires_at,
            labels,
            bootstrap: bootstrap.to_string(),
        }
    }

    /// Records that a create call is about to be issued and returns the call
    /// to issue.
    ///
    /// Only an `Authorized` intention gets a call. In any later state the
    /// answer is `Done(NoAction)`, so a repeated provision never issues a
    /// second create. The janitor is checked again here; a `Requested`
    /// intention is an [`CloudError::IllegalTransition`].
    pub fn begin_provision(&self, id: &CloudIntentId, bootstrap: &str) -> Result<ProvisionStep> {
        let now = self.clock.now();
        if bootstrap.is_empty() {
            return Err(CloudError::Invalid(
                "a bootstrap string is required".to_string(),
            ));
        }
        let before = self.intent(id)?;
        match before.state {
            IntentState::Authorized => {}
            IntentState::Requested => {
                return Err(CloudError::IllegalTransition {
                    from: before.state,
                    to: IntentState::Provisioning,
                });
            }
            state => return Ok(ProvisionStep::Done(ProvisionOutcome::NoAction { state })),
        }
        self.paid_gate(&self.provider.capabilities())?;
        self.janitor_gate(now)?;
        let hash = Digest::of_bytes(bootstrap.as_bytes()).as_str().to_string();
        self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            if current.state != IntentState::Authorized {
                return Ok(ProvisionStep::Done(ProvisionOutcome::NoAction {
                    state: current.state,
                }));
            }
            if current.expired(now) {
                let reason = "expired before provisioning";
                intent::transition(tx, id, IntentState::Terminated, reason, now)?;
                settle_if_clean(tx, id, now)?;
                return Ok(ProvisionStep::Done(ProvisionOutcome::NoAction {
                    state: IntentState::Terminated,
                }));
            }
            let hold = money::hold(tx, id)?
                .filter(|hold| hold.released_at.is_none())
                .ok_or_else(|| {
                    CloudError::Corrupt(format!("intention {id} is authorized without a hold"))
                })?;
            if current.create_attempts >= hold.attempts_allowed {
                let reason = "every permitted create attempt was used".to_string();
                intent::fail(tx, id, FailureKind::AttemptsExhausted, &reason, now)?;
                settle_if_clean(tx, id, now)?;
                return Ok(ProvisionStep::Done(ProvisionOutcome::Failed {
                    kind: FailureKind::AttemptsExhausted,
                    reason,
                    on_demand_permitted: false,
                }));
            }
            if let Some(other) = intent::find_by_bootstrap_hash(tx, &hash)?
                && other.id != *id
            {
                return Err(CloudError::Invalid(
                    "the bootstrap string already belongs to another intention".to_string(),
                ));
            }
            intent::record_create_attempt(tx, id, &hash, now)?;
            intent::transition(tx, id, IntentState::Provisioning, "create call issued", now)?;
            Ok(ProvisionStep::Call(Box::new(
                self.create_request(&current, bootstrap),
            )))
        })
    }

    /// Records the result of the create call issued for
    /// [`Engine::begin_provision`].
    pub fn finish_provision(
        &self,
        id: &CloudIntentId,
        result: std::result::Result<CreateAck, ProviderError>,
    ) -> Result<ProvisionOutcome> {
        let now = self.clock.now();
        self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            let awaiting = matches!(
                current.state,
                IntentState::Provisioning | IntentState::Unknown
            );
            match &result {
                Ok(ack) => {
                    for resource in &ack.resources {
                        intent::upsert_resource(tx, &self.provider_name, id, resource, false, now)?;
                    }
                    let instances = ack
                        .resources
                        .iter()
                        .filter(|r| r.kind == ResourceKind::Instance)
                        .count();
                    if instances == 1 {
                        intent::set_create_resolved(tx, id)?;
                    }
                    if !awaiting {
                        return Ok(ProvisionOutcome::NoAction {
                            state: current.state,
                        });
                    }
                    if instances == 1 {
                        intent::transition(
                            tx,
                            id,
                            IntentState::Bootstrapping,
                            "instance created",
                            now,
                        )?;
                        return Ok(ProvisionOutcome::Created);
                    }
                    let reason =
                        format!("the provider acknowledged the create with {instances} instances");
                    if current.state == IntentState::Provisioning {
                        intent::transition(tx, id, IntentState::Unknown, &reason, now)?;
                    }
                    Ok(ProvisionOutcome::Unknown { reason })
                }
                Err(error) if error.outcome_uncertain() => {
                    let reason = format!("the create call's outcome is not known: {error}");
                    if current.state == IntentState::Provisioning {
                        intent::transition(tx, id, IntentState::Unknown, &reason, now)?;
                    }
                    Ok(ProvisionOutcome::Unknown { reason })
                }
                Err(error) => {
                    if !awaiting {
                        return Ok(ProvisionOutcome::NoAction {
                            state: current.state,
                        });
                    }
                    // A definite refusal: the provider created nothing.
                    intent::set_create_resolved(tx, id)?;
                    self.record_refused_create(tx, &current, error, now)
                }
            }
        })
    }

    fn record_refused_create(
        &self,
        tx: &Transaction<'_>,
        current: &Intent,
        error: &ProviderError,
        now: UnixMillis,
    ) -> Result<ProvisionOutcome> {
        let id = &current.id;
        let spot = current.purchase == PurchaseModel::Spot;
        let mut on_demand_permitted = false;
        let (kind, reason) = match error {
            ProviderError::CapacityUnavailable if spot => {
                on_demand_permitted =
                    money::purchase_permitted(tx, &current.project, PurchaseModel::OnDemand)?;
                if !on_demand_permitted {
                    // Spot is all the allowance permits: wait for spot.
                    let reason = "no spot capacity is available; spot is the only purchase model the allowance permits, so the request stays blocked and was not converted".to_string();
                    intent::transition(tx, id, IntentState::Authorized, &reason, now)?;
                    return Ok(ProvisionOutcome::Blocked { reason });
                }
                (
                    FailureKind::SpotCapacityUnavailable,
                    "no spot capacity is available; the request was not converted to on-demand, which needs its own explicit request and authorization".to_string(),
                )
            }
            ProviderError::CapacityUnavailable => (
                FailureKind::CapacityUnavailable,
                "the provider has no capacity of the requested kind".to_string(),
            ),
            ProviderError::QuotaExceeded => (
                FailureKind::QuotaExceeded,
                "a provider quota would be exceeded".to_string(),
            ),
            other => (
                FailureKind::Rejected,
                format!("the provider refused the create call: {other}"),
            ),
        };
        intent::fail(tx, id, kind, &reason, now)?;
        settle_if_clean(tx, id, now)?;
        Ok(ProvisionOutcome::Failed {
            kind,
            reason,
            on_demand_permitted,
        })
    }

    /// Creates the capacity of an `Authorized` intention.
    ///
    /// The intention is moved to `Provisioning` before the provider is
    /// called, with the intention identifier as the idempotency token. A
    /// timeout or an unavailable provider leaves it `Unknown` with its hold;
    /// calling `provision` again in that state issues no create. Spot
    /// capacity that is unavailable is never replaced with on-demand.
    ///
    /// `bootstrap` is the one-use enrollment string for the new worker. Only
    /// its SHA-256 is stored.
    pub fn provision(&self, id: &CloudIntentId, bootstrap: &str) -> Result<ProvisionOutcome> {
        match self.begin_provision(id, bootstrap)? {
            ProvisionStep::Done(outcome) => Ok(outcome),
            ProvisionStep::Call(request) => {
                let result = self.provider.create(&request);
                self.finish_provision(id, result)
            }
        }
    }

    /// Brings the tracked resources of an intention in line with a provider
    /// listing: records what is listed and marks as gone what is missing,
    /// when the listing can be trusted about it.
    ///
    /// A missing resource is trusted to be gone when it appeared in an
    /// earlier listing, or when the settling period has passed since it was
    /// first recorded. Until then a listing that lags behind creation could
    /// hide a resource that exists.
    fn sync_listing(
        &self,
        tx: &Transaction<'_>,
        current: &Intent,
        listing: &[OwnedResource],
        now: UnixMillis,
    ) -> Result<()> {
        let id = &current.id;
        for resource in listing {
            intent::upsert_resource(tx, &self.provider_name, id, resource, true, now)?;
        }
        if current.create_uncertain
            && listing
                .iter()
                .any(|resource| resource.kind == ResourceKind::Instance)
        {
            intent::set_create_resolved(tx, id)?;
        }
        let listed: BTreeSet<&str> = listing.iter().map(|r| r.id.as_str()).collect();
        for tracked in intent::live_resources(tx, id)? {
            if listed.contains(tracked.resource_id.as_str()) {
                continue;
            }
            let trusted = tracked.listed
                || now.since(tracked.first_seen_at).as_secs() >= self.config.settling_secs;
            if !trusted {
                continue;
            }
            // Past the expiry the provider's own cleanup is the likely
            // cause, so the resource is charged up to the expiry at most.
            let expiry = tracked.expires_at.unwrap_or(current.expires_at);
            let gone_at = if now > expiry {
                expiry.max(tracked.last_seen_at)
            } else {
                now
            };
            intent::mark_resource_gone(tx, &self.provider_name, &tracked.resource_id, gone_at)?;
        }
        Ok(())
    }

    /// Closes the question of what an unobserved create call did.
    ///
    /// When the answer could only be sought after the expiry, the
    /// provider's own cleanup may have removed the evidence, so the hold is
    /// charged as if one set of resources ran from the call to the expiry.
    fn resolve_unobserved(
        &self,
        tx: &Transaction<'_>,
        current: &Intent,
        now: UnixMillis,
    ) -> Result<()> {
        if !current.create_uncertain {
            return Ok(());
        }
        if current.expired(now)
            && !current.resources_seen
            && let Some(called_at) = current.last_create_at
            && let Some(hold) = money::hold(tx, &current.id)?
        {
            let millis = current.expires_at.0.saturating_sub(called_at.0);
            let micros =
                prorate_up(hold.hourly_micros, millis).ok_or(CloudError::AmountOverflow)?;
            money::add_assumed_charge(
                tx,
                &current.id,
                micros,
                "the create call's outcome could only be checked after the expiry, when the provider's own cleanup may already have removed what it created; charged as if one set of resources ran from the call to the expiry",
                now,
            )?;
        }
        intent::set_create_resolved(tx, &current.id)
    }

    /// Establishes what a create call with an unknown outcome did.
    ///
    /// Asks the provider which resources carry the intention's token:
    ///
    /// * exactly one instance: it is adopted and the intention continues;
    /// * more than one: the intention is quarantined;
    /// * none, within the settling period: nothing is concluded;
    /// * none, after the settling period: the provider has positively
    ///   reported that nothing exists, and the intention returns to
    ///   `Authorized` so `provision` can retry with the same token, or fails
    ///   when its attempts are used up.
    ///
    /// The hold is kept throughout. A provider that cannot be asked changes
    /// nothing.
    pub fn reconcile(&self, id: &CloudIntentId) -> Result<ReconcileOutcome> {
        let now = self.clock.now();
        let before = self.intent(id)?;
        match before.state {
            IntentState::Unknown => {}
            IntentState::Provisioning => {
                let since = before.last_create_at.unwrap_or(before.state_since);
                if now.since(since).as_secs() < self.config.settling_secs {
                    return Ok(ReconcileOutcome::StillUnknown {
                        reason: "the create call may still be in flight".to_string(),
                    });
                }
                self.tx(|tx| {
                    let current = intent::get_intent(tx, id)?;
                    if current.state == IntentState::Provisioning {
                        intent::transition(
                            tx,
                            id,
                            IntentState::Unknown,
                            "the outcome of the create call was never recorded",
                            now,
                        )?;
                    }
                    Ok(())
                })?;
            }
            state => return Ok(ReconcileOutcome::NotUncertain { state }),
        }
        let listing = match self.owned_by(id) {
            Ok(listing) => listing,
            Err(error) => {
                return Ok(ReconcileOutcome::StillUnknown {
                    reason: format!("the provider could not be asked: {error}"),
                });
            }
        };
        let outcome = self.tx(|tx| self.apply_reconcile(tx, id, &listing, now))?;
        self.finish_closing(id)?;
        Ok(outcome)
    }

    fn apply_reconcile(
        &self,
        tx: &Transaction<'_>,
        id: &CloudIntentId,
        listing: &[OwnedResource],
        now: UnixMillis,
    ) -> Result<ReconcileOutcome> {
        let current = intent::get_intent(tx, id)?;
        if current.state != IntentState::Unknown {
            return Ok(ReconcileOutcome::NotUncertain {
                state: current.state,
            });
        }
        self.sync_listing(tx, &current, listing, now)?;
        let closing = current.cancel_requested_at.is_some() || current.expired(now);
        let instances: Vec<&OwnedResource> = listing
            .iter()
            .filter(|resource| resource.kind == ResourceKind::Instance)
            .collect();
        if instances.len() > 1 {
            let reason = format!(
                "the provider reports {} instances for one idempotency token",
                instances.len()
            );
            intent::transition(tx, id, IntentState::Quarantined, &reason, now)?;
            return Ok(ReconcileOutcome::Quarantined {
                instances: instances.len(),
            });
        }
        if let Some(instance) = instances.first() {
            if closing {
                intent::transition(
                    tx,
                    id,
                    IntentState::Terminating,
                    "found after cancellation or expiry",
                    now,
                )?;
                return Ok(ReconcileOutcome::TerminationScheduled);
            }
            intent::transition(
                tx,
                id,
                IntentState::Bootstrapping,
                "adopted the one instance the provider reports for this token",
                now,
            )?;
            return Ok(ReconcileOutcome::Adopted {
                resource_id: instance.id.clone(),
            });
        }

        let settled = current
            .last_create_at
            .is_none_or(|at| now.since(at).as_secs() >= self.config.settling_secs);
        if !settled {
            return Ok(ReconcileOutcome::StillUnknown {
                reason: "the provider reports no instance yet; that is not trusted until the settling period has passed".to_string(),
            });
        }
        if !intent::live_resources(tx, id)?.is_empty() {
            intent::set_create_resolved(tx, id)?;
            intent::fail(
                tx,
                id,
                FailureKind::PartialCreation,
                "the provider created secondary resources but no instance",
                now,
            )?;
            return Ok(ReconcileOutcome::Failed {
                kind: FailureKind::PartialCreation,
            });
        }

        // The provider positively reports nothing after the settling period.
        self.resolve_unobserved(tx, &current, now)?;
        if closing {
            intent::transition(
                tx,
                id,
                IntentState::Terminating,
                "nothing exists; closing after cancellation or expiry",
                now,
            )?;
            return Ok(ReconcileOutcome::TerminationScheduled);
        }
        let attempts_allowed = money::hold(tx, id)?
            .map(|hold| hold.attempts_allowed)
            .unwrap_or(0);
        if current.create_attempts < attempts_allowed {
            intent::transition(
                tx,
                id,
                IntentState::Authorized,
                "the provider reports nothing after the settling period; a retry with the same token is permitted",
                now,
            )?;
            return Ok(ReconcileOutcome::RetryPermitted {
                attempts_used: current.create_attempts,
                attempts_allowed,
            });
        }
        intent::fail(
            tx,
            id,
            FailureKind::AttemptsExhausted,
            "the provider created nothing and every permitted create attempt was used",
            now,
        )?;
        settle_if_clean(tx, id, now)?;
        Ok(ReconcileOutcome::Failed {
            kind: FailureKind::AttemptsExhausted,
        })
    }

    /// Answers a worker that presents its bootstrap string.
    ///
    /// Admits it once, when its intention is `Bootstrapping`, not cancelled
    /// and not expired; the intention becomes `Ready` and the string is used
    /// up. Anything else is denied. A worker whose intention was cancelled,
    /// has expired or is closed is denied and its resources are scheduled
    /// for deletion: the intention is moved to `Terminating` in the same
    /// transaction when it was still `Bootstrapping`, and the next
    /// [`Engine::sweep`] or [`Engine::terminate`] deletes them.
    pub fn on_worker_registered(&self, bootstrap_token: &str) -> Result<Registration> {
        let now = self.clock.now();
        let hash = Digest::of_bytes(bootstrap_token.as_bytes())
            .as_str()
            .to_string();
        self.tx(|tx| {
            let found = intent::find_by_bootstrap_hash(tx, &hash)?
                .filter(|found| found.provider == self.provider_name);
            let Some(current) = found else {
                return Ok(Registration::Deny {
                    reason: DenyReason::UnknownToken,
                    intent: None,
                    deletion_scheduled: false,
                });
            };
            let id = current.id.clone();
            let deny = |reason: DenyReason, deletion_scheduled: bool| Registration::Deny {
                reason,
                intent: Some(id.clone()),
                deletion_scheduled,
            };
            if current.bootstrap_used_at.is_some() {
                return Ok(deny(DenyReason::TokenAlreadyUsed, false));
            }
            let cancelled = current.cancel_requested_at.is_some();
            let expired = current.expired(now);
            let closed_reason = if cancelled {
                DenyReason::Cancelled
            } else if expired {
                DenyReason::Expired
            } else {
                DenyReason::Closed
            };
            match current.state {
                IntentState::Bootstrapping if cancelled || expired => {
                    intent::transition(
                        tx,
                        &id,
                        IntentState::Terminating,
                        "the worker registered after cancellation or expiry",
                        now,
                    )?;
                    Ok(deny(closed_reason, true))
                }
                IntentState::Bootstrapping => {
                    intent::mark_bootstrap_used(tx, &id, now)?;
                    intent::transition(tx, &id, IntentState::Ready, "worker registered", now)?;
                    Ok(Registration::Admit { intent: id.clone() })
                }
                IntentState::Terminating | IntentState::Terminated | IntentState::Failed => {
                    Ok(deny(closed_reason, true))
                }
                IntentState::Requested
                | IntentState::Authorized
                | IntentState::Provisioning
                | IntentState::Unknown => Ok(deny(DenyReason::ProviderStateUncertain, false)),
                IntentState::Quarantined => Ok(deny(DenyReason::Quarantined, false)),
                IntentState::Ready | IntentState::Busy | IntentState::Draining => {
                    Ok(deny(DenyReason::TokenAlreadyUsed, false))
                }
            }
        })
    }

    /// Moves a worker between its working states. Only the listed source
    /// states are accepted, so these calls cannot stand in for a
    /// registration or skip a cleanup.
    fn move_worker(
        &self,
        id: &CloudIntentId,
        from: &[IntentState],
        to: IntentState,
        reason: &str,
    ) -> Result<Intent> {
        let now = self.clock.now();
        self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            if current.state == to {
                return Ok(current);
            }
            if !from.contains(&current.state) {
                return Err(CloudError::IllegalTransition {
                    from: current.state,
                    to,
                });
            }
            intent::transition(tx, id, to, reason, now)
        })
    }

    /// `Ready → Busy`: the worker took a job.
    pub fn mark_busy(&self, id: &CloudIntentId) -> Result<Intent> {
        self.move_worker(
            id,
            &[IntentState::Ready],
            IntentState::Busy,
            "worker took a job",
        )
    }

    /// `Busy → Ready`: the worker is idle again.
    pub fn mark_idle(&self, id: &CloudIntentId) -> Result<Intent> {
        self.move_worker(
            id,
            &[IntentState::Busy],
            IntentState::Ready,
            "worker is idle",
        )
    }

    /// `Ready`/`Busy → Draining`: no new work. Call [`Engine::terminate`]
    /// when the current work has ended.
    pub fn drain(&self, id: &CloudIntentId) -> Result<Intent> {
        self.move_worker(
            id,
            &[IntentState::Ready, IntentState::Busy],
            IntentState::Draining,
            "draining",
        )
    }

    /// Records a cancellation and terminates the intention.
    ///
    /// The cancellation is durable before anything is deleted, so a worker
    /// that registers afterwards is denied even if deletion has not finished.
    pub fn cancel(&self, id: &CloudIntentId, reason: &str) -> Result<Termination> {
        let now = self.clock.now();
        self.tx(|tx| intent::mark_cancel_requested(tx, id, reason, now))?;
        self.terminate(id, &format!("cancelled: {reason}"))
    }

    /// Deletes everything the intention owns and closes it.
    ///
    /// An intention that never issued a create call is closed at once.
    /// Otherwise it moves to `Terminating`, each resource is deleted, a lost
    /// delete response is resolved by listing again, and the intention
    /// reaches `Terminated` and releases its hold only when the provider
    /// reports that nothing remains. A secondary resource that survives, or
    /// a host still under its minimum commitment, keeps the intention in
    /// `Terminating` with its hold; the returned [`Termination`] lists what
    /// remains. Safe to repeat.
    pub fn terminate(&self, id: &CloudIntentId, reason: &str) -> Result<Termination> {
        let now = self.clock.now();
        let needs_cleanup = self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            match current.state {
                IntentState::Requested | IntentState::Authorized => {
                    intent::transition(tx, id, IntentState::Terminated, reason, now)?;
                    settle_if_clean(tx, id, now)?;
                    Ok(false)
                }
                IntentState::Terminated => Ok(false),
                IntentState::Failed | IntentState::Terminating => Ok(current.settled_at.is_none()),
                _ => {
                    intent::transition(tx, id, IntentState::Terminating, reason, now)?;
                    Ok(true)
                }
            }
        })?;
        if needs_cleanup {
            self.cleanup(id)
        } else {
            self.termination(id, Vec::new())
        }
    }

    fn termination(&self, id: &CloudIntentId, notes: Vec<String>) -> Result<Termination> {
        self.tx(|tx| {
            Ok(Termination {
                intent: intent::get_intent(tx, id)?,
                remaining: intent::live_resources(tx, id)?,
                notes: notes.clone(),
            })
        })
    }

    /// Runs the deletion procedure when the intention is closing.
    fn finish_closing(&self, id: &CloudIntentId) -> Result<Vec<String>> {
        let current = self.intent(id)?;
        let closing = matches!(
            current.state,
            IntentState::Terminating | IntentState::Failed
        );
        if closing && current.settled_at.is_none() {
            return Ok(self.cleanup(id)?.notes);
        }
        Ok(Vec::new())
    }

    /// Deletes what a `Terminating` or `Failed` intention still owns and
    /// settles it when nothing remains.
    fn cleanup(&self, id: &CloudIntentId) -> Result<Termination> {
        let now = self.clock.now();
        let before = self.intent(id)?;
        if before.settled_at.is_some() {
            return self.termination(id, Vec::new());
        }
        if !matches!(before.state, IntentState::Terminating | IntentState::Failed) {
            return Err(CloudError::Invalid(format!(
                "intention {id} is {} and is not being closed",
                before.state
            )));
        }
        let listing = match self.owned_by(id) {
            Ok(listing) => listing,
            Err(error) => {
                let note = format!("the provider could not list the resources of {id}: {error}");
                return self.termination(id, vec![note]);
            }
        };
        // Record what is about to be deleted before any delete is issued.
        let mut doomed = self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            self.sync_listing(tx, &current, &listing, now)?;
            let live = intent::live_resources(tx, id)?;
            for resource in &live {
                if deletable(resource, now) {
                    intent::mark_delete_requested(
                        tx,
                        &self.provider_name,
                        &resource.resource_id,
                        now,
                    )?;
                }
            }
            Ok(live)
        })?;
        doomed.sort_by(|a, b| {
            (delete_order(a.kind), &a.resource_id).cmp(&(delete_order(b.kind), &b.resource_id))
        });

        let mut notes = Vec::new();
        for resource in &doomed {
            if !deletable(resource, now) {
                notes.push(format!(
                    "{} {} is under a minimum commitment and cannot be released yet",
                    resource.kind, resource.resource_id
                ));
                continue;
            }
            match self.provider.delete(&resource.resource_id) {
                Ok(()) => self.tx(|tx| {
                    intent::mark_resource_gone(tx, &self.provider_name, &resource.resource_id, now)
                })?,
                Err(ProviderError::Timeout) => notes.push(format!(
                    "the response to deleting {} {} was lost; the listing decides",
                    resource.kind, resource.resource_id
                )),
                Err(error) => notes.push(format!(
                    "deleting {} {} failed: {error}",
                    resource.kind, resource.resource_id
                )),
            }
        }

        // The listing, not the delete responses, proves that nothing remains.
        let listing = match self.owned_by(id) {
            Ok(listing) => listing,
            Err(error) => {
                notes.push(format!(
                    "the provider could not confirm the deletions for {id}: {error}"
                ));
                return self.termination(id, notes);
            }
        };
        self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            self.sync_listing(tx, &current, &listing, now)?;
            let current = intent::get_intent(tx, id)?;
            let live = intent::live_resources(tx, id)?;
            let create_settled = current
                .last_create_at
                .is_none_or(|at| now.since(at).as_secs() >= self.config.settling_secs);
            let nothing_remains = live.is_empty()
                && listing.is_empty()
                && (!current.create_uncertain || create_settled);
            if nothing_remains && current.settled_at.is_none() {
                self.resolve_unobserved(tx, &current, now)?;
                if current.state == IntentState::Terminating {
                    intent::transition(
                        tx,
                        id,
                        IntentState::Terminated,
                        "the provider reports that no resource remains",
                        now,
                    )?;
                }
                settle_if_clean(tx, id, now)?;
            }
            Ok(())
        })?;
        self.termination(id, notes)
    }

    /// Checks a working intention against the provider. Returns whether its
    /// instance was lost.
    fn observe(&self, seen: &Intent, now: UnixMillis, problems: &mut Vec<String>) -> Result<bool> {
        let id = &seen.id;
        let listing = match self.owned_by(id) {
            Ok(listing) => listing,
            Err(error) => {
                problems.push(format!("{id}: the provider could not be asked: {error}"));
                return Ok(false);
            }
        };
        self.tx(|tx| {
            let current = intent::get_intent(tx, id)?;
            if current.state != seen.state {
                return Ok(false);
            }
            self.sync_listing(tx, &current, &listing, now)?;
            let instances = listing
                .iter()
                .filter(|resource| resource.kind == ResourceKind::Instance)
                .count();
            if instances > 1 {
                let reason =
                    format!("the provider reports {instances} instances for one idempotency token");
                intent::transition(tx, id, IntentState::Quarantined, &reason, now)?;
                return Ok(false);
            }
            let has_instance = intent::live_resources(tx, id)?
                .iter()
                .any(|resource| resource.kind == ResourceKind::Instance);
            if !has_instance {
                let reason = if current.purchase == PurchaseModel::Spot {
                    "infrastructure interruption: the provider withdrew the spot instance without notice"
                } else {
                    "infrastructure interruption: the instance disappeared from the provider"
                };
                intent::fail(
                    tx,
                    id,
                    FailureKind::InfrastructureInterruption,
                    reason,
                    now,
                )?;
                settle_if_clean(tx, id, now)?;
                return Ok(true);
            }
            if current.state == IntentState::Bootstrapping
                && now.since(current.state_since).as_secs() >= self.config.bootstrap_timeout_secs
            {
                intent::fail(
                    tx,
                    id,
                    FailureKind::BootstrapTimeout,
                    "the worker did not register in time",
                    now,
                )?;
            }
            Ok(false)
        })
    }

    /// Whether a failed intention may be replaced, by attempts, time and budget.
    ///
    /// * Attempts: create calls across the retry chain against the smallest
    ///   `max_attempts` of the allowances that apply.
    /// * Time: a replacement inherits the original expiry, so enough of it
    ///   must be left for a startup and a useful run.
    /// * Budget: a fresh quote for the remaining lifetime must pass the
    ///   same admission check an authorization would run.
    ///
    /// The verdict is advice. A replacement is a new intention that goes
    /// through `request_capacity` and `authorize` like any other.
    pub fn retry_verdict(&self, id: &CloudIntentId) -> Result<RetryVerdict> {
        let now = self.clock.now();
        let failed = self.intent(id)?;
        let (attempts_used, attempts_allowed) = self.tx(|tx| {
            Ok((
                intent::chain_attempts(tx, &failed.chain)?,
                money::max_attempts_for(tx, &failed.project)?,
            ))
        })?;
        let mut blockers = Vec::new();
        if attempts_used >= attempts_allowed {
            blockers.push(RetryBlocker::AttemptsExhausted {
                used: attempts_used,
                allowed: attempts_allowed,
            });
        }
        let remaining_secs = failed.expires_at.0.saturating_sub(now.0) / 1000;
        let remaining_lifetime_secs = remaining_secs.saturating_sub(self.config.startup_secs);
        if remaining_lifetime_secs < self.config.min_retry_lifetime_secs {
            blockers.push(RetryBlocker::InsufficientTime {
                remaining_secs,
                required_secs: self
                    .config
                    .startup_secs
                    .saturating_add(self.config.min_retry_lifetime_secs),
            });
        } else if blockers.is_empty() {
            match self.provider.quote(
                &failed.preset,
                &failed.region,
                failed.purchase,
                remaining_lifetime_secs,
            ) {
                Err(error) => blockers.push(RetryBlocker::QuoteUnavailable {
                    detail: error.to_string(),
                }),
                Ok(quote) => {
                    let config = self.config.money();
                    let checked = self.tx(|tx| {
                        money::check_admission(
                            tx,
                            &AdmissionRequest {
                                project: &failed.project,
                                preset: &failed.preset,
                                region: &failed.region,
                                purchase: failed.purchase,
                                lifetime_secs: remaining_lifetime_secs,
                                quote: &quote,
                                attempts_used,
                            },
                            &config,
                            now,
                        )
                    });
                    match checked {
                        Ok(_) => {}
                        Err(CloudError::Denied(denial)) => {
                            blockers.push(RetryBlocker::Budget { denial: *denial });
                        }
                        Err(other) => return Err(other),
                    }
                }
            }
        }
        Ok(RetryVerdict {
            permitted: blockers.is_empty(),
            attempts_used,
            attempts_allowed,
            remaining_lifetime_secs,
            blockers,
        })
    }

    fn interruption(&self, id: &CloudIntentId) -> Result<Interruption> {
        let failed = self.intent(id)?;
        let retry = self.retry_verdict(id)?;
        Ok(Interruption {
            intent: failed.id,
            job: failed.job,
            covers: failed.covers,
            outcome: Outcome::InfraInterrupted {
                detail: failed.reason.unwrap_or_default(),
            },
            promote_partial_result: false,
            retry,
        })
    }

    fn sweep_one(
        &self,
        seen: &Intent,
        now: UnixMillis,
        problems: &mut Vec<String>,
    ) -> Result<Option<Interruption>> {
        let id = &seen.id;
        let expired = seen.expired(now);
        match seen.state {
            IntentState::Requested | IntentState::Authorized => {
                if expired {
                    self.terminate(id, "expired before provisioning")?;
                }
            }
            IntentState::Provisioning | IntentState::Unknown => {
                if let ReconcileOutcome::StillUnknown { reason } = self.reconcile(id)? {
                    problems.push(format!("{id}: {reason}"));
                }
            }
            IntentState::Bootstrapping
            | IntentState::Ready
            | IntentState::Busy
            | IntentState::Draining => {
                if expired || seen.cancel_requested_at.is_some() {
                    let reason = if expired { "expired" } else { "cancelled" };
                    problems.extend(self.terminate(id, reason)?.notes);
                    return Ok(None);
                }
                let interrupted = self.observe(seen, now, problems)?;
                problems.extend(self.finish_closing(id)?);
                if interrupted {
                    return Ok(Some(self.interruption(id)?));
                }
            }
            IntentState::Quarantined => {
                if expired {
                    problems.extend(self.terminate(id, "expired while quarantined")?.notes);
                } else {
                    problems.push(format!(
                        "{id}: quarantined and needs a person: {}",
                        seen.reason.clone().unwrap_or_default()
                    ));
                }
            }
            IntentState::Terminating | IntentState::Failed => {
                problems.extend(self.cleanup(id)?.notes);
            }
            IntentState::Terminated => {}
        }
        Ok(None)
    }

    fn sweep_orphans(
        &self,
        now: UnixMillis,
        orphans: &mut Vec<OrphanReport>,
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let owned = match self.provider.list_owned(&self.fleet) {
            Ok(owned) => owned,
            Err(error) => {
                problems.push(format!(
                    "the provider could not list the fleet's resources: {error}"
                ));
                return Ok(());
            }
        };
        let mut listed = BTreeSet::new();
        for resource in &owned {
            listed.insert(resource.id.clone());
            let known = match &resource.intent_id {
                Some(intent_id) => self.tx(|tx| intent::find_intent(tx, intent_id))?,
                None => None,
            };
            if known
                .as_ref()
                .is_some_and(|known| known.settled_at.is_none())
            {
                // An open intention accounts for it.
                continue;
            }
            let ownership_proven = self.carries_fleet_label(resource);
            let mut note = match &known {
                Some(known) => format!("belongs to settled intention {}", known.id),
                None => "no intention records this resource".to_string(),
            };
            self.tx(|tx| {
                intent::record_orphan(
                    tx,
                    &self.provider_name,
                    resource,
                    ownership_proven,
                    &note,
                    now,
                )
            })?;
            let mut deleted = false;
            if ownership_proven {
                match self.provider.delete(&resource.id) {
                    Ok(()) => {
                        self.tx(|tx| {
                            intent::mark_orphan_deleted(tx, &self.provider_name, &resource.id, now)
                        })?;
                        deleted = true;
                    }
                    Err(error) => note = format!("{note}; deleting it failed: {error}"),
                }
            } else {
                note = format!("{note}; not deleted because it does not carry this fleet's label");
            }
            orphans.push(OrphanReport {
                resource_id: resource.id.clone(),
                kind: resource.kind,
                hourly_micros: resource.hourly_micros,
                intent_label: resource.labels.get(LABEL_INTENT).cloned(),
                ownership_proven,
                deleted,
                note,
            });
        }
        // An orphan the provider no longer lists is gone.
        self.tx(|tx| {
            for orphan in intent::open_orphans(tx, &self.provider_name)? {
                if !listed.contains(&orphan.resource_id) {
                    intent::mark_orphan_deleted(tx, &self.provider_name, &orphan.resource_id, now)?;
                }
            }
            Ok(())
        })
    }

    /// One reconciliation pass over everything that is not settled.
    ///
    /// * An intention past its expiry is terminated.
    /// * A create call with no recorded outcome is reconciled.
    /// * A working intention whose instance is gone becomes `Failed` with an
    ///   infrastructure-interruption reason, reported with a retry verdict.
    /// * A worker that did not register in time fails its intention.
    /// * A closing intention has its remaining resources deleted and is
    ///   settled once the provider reports none.
    /// * A provider resource of this fleet that no open intention accounts
    ///   for is reported, and deleted only when it carries this fleet's
    ///   label.
    ///
    /// A new engine on the same database picks up exactly where a dead one
    /// stopped. A provider that cannot be asked changes nothing; the sweep
    /// reports it and the next one tries again.
    pub fn sweep(&self) -> Result<SweepReport> {
        let now = self.clock.now();
        let mut changes = Vec::new();
        let mut interruptions = Vec::new();
        let mut orphans = Vec::new();
        let mut problems = Vec::new();
        let open = self.tx(|tx| intent::list_unsettled(tx, &self.provider_name))?;
        for seen in &open {
            if let Some(interruption) = self.sweep_one(seen, now, &mut problems)? {
                interruptions.push(interruption);
            }
            let after = self.intent(&seen.id)?;
            if after.state != seen.state {
                changes.push(StateChange {
                    intent: seen.id.clone(),
                    from: seen.state,
                    to: after.state,
                });
            }
        }
        self.sweep_orphans(now, &mut orphans, &mut problems)?;
        Ok(SweepReport {
            at: now,
            changes,
            interruptions,
            orphans,
            problems,
            exposure: self.exposure()?,
        })
    }

    /// Everything that still costs or may cost money: every unsettled
    /// intention with a hold or a resource, and every open orphan.
    pub fn exposure(&self) -> Result<Exposure> {
        let now = self.clock.now();
        self.tx(|tx| exposure(tx, &self.provider_name, now))
    }

    /// The overflow policy's view of this provider: capacity that is
    /// pending or active, in creation order, and launches since
    /// `history_since`.
    ///
    /// Free capacity is the whole preset for capacity that is pending or
    /// idle and nothing for a busy worker; a caller that knows the worker's
    /// real reservations can refine it.
    pub fn overflow_view(
        &self,
        history_since: UnixMillis,
    ) -> Result<(Vec<PendingOrActive>, LaunchHistory)> {
        let capabilities = self.provider.capabilities();
        let intents = self.tx(|tx| intent::list_intents(tx, &self.provider_name))?;
        let mut capacity = Vec::new();
        let mut launches = Vec::new();
        for current in &intents {
            let Some(preset) = capabilities.preset(&current.preset) else {
                continue;
            };
            if let Some(at) = current.last_create_at
                && at >= history_since
            {
                let outcome = match (current.state, current.failure) {
                    (IntentState::Failed, Some(FailureKind::BootstrapTimeout)) => {
                        LaunchOutcome::BootstrapFailed {
                            at: current.state_since,
                        }
                    }
                    (IntentState::Terminated, _) => LaunchOutcome::Released {
                        at: current.state_since,
                    },
                    _ => LaunchOutcome::Launched,
                };
                launches.push(LaunchRecord {
                    provider: self.provider_name.clone(),
                    platform: preset.platform,
                    preset: current.preset.clone(),
                    at,
                    outcome,
                });
            }
            if current.settled_at.is_some() {
                continue;
            }
            let phase = match current.state {
                IntentState::Requested
                | IntentState::Authorized
                | IntentState::Provisioning
                | IntentState::Unknown
                | IntentState::Bootstrapping => CapacityPhase::Pending,
                IntentState::Ready | IntentState::Busy => CapacityPhase::Active,
                IntentState::Draining
                | IntentState::Terminating
                | IntentState::Quarantined
                | IntentState::Failed
                | IntentState::Terminated => CapacityPhase::Draining,
            };
            let free = match (phase, current.state) {
                (CapacityPhase::Pending, _) | (_, IntentState::Ready) => preset.resources.clone(),
                _ => ResourceVector::default(),
            };
            let mut covers = current.covers.clone();
            if let Some(job) = &current.job
                && !covers.contains(job)
            {
                covers.push(job.clone());
            }
            capacity.push(PendingOrActive {
                intent: current.id.clone(),
                provider: self.provider_name.clone(),
                project: current.project.clone(),
                platform: preset.platform,
                phase,
                free,
                covers,
                since: current.created_at,
                idle_since: (current.state == IntentState::Ready).then_some(current.state_since),
            });
        }
        Ok((capacity, LaunchHistory { launches }))
    }
}
