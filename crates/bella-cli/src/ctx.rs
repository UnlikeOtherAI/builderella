//! What every command shares: output mode, credentials, clients, the async
//! runtime and the shutdown signal.

use crate::error::{CliError, CliResult};
use crate::output::Out;
use bella_client::{BlockingClient, Client};
use bella_net::{ClientCredentials, CredentialsError, PrivateFileMode};
use std::path::PathBuf;

pub struct Ctx {
    pub out: Out,
    /// `--credentials` or `BELLA_CREDENTIALS`.
    pub credentials: Option<PathBuf>,
    pub verbose: u8,
}

impl Ctx {
    /// Where credentials are read from and saved to.
    pub fn credentials_path(&self) -> CliResult<PathBuf> {
        match &self.credentials {
            Some(path) => Ok(path.clone()),
            None => ClientCredentials::default_path().ok_or_else(|| {
                CliError::local(
                    "cannot tell where to keep credentials: no configuration directory is set \
                     in the environment; pass --credentials <file>",
                )
            }),
        }
    }

    /// The stored credentials, or `None` when there are none.
    pub fn try_credentials(&self) -> CliResult<Option<ClientCredentials>> {
        let path = self.credentials_path()?;
        match ClientCredentials::load(&path) {
            Ok((credentials, mode)) => {
                if let PrivateFileMode::TooOpen { mode } = mode {
                    self.out.note(format!(
                        "warning: {} is readable by other users (mode {mode:o}); treat its token \
                         as exposed, mint a new one and restrict the file to its owner",
                        path.display()
                    ));
                }
                Ok(Some(credentials))
            }
            Err(CredentialsError::File(error)) if error.is_not_found() => Ok(None),
            Err(error) => Err(CliError::local(error.to_string())),
        }
    }

    pub fn credentials(&self) -> CliResult<ClientCredentials> {
        self.try_credentials()?.ok_or_else(|| {
            CliError::local(format!(
                "not logged in: no credentials at {}; run `bella login <url> --fingerprint <fp> \
                 --token-stdin` first",
                self.credentials_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            ))
        })
    }

    pub fn client(&self) -> CliResult<Client> {
        Ok(Client::from_credentials(&self.credentials()?)?)
    }

    pub fn blocking(&self) -> CliResult<BlockingClient> {
        Ok(BlockingClient::from_credentials(&self.credentials()?)?)
    }
}

/// A multi-threaded runtime for the commands that run async code. It is
/// only ever entered through `block_on`, so code outside that call is not on
/// a runtime thread and may use the blocking client.
pub fn runtime() -> CliResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bella")
        .build()
        .map_err(|error| CliError::local(format!("could not start the async runtime: {error}")))
}

/// Completes on Ctrl-C, and on Unix also on `SIGTERM`.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
                return;
            }
            Err(error) => tracing::warn!(%error, "cannot listen for SIGTERM; only Ctrl-C stops"),
        }
    }
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "cannot listen for Ctrl-C");
        std::future::pending::<()>().await;
    }
}

/// Completes on Ctrl-C only: the "stop waiting" gesture of `wait` and
/// `submit --wait`.
pub async fn interrupted() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
