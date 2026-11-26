//! Controller configuration: where it listens, where it keeps its state and
//! every interval it runs on.

use bella_core::defaults;
use bella_core::{Clock, SystemClock};
use bella_scheduler::SchedulerConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How the controller's listener is protected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tls {
    /// TLS with the fleet certificate kept under `<data_dir>/tls`. The
    /// certificate is generated on first start, naming `subject_alt_names`
    /// (IP addresses or DNS names), and never regenerated afterwards, so the
    /// fingerprint paired machines have pinned stays the same.
    SelfSigned { subject_alt_names: Vec<String> },
    /// Plain HTTP. Accepted only when the listen address is loopback or
    /// [`ControllerConfig::allow_insecure`] is set.
    Disabled,
}

/// Limits on `POST /enroll`, the one route that takes a guessable secret
/// without a bearer token.
///
/// Both limits are token buckets driven by the controller's clock. Every
/// request is counted, whatever its outcome. A request over either limit is
/// answered with `unavailable` (503) and a `retry-after` header before the
/// pairing code is looked at.
///
/// With the defaults an address gets 5 attempts at once and 5 a minute after
/// that, and the whole fleet 20 at once and 20 a minute. A pairing code has
/// 50 random bits, so 20 guesses a minute for a day find one outstanding
/// code with a probability of about 3 in 10^11.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnrollLimits {
    /// Attempts one source address may make in a burst. IPv6 sources are
    /// grouped by their /64 prefix.
    pub per_source_burst: u32,
    /// One further attempt is granted to a source every this long.
    pub per_source_refill: Duration,
    /// Attempts all sources together may make in a burst.
    pub global_burst: u32,
    /// One further attempt is granted fleet-wide every this long.
    pub global_refill: Duration,
}

impl Default for EnrollLimits {
    fn default() -> Self {
        Self {
            per_source_burst: 5,
            per_source_refill: Duration::from_secs(12),
            global_burst: 20,
            global_refill: Duration::from_secs(3),
        }
    }
}

/// Every interval, timeout and size limit the controller uses. The defaults
/// come from [`bella_core::defaults`] where a value is defined there; all of
/// them are starting values, and tests shorten them.
///
/// Durations that describe *when something is due* (offer expiry, staleness,
/// pairing code lifetime) are measured on the configured [`Clock`].
/// Durations that describe *how often the controller looks* (ticks, sweeps)
/// and transport timeouts are real elapsed time.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Upper bound between two dispatch passes; a pass also runs on every
    /// relevant change.
    pub scheduler_tick: Duration,
    /// Shortest pause between two dispatch passes, so a burst of changes is
    /// handled by one pass.
    pub dispatch_min_interval: Duration,
    /// Most queued jobs one dispatch pass considers, oldest first.
    pub max_queue_scan: u32,
    /// Scheduling parameters handed to `bella-scheduler`.
    pub scheduler: SchedulerConfig,
    /// Heartbeat interval announced to workers in `Welcome`.
    pub heartbeat_interval: Duration,
    /// No heartbeat for this long: the machine is flagged stale and gets no
    /// new offers.
    pub telemetry_stale_after: Duration,
    /// No heartbeat for this long: the machine is flagged unreachable and
    /// its attempts uncertain. Nothing is released.
    pub unreachable_after: Duration,
    /// An offer neither started nor rejected within this time is withdrawn.
    pub offer_expiry: Duration,
    /// How long a job is not offered again to a machine that rejected it or
    /// let its offer expire.
    pub rejection_backoff: Duration,
    /// How often execution authority is renewed for running attempts.
    pub authority_renew_interval: Duration,
    /// How often the offer-expiry, liveness, cancellation and environment
    /// lease sweeps run.
    pub sweep_interval: Duration,
    /// Shortest pause before a `Cancel` or `FinishEnvironment` command is
    /// sent again while the attempt has not reported that it is stopping.
    pub command_resend_interval: Duration,
    /// How often unreferenced objects are collected.
    pub gc_interval: Duration,
    /// Objects published or re-uploaded more recently than this are never
    /// collected. It must be comfortably longer than the longest upload and
    /// than the time between uploading a snapshot and submitting its job.
    pub gc_grace: Duration,
    /// Lifetime of a pairing code when the request names none.
    pub pairing_code_ttl: Duration,
    /// Longest lifetime a pairing code may be given.
    pub max_pairing_code_ttl: Duration,
    /// Rate limits on `POST /enroll`.
    pub enroll_limits: EnrollLimits,
    /// Longest a `GET /jobs/{id}/wait` request is held.
    pub max_wait: Duration,
    /// Interval of the comment line that keeps an idle event stream open.
    pub sse_keepalive: Duration,
    /// How often the event stream looks for events that were written
    /// without waking it.
    pub event_poll_interval: Duration,
    /// Largest JSON request body on ordinary routes.
    pub max_json_bytes: usize,
    /// Largest JSON request body on `POST /snapshots` and
    /// `POST /objects/missing`, which carry one entry per file.
    pub max_manifest_json_bytes: usize,
    /// Largest uncompressed object the object routes accept.
    pub max_object_bytes: u64,
    /// An object upload that sends nothing for this long is abandoned.
    pub upload_idle_timeout: Duration,
    /// How long a request may take to send its headers.
    pub header_read_timeout: Duration,
    /// How long a TLS handshake may take.
    pub tls_handshake_timeout: Duration,
    /// How long a worker may take to send `Hello` after connecting.
    pub hello_timeout: Duration,
    /// Interval of WebSocket pings to each worker.
    pub ws_ping_interval: Duration,
    /// A worker connection that delivers no frame at all for this long is
    /// closed. Pongs count, so this is at least two ping intervals.
    pub ws_idle_timeout: Duration,
    /// A frame that cannot be written to a worker within this time closes
    /// its connection.
    pub ws_send_timeout: Duration,
    /// Largest WebSocket message accepted from a worker.
    pub ws_max_message_bytes: usize,
    /// Commands that may wait for one worker. A worker that lets this many
    /// pile up is disconnected.
    pub ws_control_queue: usize,
    /// Log acknowledgements that may wait for one worker. When the queue is
    /// full the controller stops reading that worker's socket until it
    /// drains, which slows that worker's log stream and nothing else.
    pub ws_ack_queue: usize,
    /// How long shutdown waits for open requests and worker sockets.
    pub shutdown_grace: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            scheduler_tick: defaults::SCHEDULER_TICK,
            dispatch_min_interval: Duration::from_millis(50),
            max_queue_scan: 5_000,
            scheduler: SchedulerConfig::default(),
            heartbeat_interval: defaults::HEARTBEAT_INTERVAL,
            telemetry_stale_after: defaults::TELEMETRY_STALE_AFTER,
            unreachable_after: defaults::UNREACHABLE_AFTER,
            offer_expiry: defaults::OFFER_EXPIRY,
            rejection_backoff: Duration::from_secs(30),
            authority_renew_interval: defaults::AUTHORITY_RENEW_INTERVAL,
            sweep_interval: Duration::from_secs(1),
            command_resend_interval: Duration::from_secs(5),
            gc_interval: Duration::from_secs(60 * 60),
            gc_grace: Duration::from_secs(24 * 60 * 60),
            pairing_code_ttl: defaults::PAIRING_CODE_TTL,
            max_pairing_code_ttl: Duration::from_secs(24 * 60 * 60),
            enroll_limits: EnrollLimits::default(),
            max_wait: Duration::from_secs(60),
            sse_keepalive: Duration::from_secs(15),
            event_poll_interval: Duration::from_secs(1),
            max_json_bytes: 1024 * 1024,
            max_manifest_json_bytes: 64 * 1024 * 1024,
            max_object_bytes: defaults::MAX_OBJECT_BYTES,
            upload_idle_timeout: Duration::from_secs(120),
            header_read_timeout: Duration::from_secs(30),
            tls_handshake_timeout: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(10),
            ws_ping_interval: Duration::from_secs(10),
            ws_idle_timeout: defaults::UNREACHABLE_AFTER,
            ws_send_timeout: Duration::from_secs(10),
            ws_max_message_bytes: 8 * 1024 * 1024,
            ws_control_queue: 1024,
            ws_ack_queue: 256,
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// Everything [`crate::Controller::start`] needs.
#[derive(Clone)]
pub struct ControllerConfig {
    /// State directory. The controller creates it and keeps
    /// `ledger.sqlite`, `logs/`, `objects/`, `tls/` and `admin-token` in it.
    /// One controller process per directory.
    pub data_dir: PathBuf,
    /// Address to listen on. Port 0 picks a free port, reported in
    /// [`crate::ControllerHandle::local_addr`].
    pub listen: SocketAddr,
    /// The address workers and clients reach this controller at, as
    /// `https://host:port` or `http://host:port`. It is only used to build
    /// enrollment strings. When absent it is derived from the listen
    /// address; set it whenever the controller listens on a wildcard
    /// address.
    pub public_url: Option<String>,
    pub tls: Tls,
    /// Permit [`Tls::Disabled`] on an address other than loopback. Source
    /// code and tokens then cross the network unencrypted.
    pub allow_insecure: bool,
    /// Source of wall-clock time for every deadline the controller keeps.
    pub clock: Arc<dyn Clock>,
    pub tuning: Tuning,
}

impl ControllerConfig {
    /// TLS with a self-signed fleet certificate, the system clock and the
    /// default tuning.
    pub fn new(data_dir: impl Into<PathBuf>, listen: SocketAddr) -> Self {
        Self {
            data_dir: data_dir.into(),
            listen,
            public_url: None,
            tls: Tls::SelfSigned {
                subject_alt_names: Vec::new(),
            },
            allow_insecure: false,
            clock: Arc::new(SystemClock),
            tuning: Tuning::default(),
        }
    }
}

impl std::fmt::Debug for ControllerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerConfig")
            .field("data_dir", &self.data_dir)
            .field("listen", &self.listen)
            .field("public_url", &self.public_url)
            .field("tls", &self.tls)
            .field("allow_insecure", &self.allow_insecure)
            .field("tuning", &self.tuning)
            .finish_non_exhaustive()
    }
}
