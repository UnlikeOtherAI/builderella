//! Cloud capacity: provider contract, durable intentions, budget holds, overflow policy.
//!
//! This crate models temporary cloud capacity end to end without calling a
//! real provider. It contains:
//!
//! * [`provider`]: the blocking [`Provider`] contract a real adapter will
//!   implement, and the types that cross it.
//! * [`sim`]: [`SimProvider`], an in-memory provider driven by an injected
//!   clock, with fault switches for every failure the engine must survive.
//! * [`intent`]: the durable intention, its state machine and its event log.
//! * [`money`]: allowances, the atomic budget hold, the accrued estimate and
//!   the provider-reported actual, all in integer micro-units.
//! * [`engine`]: the operations that tie those together. Each one writes its
//!   intent to the database before it calls the provider and can be repeated.
//! * [`overflow`]: the pure policy that decides when queued work justifies a
//!   new worker.
//!
//! Nothing here spends money. Paid provisioning needs a provider adapter that
//! does not exist in this crate, an explicit engine switch, a project whose
//! cloud mode allows it, and allowances that only an administrator can set.
//! See `docs/cloud.md`.
//!
//! Persistence: the tables live in the controller's SQLite database. Run
//! [`schema::migrate`] once on the connection; every other function takes the
//! caller's connection or transaction.

/// Declares a field-less enum with a stable text form used in the database
/// and on the wire.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
            serde::Serialize, serde::Deserialize,
        )]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The stable text form.
            pub fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $text ),+ }
            }

            /// Parses the stable text form.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

mod convert;
pub mod engine;
pub mod error;
pub mod intent;
pub mod money;
pub mod overflow;
pub mod provider;
pub mod schema;
pub mod sim;

pub use engine::{
    Actor, Database, DenyReason, Engine, EngineConfig, Exposure, Interruption, NewCapacity,
    ProvisionOutcome, ProvisionStep, ReconcileOutcome, Registration, RetryBlocker, RetryVerdict,
    SweepReport, Termination,
};
pub use error::{CloudError, Refusal, Result};
pub use intent::{FailureKind, Intent, IntentEvent, IntentState, RequestMode, TrackedResource};
pub use money::{
    Accrued, ActualCost, Allowance, Budget, Denial, Hold, HoldBreakdown, Money, MoneyConfig,
    Period, Scope, ScopeKind,
};
pub use overflow::{
    CapacityPhase, CapacityRequest, Demand, LaunchHistory, LaunchOutcome, LaunchRecord,
    LocalMachine, OverflowPolicy, OverflowPreset, PendingOrActive, Trigger, evaluate,
    release_candidates,
};
pub use provider::{
    Billing, Capabilities, CreateAck, CreateRequest, JanitorStatus, LABEL_FLEET, LABEL_INTENT,
    OwnedResource, Preset, Provider, ProviderError, PurchaseModel, Quote, ResourceKind,
    ResourceState,
};
pub use schema::{MIGRATIONS, migrate};
pub use sim::{SimCreateCall, SimFaults, SimProvider};
