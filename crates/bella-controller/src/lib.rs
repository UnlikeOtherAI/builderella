//! Controller: HTTP API, worker hub, dispatch loop and embedded dashboard.
//!
//! One process owns one data directory. It is the only writer of the ledger
//! and the only scheduler. [`Controller::start`] is a library call, so tests
//! and the `bella` binary run the same code in-process.
//!
//! ```no_run
//! # async fn demo() -> Result<(), bella_controller::ControllerError> {
//! use bella_controller::{Controller, ControllerConfig};
//!
//! let config = ControllerConfig::new("/var/lib/builderella", "0.0.0.0:7420".parse().unwrap());
//! let controller = Controller::start(config).await?;
//! println!("listening on {}", controller.local_addr);
//! controller.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # What runs
//!
//! * **HTTP API** under `/api/v1` (routes in `bella_core::api`). Every route
//!   but `GET /healthz` and `POST /enroll` needs a bearer token, looked up
//!   in the ledger on each request. Failures are `ApiError` JSON with the
//!   status of their code.
//! * **Worker hub** at `GET /api/v1/worker/ws`: one connection per machine.
//!   `Hello` is reconciled against the ledger before `Welcome` and
//!   `Reconcile` are sent, and only then is the machine offered work.
//!   Events and logs are acknowledged after the ledger has committed them.
//! * **Dispatch loop**: the only place offers are created. It reserves in
//!   the ledger, then sends the offer; the worker's acceptance commits the
//!   start permit, and only then is `Start` sent (`docs/architecture.md` §3).
//! * **Sweeps**: offer expiry, authority renewal, stale and unreachable
//!   marking, cancellation and environment-finish retransmission,
//!   environment lease expiry, object garbage collection.
//! * **Dashboard** at `/`, embedded at build time when it has been built.
//!
//! # What a start does
//!
//! Opening the ledger bumps the controller epoch and marks every machine
//! disconnected. Offers the previous epoch left without a start permit are
//! withdrawn; until that is done [`FleetOverview::reconciling`] is true and
//! nothing is dispatched. Attempts that hold a start permit stay held until
//! their worker reconnects and accounts for them.
//!
//! # Guarantees and their limits
//!
//! * A machine's silence releases nothing. Stale and unreachable are flags;
//!   reservations return only on a worker's release proof, a proven reboot
//!   or an administrator's word.
//! * An event from an attempt that is not its job's current generation is
//!   dropped and never sets a result.
//! * A worker that stops reading or floods its socket is disconnected or
//!   slowed down on its own connection; no other worker and no API request
//!   waits for it.
//! * `POST /enroll` is rate limited per source address and fleet-wide; see
//!   [`EnrollLimits`].
//! * With [`Tls::Disabled`] tokens and source cross the network in the
//!   clear. It is refused on anything but loopback unless
//!   [`ControllerConfig::allow_insecure`] is set.
//!
//! [`FleetOverview::reconciling`]: bella_core::api::FleetOverview::reconciling

mod auth;
mod config;
mod dashboard;
mod dispatch;
mod error;
mod fleet;
mod http;
mod hub;
mod ratelimit;
mod server;
mod state;
mod sweeps;
mod worker;

pub use config::{ControllerConfig, EnrollLimits, Tls, Tuning};
pub use error::ControllerError;
pub use fleet::{CAP_CASE_SENSITIVE_FS, CAP_SYMLINKS};

use bella_core::api::Role;
use bella_ledger::Ledger;
use bella_net::{
    ControllerUrl, Fingerprint, Host, PrivateFileMode, Scheme, ServerIdentity, Token, TokenKind,
};
use bella_store::{FsStore, StoreOptions};
use state::{App, Shared};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// File in the data directory holding the bootstrap administrator token.
pub const ADMIN_TOKEN_FILE: &str = "admin-token";
const LEDGER_FILE: &str = "ledger.sqlite";
const OBJECTS_DIR: &str = "objects";
const TLS_DIR: &str = "tls";
const BOOTSTRAP_ADMIN_NAME: &str = "bootstrap-admin";

/// Whether a built dashboard is embedded in this binary. Without one, `/`
/// serves a placeholder page that says how to build it.
pub fn dashboard_embedded() -> bool {
    dashboard::is_embedded()
}

/// The controller process, as a library.
pub struct Controller;

/// What a garbage collection pass did, from
/// [`ControllerHandle::collect_garbage`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcSummary {
    pub objects_removed: u64,
    pub objects_live: u64,
    /// Unreferenced but younger than the grace period, so kept.
    pub objects_in_grace: u64,
    pub bytes_freed: u64,
}

/// A running controller. Dropping it without calling
/// [`ControllerHandle::shutdown`] stops the controller without waiting for
/// open requests.
pub struct ControllerHandle {
    /// The address actually bound; differs from the configured one when the
    /// port was 0.
    pub local_addr: SocketAddr,
    /// SHA-256 fingerprint of the fleet certificate clients pin. `None` when
    /// serving plain HTTP.
    pub fingerprint: Option<Fingerprint>,
    /// The administrator token, only on the start that created it. It is
    /// also in `<data_dir>/admin-token`, readable by the owner only, and is
    /// never logged.
    pub bootstrap_admin_token: Option<String>,
    app: App,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ControllerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerHandle")
            .field("local_addr", &self.local_addr)
            .field("fingerprint", &self.fingerprint)
            .field(
                "bootstrap_admin_token",
                &self.bootstrap_admin_token.as_ref().map(|_| "<redacted>"),
            )
            .finish_non_exhaustive()
    }
}

/// Everything opened from the data directory, on a blocking thread.
struct Opened {
    ledger: Ledger,
    store: FsStore,
    identity: Option<ServerIdentity>,
    bootstrap_admin_token: Option<String>,
}

fn open_data_dir(config: &ControllerConfig) -> Result<Opened, ControllerError> {
    let dir = &config.data_dir;
    std::fs::create_dir_all(dir).map_err(ControllerError::DataDir)?;

    let identity = match &config.tls {
        Tls::SelfSigned { subject_alt_names } => {
            let mut names = subject_alt_names.clone();
            if names.is_empty() {
                if !config.listen.ip().is_unspecified() {
                    names.push(config.listen.ip().to_string());
                }
                names.push("localhost".to_string());
            }
            let identity = ServerIdentity::load_or_generate_at(
                &dir.join(TLS_DIR),
                &names,
                config.clock.as_ref(),
            )?;
            if let PrivateFileMode::TooOpen { .. } = identity.key_file_mode() {
                warn!(
                    "the fleet private key is readable by other users; treat it as exposed and \
                     tighten its permissions"
                );
            }
            Some(identity)
        }
        Tls::Disabled => None,
    };

    let ledger = Ledger::open(dir.join(LEDGER_FILE), config.clock.clone())?;
    let store = FsStore::open_with(
        dir.join(OBJECTS_DIR),
        StoreOptions {
            max_object_bytes: config.tuning.max_object_bytes,
            clock: config.clock.clone(),
        },
    )?;
    let bootstrap_admin_token = bootstrap_admin(&ledger, dir)?;
    Ok(Opened {
        ledger,
        store,
        identity,
        bootstrap_admin_token,
    })
}

/// On the first start of a data directory, mint the administrator every
/// other credential descends from. The token is written to a private file
/// before its principal exists, so a crash in between leaves a file without
/// authority, never an administrator whose token was lost.
fn bootstrap_admin(ledger: &Ledger, dir: &Path) -> Result<Option<String>, ControllerError> {
    if !ledger.principals()?.is_empty() {
        return Ok(None);
    }
    let failed = |error: &dyn std::fmt::Display| ControllerError::AdminToken(error.to_string());
    let (token, hash) = Token::generate(TokenKind::Admin).map_err(|error| failed(&error))?;
    bella_net::write_private(&dir.join(ADMIN_TOKEN_FILE), format!("{token}\n").as_bytes())
        .map_err(|error| failed(&error))?;
    ledger.create_principal(BOOTSTRAP_ADMIN_NAME, Role::Admin, &hash.to_hex())?;
    Ok(Some(token))
}

/// Where enrollment strings point: the configured public URL, or one derived
/// from the listen address.
fn public_url(
    config: &ControllerConfig,
    local_addr: SocketAddr,
) -> Result<ControllerUrl, ControllerError> {
    let invalid = |error: &dyn std::fmt::Display| ControllerError::PublicUrl(error.to_string());
    let tls = matches!(config.tls, Tls::SelfSigned { .. });
    if let Some(text) = &config.public_url {
        let url =
            ControllerUrl::parse(text, config.allow_insecure).map_err(|error| invalid(&error))?;
        if url.is_tls() != tls {
            return Err(ControllerError::PublicUrl(format!(
                "`{text}` does not match the listener, which serves {}",
                if tls { "https" } else { "http" }
            )));
        }
        return Ok(url);
    }
    let ip = local_addr.ip();
    let host = if !ip.is_unspecified() {
        Host::parse(&ip.to_string())
    } else {
        // A wildcard address is not somewhere a worker can connect to. Use a
        // name the certificate was issued for, else loopback.
        let named = match &config.tls {
            Tls::SelfSigned { subject_alt_names } => subject_alt_names
                .iter()
                .find_map(|name| Host::parse(name).ok()),
            Tls::Disabled => None,
        };
        match named {
            Some(host) => Ok(host),
            None => {
                warn!(
                    "listening on a wildcard address without a public URL; enrollment strings \
                     will point at loopback"
                );
                Host::parse(&IpAddr::from(std::net::Ipv4Addr::LOCALHOST).to_string())
            }
        }
    }
    .map_err(|error| invalid(&error))?;
    let scheme = if tls { Scheme::Https } else { Scheme::Http };
    ControllerUrl::new(scheme, host, local_addr.port(), config.allow_insecure)
        .map_err(|error| invalid(&error))
}

/// Read new events from the ledger and hand them to event streams and long
/// polls. It also looks on a timer, so an event written without a wake-up
/// is delayed, never lost.
async fn pump_events(app: App) {
    let mut position = match app.db(|ledger| ledger.last_event_seq()).await {
        Ok(seq) => seq,
        Err(error) => {
            warn!(%error, "could not read the event position");
            0
        }
    };
    loop {
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            () = app.events.wait_for_touch() => {}
            () = tokio::time::sleep(app.tuning.event_poll_interval) => {}
        }
        loop {
            let after = position;
            match app.db(move |ledger| ledger.events_after(after, 1000)).await {
                Ok(events) if events.is_empty() => break,
                Ok(events) => {
                    for event in events {
                        position = event.seq;
                        app.events.publish(event);
                    }
                }
                Err(error) => {
                    warn!(%error, "could not read new events");
                    break;
                }
            }
        }
    }
}

impl Controller {
    /// Open the data directory, take over the ledger, bind the listener and
    /// start serving. Returns once the controller accepts connections.
    ///
    /// Fails without side effects on the network when the configuration is
    /// refused, the data directory cannot be opened or the address cannot be
    /// bound. The ledger's epoch is bumped as soon as it is opened, which
    /// fences off any controller still running on the same directory.
    pub async fn start(config: ControllerConfig) -> Result<ControllerHandle, ControllerError> {
        if config.tls == Tls::Disabled
            && !config.listen.ip().is_loopback()
            && !config.allow_insecure
        {
            return Err(ControllerError::InsecureListen(config.listen));
        }

        let for_open = config.clone();
        let opened = tokio::task::spawn_blocking(move || open_data_dir(&for_open))
            .await
            .map_err(|join| ControllerError::Task(join.to_string()))??;
        let Opened {
            ledger,
            store,
            identity,
            bootstrap_admin_token,
        } = opened;

        let (fingerprint, tls) = match &identity {
            Some(identity) => (
                Some(identity.fingerprint()),
                Some(bella_net::server_config(identity)?),
            ),
            None => (None, None),
        };

        let listener =
            TcpListener::bind(config.listen)
                .await
                .map_err(|source| ControllerError::Bind {
                    addr: config.listen,
                    source,
                })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ControllerError::Bind {
                addr: config.listen,
                source,
            })?;
        let url = public_url(&config, local_addr)?;

        let app: App = Arc::new(Shared::new(
            Arc::new(ledger),
            Arc::new(store),
            config.clock.clone(),
            config.tuning.clone(),
            fingerprint,
            url,
        ));
        info!(
            addr = %local_addr,
            epoch = app.epoch,
            fleet_id = %app.fleet_id,
            tls = tls.is_some(),
            fingerprint = %fingerprint.map(|f| f.to_base64url()).unwrap_or_default(),
            public_url = %app.public_url,
            dashboard = dashboard::is_embedded(),
            first_start = bootstrap_admin_token.is_some(),
            "controller started"
        );

        let router = http::router(app.clone());
        let tasks = vec![
            tokio::spawn(pump_events(app.clone())),
            tokio::spawn(dispatch::run(app.clone())),
            tokio::spawn(sweeps::run(app.clone())),
            tokio::spawn(sweeps::run_gc(app.clone())),
            tokio::spawn(server::run(app.clone(), listener, tls, router)),
        ];
        Ok(ControllerHandle {
            local_addr,
            fingerprint,
            bootstrap_admin_token,
            app,
            tasks,
        })
    }
}

impl ControllerHandle {
    /// The controller epoch of this start: one more than the previous start
    /// of the same data directory.
    pub fn epoch(&self) -> u64 {
        self.app.epoch
    }

    /// The address enrollment strings name, as `scheme://host:port`.
    pub fn public_url(&self) -> String {
        self.app.public_url.to_string()
    }

    /// Whether start-up recovery is still running. Nothing is dispatched
    /// while it is.
    pub fn is_reconciling(&self) -> bool {
        self.app.is_reconciling()
    }

    /// Run one object garbage collection pass now, in addition to the
    /// periodic ones.
    pub async fn collect_garbage(&self) -> Result<GcSummary, ControllerError> {
        let report = sweeps::collect_garbage(&self.app)
            .await
            .map_err(|failure| ControllerError::Task(failure.message().to_string()))?;
        Ok(GcSummary {
            objects_removed: report.objects_removed,
            objects_live: report.objects_live,
            objects_in_grace: report.objects_in_grace,
            bytes_freed: report.bytes_freed,
        })
    }

    /// Stop gracefully: accept no new connections, tell every worker why its
    /// socket closes, let open requests finish within the shutdown grace,
    /// and stop the background tasks. Everything acknowledged before this
    /// returns is already durable in the ledger; nothing is released.
    pub async fn shutdown(mut self) {
        info!("controller shutting down");
        let workers = self.app.hub.close_all("the controller is shutting down");
        self.app.shutdown.cancel();
        let grace = self.app.tuning.shutdown_grace;
        let farewells = async {
            for worker in &workers {
                worker.finished().await;
            }
        };
        if tokio::time::timeout(grace, farewells).await.is_err() {
            warn!("some worker connections did not close within the shutdown grace");
        }
        for mut task in std::mem::take(&mut self.tasks) {
            // Each task ends on its own once shutdown is signalled; the
            // listener is given its grace and a little more.
            if tokio::time::timeout(grace * 2, &mut task).await.is_err() {
                task.abort();
            }
        }
        info!("controller stopped");
    }
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        self.app.shutdown.cancel();
        self.app.hub.close_all("the controller is shutting down");
        for task in &self.tasks {
            task.abort();
        }
    }
}
