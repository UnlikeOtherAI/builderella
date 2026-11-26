//! Typed errors.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Why a service operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The specification cannot be turned into a definition.
    #[error("invalid service specification: {0}")]
    InvalidSpec(String),
    /// This operating system or scope cannot do what was asked.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The caller lacks the rights to manage this service.
    #[error("insufficient privileges to {action}: {hint}")]
    NeedsPrivilege {
        /// What was being attempted.
        action: String,
        /// How to get the rights.
        hint: &'static str,
    },
    /// macOS: a LaunchAgent can only be loaded into a graphical session.
    #[error(
        "user {uid} has no graphical login session, which a LaunchAgent needs: \
         log that user in on the machine's screen and try again (an SSH login is not enough)"
    )]
    NoGuiSession {
        /// The user without a session.
        uid: u32,
    },
    /// Windows: an ordinary account cannot run a service without its password.
    #[error("a password is required to run the service as `{account}`")]
    PasswordRequired {
        /// The account.
        account: String,
    },
    /// A different definition is installed and replacing was not requested.
    #[error(
        "service `{name}` is already installed with a different {differences}; \
         ask for replacement to overwrite it"
    )]
    AlreadyInstalled {
        /// Service name.
        name: String,
        /// What differs.
        differences: String,
    },
    /// The installed service was not created by this crate.
    #[error("service `{name}` was not installed by Builderella and is left alone")]
    NotManaged {
        /// Service name.
        name: String,
    },
    /// The operation needs an installed service.
    #[error("service `{name}` is not installed")]
    NotInstalled {
        /// Service name.
        name: String,
    },
    /// The service manager could not start the service.
    #[error("service `{name}` did not start: {reason}")]
    StartFailed {
        /// Service name.
        name: String,
        /// What the manager reported.
        reason: String,
    },
    /// The service manager did not finish in time.
    #[error("service `{name}` did not {action} within {} s", after.as_secs())]
    Timeout {
        /// Service name.
        name: String,
        /// What was awaited.
        action: &'static str,
        /// How long was waited.
        after: Duration,
    },
    /// A management command failed.
    #[error("`{command}` failed ({status}): {output}")]
    Command {
        /// The command line.
        command: String,
        /// Exit status.
        status: String,
        /// Its error output.
        output: String,
    },
    /// A file or process operation failed.
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or program.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The Windows Service Control Manager or registry reported an error.
    #[error("Windows service manager: could not {action}: {reason}")]
    Manager {
        /// What was being done.
        action: String,
        /// The OS error.
        reason: String,
    },
    /// Windows: the process was not started by the Service Control Manager.
    #[error("this process was not started by the Windows Service Control Manager")]
    NotStartedByServiceManager,
    /// The body given to `run_as_service` failed or panicked.
    #[error("the service failed: {0}")]
    Body(String),
}
