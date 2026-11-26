//! Runs the controller and the worker as operating-system services, so they
//! start at boot or logon, are restarted after a crash and stop cleanly.
//!
//! [`render`] turns a [`ServiceSpec`] into the definition for any supported
//! operating system without touching the host. [`install`], [`uninstall`],
//! [`status`], [`start`] and [`stop`] act on the host's own service manager:
//!
//! | Host | Manager | [`Scope::System`] | [`Scope::User`] |
//! | --- | --- | --- | --- |
//! | Linux | systemd | unit in `/etc/systemd/system`, starts at boot (root to manage) | unit of the user's instance, starts at boot only with lingering |
//! | macOS | launchd | controller only: LaunchDaemon, starts at boot (root to manage) | LaunchAgent, starts at logon; needs a graphical session |
//! | Windows | Service Control Manager | real service, starts at boot (administrator to manage) | not available |
//!
//! Every definition restarts the program after a crash or a non-zero exit and
//! leaves it down after a clean exit or a requested stop. A stop gives the
//! program [`STOP_TIMEOUT`] before the platform ends it.
//!
//! # Ownership
//!
//! Each definition sets [`ENV_SERVICE_NAME`] for the program. That marker is
//! also how this crate recognises its own definitions: it refuses to replace
//! or remove a service that does not carry it.
//!
//! # Secrets
//!
//! Definitions are readable by other users of the machine. Environment keys
//! that look like secrets are refused; tokens belong in the program's private
//! files. A Windows account password is passed to the Service Control Manager
//! only, never rendered or stored by this crate.
//!
//! # Inside the service
//!
//! On Linux and macOS the service is an ordinary process: it is stopped with
//! `SIGTERM`. On Windows it must call `run_as_service`, which speaks to the
//! Service Control Manager and delivers the stop request as a
//! [`ShutdownSignal`]. [`running_as_service`] tells the two situations apart.

#![forbid(unsafe_code)]

mod error;
mod host;
mod render;
#[cfg(windows)]
mod service_main;
mod shutdown;
mod spec;

pub use bella_core::platform::Os;
pub use error::ServiceError;
pub use render::{Rendered, render};
#[cfg(windows)]
pub use service_main::run_as_service;
pub use shutdown::{ShutdownSignal, ShutdownTrigger, shutdown_channel};
pub use spec::{Role, Scope, ServiceId, ServiceSpec};

use bella_core::defaults::{CLEANUP_TIMEOUT, TERMINATE_GRACE};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Time allowed on top of attempt cleanup for the program itself to exit.
pub const STOP_MARGIN: Duration = Duration::from_secs(30);

/// How long a requested stop may take before the platform ends the program:
/// the cleanup budget, plus the polite-stop grace, plus [`STOP_MARGIN`].
pub const STOP_TIMEOUT: Duration = Duration::from_secs(
    CLEANUP_TIMEOUT.as_secs() + TERMINATE_GRACE.as_secs() + STOP_MARGIN.as_secs(),
);

/// Environment variable every definition sets to the service's name.
pub const ENV_SERVICE_NAME: &str = "BELLA_SERVICE_NAME";
/// Windows only: where the service should write its logs. A Windows service
/// has no standard output, so the program has to open files there itself.
pub const ENV_SERVICE_LOG_DIR: &str = "BELLA_SERVICE_LOG_DIR";
/// Windows only: the working directory `run_as_service` changes to, because
/// the Service Control Manager has no such setting.
pub const ENV_SERVICE_WORKING_DIR: &str = "BELLA_SERVICE_WORKING_DIR";

/// Whether this process was started from a definition written by this crate.
///
/// It reads [`ENV_SERVICE_NAME`], so a child that inherits the service's
/// environment reports `true` as well. On Windows `run_as_service` then fails
/// with [`ServiceError::NotStartedByServiceManager`], and the caller can fall
/// back to running in the console.
pub fn running_as_service() -> bool {
    std::env::var_os(ENV_SERVICE_NAME).is_some_and(|name| !name.is_empty())
}

/// The directory named by [`ENV_SERVICE_LOG_DIR`], when the definition set one.
pub fn service_log_dir() -> Option<PathBuf> {
    std::env::var_os(ENV_SERVICE_LOG_DIR)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// A Windows account password. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// Wrap a password.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(..)")
    }
}

/// How [`install`] should behave.
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Overwrite a definition of ours that differs from the requested one.
    pub replace: bool,
    /// Start the service now. Without it the service starts at the next boot
    /// or logon.
    pub start: bool,
    /// Password of the Windows account in [`ServiceSpec::account`]. Needed to
    /// create or replace a service that runs as an ordinary user. It is not
    /// compared with the installed one, so changing only the password needs
    /// `replace` together with another change, or a reinstall.
    pub password: Option<Password>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            replace: false,
            start: true,
            password: None,
        }
    }
}

/// What [`install`] did to the definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// There was no definition and one was written.
    Created,
    /// A different definition was overwritten.
    Replaced,
    /// An identical definition was already installed.
    Unchanged,
}

/// Result of a successful [`install`].
#[derive(Clone, Debug)]
pub struct Installed {
    /// Service name.
    pub name: String,
    /// The definition file, or the registry key of a Windows service.
    pub definition: PathBuf,
    /// What happened to the definition.
    pub outcome: InstallOutcome,
    /// State observed afterwards. Not necessarily `Running`, even when a
    /// start was requested: the program may have exited already.
    pub status: ServiceStatus,
    /// Things the operator should know, such as missing lingering.
    pub warnings: Vec<String>,
}

/// State of a service as its manager reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceStatus {
    /// The manager has no such service.
    NotInstalled,
    /// Installed and not running, after a clean stop or before the first start.
    Stopped,
    /// Running, starting or stopping. `pid` is the main process when the
    /// manager reports one.
    Running {
        /// Process id of the main process.
        pid: Option<u32>,
    },
    /// The program ended badly. The manager may be about to restart it.
    Failed {
        /// What the manager reports.
        reason: String,
    },
}

impl fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("not installed"),
            Self::Stopped => f.write_str("stopped"),
            Self::Running { pid: Some(pid) } => write!(f, "running (pid {pid})"),
            Self::Running { pid: None } => f.write_str("running"),
            Self::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}

/// Write, register, enable and (unless told otherwise) start a service.
///
/// Repeating a call with the same specification changes nothing and reports
/// [`InstallOutcome::Unchanged`]. A different installed definition is an
/// [`ServiceError::AlreadyInstalled`] error unless [`InstallOptions::replace`]
/// is set; replacing restarts a running service. Missing log and working
/// directories are created and, on Linux and macOS, handed to the account.
pub fn install(spec: &ServiceSpec, options: &InstallOptions) -> Result<Installed, ServiceError> {
    host::backend()?.install(spec, options)
}

/// Stop, disable and remove a service. `Ok(false)` means there was none.
/// Logs and data directories are kept.
pub fn uninstall(id: &ServiceId) -> Result<bool, ServiceError> {
    id.validate()?;
    host::backend()?.uninstall(id)
}

/// Report the state of a service. Changes nothing.
pub fn status(id: &ServiceId) -> Result<ServiceStatus, ServiceError> {
    id.validate()?;
    host::backend()?.status(id)
}

/// Start an installed service. Starting a running service is not an error.
pub fn start(id: &ServiceId) -> Result<(), ServiceError> {
    id.validate()?;
    host::backend()?.start(id)
}

/// Stop an installed service and wait for it, until the next boot or logon or
/// the next [`start`]. Stopping a stopped service is not an error.
pub fn stop(id: &ServiceId) -> Result<(), ServiceError> {
    id.validate()?;
    host::backend()?.stop(id)
}
