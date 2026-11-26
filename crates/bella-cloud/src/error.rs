//! Typed errors. A refusal or a denial is an answer, not a malfunction: the
//! caller can show it to a person and nothing was created.

use crate::intent::IntentState;
use crate::money::{Denial, ScopeKind};
use crate::provider::{ProviderError, PurchaseModel};
use bella_core::CloudIntentId;
use serde::Serialize;

pub type Result<T> = std::result::Result<T, CloudError>;

/// Why a capacity request was refused before any intention was written.
///
/// Every variant means that nothing was created: no intention row, no hold
/// and no provider call that could create a resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The project or the job uses `CloudMode::ExistingOnly`.
    #[error("cloud mode is existing-only: no paid capacity may be created for this work")]
    ExistingOnly,
    /// An automatic request met `CloudMode::Manual`.
    #[error(
        "cloud mode is manual: a person must authorise capacity for one job, automatic expansion is not permitted"
    )]
    ManualOnly,
    /// A manual request met `CloudMode::AutoOverflow`, which has no manual exceptions.
    #[error(
        "cloud mode is auto-overflow: only the standing policy may request capacity, use hybrid mode to allow manual requests"
    )]
    AutomaticOnly,
    /// The source may not leave the LAN. No priority or escalation changes this.
    #[error(
        "the source is LAN-only and is never placed on cloud capacity; priority and escalation do not change this"
    )]
    SourceLanOnly,
    /// The principal's role may not request capacity.
    #[error(
        "this principal may not request cloud capacity; a developer or administrator must do it"
    )]
    Forbidden,
    /// No administrator has set an allowance at this scope. This is the default.
    #[error(
        "cloud provisioning is off: no {scope} allowance exists; an administrator must set one before capacity can be requested"
    )]
    NoAllowance { scope: ScopeKind },
    /// The provider bills real money and the engine was not explicitly allowed to use it.
    #[error(
        "provider `{provider}` bills real money and paid provisioning is not enabled on this controller"
    )]
    PaidProvisioningDisabled { provider: String },
    /// The provider cannot remove expired resources on its own.
    #[error(
        "provider `{provider}` has no provider-side expiry, so nothing would remove resources if the controller were lost"
    )]
    NoProviderSideExpiry { provider: String },
    #[error(
        "the provider-side janitor is not installed for provider `{provider}`. To repair: install the janitor in the provider account (docs/cloud.md, \"Provider-side janitor\"), let it complete one run, then retry"
    )]
    JanitorNotInstalled { provider: String },
    #[error(
        "the provider-side janitor for provider `{provider}` has never completed a run. To repair: check that it is scheduled and permitted to list and delete fleet resources (docs/cloud.md, \"Provider-side janitor\"), then retry after its first success"
    )]
    JanitorNeverSucceeded { provider: String },
    #[error(
        "the provider-side janitor for provider `{provider}` last succeeded {age_secs}s ago (limit {max_age_secs}s). To repair: check its schedule, credentials and logs in the provider account (docs/cloud.md, \"Provider-side janitor\"), then retry after a successful run"
    )]
    JanitorStale {
        provider: String,
        age_secs: u64,
        max_age_secs: u64,
    },
    #[error(
        "the health of the provider-side janitor for provider `{provider}` could not be read ({detail}). To repair: restore access to the provider and confirm the janitor reports a recent success, then retry"
    )]
    JanitorStatusUnavailable { provider: String, detail: String },
    #[error("provider `{provider}` does not offer preset `{preset}`")]
    UnsupportedPreset { provider: String, preset: String },
    #[error("provider `{provider}` does not offer region `{region}`")]
    UnsupportedRegion { provider: String, region: String },
    #[error("preset `{preset}` is not sold as {purchase}")]
    UnsupportedPurchase {
        preset: String,
        purchase: PurchaseModel,
    },
    /// A replacement for a failed intention is not permitted.
    #[error("a retry is not permitted: {detail}")]
    RetryNotPermitted { detail: String },
}

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// The request was refused and nothing was created.
    #[error("capacity request refused: {0}")]
    Refused(Box<Refusal>),
    /// The budget authorization was denied and no hold was taken.
    #[error("authorization denied: {0}")]
    Denied(Box<Denial>),
    #[error("illegal intention transition {from} -> {to}")]
    IllegalTransition { from: IntentState, to: IntentState },
    #[error("cloud intention {0} not found")]
    NotFound(CloudIntentId),
    /// The intention still owns provider resources, so it cannot be closed.
    #[error("cloud intention {0} still owns provider resources")]
    ResourcesRemain(CloudIntentId),
    /// The same identifier was reused with different content.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The operation is reserved for administrators.
    #[error("only an administrator may {0}")]
    AdminRequired(&'static str),
    #[error("invalid request: {0}")]
    Invalid(String),
    /// An amount or timestamp does not fit the stored integer range.
    #[error("amount out of range")]
    AmountOverflow,
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("cloud schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: usize },
    #[error("stored cloud data is corrupt: {0}")]
    Corrupt(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
}

impl CloudError {
    /// The refusal, when this error is one.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            CloudError::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }

    /// The denial, when this error is one.
    pub fn denial(&self) -> Option<&Denial> {
        match self {
            CloudError::Denied(denial) => Some(denial),
            _ => None,
        }
    }
}

impl From<Refusal> for CloudError {
    fn from(refusal: Refusal) -> Self {
        CloudError::Refused(Box::new(refusal))
    }
}

impl From<Denial> for CloudError {
    fn from(denial: Denial) -> Self {
        CloudError::Denied(Box::new(denial))
    }
}
