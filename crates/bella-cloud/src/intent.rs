//! Durable intentions: the record that capacity was wanted, what became of
//! it, and which provider resources belong to it.
//!
//! An intention is written before the provider is asked for anything, so a
//! controller that dies at any point restarts with a record of what it may
//! have created. Every state change writes an event row in the same
//! transaction.

use crate::convert::{
    col_json, col_ms, col_opt_ms, col_opt_parsed, col_parsed, col_u32, col_u64, json_text, ms,
    opt_ms, sql_i64,
};
use crate::error::{CloudError, Result};
use crate::provider::{OwnedResource, PurchaseModel, ResourceKind, ResourceState};
use bella_core::job::Priority;
use bella_core::{CloudIntentId, FleetId, JobId, PrincipalId, ProjectId, UnixMillis};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

string_enum! {
    /// Where an intention is in its life.
    ///
    /// ```text
    /// Requested → Authorized → Provisioning → Bootstrapping → Ready ⇄ Busy
    ///                               │                           │
    ///                               └→ Unknown        Draining ←┘
    ///                                                    │
    ///                              Terminating → Terminated
    /// ```
    ///
    /// `Failed` and `Terminated` are final. `Failed` says the capacity never
    /// became or stopped being usable; its hold is still kept until no
    /// provider resource remains.
    pub enum IntentState {
        /// Recorded; no budget is held.
        Requested => "requested",
        /// A budget hold exists; the provider has not been asked.
        Authorized => "authorized",
        /// A create call was issued and its outcome is not yet recorded.
        Provisioning => "provisioning",
        /// The instance exists; its worker has not registered.
        Bootstrapping => "bootstrapping",
        /// The worker registered and is idle.
        Ready => "ready",
        /// The worker is running a job.
        Busy => "busy",
        /// No new work; waiting for the current work to end.
        Draining => "draining",
        /// Deletion was requested; provider resources may still exist.
        Terminating => "terminating",
        /// No provider resource remains and the hold is released.
        Terminated => "terminated",
        /// The capacity could not be created or was lost.
        Failed => "failed",
        /// A create call may or may not have created something.
        Unknown => "unknown",
        /// The provider state contradicts the record and needs a person.
        Quarantined => "quarantined",
    }
}

impl IntentState {
    /// Whether the state machine allows moving from `self` to `next`.
    pub fn can_transition_to(self, next: IntentState) -> bool {
        use IntentState::*;
        matches!(
            (self, next),
            (Requested, Authorized | Terminated | Failed)
                | (Authorized, Provisioning | Terminated | Failed)
                | (
                    Provisioning,
                    Bootstrapping | Unknown | Authorized | Failed | Terminating
                )
                | (
                    Unknown,
                    Bootstrapping | Authorized | Failed | Terminating | Quarantined
                )
                | (Bootstrapping, Ready | Failed | Terminating | Quarantined)
                | (Ready, Busy | Draining | Terminating | Failed | Quarantined)
                | (Busy, Ready | Draining | Terminating | Failed | Quarantined)
                | (Draining, Terminating | Failed | Quarantined)
                | (Terminating, Terminated)
                | (Quarantined, Terminating)
        )
    }

    /// `Terminated` or `Failed`: the state will not change again.
    pub fn is_final(self) -> bool {
        matches!(self, IntentState::Terminated | IntentState::Failed)
    }

    /// Authorized but not yet usable.
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            IntentState::Authorized
                | IntentState::Provisioning
                | IntentState::Unknown
                | IntentState::Bootstrapping
        )
    }

    /// A registered worker exists for the intention.
    pub fn has_worker(self) -> bool {
        matches!(
            self,
            IntentState::Ready | IntentState::Busy | IntentState::Draining
        )
    }
}

string_enum! {
    /// Who asked for the capacity.
    pub enum RequestMode {
        /// A person authorised it for one job.
        Manual => "manual",
        /// The standing overflow policy asked.
        Automatic => "automatic",
    }
}

string_enum! {
    /// Why an intention is `Failed`, in a form code can act on.
    pub enum FailureKind {
        /// A spot request found no spot capacity. It was not converted.
        SpotCapacityUnavailable => "spot_capacity_unavailable",
        CapacityUnavailable => "capacity_unavailable",
        QuotaExceeded => "quota_exceeded",
        /// The provider refused the create call.
        Rejected => "rejected",
        /// Every permitted create attempt was used.
        AttemptsExhausted => "attempts_exhausted",
        /// The worker never registered.
        BootstrapTimeout => "bootstrap_timeout",
        /// The instance disappeared: a withdrawn spot instance or an
        /// infrastructure fault.
        InfrastructureInterruption => "infrastructure_interruption",
        /// The provider created secondary resources but no instance.
        PartialCreation => "partial_creation",
    }
}

/// One durable intention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// Also the provider idempotency token for every create call.
    pub id: CloudIntentId,
    pub provider: String,
    pub fleet: FleetId,
    pub project: ProjectId,
    /// The job the capacity was requested for, when there is one.
    pub job: Option<JobId>,
    /// Every job the overflow policy counted on this capacity for.
    pub covers: Vec<JobId>,
    /// The first intention of the retry chain this one belongs to.
    pub chain: CloudIntentId,
    pub retry_of: Option<CloudIntentId>,
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub image: String,
    pub lifetime_secs: u64,
    /// Absolute end of the intention. Resources are created with this
    /// expiry and nothing extends it.
    pub expires_at: UnixMillis,
    pub requested_by: PrincipalId,
    pub mode: RequestMode,
    pub priority: Priority,
    pub escalated: bool,
    pub state: IntentState,
    pub state_since: UnixMillis,
    pub reason: Option<String>,
    pub failure: Option<FailureKind>,
    /// SHA-256 of the one-use bootstrap string. The string is never stored.
    pub bootstrap_token_hash: Option<String>,
    pub bootstrap_used_at: Option<UnixMillis>,
    /// Create calls issued for this intention.
    pub create_attempts: u32,
    pub last_create_at: Option<UnixMillis>,
    /// A create call was issued and nothing has yet proven what it did.
    pub create_uncertain: bool,
    /// At least one provider resource was observed for this intention.
    pub resources_seen: bool,
    pub cancel_requested_at: Option<UnixMillis>,
    /// Set once no provider resource remains and the hold is released.
    pub settled_at: Option<UnixMillis>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
}

impl Intent {
    /// Whether the intention's absolute expiry has passed.
    pub fn expired(&self, now: UnixMillis) -> bool {
        now >= self.expires_at
    }
}

/// Fields of a new intention. It always starts in `Requested`.
#[derive(Clone, Debug)]
pub struct NewIntent {
    pub id: CloudIntentId,
    pub provider: String,
    pub fleet: FleetId,
    pub project: ProjectId,
    pub job: Option<JobId>,
    pub covers: Vec<JobId>,
    pub chain: CloudIntentId,
    pub retry_of: Option<CloudIntentId>,
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub image: String,
    pub lifetime_secs: u64,
    pub expires_at: UnixMillis,
    pub requested_by: PrincipalId,
    pub mode: RequestMode,
    pub priority: Priority,
    pub escalated: bool,
}

/// One row of an intention's history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IntentEvent {
    pub seq: u64,
    /// `None` for fleet-level events such as an allowance change.
    pub intent: Option<CloudIntentId>,
    pub at: UnixMillis,
    pub kind: String,
    pub from: Option<IntentState>,
    pub to: Option<IntentState>,
    pub detail: serde_json::Value,
}

/// A provider resource as the engine tracks it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedResource {
    pub resource_id: String,
    pub intent: CloudIntentId,
    pub kind: ResourceKind,
    pub state: ResourceState,
    pub hourly_micros: u64,
    /// Provider-reported creation time; cost accrues from here.
    pub created_at: UnixMillis,
    pub first_seen_at: UnixMillis,
    pub last_seen_at: UnixMillis,
    /// The resource appeared in a provider listing at least once, so a
    /// later listing without it proves it is gone.
    pub listed: bool,
    pub expires_at: Option<UnixMillis>,
    pub releasable_at: Option<UnixMillis>,
    pub delete_requested_at: Option<UnixMillis>,
    /// When the engine established that the resource no longer exists.
    pub gone_at: Option<UnixMillis>,
}

/// A provider resource that no open intention accounts for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orphan {
    pub resource_id: String,
    pub kind: ResourceKind,
    pub hourly_micros: u64,
    pub intent_label: Option<String>,
    /// The resource carries this fleet's label. Only such orphans are deleted.
    pub ownership_proven: bool,
    pub note: String,
    pub first_seen_at: UnixMillis,
    pub last_seen_at: UnixMillis,
    pub deleted_at: Option<UnixMillis>,
}

const INTENT_COLUMNS: &str = "id, provider, fleet, project, job, covers, chain, retry_of, preset, \
     region, purchase, image, lifetime_secs, expires_at, requested_by, mode, priority, escalated, \
     state, state_since, reason, failure, bootstrap_hash, bootstrap_used_at, create_attempts, \
     last_create_at, create_uncertain, resources_seen, cancel_requested_at, settled_at, \
     created_at, updated_at";

fn intent_from_row(row: &Row<'_>) -> rusqlite::Result<Intent> {
    Ok(Intent {
        id: col_parsed(row, 0, |s| CloudIntentId::parse(s).ok())?,
        provider: row.get(1)?,
        fleet: col_parsed(row, 2, |s| FleetId::parse(s).ok())?,
        project: col_parsed(row, 3, |s| ProjectId::parse(s).ok())?,
        job: col_opt_parsed(row, 4, |s| JobId::parse(s).ok())?,
        covers: col_json(row, 5)?,
        chain: col_parsed(row, 6, |s| CloudIntentId::parse(s).ok())?,
        retry_of: col_opt_parsed(row, 7, |s| CloudIntentId::parse(s).ok())?,
        preset: row.get(8)?,
        region: row.get(9)?,
        purchase: col_parsed(row, 10, PurchaseModel::parse)?,
        image: row.get(11)?,
        lifetime_secs: col_u64(row, 12)?,
        expires_at: col_ms(row, 13)?,
        requested_by: col_parsed(row, 14, |s| PrincipalId::parse(s).ok())?,
        mode: col_parsed(row, 15, RequestMode::parse)?,
        priority: col_json(row, 16)?,
        escalated: row.get(17)?,
        state: col_parsed(row, 18, IntentState::parse)?,
        state_since: col_ms(row, 19)?,
        reason: row.get(20)?,
        failure: col_opt_parsed(row, 21, FailureKind::parse)?,
        bootstrap_token_hash: row.get(22)?,
        bootstrap_used_at: col_opt_ms(row, 23)?,
        create_attempts: col_u32(row, 24)?,
        last_create_at: col_opt_ms(row, 25)?,
        create_uncertain: row.get(26)?,
        resources_seen: row.get(27)?,
        cancel_requested_at: col_opt_ms(row, 28)?,
        settled_at: col_opt_ms(row, 29)?,
        created_at: col_ms(row, 30)?,
        updated_at: col_ms(row, 31)?,
    })
}

/// Writes a new intention in `Requested` and its first event.
pub fn insert_intent(tx: &Transaction<'_>, new: &NewIntent, now: UnixMillis) -> Result<Intent> {
    tx.execute(
        "INSERT INTO cloud_intents (id, provider, fleet, project, job, covers, chain, retry_of, \
         preset, region, purchase, image, lifetime_secs, expires_at, requested_by, mode, \
         priority, escalated, state, state_since, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
         ?18, ?19, ?20, ?20, ?20)",
        params![
            new.id.as_str(),
            new.provider,
            new.fleet.as_str(),
            new.project.as_str(),
            new.job.as_ref().map(JobId::as_str),
            json_text(&new.covers)?,
            new.chain.as_str(),
            new.retry_of.as_ref().map(CloudIntentId::as_str),
            new.preset,
            new.region,
            new.purchase.as_str(),
            new.image,
            sql_i64(new.lifetime_secs)?,
            ms(new.expires_at)?,
            new.requested_by.as_str(),
            new.mode.as_str(),
            json_text(&new.priority)?,
            new.escalated,
            IntentState::Requested.as_str(),
            ms(now)?,
        ],
    )?;
    insert_event(
        tx,
        Some(&new.id),
        "requested",
        None,
        Some(IntentState::Requested),
        &serde_json::json!({
            "project": new.project,
            "job": new.job,
            "preset": new.preset,
            "region": new.region,
            "purchase": new.purchase,
            "mode": new.mode,
            "requested_by": new.requested_by,
            "retry_of": new.retry_of,
        }),
        now,
    )?;
    get_intent(tx, &new.id)
}

/// The intention, or `None` when it does not exist.
pub fn find_intent(conn: &Connection, id: &CloudIntentId) -> Result<Option<Intent>> {
    let sql = format!("SELECT {INTENT_COLUMNS} FROM cloud_intents WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id.as_str()], intent_from_row)
        .optional()?)
}

/// The intention, or [`CloudError::NotFound`].
pub fn get_intent(conn: &Connection, id: &CloudIntentId) -> Result<Intent> {
    find_intent(conn, id)?.ok_or_else(|| CloudError::NotFound(id.clone()))
}

/// The intention whose bootstrap string hashes to `hash`.
pub fn find_by_bootstrap_hash(conn: &Connection, hash: &str) -> Result<Option<Intent>> {
    let sql = format!("SELECT {INTENT_COLUMNS} FROM cloud_intents WHERE bootstrap_hash = ?1");
    Ok(conn.query_row(&sql, [hash], intent_from_row).optional()?)
}

fn query_intents(conn: &Connection, tail: &str, provider: &str) -> Result<Vec<Intent>> {
    let sql = format!("SELECT {INTENT_COLUMNS} FROM cloud_intents WHERE provider = ?1 {tail}");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([provider], intent_from_row)?;
    let mut intents = Vec::new();
    for row in rows {
        intents.push(row?);
    }
    Ok(intents)
}

/// Every intention of `provider`, in creation order.
pub fn list_intents(conn: &Connection, provider: &str) -> Result<Vec<Intent>> {
    query_intents(conn, "ORDER BY rowid", provider)
}

/// Every intention of `provider` that is not settled, in creation order.
/// These are the ones a sweep has to look at.
pub fn list_unsettled(conn: &Connection, provider: &str) -> Result<Vec<Intent>> {
    query_intents(conn, "AND settled_at IS NULL ORDER BY rowid", provider)
}

/// Create calls issued across a whole retry chain.
pub fn chain_attempts(conn: &Connection, chain: &CloudIntentId) -> Result<u32> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(create_attempts), 0) FROM cloud_intents WHERE chain = ?1",
        [chain.as_str()],
        |row| row.get(0),
    )?;
    u32::try_from(total).map_err(|_| CloudError::Corrupt("attempt count out of range".into()))
}

/// Moves an intention to `to`, refusing a transition the state machine does
/// not allow, and writes the event in the same transaction.
///
/// `Terminated` is additionally refused while any tracked resource of the
/// intention is not known to be gone.
pub fn transition(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    to: IntentState,
    reason: &str,
    now: UnixMillis,
) -> Result<Intent> {
    let current = get_intent(tx, id)?;
    if !current.state.can_transition_to(to) {
        return Err(CloudError::IllegalTransition {
            from: current.state,
            to,
        });
    }
    if to == IntentState::Terminated && !live_resources(tx, id)?.is_empty() {
        return Err(CloudError::ResourcesRemain(id.clone()));
    }
    tx.execute(
        "UPDATE cloud_intents SET state = ?2, state_since = ?3, reason = ?4, updated_at = ?3 \
         WHERE id = ?1",
        params![id.as_str(), to.as_str(), ms(now)?, reason],
    )?;
    insert_event(
        tx,
        Some(id),
        "transition",
        Some(current.state),
        Some(to),
        &serde_json::json!({ "reason": reason }),
        now,
    )?;
    get_intent(tx, id)
}

/// Moves an intention to `Failed` and records why in machine-readable form.
pub fn fail(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    kind: FailureKind,
    reason: &str,
    now: UnixMillis,
) -> Result<Intent> {
    transition(tx, id, IntentState::Failed, reason, now)?;
    tx.execute(
        "UPDATE cloud_intents SET failure = ?2 WHERE id = ?1",
        params![id.as_str(), kind.as_str()],
    )?;
    get_intent(tx, id)
}

/// Records that a create call is about to be issued: the attempt counter,
/// the time, the bootstrap hash and the fact that its outcome is unknown.
pub fn record_create_attempt(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    bootstrap_hash: &str,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "UPDATE cloud_intents SET create_attempts = create_attempts + 1, last_create_at = ?2, \
         create_uncertain = 1, bootstrap_hash = ?3, bootstrap_used_at = NULL, updated_at = ?2 \
         WHERE id = ?1",
        params![id.as_str(), ms(now)?, bootstrap_hash],
    )?;
    Ok(())
}

/// Records that the outcome of the last create call is now known.
pub fn set_create_resolved(tx: &Transaction<'_>, id: &CloudIntentId) -> Result<()> {
    tx.execute(
        "UPDATE cloud_intents SET create_uncertain = 0 WHERE id = ?1",
        [id.as_str()],
    )?;
    Ok(())
}

/// Records that the bootstrap string was presented and accepted.
pub fn mark_bootstrap_used(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "UPDATE cloud_intents SET bootstrap_used_at = ?2, updated_at = ?2 WHERE id = ?1",
        params![id.as_str(), ms(now)?],
    )?;
    Ok(())
}

/// Records a cancellation request. The first request wins; repeating it
/// changes nothing.
pub fn mark_cancel_requested(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    reason: &str,
    now: UnixMillis,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE cloud_intents SET cancel_requested_at = ?2, updated_at = ?2 \
         WHERE id = ?1 AND cancel_requested_at IS NULL",
        params![id.as_str(), ms(now)?],
    )?;
    if changed > 0 {
        insert_event(
            tx,
            Some(id),
            "cancel_requested",
            None,
            None,
            &serde_json::json!({ "reason": reason }),
            now,
        )?;
    }
    Ok(())
}

pub(crate) fn mark_settled(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "UPDATE cloud_intents SET settled_at = ?2, updated_at = ?2 \
         WHERE id = ?1 AND settled_at IS NULL",
        params![id.as_str(), ms(now)?],
    )?;
    Ok(())
}

/// Appends an event row.
pub fn insert_event(
    tx: &Transaction<'_>,
    intent: Option<&CloudIntentId>,
    kind: &str,
    from: Option<IntentState>,
    to: Option<IntentState>,
    detail: &serde_json::Value,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "INSERT INTO cloud_events (intent, at, kind, from_state, to_state, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            intent.map(CloudIntentId::as_str),
            ms(now)?,
            kind,
            from.map(IntentState::as_str),
            to.map(IntentState::as_str),
            detail.to_string(),
        ],
    )?;
    Ok(())
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<IntentEvent> {
    Ok(IntentEvent {
        seq: col_u64(row, 0)?,
        intent: col_opt_parsed(row, 1, |s| CloudIntentId::parse(s).ok())?,
        at: col_ms(row, 2)?,
        kind: row.get(3)?,
        from: col_opt_parsed(row, 4, IntentState::parse)?,
        to: col_opt_parsed(row, 5, IntentState::parse)?,
        detail: col_json(row, 6)?,
    })
}

/// The history of one intention, oldest first.
pub fn events(conn: &Connection, id: &CloudIntentId) -> Result<Vec<IntentEvent>> {
    let mut statement = conn.prepare(
        "SELECT seq, intent, at, kind, from_state, to_state, detail FROM cloud_events \
         WHERE intent = ?1 ORDER BY seq",
    )?;
    let rows = statement.query_map([id.as_str()], event_from_row)?;
    let mut events = Vec::new();
    for row in rows {
        events.push(row?);
    }
    Ok(events)
}

/// Every event after `after_seq`, oldest first, at most `limit` rows.
pub fn events_after(conn: &Connection, after_seq: u64, limit: u32) -> Result<Vec<IntentEvent>> {
    let mut statement = conn.prepare(
        "SELECT seq, intent, at, kind, from_state, to_state, detail FROM cloud_events \
         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![sql_i64(after_seq)?, i64::from(limit)],
        event_from_row,
    )?;
    let mut events = Vec::new();
    for row in rows {
        events.push(row?);
    }
    Ok(events)
}

const RESOURCE_COLUMNS: &str = "resource_id, intent, kind, state, hourly_micros, created_at, \
     first_seen_at, last_seen_at, listed, expires_at, releasable_at, delete_requested_at, gone_at";

fn resource_from_row(row: &Row<'_>) -> rusqlite::Result<TrackedResource> {
    Ok(TrackedResource {
        resource_id: row.get(0)?,
        intent: col_parsed(row, 1, |s| CloudIntentId::parse(s).ok())?,
        kind: col_parsed(row, 2, ResourceKind::parse)?,
        state: col_parsed(row, 3, ResourceState::parse)?,
        hourly_micros: col_u64(row, 4)?,
        created_at: col_ms(row, 5)?,
        first_seen_at: col_ms(row, 6)?,
        last_seen_at: col_ms(row, 7)?,
        listed: row.get(8)?,
        expires_at: col_opt_ms(row, 9)?,
        releasable_at: col_opt_ms(row, 10)?,
        delete_requested_at: col_opt_ms(row, 11)?,
        gone_at: col_opt_ms(row, 12)?,
    })
}

/// Records a resource the provider reported for `intent`, or refreshes it.
///
/// `listed` says the report came from a listing rather than a create
/// acknowledgement. A resource that reappears after being marked gone
/// becomes live again, so it is deleted again rather than forgotten.
pub fn upsert_resource(
    tx: &Transaction<'_>,
    provider: &str,
    intent: &CloudIntentId,
    resource: &OwnedResource,
    listed: bool,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "INSERT INTO cloud_resources (provider, resource_id, intent, kind, state, hourly_micros, \
         created_at, first_seen_at, last_seen_at, listed, expires_at, releasable_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11) \
         ON CONFLICT(provider, resource_id) DO UPDATE SET \
             state = excluded.state, \
             hourly_micros = excluded.hourly_micros, \
             last_seen_at = excluded.last_seen_at, \
             listed = MAX(listed, excluded.listed), \
             expires_at = excluded.expires_at, \
             releasable_at = excluded.releasable_at, \
             gone_at = NULL",
        params![
            provider,
            resource.id,
            intent.as_str(),
            resource.kind.as_str(),
            resource.state.as_str(),
            sql_i64(resource.hourly_micros)?,
            ms(resource.created_at)?,
            ms(now)?,
            listed,
            opt_ms(resource.expires_at)?,
            opt_ms(resource.releasable_at)?,
        ],
    )?;
    tx.execute(
        "UPDATE cloud_intents SET resources_seen = 1 WHERE id = ?1 AND resources_seen = 0",
        [intent.as_str()],
    )?;
    Ok(())
}

/// Records that a delete call is about to be issued for a resource.
pub fn mark_delete_requested(
    tx: &Transaction<'_>,
    provider: &str,
    resource_id: &str,
    now: UnixMillis,
) -> Result<()> {
    tx.execute(
        "UPDATE cloud_resources SET delete_requested_at = COALESCE(delete_requested_at, ?3) \
         WHERE provider = ?1 AND resource_id = ?2",
        params![provider, resource_id, ms(now)?],
    )?;
    Ok(())
}

/// Records that a resource no longer exists, as of `at`.
pub fn mark_resource_gone(
    tx: &Transaction<'_>,
    provider: &str,
    resource_id: &str,
    at: UnixMillis,
) -> Result<()> {
    tx.execute(
        "UPDATE cloud_resources SET gone_at = ?3 \
         WHERE provider = ?1 AND resource_id = ?2 AND gone_at IS NULL",
        params![provider, resource_id, ms(at)?],
    )?;
    Ok(())
}

fn query_resources(
    conn: &Connection,
    id: &CloudIntentId,
    tail: &str,
) -> Result<Vec<TrackedResource>> {
    let sql = format!(
        "SELECT {RESOURCE_COLUMNS} FROM cloud_resources WHERE intent = ?1 {tail} \
         ORDER BY resource_id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([id.as_str()], resource_from_row)?;
    let mut resources = Vec::new();
    for row in rows {
        resources.push(row?);
    }
    Ok(resources)
}

/// Every resource ever tracked for the intention, including removed ones.
pub fn resources(conn: &Connection, id: &CloudIntentId) -> Result<Vec<TrackedResource>> {
    query_resources(conn, id, "")
}

/// The intention's resources that are not known to be gone.
pub fn live_resources(conn: &Connection, id: &CloudIntentId) -> Result<Vec<TrackedResource>> {
    query_resources(conn, id, "AND gone_at IS NULL")
}

/// Records a provider resource that no open intention accounts for.
pub fn record_orphan(
    tx: &Transaction<'_>,
    provider: &str,
    resource: &OwnedResource,
    ownership_proven: bool,
    note: &str,
    now: UnixMillis,
) -> Result<()> {
    let intent_label = resource
        .labels
        .get(crate::provider::LABEL_INTENT)
        .map(String::as_str);
    let inserted = tx.execute(
        "INSERT INTO cloud_orphans (provider, resource_id, kind, hourly_micros, intent_label, \
         ownership_proven, note, first_seen_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
         ON CONFLICT(provider, resource_id) DO NOTHING",
        params![
            provider,
            resource.id,
            resource.kind.as_str(),
            sql_i64(resource.hourly_micros)?,
            intent_label,
            ownership_proven,
            note,
            ms(now)?,
        ],
    )?;
    if inserted > 0 {
        insert_event(
            tx,
            None,
            "orphan_found",
            None,
            None,
            &serde_json::json!({
                "provider": provider,
                "resource": resource.id,
                "kind": resource.kind,
                "ownership_proven": ownership_proven,
                "note": note,
            }),
            now,
        )?;
    } else {
        tx.execute(
            "UPDATE cloud_orphans SET last_seen_at = ?3, ownership_proven = ?4, note = ?5, \
             hourly_micros = ?6, deleted_at = NULL WHERE provider = ?1 AND resource_id = ?2",
            params![
                provider,
                resource.id,
                ms(now)?,
                ownership_proven,
                note,
                sql_i64(resource.hourly_micros)?,
            ],
        )?;
    }
    Ok(())
}

/// Records that an orphan no longer exists.
pub fn mark_orphan_deleted(
    tx: &Transaction<'_>,
    provider: &str,
    resource_id: &str,
    now: UnixMillis,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE cloud_orphans SET deleted_at = ?3 \
         WHERE provider = ?1 AND resource_id = ?2 AND deleted_at IS NULL",
        params![provider, resource_id, ms(now)?],
    )?;
    if changed > 0 {
        insert_event(
            tx,
            None,
            "orphan_gone",
            None,
            None,
            &serde_json::json!({ "provider": provider, "resource": resource_id }),
            now,
        )?;
    }
    Ok(())
}

/// Orphans of `provider` that are not known to be gone.
pub fn open_orphans(conn: &Connection, provider: &str) -> Result<Vec<Orphan>> {
    let mut statement = conn.prepare(
        "SELECT resource_id, kind, hourly_micros, intent_label, ownership_proven, note, \
         first_seen_at, last_seen_at, deleted_at FROM cloud_orphans \
         WHERE provider = ?1 AND deleted_at IS NULL ORDER BY resource_id",
    )?;
    let rows = statement.query_map([provider], |row| {
        Ok(Orphan {
            resource_id: row.get(0)?,
            kind: col_parsed(row, 1, ResourceKind::parse)?,
            hourly_micros: col_u64(row, 2)?,
            intent_label: row.get(3)?,
            ownership_proven: row.get(4)?,
            note: row.get(5)?,
            first_seen_at: col_ms(row, 6)?,
            last_seen_at: col_ms(row, 7)?,
            deleted_at: col_opt_ms(row, 8)?,
        })
    })?;
    let mut orphans = Vec::new();
    for row in rows {
        orphans.push(row?);
    }
    Ok(orphans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_happy_path_is_legal_in_order() {
        use IntentState::*;
        let path = [
            Requested,
            Authorized,
            Provisioning,
            Bootstrapping,
            Ready,
            Busy,
            Draining,
            Terminating,
            Terminated,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{} -> {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_transition_table_is_exactly_the_documented_one() {
        use IntentState::*;
        let allowed: &[(IntentState, &[IntentState])] = &[
            (Requested, &[Authorized, Terminated, Failed]),
            (Authorized, &[Provisioning, Terminated, Failed]),
            (
                Provisioning,
                &[Bootstrapping, Unknown, Authorized, Failed, Terminating],
            ),
            (
                Unknown,
                &[Bootstrapping, Authorized, Failed, Terminating, Quarantined],
            ),
            (Bootstrapping, &[Ready, Failed, Terminating, Quarantined]),
            (Ready, &[Busy, Draining, Terminating, Failed, Quarantined]),
            (Busy, &[Ready, Draining, Terminating, Failed, Quarantined]),
            (Draining, &[Terminating, Failed, Quarantined]),
            (Terminating, &[Terminated]),
            (Terminated, &[]),
            (Failed, &[]),
            (Quarantined, &[Terminating]),
        ];
        assert_eq!(allowed.len(), IntentState::ALL.len());
        for (from, targets) in allowed {
            for to in IntentState::ALL {
                assert_eq!(
                    from.can_transition_to(*to),
                    targets.contains(to),
                    "{from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn final_states_never_move_and_nothing_skips_cleanup() {
        use IntentState::*;
        for to in IntentState::ALL {
            assert!(!Terminated.can_transition_to(*to));
            assert!(!Failed.can_transition_to(*to));
        }
        // A state in which provider resources may exist reaches `Terminated`
        // only through `Terminating`.
        for from in [
            Provisioning,
            Unknown,
            Bootstrapping,
            Ready,
            Busy,
            Draining,
            Quarantined,
        ] {
            assert!(!from.can_transition_to(Terminated), "{from}");
            assert!(
                from.can_transition_to(Terminating),
                "{from} must be terminable"
            );
        }
        // A spot or create failure never turns into a running state.
        assert!(!Failed.can_transition_to(Provisioning));
        assert!(!Unknown.can_transition_to(Provisioning));
    }

    #[test]
    fn text_forms_round_trip() {
        for state in IntentState::ALL {
            assert_eq!(IntentState::parse(state.as_str()), Some(*state));
        }
        assert_eq!(IntentState::parse("running"), None);
        let json = serde_json::to_string(&IntentState::Bootstrapping).unwrap();
        assert_eq!(json, "\"bootstrapping\"");
        assert_eq!(
            serde_json::from_str::<FailureKind>("\"bootstrap_timeout\"").unwrap(),
            FailureKind::BootstrapTimeout
        );
    }
}
