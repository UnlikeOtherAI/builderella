//! The money ledger: allowances, budget holds, accrued cost and reported cost.
//!
//! Every amount is an integer number of micro-units (millionths) of the
//! provider's billing currency. Three numbers are kept apart and never
//! substituted for one another without saying so:
//!
//! * the **admission hold**: what an intention may cost at most, reserved
//!   against the allowance before the provider is asked for anything;
//! * the **accrued estimate**: what the tracked resources have cost so far by
//!   this crate's own arithmetic, with the time and the assumptions it used;
//! * the **provider-reported actual**: what the provider says it billed,
//!   recorded by reconciliation when a figure exists.
//!
//! An allowance exists only after an administrator sets one. Without a
//! fleet allowance and a project allowance nothing can be authorized.

use crate::convert::{
    add, col_json, col_ms, col_opt_ms, col_opt_u64, col_parsed, col_u32, col_u64, json_text, ms,
    prorate_up, sql_i64,
};
use crate::engine::Actor;
use crate::error::{CloudError, Result};
use crate::intent::{
    self, Intent, IntentState, chain_attempts, get_intent, insert_event, live_resources,
};
use crate::provider::{PurchaseModel, Quote};
use bella_core::api::Role;
use bella_core::{CloudIntentId, ProjectId, UnixMillis};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

const MILLIS_PER_DAY: u64 = 86_400_000;

string_enum! {
    /// The level an allowance applies at.
    pub enum ScopeKind {
        Fleet => "fleet",
        Project => "project",
    }
}

/// What an allowance covers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Scope {
    /// Every intention in this database.
    Fleet,
    /// The intentions of one project.
    Project(ProjectId),
}

impl Scope {
    /// The level this scope is at.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Scope::Fleet => ScopeKind::Fleet,
            Scope::Project(_) => ScopeKind::Project,
        }
    }

    /// The project filter used in queries; empty for the fleet.
    fn project_key(&self) -> &str {
        match self {
            Scope::Fleet => "",
            Scope::Project(project) => project.as_str(),
        }
    }
}

string_enum! {
    /// The window settled spend is counted over. Windows are UTC.
    pub enum Period {
        /// Midnight to midnight UTC.
        Daily => "daily",
        /// First of the month to the first of the next, UTC.
        Monthly => "monthly",
    }
}

/// A spending limit set by an administrator.
///
/// Lists are allow-lists: an empty list permits nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowance {
    pub scope: Scope,
    pub period: Period,
    /// Billing currency the limits are expressed in. A quote in another
    /// currency is denied rather than converted.
    pub currency: String,
    /// Most that outstanding holds plus settled spend may reach in a period.
    pub limit_micros: u64,
    /// Most that the combined hourly rate of intentions with an outstanding
    /// hold may reach.
    pub max_hourly_micros: u64,
    /// Most intentions that may have an outstanding hold at once.
    pub max_active: u32,
    /// Most intentions that may be authorized but not yet usable at once.
    pub max_pending: u32,
    pub allowed_presets: Vec<String>,
    pub allowed_regions: Vec<String>,
    pub allowed_purchase: Vec<PurchaseModel>,
    pub max_lifetime_secs: u64,
    /// Create calls permitted for one request, across its retry chain.
    pub max_attempts: u32,
}

/// Settings of the admission arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoneyConfig {
    /// A quote older than this is not accepted.
    pub quote_max_age_secs: u64,
    /// Time a new instance is assumed to bill before it does useful work.
    pub startup_secs: u64,
}

/// The parts of a hold. `total_micros` is their sum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldBreakdown {
    /// Hourly price for the startup time.
    pub startup_micros: u64,
    /// Hourly price for the permitted lifetime.
    pub lifetime_micros: u64,
    /// The provider's minimum billing commitment, in full.
    pub minimum_micros: u64,
    /// For each further permitted create attempt: one startup and one
    /// minimum commitment.
    pub retries_micros: u64,
    pub total_micros: u64,
}

/// The hold for one intention.
///
/// `startup + lifetime × hourly + billing minimum + allowed retries`. The
/// billing minimum is added in full rather than netted against the run: a
/// quote does not say which part of the hourly price the commitment covers,
/// so only the sum is certain to be an upper bound. Returns `None` when the
/// amount does not fit.
pub fn compute_hold(
    hourly_micros: u64,
    minimum_commitment_micros: u64,
    lifetime_secs: u64,
    startup_secs: u64,
    attempts_allowed: u32,
) -> Option<HoldBreakdown> {
    let startup_micros = prorate_up(hourly_micros, startup_secs.checked_mul(1000)?)?;
    let lifetime_micros = prorate_up(hourly_micros, lifetime_secs.checked_mul(1000)?)?;
    let run = startup_micros.checked_add(lifetime_micros)?;
    let minimum_micros = minimum_commitment_micros;
    let retries = u64::from(attempts_allowed.saturating_sub(1));
    let per_retry = startup_micros.checked_add(minimum_commitment_micros)?;
    let retries_micros = per_retry.checked_mul(retries)?;
    let total_micros = run
        .checked_add(minimum_micros)?
        .checked_add(retries_micros)?;
    // The total has to be storable.
    i64::try_from(total_micros).ok()?;
    Some(HoldBreakdown {
        startup_micros,
        lifetime_micros,
        minimum_micros,
        retries_micros,
        total_micros,
    })
}

/// `[start, end)` of the period containing `now`, in UTC.
pub fn period_window(period: Period, now: UnixMillis) -> (UnixMillis, UnixMillis) {
    let day = now.0 / MILLIS_PER_DAY;
    match period {
        Period::Daily => (
            UnixMillis(day * MILLIS_PER_DAY),
            UnixMillis((day + 1).saturating_mul(MILLIS_PER_DAY)),
        ),
        Period::Monthly => {
            let (year, month, _) = civil_from_days(day);
            let (next_year, next_month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            (
                UnixMillis(days_from_civil(year, month, 1) * MILLIS_PER_DAY),
                UnixMillis(
                    days_from_civil(next_year, next_month, 1).saturating_mul(MILLIS_PER_DAY),
                ),
            )
        }
    }
}

/// Civil date of a day count since 1970-01-01 (proleptic Gregorian, UTC).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Day count since 1970-01-01 of a civil date on or after it.
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Why an authorization was denied. No hold was taken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Denial {
    #[error("the quote is {age_secs}s old; quotes older than {max_age_secs}s are not accepted")]
    QuoteStale { age_secs: u64, max_age_secs: u64 },
    #[error("the quote is dated in the future")]
    QuoteFromFuture,
    #[error("the quote is for a different {field} than the intention")]
    QuoteMismatch { field: &'static str },
    #[error("no {scope} allowance exists; an administrator must set one")]
    NoAllowance { scope: ScopeKind },
    #[error("the {scope} allowance is in {allowance} and the quote is in {quote}")]
    CurrencyMismatch {
        scope: ScopeKind,
        allowance: String,
        quote: String,
    },
    #[error("preset `{preset}` is not allowed by the {scope} allowance")]
    PresetNotAllowed { scope: ScopeKind, preset: String },
    #[error("region `{region}` is not allowed by the {scope} allowance")]
    RegionNotAllowed { scope: ScopeKind, region: String },
    #[error("{purchase} capacity is not allowed by the {scope} allowance")]
    PurchaseNotAllowed {
        scope: ScopeKind,
        purchase: PurchaseModel,
    },
    #[error("a lifetime of {requested_secs}s exceeds the {scope} allowance's {max_secs}s")]
    LifetimeTooLong {
        scope: ScopeKind,
        requested_secs: u64,
        max_secs: u64,
    },
    #[error(
        "the combined hourly rate would be {would_be_micros} micro-units; the {scope} allowance permits {max_micros}"
    )]
    HourlyRateExceeded {
        scope: ScopeKind,
        would_be_micros: u64,
        max_micros: u64,
    },
    #[error("{active} intentions already hold budget; the {scope} allowance permits {max}")]
    TooManyActive {
        scope: ScopeKind,
        active: u32,
        max: u32,
    },
    #[error("{pending} intentions are already pending; the {scope} allowance permits {max}")]
    TooManyPending {
        scope: ScopeKind,
        pending: u32,
        max: u32,
    },
    #[error("{used} of {allowed} permitted create attempts are already used")]
    AttemptsExhausted { used: u32, allowed: u32 },
    #[error(
        "the {scope} {period} allowance of {limit_micros} micro-units has {held_micros} held and {spent_micros} spent; {requested_micros} more does not fit"
    )]
    BudgetExceeded {
        scope: ScopeKind,
        period: Period,
        limit_micros: u64,
        held_micros: u64,
        spent_micros: u64,
        requested_micros: u64,
    },
    #[error("the intention has expired")]
    IntentExpired,
    #[error("the amount does not fit the ledger's range")]
    AmountOverflow,
}

/// A budget hold as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub intent: CloudIntentId,
    pub currency: String,
    pub amount_micros: u64,
    pub breakdown: HoldBreakdown,
    /// Quoted hourly price the hold was computed from.
    pub hourly_micros: u64,
    pub minimum_commitment_micros: u64,
    /// Create calls this intention may issue.
    pub attempts_allowed: u32,
    pub quote_priced_at: UnixMillis,
    pub quote_assumptions: Vec<String>,
    pub created_at: UnixMillis,
    /// `None` while the hold counts against the allowance.
    pub released_at: Option<UnixMillis>,
}

/// What the tracked resources have cost so far by this crate's arithmetic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accrued {
    pub micros: u64,
    pub as_of: UnixMillis,
    /// How the figure was reached, in plain words.
    pub assumptions: Vec<String>,
}

/// What the provider reported as billed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActualCost {
    pub micros: u64,
    pub reported_at: UnixMillis,
    /// The provider will not revise the figure.
    pub is_final: bool,
}

/// The three numbers for one intention, side by side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    /// The admission hold; `None` when the intention was never authorized.
    pub hold: Option<Hold>,
    /// The estimate: live while the hold is outstanding, frozen at settlement.
    pub accrued: Accrued,
    /// `None` until reconciliation records a provider figure.
    pub actual: Option<ActualCost>,
}

/// How an allowance stands right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub scope: Scope,
    pub period: Period,
    pub currency: String,
    pub limit_micros: u64,
    /// Outstanding holds. Counted in every period until released; a period
    /// rollover does not remove them.
    pub held_micros: u64,
    /// Settled spend released within the current window.
    pub spent_micros: u64,
    pub available_micros: u64,
    pub window_start: UnixMillis,
    pub window_end: UnixMillis,
    /// Intentions with an outstanding hold.
    pub active: u32,
    /// Intentions authorized but not yet usable.
    pub pending: u32,
    /// Combined quoted hourly rate of intentions with an outstanding hold.
    pub hourly_micros: u64,
}

struct MoneyRow {
    hold: Hold,
    estimate: Option<Accrued>,
    assumed_micros: u64,
    assumed_note: Option<String>,
    actual: Option<ActualCost>,
}

const MONEY_COLUMNS: &str = "intent, currency, amount_micros, startup_micros, lifetime_micros, \
     minimum_micros, retries_micros, hourly_micros, minimum_commitment_micros, attempts_allowed, \
     quote_priced_at, quote_assumptions, created_at, released_at, estimate_micros, estimate_at, \
     estimate_assumptions, assumed_micros, assumed_note, actual_micros, actual_reported_at, \
     actual_final";

fn money_from_row(row: &Row<'_>) -> rusqlite::Result<MoneyRow> {
    let amount_micros = col_u64(row, 2)?;
    let estimate = match (col_opt_u64(row, 14)?, col_opt_ms(row, 15)?) {
        (Some(micros), Some(as_of)) => {
            let text: Option<String> = row.get(16)?;
            let assumptions = match text {
                Some(text) => serde_json::from_str(&text)
                    .map_err(|e| crate::convert::bad_column(16, e.to_string()))?,
                None => Vec::new(),
            };
            Some(Accrued {
                micros,
                as_of,
                assumptions,
            })
        }
        _ => None,
    };
    let actual = match (col_opt_u64(row, 19)?, col_opt_ms(row, 20)?) {
        (Some(micros), Some(reported_at)) => Some(ActualCost {
            micros,
            reported_at,
            is_final: row.get(21)?,
        }),
        _ => None,
    };
    Ok(MoneyRow {
        hold: Hold {
            intent: col_parsed(row, 0, |s| CloudIntentId::parse(s).ok())?,
            currency: row.get(1)?,
            amount_micros,
            breakdown: HoldBreakdown {
                startup_micros: col_u64(row, 3)?,
                lifetime_micros: col_u64(row, 4)?,
                minimum_micros: col_u64(row, 5)?,
                retries_micros: col_u64(row, 6)?,
                total_micros: amount_micros,
            },
            hourly_micros: col_u64(row, 7)?,
            minimum_commitment_micros: col_u64(row, 8)?,
            attempts_allowed: col_u32(row, 9)?,
            quote_priced_at: col_ms(row, 10)?,
            quote_assumptions: col_json(row, 11)?,
            created_at: col_ms(row, 12)?,
            released_at: col_opt_ms(row, 13)?,
        },
        estimate,
        assumed_micros: col_u64(row, 17)?,
        assumed_note: row.get(18)?,
        actual,
    })
}

fn money_row(conn: &Connection, id: &CloudIntentId) -> Result<Option<MoneyRow>> {
    let sql = format!("SELECT {MONEY_COLUMNS} FROM cloud_money WHERE intent = ?1");
    Ok(conn
        .query_row(&sql, [id.as_str()], money_from_row)
        .optional()?)
}

/// The hold of an intention, released or not; `None` when it was never authorized.
pub fn hold(conn: &Connection, id: &CloudIntentId) -> Result<Option<Hold>> {
    Ok(money_row(conn, id)?.map(|row| row.hold))
}

fn require_admin(actor: &Actor, action: &'static str) -> Result<()> {
    if actor.role == Role::Admin {
        Ok(())
    } else {
        Err(CloudError::AdminRequired(action))
    }
}

/// Sets or replaces the allowance for its scope and period.
///
/// Only an administrator may do this; any other role gets
/// [`CloudError::AdminRequired`] and nothing changes. The change is recorded
/// as an event with the principal who made it.
pub fn set_allowance(
    tx: &Transaction<'_>,
    actor: &Actor,
    allowance: &Allowance,
    now: UnixMillis,
) -> Result<()> {
    require_admin(actor, "set a cloud allowance")?;
    if allowance.currency.trim().is_empty() {
        return Err(CloudError::Invalid(
            "an allowance needs a currency".to_string(),
        ));
    }
    tx.execute(
        "INSERT INTO cloud_allowances (scope_kind, scope_id, period, currency, limit_micros, \
         max_hourly_micros, max_active, max_pending, allowed_presets, allowed_regions, \
         allowed_purchase, max_lifetime_secs, max_attempts, set_by, set_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
         ON CONFLICT(scope_kind, scope_id, period) DO UPDATE SET \
             currency = excluded.currency, \
             limit_micros = excluded.limit_micros, \
             max_hourly_micros = excluded.max_hourly_micros, \
             max_active = excluded.max_active, \
             max_pending = excluded.max_pending, \
             allowed_presets = excluded.allowed_presets, \
             allowed_regions = excluded.allowed_regions, \
             allowed_purchase = excluded.allowed_purchase, \
             max_lifetime_secs = excluded.max_lifetime_secs, \
             max_attempts = excluded.max_attempts, \
             set_by = excluded.set_by, \
             set_at = excluded.set_at",
        params![
            allowance.scope.kind().as_str(),
            allowance.scope.project_key(),
            allowance.period.as_str(),
            allowance.currency,
            sql_i64(allowance.limit_micros)?,
            sql_i64(allowance.max_hourly_micros)?,
            i64::from(allowance.max_active),
            i64::from(allowance.max_pending),
            json_text(&allowance.allowed_presets)?,
            json_text(&allowance.allowed_regions)?,
            json_text(&allowance.allowed_purchase)?,
            sql_i64(allowance.max_lifetime_secs)?,
            i64::from(allowance.max_attempts),
            actor.principal.as_str(),
            ms(now)?,
        ],
    )?;
    insert_event(
        tx,
        None,
        "allowance_set",
        None,
        None,
        &serde_json::json!({ "allowance": allowance, "by": actor.principal }),
        now,
    )
}

/// Removes the allowance for a scope and period, which turns provisioning
/// off for that scope when it was the last one. Administrators only.
/// Returns whether an allowance existed.
pub fn remove_allowance(
    tx: &Transaction<'_>,
    actor: &Actor,
    scope: &Scope,
    period: Period,
    now: UnixMillis,
) -> Result<bool> {
    require_admin(actor, "remove a cloud allowance")?;
    let removed = tx.execute(
        "DELETE FROM cloud_allowances WHERE scope_kind = ?1 AND scope_id = ?2 AND period = ?3",
        params![scope.kind().as_str(), scope.project_key(), period.as_str()],
    )?;
    if removed > 0 {
        insert_event(
            tx,
            None,
            "allowance_removed",
            None,
            None,
            &serde_json::json!({ "scope": scope, "period": period, "by": actor.principal }),
            now,
        )?;
    }
    Ok(removed > 0)
}

fn allowance_from_row(row: &Row<'_>) -> rusqlite::Result<Allowance> {
    let kind = col_parsed(row, 0, ScopeKind::parse)?;
    let scope = match kind {
        ScopeKind::Fleet => Scope::Fleet,
        ScopeKind::Project => Scope::Project(col_parsed(row, 1, |s| ProjectId::parse(s).ok())?),
    };
    Ok(Allowance {
        scope,
        period: col_parsed(row, 2, Period::parse)?,
        currency: row.get(3)?,
        limit_micros: col_u64(row, 4)?,
        max_hourly_micros: col_u64(row, 5)?,
        max_active: col_u32(row, 6)?,
        max_pending: col_u32(row, 7)?,
        allowed_presets: col_json(row, 8)?,
        allowed_regions: col_json(row, 9)?,
        allowed_purchase: col_json(row, 10)?,
        max_lifetime_secs: col_u64(row, 11)?,
        max_attempts: col_u32(row, 12)?,
    })
}

const ALLOWANCE_COLUMNS: &str = "scope_kind, scope_id, period, currency, limit_micros, \
     max_hourly_micros, max_active, max_pending, allowed_presets, allowed_regions, \
     allowed_purchase, max_lifetime_secs, max_attempts";

/// Every allowance, fleet first.
pub fn allowances(conn: &Connection) -> Result<Vec<Allowance>> {
    let sql = format!(
        "SELECT {ALLOWANCE_COLUMNS} FROM cloud_allowances ORDER BY scope_kind, scope_id, period"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], allowance_from_row)?;
    let mut found = Vec::new();
    for row in rows {
        found.push(row?);
    }
    Ok(found)
}

/// The allowances that apply to one project: the fleet's and its own.
pub fn allowances_for(conn: &Connection, project: &ProjectId) -> Result<Vec<Allowance>> {
    Ok(allowances(conn)?
        .into_iter()
        .filter(|allowance| match &allowance.scope {
            Scope::Fleet => true,
            Scope::Project(owner) => owner == project,
        })
        .collect())
}

/// Create attempts permitted for one request of `project`: the smallest
/// `max_attempts` among the allowances that apply, zero when none does.
pub fn max_attempts_for(conn: &Connection, project: &ProjectId) -> Result<u32> {
    Ok(allowances_for(conn, project)?
        .iter()
        .map(|allowance| allowance.max_attempts)
        .min()
        .unwrap_or(0))
}

/// Whether every allowance that applies to `project` permits `purchase`.
/// False when no allowance applies.
pub fn purchase_permitted(
    conn: &Connection,
    project: &ProjectId,
    purchase: PurchaseModel,
) -> Result<bool> {
    let applicable = allowances_for(conn, project)?;
    Ok(!applicable.is_empty()
        && applicable
            .iter()
            .all(|allowance| allowance.allowed_purchase.contains(&purchase)))
}

fn accrued_for(
    conn: &Connection,
    intent: &Intent,
    row: Option<&MoneyRow>,
    now: UnixMillis,
) -> Result<Accrued> {
    let tracked = intent::resources(conn, &intent.id)?;
    let mut micros = 0u64;
    let mut assumptions = Vec::new();
    for resource in &tracked {
        let end = resource.gone_at.unwrap_or(now);
        let millis = end.0.saturating_sub(resource.created_at.0);
        let cost = prorate_up(resource.hourly_micros, millis).ok_or(CloudError::AmountOverflow)?;
        micros = add(micros, cost)?;
    }
    if !tracked.is_empty() {
        assumptions.push(format!(
            "{} tracked resource(s), each at the hourly rate the provider reported, from its creation to its removal or to now",
            tracked.len()
        ));
    }
    let mut existed = !tracked.is_empty();
    if let Some(row) = row {
        if tracked.is_empty()
            && intent.create_uncertain
            && let Some(called_at) = intent.last_create_at
        {
            let end = now.min(intent.expires_at);
            let millis = end.0.saturating_sub(called_at.0);
            let cost =
                prorate_up(row.hold.hourly_micros, millis).ok_or(CloudError::AmountOverflow)?;
            micros = add(micros, cost)?;
            existed = true;
            assumptions.push(
                "the outcome of the create call is not known; one set of resources is assumed to have run at the quoted rate since the call".to_string(),
            );
        }
        if row.assumed_micros > 0 {
            micros = add(micros, row.assumed_micros)?;
            existed = true;
            if let Some(note) = &row.assumed_note {
                assumptions.push(note.clone());
            }
        }
        if existed && micros < row.hold.minimum_commitment_micros {
            micros = row.hold.minimum_commitment_micros;
            assumptions.push("raised to the provider's minimum billing commitment".to_string());
        }
    }
    if assumptions.is_empty() {
        assumptions.push("no provider resource was observed".to_string());
    }
    Ok(Accrued {
        micros,
        as_of: now,
        assumptions,
    })
}

/// The accrued estimate of an intention as of `now`.
///
/// It is this crate's own arithmetic, not a bill: tracked resource time at
/// the reported hourly rates, raised to the minimum commitment when a
/// resource existed, plus anything assumed for a create call whose outcome
/// was never observed.
pub fn accrued(conn: &Connection, id: &CloudIntentId, now: UnixMillis) -> Result<Accrued> {
    let intent = get_intent(conn, id)?;
    let row = money_row(conn, id)?;
    accrued_for(conn, &intent, row.as_ref(), now)
}

/// The hold, the accrued estimate and the reported actual of one intention.
pub fn money(conn: &Connection, id: &CloudIntentId, now: UnixMillis) -> Result<Money> {
    let intent = get_intent(conn, id)?;
    let row = money_row(conn, id)?;
    let accrued = match row.as_ref().and_then(|row| row.estimate.clone()) {
        Some(frozen) => frozen,
        None => accrued_for(conn, &intent, row.as_ref(), now)?,
    };
    Ok(Money {
        actual: row.as_ref().and_then(|row| row.actual.clone()),
        hold: row.map(|row| row.hold),
        accrued,
    })
}

/// How `allowance` stands at `now`.
///
/// An outstanding hold counts as the larger of the hold and the intention's
/// accrued estimate, so resources that outlive their hold keep consuming
/// the allowance visibly. Settled spend counts as the final provider figure
/// when there is one, otherwise the larger of the estimate and whatever the
/// provider has reported so far.
pub fn budget(conn: &Connection, allowance: &Allowance, now: UnixMillis) -> Result<Budget> {
    let (window_start, window_end) = period_window(allowance.period, now);
    let project = allowance.scope.project_key();

    let mut statement = conn.prepare(
        "SELECT i.id FROM cloud_money m JOIN cloud_intents i ON i.id = m.intent \
         WHERE m.released_at IS NULL AND (?1 = '' OR i.project = ?1) ORDER BY i.rowid",
    )?;
    let rows = statement.query_map([project], |row| {
        col_parsed(row, 0, |s| CloudIntentId::parse(s).ok())
    })?;
    let mut outstanding = Vec::new();
    for row in rows {
        outstanding.push(row?);
    }

    let mut held_micros = 0u64;
    let mut hourly_micros = 0u64;
    let mut active = 0u32;
    for id in &outstanding {
        let intent = get_intent(conn, id)?;
        let row = money_row(conn, id)?
            .ok_or_else(|| CloudError::Corrupt(format!("hold of {id} disappeared")))?;
        let accrued = accrued_for(conn, &intent, Some(&row), now)?;
        held_micros = add(held_micros, row.hold.amount_micros.max(accrued.micros))?;
        hourly_micros = add(hourly_micros, row.hold.hourly_micros)?;
        active = active.saturating_add(1);
    }

    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cloud_intents i WHERE (?1 = '' OR i.project = ?1) \
         AND i.state IN ('authorized', 'provisioning', 'unknown', 'bootstrapping')",
        [project],
        |row| row.get(0),
    )?;
    let pending = u32::try_from(pending).unwrap_or(u32::MAX);

    let mut statement = conn.prepare(
        "SELECT m.estimate_micros, m.actual_micros, m.actual_final \
         FROM cloud_money m JOIN cloud_intents i ON i.id = m.intent \
         WHERE m.released_at IS NOT NULL AND m.released_at >= ?2 AND m.released_at < ?3 \
         AND (?1 = '' OR i.project = ?1)",
    )?;
    let rows = statement.query_map(
        params![project, ms(window_start)?, ms(window_end)?],
        |row| {
            let estimate = col_opt_u64(row, 0)?.unwrap_or(0);
            let actual = col_opt_u64(row, 1)?;
            let is_final: bool = row.get(2)?;
            Ok(match actual {
                Some(actual) if is_final => actual,
                Some(actual) => actual.max(estimate),
                None => estimate,
            })
        },
    )?;
    let mut spent_micros = 0u64;
    for row in rows {
        spent_micros = add(spent_micros, row?)?;
    }

    Ok(Budget {
        scope: allowance.scope.clone(),
        period: allowance.period,
        currency: allowance.currency.clone(),
        limit_micros: allowance.limit_micros,
        held_micros,
        spent_micros,
        available_micros: allowance
            .limit_micros
            .saturating_sub(held_micros)
            .saturating_sub(spent_micros),
        window_start,
        window_end,
        active,
        pending,
        hourly_micros,
    })
}

/// What an admission check needs to know about the request.
#[derive(Clone, Debug)]
pub struct AdmissionRequest<'a> {
    pub project: &'a ProjectId,
    pub preset: &'a str,
    pub region: &'a str,
    pub purchase: PurchaseModel,
    pub lifetime_secs: u64,
    pub quote: &'a Quote,
    /// Create calls already issued in the request's retry chain.
    pub attempts_used: u32,
}

/// The result of a successful admission check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub breakdown: HoldBreakdown,
    pub attempts_allowed: u32,
}

/// Checks a request against every allowance that applies, without writing.
///
/// Fails with [`CloudError::Denied`] and the first rule that does not hold.
/// The answer is only binding inside the transaction that also takes the
/// hold; [`authorize`] does both.
pub fn check_admission(
    conn: &Connection,
    request: &AdmissionRequest<'_>,
    config: &MoneyConfig,
    now: UnixMillis,
) -> Result<Admission> {
    let quote = request.quote;
    if quote.preset != request.preset {
        return Err(Denial::QuoteMismatch { field: "preset" }.into());
    }
    if quote.region != request.region {
        return Err(Denial::QuoteMismatch { field: "region" }.into());
    }
    if quote.purchase != request.purchase {
        return Err(Denial::QuoteMismatch {
            field: "purchase model",
        }
        .into());
    }
    if quote.lifetime_secs != request.lifetime_secs {
        return Err(Denial::QuoteMismatch { field: "lifetime" }.into());
    }
    if quote.priced_at > now {
        return Err(Denial::QuoteFromFuture.into());
    }
    let age_secs = now.since(quote.priced_at).as_secs();
    if age_secs > config.quote_max_age_secs {
        return Err(Denial::QuoteStale {
            age_secs,
            max_age_secs: config.quote_max_age_secs,
        }
        .into());
    }

    let applicable = allowances_for(conn, request.project)?;
    for scope in [ScopeKind::Fleet, ScopeKind::Project] {
        if !applicable.iter().any(|a| a.scope.kind() == scope) {
            return Err(Denial::NoAllowance { scope }.into());
        }
    }
    let max_attempts = applicable.iter().map(|a| a.max_attempts).min().unwrap_or(0);
    if request.attempts_used >= max_attempts {
        return Err(Denial::AttemptsExhausted {
            used: request.attempts_used,
            allowed: max_attempts,
        }
        .into());
    }
    let attempts_allowed = max_attempts - request.attempts_used;
    let breakdown = compute_hold(
        quote.hourly_micros,
        quote.minimum_commitment_micros,
        request.lifetime_secs,
        config.startup_secs,
        attempts_allowed,
    )
    .ok_or(Denial::AmountOverflow)?;

    for allowance in &applicable {
        let scope = allowance.scope.kind();
        if allowance.currency != quote.currency {
            return Err(Denial::CurrencyMismatch {
                scope,
                allowance: allowance.currency.clone(),
                quote: quote.currency.clone(),
            }
            .into());
        }
        if !allowance
            .allowed_presets
            .iter()
            .any(|p| p == request.preset)
        {
            return Err(Denial::PresetNotAllowed {
                scope,
                preset: request.preset.to_string(),
            }
            .into());
        }
        if !allowance
            .allowed_regions
            .iter()
            .any(|r| r == request.region)
        {
            return Err(Denial::RegionNotAllowed {
                scope,
                region: request.region.to_string(),
            }
            .into());
        }
        if !allowance.allowed_purchase.contains(&request.purchase) {
            return Err(Denial::PurchaseNotAllowed {
                scope,
                purchase: request.purchase,
            }
            .into());
        }
        if request.lifetime_secs > allowance.max_lifetime_secs {
            return Err(Denial::LifetimeTooLong {
                scope,
                requested_secs: request.lifetime_secs,
                max_secs: allowance.max_lifetime_secs,
            }
            .into());
        }
        let standing = budget(conn, allowance, now)?;
        let would_be_micros = standing
            .hourly_micros
            .checked_add(quote.hourly_micros)
            .ok_or(Denial::AmountOverflow)?;
        if would_be_micros > allowance.max_hourly_micros {
            return Err(Denial::HourlyRateExceeded {
                scope,
                would_be_micros,
                max_micros: allowance.max_hourly_micros,
            }
            .into());
        }
        if standing.active >= allowance.max_active {
            return Err(Denial::TooManyActive {
                scope,
                active: standing.active,
                max: allowance.max_active,
            }
            .into());
        }
        if standing.pending >= allowance.max_pending {
            return Err(Denial::TooManyPending {
                scope,
                pending: standing.pending,
                max: allowance.max_pending,
            }
            .into());
        }
        if standing.available_micros < breakdown.total_micros {
            return Err(Denial::BudgetExceeded {
                scope,
                period: allowance.period,
                limit_micros: allowance.limit_micros,
                held_micros: standing.held_micros,
                spent_micros: standing.spent_micros,
                requested_micros: breakdown.total_micros,
            }
            .into());
        }
    }
    Ok(Admission {
        breakdown,
        attempts_allowed,
    })
}

/// Authorizes a `Requested` intention: checks every rule, takes the hold and
/// moves the intention to `Authorized`, all in the caller's transaction.
///
/// Because the check and the hold share one transaction, two authorizations
/// racing for the last of an allowance cannot both succeed. Repeating the
/// call for an intention that is already authorized returns its hold and
/// takes no second one. A denial writes nothing.
pub fn authorize(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    quote: &Quote,
    config: &MoneyConfig,
    now: UnixMillis,
) -> Result<Hold> {
    let intent = get_intent(tx, id)?;
    if let Some(existing) = hold(tx, id)?
        && intent.state == IntentState::Authorized
        && existing.released_at.is_none()
    {
        return Ok(existing);
    }
    if intent.state != IntentState::Requested {
        return Err(CloudError::IllegalTransition {
            from: intent.state,
            to: IntentState::Authorized,
        });
    }
    if intent.expired(now) {
        return Err(Denial::IntentExpired.into());
    }
    let attempts_used = chain_attempts(tx, &intent.chain)?;
    let admission = check_admission(
        tx,
        &AdmissionRequest {
            project: &intent.project,
            preset: &intent.preset,
            region: &intent.region,
            purchase: intent.purchase,
            lifetime_secs: intent.lifetime_secs,
            quote,
            attempts_used,
        },
        config,
        now,
    )?;
    let breakdown = admission.breakdown;
    tx.execute(
        "INSERT INTO cloud_money (intent, currency, amount_micros, startup_micros, \
         lifetime_micros, minimum_micros, retries_micros, hourly_micros, \
         minimum_commitment_micros, attempts_allowed, quote_priced_at, quote_assumptions, \
         created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            id.as_str(),
            quote.currency,
            sql_i64(breakdown.total_micros)?,
            sql_i64(breakdown.startup_micros)?,
            sql_i64(breakdown.lifetime_micros)?,
            sql_i64(breakdown.minimum_micros)?,
            sql_i64(breakdown.retries_micros)?,
            sql_i64(quote.hourly_micros)?,
            sql_i64(quote.minimum_commitment_micros)?,
            i64::from(admission.attempts_allowed),
            ms(quote.priced_at)?,
            json_text(&quote.assumptions)?,
            ms(now)?,
        ],
    )?;
    intent::transition(tx, id, IntentState::Authorized, "budget hold taken", now)?;
    insert_event(
        tx,
        Some(id),
        "hold_taken",
        None,
        None,
        &serde_json::json!({
            "hold": breakdown,
            "hourly_micros": quote.hourly_micros,
            "currency": quote.currency,
            "attempts_allowed": admission.attempts_allowed,
            "quote_priced_at": quote.priced_at,
        }),
        now,
    )?;
    hold(tx, id)?.ok_or_else(|| CloudError::Corrupt(format!("hold of {id} was not stored")))
}

/// Adds a conservative charge for resources that were never observed, with
/// the reason. Used when a create call's outcome could only be checked after
/// the provider's own expiry may already have removed what it created.
pub fn add_assumed_charge(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    micros: u64,
    note: &str,
    now: UnixMillis,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE cloud_money SET assumed_micros = assumed_micros + ?2, assumed_note = ?3 \
         WHERE intent = ?1",
        params![id.as_str(), sql_i64(micros)?, note],
    )?;
    if changed > 0 {
        insert_event(
            tx,
            Some(id),
            "assumed_charge",
            None,
            None,
            &serde_json::json!({ "micros": micros, "note": note }),
            now,
        )?;
    }
    Ok(())
}

/// Records what the provider reports it billed for an intention.
///
/// The figure is kept next to the estimate, never in place of it. It may be
/// revised until `is_final` is set. Fails with `NotFound` when the intention
/// was never authorized.
pub fn record_actual(
    tx: &Transaction<'_>,
    id: &CloudIntentId,
    micros: u64,
    is_final: bool,
    now: UnixMillis,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE cloud_money SET actual_micros = ?2, actual_reported_at = ?3, actual_final = ?4 \
         WHERE intent = ?1",
        params![id.as_str(), sql_i64(micros)?, ms(now)?, is_final],
    )?;
    if changed == 0 {
        return Err(CloudError::NotFound(id.clone()));
    }
    insert_event(
        tx,
        Some(id),
        "actual_recorded",
        None,
        None,
        &serde_json::json!({ "micros": micros, "final": is_final }),
        now,
    )
}

/// Settles an intention: freezes the accrued estimate, releases the hold and
/// marks the intention settled.
///
/// Refused with [`CloudError::ResourcesRemain`] while any tracked resource
/// is not known to be gone, and while a create call's outcome is unknown.
/// Settling twice changes nothing.
pub fn settle(tx: &Transaction<'_>, id: &CloudIntentId, now: UnixMillis) -> Result<()> {
    let intent = get_intent(tx, id)?;
    if intent.settled_at.is_some() {
        return Ok(());
    }
    if intent.create_uncertain || !live_resources(tx, id)?.is_empty() {
        return Err(CloudError::ResourcesRemain(id.clone()));
    }
    if !intent.state.is_final() {
        return Err(CloudError::Invalid(format!(
            "intention {id} is {} and cannot be settled",
            intent.state
        )));
    }
    if let Some(row) = money_row(tx, id)? {
        let estimate = accrued_for(tx, &intent, Some(&row), now)?;
        tx.execute(
            "UPDATE cloud_money SET released_at = ?2, estimate_micros = ?3, estimate_at = ?2, \
             estimate_assumptions = ?4 WHERE intent = ?1 AND released_at IS NULL",
            params![
                id.as_str(),
                ms(now)?,
                sql_i64(estimate.micros)?,
                json_text(&estimate.assumptions)?,
            ],
        )?;
        insert_event(
            tx,
            Some(id),
            "hold_released",
            None,
            None,
            &serde_json::json!({
                "hold_micros": row.hold.amount_micros,
                "estimate_micros": estimate.micros,
                "assumptions": estimate.assumptions,
            }),
            now,
        )?;
    }
    intent::mark_settled(tx, id, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;

    #[test]
    fn hold_is_startup_plus_lifetime_plus_minimum_plus_retries() {
        // 0.36/h, 10 min startup, 2 h lifetime, one attempt, no minimum.
        let hold = compute_hold(360_000, 0, 2 * HOUR, 600, 1).unwrap();
        assert_eq!(hold.startup_micros, 60_000);
        assert_eq!(hold.lifetime_micros, 720_000);
        assert_eq!(hold.minimum_micros, 0);
        assert_eq!(hold.retries_micros, 0);
        assert_eq!(hold.total_micros, 780_000);

        // Three attempts add two more startups.
        let hold = compute_hold(360_000, 0, 2 * HOUR, 600, 3).unwrap();
        assert_eq!(hold.retries_micros, 120_000);
        assert_eq!(hold.total_micros, 900_000);
    }

    #[test]
    fn a_billing_minimum_is_held_in_full_and_repeated_per_retry() {
        // 1.2/h host with a 24 h minimum, 1 h job, 10 min startup.
        let minimum = 24 * 1_200_000;
        let hold = compute_hold(1_200_000, minimum, HOUR, 600, 1).unwrap();
        assert_eq!(hold.startup_micros, 200_000);
        assert_eq!(hold.lifetime_micros, 1_200_000);
        assert_eq!(hold.minimum_micros, minimum);
        assert_eq!(hold.total_micros, 1_400_000 + minimum);

        // The hold is never below the commitment, however short the run.
        let hold = compute_hold(1_200_000, minimum, 1, 0, 1).unwrap();
        assert!(hold.total_micros >= minimum);

        // Each retry may start another host: startup plus a whole minimum.
        let hold = compute_hold(1_200_000, minimum, HOUR, 600, 2).unwrap();
        assert_eq!(hold.retries_micros, 200_000 + minimum);
        assert_eq!(hold.total_micros, 1_400_000 + 2 * minimum + 200_000);
    }

    #[test]
    fn hold_rounds_up_and_refuses_amounts_that_do_not_fit() {
        // 1 micro-unit per hour for one second still holds one micro-unit.
        let hold = compute_hold(1, 0, 1, 0, 1).unwrap();
        assert_eq!(hold.total_micros, 1);
        assert_eq!(compute_hold(0, 0, HOUR, 600, 5).unwrap().total_micros, 0);
        assert!(compute_hold(u64::MAX, 0, 2 * HOUR, 0, 1).is_none());
        assert!(compute_hold(1, u64::MAX, HOUR, 0, 2).is_none());
        assert!(compute_hold(1_000_000, 0, u64::MAX, 0, 1).is_none());
        // Fits u64 but not the stored signed range.
        assert!(compute_hold(1 << 63, 0, HOUR, 0, 1).is_none());
    }

    #[test]
    fn daily_and_monthly_windows_are_utc_calendar_periods() {
        // 2026-11-26T05:00:00Z
        let now = UnixMillis(1_795_669_200_000);
        let (start, end) = period_window(Period::Daily, now);
        assert_eq!(start, UnixMillis(1_795_651_200_000)); // 2026-11-26T00:00:00Z
        assert_eq!(end.0 - start.0, MILLIS_PER_DAY);
        assert!(start <= now && now < end);

        let (start, end) = period_window(Period::Monthly, now);
        assert_eq!(start, UnixMillis(1_793_491_200_000)); // 2026-11-01T00:00:00Z
        assert_eq!(end, UnixMillis(1_796_083_200_000)); // 2026-12-01T00:00:00Z

        // December rolls into January of the next year.
        let december = UnixMillis(1_796_083_200_000 + 5 * MILLIS_PER_DAY);
        let (start, end) = period_window(Period::Monthly, december);
        assert_eq!(start, UnixMillis(1_796_083_200_000));
        assert_eq!(end, UnixMillis(1_798_761_600_000)); // 2027-01-01T00:00:00Z

        // Leap day: 2028-02-29 belongs to a 29-day February.
        let leap = UnixMillis(days_from_civil(2028, 2, 29) * MILLIS_PER_DAY + 1);
        let (start, end) = period_window(Period::Monthly, leap);
        assert_eq!((end.0 - start.0) / MILLIS_PER_DAY, 29);
    }

    #[test]
    fn civil_date_conversion_round_trips() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        assert_eq!(
            civil_from_days(days_from_civil(2026, 11, 26)),
            (2026, 11, 26)
        );
        for days in (0..60_000).step_by(17) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
