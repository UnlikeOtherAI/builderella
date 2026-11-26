//! The provider contract.
//!
//! A provider adapter turns these blocking calls into one cloud's API. The
//! engine never trusts a single answer from it: a create that timed out may
//! have created something, a delete that timed out may have deleted
//! something, and a listing is the only proof that a resource is gone.

use bella_core::platform::Platform;
use bella_core::resources::ResourceVector;
use bella_core::{CloudIntentId, FleetId, UnixMillis};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Label carrying the fleet identifier on every resource the fleet creates.
/// A resource without it is never deleted by the engine.
pub const LABEL_FLEET: &str = "builderella-fleet";
/// Label carrying the intention identifier on every resource the fleet creates.
pub const LABEL_INTENT: &str = "builderella-intent";

string_enum! {
    /// How capacity is bought. A request for one model is never fulfilled
    /// with the other.
    pub enum PurchaseModel {
        /// Billed at the list price until deleted.
        OnDemand => "on_demand",
        /// Cheaper, and the provider may withdraw it without notice.
        Spot => "spot",
    }
}

string_enum! {
    /// What a provider resource is. Everything except `Instance` is a
    /// secondary resource that can outlive the instance and keep costing money.
    pub enum ResourceKind {
        Instance => "instance",
        Disk => "disk",
        Address => "address",
        /// A dedicated host, usually with a minimum billing commitment.
        Host => "host",
    }
}

string_enum! {
    /// Provider-reported state of a resource that still exists.
    pub enum ResourceState {
        /// Created, not yet usable.
        Pending => "pending",
        Running => "running",
        /// Deletion was accepted and has not finished.
        Terminating => "terminating",
    }
}

string_enum! {
    /// Whether using the provider costs real money.
    pub enum Billing {
        /// No money moves. The simulated provider.
        Simulated => "simulated",
        /// Real invoices. The engine refuses such a provider unless paid
        /// provisioning was explicitly enabled.
        Real => "real",
    }
}

/// A machine shape a provider sells.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub platform: Platform,
    /// CPU, memory and disk of one instance of this preset.
    pub resources: ResourceVector,
    /// Purchase models this preset is sold under.
    pub purchase: Vec<PurchaseModel>,
    /// Shortest period the provider bills once the preset is created, such
    /// as 24 hours for a dedicated host. Zero when there is none.
    pub minimum_commitment_secs: u64,
}

/// What a provider can do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Stable provider name, recorded on every intention.
    pub provider: String,
    pub billing: Billing,
    /// Billing currency of every amount this provider reports.
    pub currency: String,
    pub regions: Vec<String>,
    pub presets: Vec<Preset>,
    /// Purchase models offered by at least one preset.
    pub purchase_models: Vec<PurchaseModel>,
    /// Whether the provider can remove expired resources without the
    /// controller. Capacity is never requested from a provider that cannot.
    pub provider_side_expiry: bool,
}

impl Capabilities {
    /// The preset called `name`, if the provider offers it.
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.name == name)
    }
}

/// A price for one preset, region, purchase model and lifetime.
///
/// A quote is evidence with a timestamp, not a promise: authorization rejects
/// one that is too old or that does not match the intention it is used for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub lifetime_secs: u64,
    /// Combined hourly price of everything the preset creates, in
    /// micro-units of `currency`.
    pub hourly_micros: u64,
    pub currency: String,
    pub priced_at: UnixMillis,
    /// Least the provider bills once the preset exists, in micro-units.
    pub minimum_commitment_micros: u64,
    /// What the price does and does not include, in plain words.
    pub assumptions: Vec<String>,
}

/// A request to create capacity for one intention.
#[derive(Clone, PartialEq, Eq)]
pub struct CreateRequest {
    /// The idempotency token. Repeating a create with the same token must
    /// not create a second set of resources.
    pub intent_id: CloudIntentId,
    pub fleet_id: FleetId,
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub image: String,
    /// When the provider-side janitor may remove the resources on its own.
    pub expires_at: UnixMillis,
    /// Labels to attach to every created resource. Always contains
    /// [`LABEL_FLEET`] and [`LABEL_INTENT`].
    pub labels: BTreeMap<String, String>,
    /// One-use enrollment string handed to the new worker. Secret.
    pub bootstrap: String,
}

impl fmt::Debug for CreateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateRequest")
            .field("intent_id", &self.intent_id)
            .field("fleet_id", &self.fleet_id)
            .field("preset", &self.preset)
            .field("region", &self.region)
            .field("purchase", &self.purchase)
            .field("image", &self.image)
            .field("expires_at", &self.expires_at)
            .field("labels", &self.labels)
            .field("bootstrap", &"<redacted>")
            .finish()
    }
}

/// The provider's answer to a successful create: everything it created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAck {
    pub resources: Vec<OwnedResource>,
}

/// One provider resource as the provider reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedResource {
    /// Provider-assigned identifier, unique within the provider.
    pub id: String,
    pub kind: ResourceKind,
    pub state: ResourceState,
    /// The intention named by the resource's [`LABEL_INTENT`] label.
    pub intent_id: Option<CloudIntentId>,
    /// The labels actually present on the resource.
    pub labels: BTreeMap<String, String>,
    /// When the provider created it.
    pub created_at: UnixMillis,
    pub expires_at: Option<UnixMillis>,
    /// Earliest time the provider accepts a release, for resources under a
    /// minimum commitment.
    pub releasable_at: Option<UnixMillis>,
    /// What this resource alone costs per hour, in micro-units.
    pub hourly_micros: u64,
}

/// Why a provider call did not succeed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ProviderError {
    /// No answer arrived. The call may or may not have taken effect.
    #[error("the provider did not answer in time")]
    Timeout,
    /// The provider refused the call and changed nothing.
    #[error("the provider rejected the call: {reason}")]
    Rejected { reason: String },
    /// An account quota would be exceeded. Nothing was changed.
    #[error("a provider quota would be exceeded")]
    QuotaExceeded,
    /// The provider has no capacity of the requested kind. Nothing was changed.
    #[error("the provider has no capacity of the requested kind")]
    CapacityUnavailable,
    /// The provider could not be reached. The call may or may not have taken effect.
    #[error("the provider is unavailable")]
    Unavailable,
}

impl ProviderError {
    /// Whether the call may have taken effect despite the error.
    pub fn outcome_uncertain(&self) -> bool {
        matches!(self, ProviderError::Timeout | ProviderError::Unavailable)
    }
}

/// Health of the independent, provider-side cleanup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JanitorStatus {
    pub installed: bool,
    pub last_success_at: Option<UnixMillis>,
}

/// One cloud provider. Blocking; callers run it off the async runtime.
///
/// Requirements on an implementation:
///
/// * `create` with a token it has already honoured returns the resources of
///   the first call instead of creating more.
/// * `delete` of a resource that no longer exists succeeds.
/// * `find_by_intent` and `list_owned` return every resource carrying the
///   matching label, including secondary resources left behind by a delete.
/// * A definite error (`Rejected`, `QuotaExceeded`, `CapacityUnavailable`)
///   means nothing was changed.
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    /// The current price. Never cached by the engine across authorizations.
    fn quote(
        &self,
        preset: &str,
        region: &str,
        purchase: PurchaseModel,
        lifetime_secs: u64,
    ) -> Result<Quote, ProviderError>;

    fn create(&self, request: &CreateRequest) -> Result<CreateAck, ProviderError>;

    /// Every existing resource labelled with this intention.
    fn find_by_intent(&self, intent: &CloudIntentId) -> Result<Vec<OwnedResource>, ProviderError>;

    /// Every existing resource the provider attributes to this fleet.
    fn list_owned(&self, fleet: &FleetId) -> Result<Vec<OwnedResource>, ProviderError>;

    fn delete(&self, resource_id: &str) -> Result<(), ProviderError>;

    fn janitor_status(&self) -> Result<JanitorStatus, ProviderError>;
}
