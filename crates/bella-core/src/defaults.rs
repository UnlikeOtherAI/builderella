//! Starting values for timing and policy. These are configurable proposals,
//! not measured requirements; commissioning tunes them per machine.

use std::time::Duration;

/// Default controller listen port.
pub const CONTROLLER_PORT: u16 = 7420;

/// Local resource/protection loop on a worker.
pub const PROTECTION_INTERVAL: Duration = Duration::from_secs(1);
/// Worker heartbeat and capacity update.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// After this long without a heartbeat, telemetry is stale: no new offers.
pub const TELEMETRY_STALE_AFTER: Duration = Duration::from_secs(15);
/// After this long the machine is unreachable. Reservations stay held.
pub const UNREACHABLE_AFTER: Duration = Duration::from_secs(30);
/// Scheduler reconciliation tick; scheduling is otherwise event driven.
pub const SCHEDULER_TICK: Duration = Duration::from_secs(1);

/// An offer that is neither accepted nor rejected by then is revoked.
pub const OFFER_EXPIRY: Duration = Duration::from_secs(15);
/// Execution authority window granted on start and on every renewal.
pub const AUTHORITY_WINDOW: Duration = Duration::from_secs(60);
/// How often the controller renews authority for connected attempts.
pub const AUTHORITY_RENEW_INTERVAL: Duration = Duration::from_secs(10);

/// Grace between a polite stop and a forced kill.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(10);
/// Budget for the cleanup phase itself.
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Default wall-clock limit for a job that declares none.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Interactive environment lease defaults.
pub const ENV_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const ENV_LEASE: Duration = Duration::from_secs(30 * 60);
pub const ENV_MAX_LIFETIME: Duration = Duration::from_secs(4 * 60 * 60);

/// Pairing codes are single use and short lived.
pub const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// Per-attempt log allowance before truncation with an explicit marker.
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Per-attempt artifact allowance.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest single object accepted by the object store API.
pub const MAX_OBJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest log chunk carried in one worker message.
pub const LOG_CHUNK_BYTES: usize = 32 * 1024;

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;
