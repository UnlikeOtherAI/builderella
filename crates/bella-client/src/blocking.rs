//! Synchronous access to the same calls, for programs without an async
//! runtime of their own: simple CLI commands and the MCP bridge.

use crate::client::{Client, ClientOptions};
use crate::error::ClientError;
use crate::events::EventStream;
use crate::object_store::HttpObjectStore;
use bella_core::api::{
    ArtifactView, CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, EnrollResponse,
    EnvironmentView, EventRecord, ExplainResponse, FleetOverview, JobView, ListJobsQuery, LogPage,
    LogQuery, MachineView, MissingObjectsRequest, MissingObjectsResponse, PairingCodeResponse,
    PlanRequest, PlanResponse, ProjectPolicy, ProjectView, QueueView, RegisterSnapshotRequest,
    SnapshotView, SubmitJobRequest, TokenResponse, UpdateMachineRequest, WaitQuery, WaitResponse,
    WhoAmI,
};
use bella_core::{EnvironmentId, JobId, MachineId, SnapshotId};
use bella_net::{ClientCredentials, ControllerUrl, Fingerprint};
use std::future::Future;
use std::sync::Arc;
use tokio::runtime::{Handle, Runtime};

const IN_RUNTIME: &str = "a blocking client call was made from a thread that belongs to a Tokio \
     runtime (an async task or a spawn_blocking closure); use the async Client there, or call \
     from a plain thread";

/// A runtime that can be dropped from any thread.
///
/// Dropping a plain `Runtime` inside asynchronous code panics, because the
/// drop waits for the runtime's threads. This one is shut down in the
/// background instead.
pub(crate) struct OwnedRuntime {
    /// Present until drop.
    runtime: Option<Runtime>,
    handle: Handle,
}

impl OwnedRuntime {
    fn new() -> Result<Self, ClientError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("bella-client")
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| {
                ClientError::invalid(format!("could not start the client runtime: {e}"))
            })?;
        let handle = runtime.handle().clone();
        Ok(Self {
            runtime: Some(runtime),
            handle,
        })
    }

    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Runs `future` to completion on the calling thread, unless that
    /// thread belongs to a runtime.
    fn block_on<F: Future>(&self, future: F) -> Result<F::Output, ClientError> {
        if Handle::try_current().is_ok() {
            return Err(ClientError::invalid(IN_RUNTIME));
        }
        Ok(self.handle.block_on(future))
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for OwnedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedRuntime").finish_non_exhaustive()
    }
}

/// A [`Client`] with its own small runtime, exposing every call as a
/// blocking method.
///
/// Each method does what the method of the same name on [`Client`] does,
/// with the same errors and retries, and blocks the calling thread until it
/// is done. Cheap to clone; clones share the runtime and the connections.
///
/// Calls must come from a thread that does not belong to a Tokio runtime.
/// From an async task or a `spawn_blocking` closure every call returns
/// [`ClientError::Invalid`] instead of blocking the runtime or panicking.
/// Creating and dropping a `BlockingClient` is allowed anywhere.
#[derive(Clone, Debug)]
pub struct BlockingClient {
    client: Client,
    runtime: Arc<OwnedRuntime>,
}

impl BlockingClient {
    /// See [`Client::new`].
    pub fn new(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        token: Option<String>,
    ) -> Result<Self, ClientError> {
        Self::from_client(Client::new(url, fingerprint, token)?)
    }

    /// See [`Client::from_credentials`].
    pub fn from_credentials(credentials: &ClientCredentials) -> Result<Self, ClientError> {
        Self::from_client(Client::from_credentials(credentials)?)
    }

    /// See [`Client::with_options`].
    pub fn with_options(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        token: Option<String>,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        Self::from_client(Client::with_options(url, fingerprint, token, options)?)
    }

    /// Wraps an existing client in a new runtime.
    pub fn from_client(client: Client) -> Result<Self, ClientError> {
        Ok(Self {
            client,
            runtime: Arc::new(OwnedRuntime::new()?),
        })
    }

    /// The asynchronous client underneath. Its futures can be awaited on
    /// any runtime.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The controller's object store, running its requests on this
    /// client's runtime. The store keeps that runtime alive.
    pub fn object_store(&self) -> HttpObjectStore {
        HttpObjectStore::with_runtime(self.client.clone(), self.runtime.clone())
    }

    /// The event stream as a blocking iterator. See [`Client::events`]: it
    /// never ends by itself, and an `Err` item does not end it.
    pub fn events(&self, after: Option<u64>) -> BlockingEvents {
        BlockingEvents {
            stream: self.client.events(after),
            runtime: self.runtime.clone(),
        }
    }

    fn run<T>(
        &self,
        future: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        self.runtime.block_on(future)?
    }
}

/// Blocking iterator over the controller's events.
#[derive(Debug)]
pub struct BlockingEvents {
    stream: EventStream,
    runtime: Arc<OwnedRuntime>,
}

impl Iterator for BlockingEvents {
    type Item = Result<EventRecord, ClientError>;

    /// Blocks until the next event or error. Never returns `None`.
    fn next(&mut self) -> Option<Self::Item> {
        match self.runtime.block_on(self.stream.next()) {
            Ok(item) => item,
            Err(error) => Some(Err(error)),
        }
    }
}

macro_rules! blocking_calls {
    ($(
        fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty;
    )*) => {
        impl BlockingClient {
            $(
                #[doc = concat!("Blocking [`Client::", stringify!($name), "`].")]
                pub fn $name(&self $(, $arg: $ty)*) -> Result<$ret, ClientError> {
                    self.run(self.client.$name($($arg),*))
                }
            )*
        }
    };
}

blocking_calls! {
    fn health(&self) -> ();
    fn version(&self) -> serde_json::Value;
    fn whoami(&self) -> WhoAmI;
    fn create_token(&self, request: &CreateTokenRequest) -> TokenResponse;
    fn create_pairing_code(&self, request: &CreatePairingCodeRequest) -> PairingCodeResponse;
    fn enroll(&self, request: &EnrollRequest) -> EnrollResponse;
    fn missing_objects(&self, request: &MissingObjectsRequest) -> MissingObjectsResponse;
    fn register_snapshot(&self, request: &RegisterSnapshotRequest) -> SnapshotView;
    fn snapshot(&self, id: &SnapshotId) -> SnapshotView;
    fn projects(&self) -> Vec<ProjectView>;
    fn put_project(&self, name: &str, policy: &ProjectPolicy) -> ProjectView;
    fn plan(&self, request: &PlanRequest) -> PlanResponse;
    fn submit_job(&self, request: &SubmitJobRequest, idempotency_key: &str) -> JobView;
    fn jobs(&self, query: &ListJobsQuery) -> Vec<JobView>;
    fn job(&self, id: &JobId) -> JobView;
    fn wait_job(&self, id: &JobId, query: &WaitQuery) -> WaitResponse;
    fn cancel_job(&self, id: &JobId) -> JobView;
    fn job_logs(&self, id: &JobId, query: &LogQuery) -> LogPage;
    fn job_artifacts(&self, id: &JobId) -> Vec<ArtifactView>;
    fn explain_job(&self, id: &JobId) -> ExplainResponse;
    fn environments(&self) -> Vec<EnvironmentView>;
    fn environment(&self, id: &EnvironmentId) -> EnvironmentView;
    fn renew_environment(&self, id: &EnvironmentId) -> EnvironmentView;
    fn finish_environment(&self, id: &EnvironmentId) -> EnvironmentView;
    fn fleet(&self) -> FleetOverview;
    fn machine(&self, id: &MachineId) -> MachineView;
    fn update_machine(&self, id: &MachineId, request: &UpdateMachineRequest) -> MachineView;
    fn queue(&self) -> QueueView;
}
