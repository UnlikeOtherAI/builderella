//! One method per route of `bella_core::api`.

use crate::client::{Call, Client};
use crate::error::ClientError;
use bella_core::api::{
    ArtifactView, CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, EnrollResponse,
    EnvironmentView, ExplainResponse, FleetOverview, IDEMPOTENCY_HEADER, JobView, ListJobsQuery,
    LogPage, LogQuery, MachineView, MissingObjectsRequest, MissingObjectsResponse,
    PairingCodeResponse, PlanRequest, PlanResponse, ProjectPolicy, ProjectView, QueueView,
    RegisterSnapshotRequest, SnapshotView, SubmitJobRequest, TokenResponse, UpdateMachineRequest,
    WaitQuery, WaitResponse, WhoAmI,
};
use bella_core::{EnvironmentId, JobId, MachineId, SnapshotId};
use reqwest::Method;
use reqwest::header::{HeaderName, HeaderValue};
use std::time::{Duration, Instant};

/// The longest the controller holds `GET /jobs/{id}/wait` open.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// A fresh idempotency key for [`Client::submit_job`].
///
/// Generate one key per intended job and reuse it for every repeat of that
/// submission, including repeats after a crash of the caller: the
/// controller returns the same job for the same key.
pub fn new_idempotency_key() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

impl Client {
    /// `GET /healthz`: the controller is up. Needs no token. The body of
    /// the answer is not interpreted.
    pub async fn health(&self) -> Result<(), ClientError> {
        let call = Call::new(Method::GET, self.endpoint(&["healthz"])?).unauthenticated();
        self.fetch(&call).await.map(|_| ())
    }

    /// `GET /whoami`: the principal behind this client's token.
    pub async fn whoami(&self) -> Result<WhoAmI, ClientError> {
        self.json(&Call::new(Method::GET, self.endpoint(&["whoami"])?))
            .await
    }

    /// `POST /tokens` (admin): mint a token. Sent once: a repeat would mint
    /// a second token.
    pub async fn create_token(
        &self,
        request: &CreateTokenRequest,
    ) -> Result<TokenResponse, ClientError> {
        let call = Call::new(Method::POST, self.endpoint(&["tokens"])?).json(request)?;
        self.json(&call).await
    }

    /// `POST /enroll/pairing-codes` (admin): create a single-use pairing
    /// code. Sent once.
    pub async fn create_pairing_code(
        &self,
        request: &CreatePairingCodeRequest,
    ) -> Result<PairingCodeResponse, ClientError> {
        let call =
            Call::new(Method::POST, self.endpoint(&["enroll", "pairing-codes"])?).json(request)?;
        self.json(&call).await
    }

    /// `POST /enroll`: exchange a pairing code for a machine identity. The
    /// code in the body is the credential, so no token is sent. Sent once:
    /// the code is single use.
    pub async fn enroll(&self, request: &EnrollRequest) -> Result<EnrollResponse, ClientError> {
        let call = Call::new(Method::POST, self.endpoint(&["enroll"])?)
            .unauthenticated()
            .json(request)?;
        self.json(&call).await
    }

    /// `POST /objects/missing`: which of these objects the controller does
    /// not have. A lookup, so it is retried.
    pub async fn missing_objects(
        &self,
        request: &MissingObjectsRequest,
    ) -> Result<MissingObjectsResponse, ClientError> {
        let call = Call::new(Method::POST, self.endpoint(&["objects", "missing"])?)
            .retry()
            .json(request)?;
        self.json(&call).await
    }

    /// `POST /snapshots`: publish a snapshot whose objects are uploaded.
    pub async fn register_snapshot(
        &self,
        request: &RegisterSnapshotRequest,
    ) -> Result<SnapshotView, ClientError> {
        let call = Call::new(Method::POST, self.endpoint(&["snapshots"])?).json(request)?;
        self.json(&call).await
    }

    /// `GET /snapshots/{id}`.
    pub async fn snapshot(&self, id: &SnapshotId) -> Result<SnapshotView, ClientError> {
        let url = self.endpoint(&["snapshots", id.digest().as_str()])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `GET /projects`.
    pub async fn projects(&self) -> Result<Vec<ProjectView>, ClientError> {
        self.json(&Call::new(Method::GET, self.endpoint(&["projects"])?))
            .await
    }

    /// `PUT /projects/{name}` (admin): create the project or replace its
    /// policy.
    pub async fn put_project(
        &self,
        name: &str,
        policy: &ProjectPolicy,
    ) -> Result<ProjectView, ClientError> {
        let call = Call::new(Method::PUT, self.endpoint(&["projects", name])?).json(policy)?;
        self.json(&call).await
    }

    /// `POST /plan`: could the fleet run this, and where? Changes nothing.
    pub async fn plan(&self, request: &PlanRequest) -> Result<PlanResponse, ClientError> {
        let call = Call::new(Method::POST, self.endpoint(&["plan"])?).json(request)?;
        self.json(&call).await
    }

    /// `POST /jobs`: submit a job.
    ///
    /// `idempotency_key` is required (see [`new_idempotency_key`]). Because
    /// of it the request is retried after a transient failure, every time
    /// with the same key, so a submission whose answer was lost yields the
    /// job that was created and not a second one.
    pub async fn submit_job(
        &self,
        request: &SubmitJobRequest,
        idempotency_key: &str,
    ) -> Result<JobView, ClientError> {
        if idempotency_key.trim().is_empty() {
            return Err(ClientError::invalid(
                "submit_job needs a non-empty idempotency key",
            ));
        }
        let key = HeaderValue::from_str(idempotency_key).map_err(|_| {
            ClientError::invalid(
                "the idempotency key contains characters that cannot be sent in a header",
            )
        })?;
        let call = Call::new(Method::POST, self.endpoint(&["jobs"])?)
            .header(HeaderName::from_static(IDEMPOTENCY_HEADER), key)
            .retry()
            .json(request)?;
        self.json(&call).await
    }

    /// `GET /jobs`.
    pub async fn jobs(&self, query: &ListJobsQuery) -> Result<Vec<JobView>, ClientError> {
        let call = Call::new(Method::GET, self.endpoint(&["jobs"])?).query(query)?;
        self.json(&call).await
    }

    /// `GET /jobs/{id}`: the job with its frozen specification.
    pub async fn job(&self, id: &JobId) -> Result<JobView, ClientError> {
        let url = self.endpoint(&["jobs", id.as_str()])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `GET /jobs/{id}/wait`: long poll until the job has an outcome (or an
    /// event newer than `after_event`), or the timeout passes.
    ///
    /// `timeout_ms` is clamped to the controller's limit of
    /// [`MAX_WAIT`]. The HTTP timeout is that wait plus the configured
    /// request timeout, so the answer to a full-length poll still arrives.
    /// A timeout is a normal answer with `timed_out` set and cancels
    /// nothing.
    ///
    /// After a transient failure the poll is repeated for the time that is
    /// left, so the call as a whole stays within the wait asked for.
    pub async fn wait_job(
        &self,
        id: &JobId,
        query: &WaitQuery,
    ) -> Result<WaitResponse, ClientError> {
        let max_ms = u64::try_from(MAX_WAIT.as_millis()).unwrap_or(u64::MAX);
        let requested_ms = query.timeout_ms.map(|ms| ms.min(max_ms));
        let wait = requested_ms.map_or(MAX_WAIT, Duration::from_millis);
        let started = Instant::now();
        let url = self.endpoint(&["jobs", id.as_str(), "wait"])?;
        let mut first = true;
        self.with_retries(true, || {
            let remaining = wait.saturating_sub(started.elapsed());
            let query = WaitQuery {
                // The first try sends what the caller asked for, absent or
                // not; a repeat only has the remaining time.
                timeout_ms: if first {
                    requested_ms
                } else {
                    Some(u64::try_from(remaining.as_millis()).unwrap_or(max_ms))
                },
                after_event: query.after_event,
            };
            first = false;
            let url = url.clone();
            async move {
                let call = Call::new(Method::GET, url)
                    .query(&query)?
                    .timeout(remaining.saturating_add(self.options().request_timeout));
                self.json_once(&call).await
            }
        })
        .await
    }

    /// `POST /jobs/{id}/cancel`: request cancellation. Idempotent on the
    /// controller, so it is retried. The returned view shows how far
    /// cleanup got; cancellation is complete only when the job is settled.
    pub async fn cancel_job(&self, id: &JobId) -> Result<JobView, ClientError> {
        let url = self.endpoint(&["jobs", id.as_str(), "cancel"])?;
        self.json(&Call::new(Method::POST, url).retry()).await
    }

    /// `GET /jobs/{id}/logs`: one bounded page of log output. The text is
    /// untrusted output of the build.
    pub async fn job_logs(&self, id: &JobId, query: &LogQuery) -> Result<LogPage, ClientError> {
        let url = self.endpoint(&["jobs", id.as_str(), "logs"])?;
        self.json(&Call::new(Method::GET, url).query(query)?).await
    }

    /// `GET /jobs/{id}/artifacts`.
    pub async fn job_artifacts(&self, id: &JobId) -> Result<Vec<ArtifactView>, ClientError> {
        let url = self.endpoint(&["jobs", id.as_str(), "artifacts"])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `GET /jobs/{id}/explain`: why the job is where it is.
    pub async fn explain_job(&self, id: &JobId) -> Result<ExplainResponse, ClientError> {
        let url = self.endpoint(&["jobs", id.as_str(), "explain"])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `GET /environments`.
    pub async fn environments(&self) -> Result<Vec<EnvironmentView>, ClientError> {
        self.json(&Call::new(Method::GET, self.endpoint(&["environments"])?))
            .await
    }

    /// `GET /environments/{id}`.
    pub async fn environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ClientError> {
        let url = self.endpoint(&["environments", id.as_str()])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `POST /environments/{id}/renew`: extend the lease within policy.
    /// Idempotent on the controller, so it is retried.
    pub async fn renew_environment(
        &self,
        id: &EnvironmentId,
    ) -> Result<EnvironmentView, ClientError> {
        let url = self.endpoint(&["environments", id.as_str(), "renew"])?;
        self.json(&Call::new(Method::POST, url).retry()).await
    }

    /// `POST /environments/{id}/finish`: ask for the environment to be torn
    /// down. Idempotent on the controller, so it is retried.
    pub async fn finish_environment(
        &self,
        id: &EnvironmentId,
    ) -> Result<EnvironmentView, ClientError> {
        let url = self.endpoint(&["environments", id.as_str(), "finish"])?;
        self.json(&Call::new(Method::POST, url).retry()).await
    }

    /// `GET /fleet`.
    pub async fn fleet(&self) -> Result<FleetOverview, ClientError> {
        self.json(&Call::new(Method::GET, self.endpoint(&["fleet"])?))
            .await
    }

    /// `GET /machines/{id}`.
    pub async fn machine(&self, id: &MachineId) -> Result<MachineView, ClientError> {
        let url = self.endpoint(&["machines", id.as_str()])?;
        self.json(&Call::new(Method::GET, url)).await
    }

    /// `PATCH /machines/{id}` (admin): rename, replace the policy, drain or
    /// retire. Sent once.
    pub async fn update_machine(
        &self,
        id: &MachineId,
        request: &UpdateMachineRequest,
    ) -> Result<MachineView, ClientError> {
        let url = self.endpoint(&["machines", id.as_str()])?;
        self.json(&Call::new(Method::PATCH, url).json(request)?)
            .await
    }

    /// `GET /queue`.
    pub async fn queue(&self) -> Result<QueueView, ClientError> {
        self.json(&Call::new(Method::GET, self.endpoint(&["queue"])?))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_are_unique_header_safe_text() {
        let a = new_idempotency_key();
        let b = new_idempotency_key();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(HeaderValue::from_str(&a).is_ok());
    }

    #[test]
    fn the_idempotency_header_name_is_a_valid_static_header() {
        assert_eq!(
            HeaderName::from_static(IDEMPOTENCY_HEADER).as_str(),
            "idempotency-key"
        );
    }
}
