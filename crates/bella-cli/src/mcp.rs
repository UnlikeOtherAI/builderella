//! `bella mcp`: the MCP server of `bella-mcp` over standard input and
//! output, with the fleet operations implemented by the blocking client and
//! the same capture and submission path as `bella submit`.

use crate::cli::McpArgs;
use crate::ctx::Ctx;
use crate::error::{CliError, CliResult, Exit};
use crate::jobs::{download_artifact, newest_artifacts};
use crate::work::{self, WorkRequest};
use bella_client::BlockingClient;
use bella_core::api::{
    ArtifactView, EnvironmentView, ExplainResponse, FleetOverview, JobView, LogPage, LogQuery,
    PlanResponse, SessionInfo, WaitQuery, WaitResponse,
};
use bella_core::{ApiError, EnvironmentId, ErrorCode, JobId, SessionId};
use bella_mcp::{FleetOps, PlanArgs, ServerOptions, SubmitArgs, SubmitOutcome};
use bella_recipe::Overrides;
use bella_recipe::units::parse_duration_secs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// [`FleetOps`] over the blocking client. Every call runs on the thread
/// the server gives it, none of which belongs to a Tokio runtime.
struct Fleet {
    client: BlockingClient,
}

fn request(
    workspace: PathBuf,
    recipe: Option<String>,
    command: Option<bella_core::job::CommandSpec>,
    overrides: Overrides,
    resources: Option<bella_core::resources::ResourceVector>,
) -> WorkRequest {
    WorkRequest {
        workspace,
        recipe,
        command,
        project: None,
        overrides,
        resources,
    }
}

fn overrides(
    priority: Option<bella_core::job::Priority>,
    placement: Option<bella_core::platform::PlacementChoice>,
    env: std::collections::BTreeMap<String, String>,
) -> Overrides {
    Overrides {
        priority,
        placement,
        env,
        ..Overrides::default()
    }
}

impl FleetOps for Fleet {
    fn plan(&self, req: PlanArgs) -> Result<PlanResponse, ApiError> {
        let request = request(
            req.workspace,
            req.recipe,
            req.command,
            overrides(req.priority, req.placement, req.env),
            req.resources,
        );
        work::plan(&self.client, &request)
            .map(|(_, response)| response)
            .map_err(work::WorkError::into_api_error)
    }

    fn submit(&self, req: SubmitArgs) -> Result<SubmitOutcome, ApiError> {
        let request = request(
            req.workspace,
            req.recipe,
            req.command,
            overrides(req.priority, req.placement, req.env),
            req.resources,
        );
        let submitted = work::submit(
            &self.client,
            &request,
            &req.idempotency_key,
            Some(req.session),
        )
        .map_err(work::WorkError::into_api_error)?;
        Ok(SubmitOutcome {
            job: submitted.job,
            snapshot_id: submitted.snapshot.id,
            consistency: submitted.capture.meta.consistency,
            excluded_secrets: submitted.capture.excluded_secrets,
            warnings: submitted.warnings,
        })
    }

    fn job(&self, id: &JobId) -> Result<JobView, ApiError> {
        self.client.job(id).map_err(|e| e.into_api_error())
    }

    fn wait(&self, id: &JobId, query: WaitQuery) -> Result<WaitResponse, ApiError> {
        self.client
            .wait_job(id, &query)
            .map_err(|e| e.into_api_error())
    }

    fn logs(&self, id: &JobId, query: LogQuery) -> Result<LogPage, ApiError> {
        self.client
            .job_logs(id, &query)
            .map_err(|e| e.into_api_error())
    }

    fn cancel(&self, id: &JobId) -> Result<JobView, ApiError> {
        self.client.cancel_job(id).map_err(|e| e.into_api_error())
    }

    fn artifacts(&self, id: &JobId) -> Result<Vec<ArtifactView>, ApiError> {
        self.client
            .job_artifacts(id)
            .map_err(|e| e.into_api_error())
    }

    fn fetch_artifact(&self, id: &JobId, path: &str, dest: &Path) -> Result<PathBuf, ApiError> {
        let job = self.client.job(id).map_err(|e| e.into_api_error())?;
        let listed = self
            .client
            .job_artifacts(id)
            .map_err(|e| e.into_api_error())?;
        let artifact = newest_artifacts(&job, listed)
            .into_iter()
            .find(|artifact| artifact.path == path)
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::NotFound,
                    format!("job {id} has no artifact `{path}`"),
                )
            })?;
        download_artifact(&self.client, dest, &artifact).map_err(|(exists, message)| {
            let code = if exists {
                ErrorCode::Conflict
            } else {
                ErrorCode::Internal
            };
            ApiError::new(code, message)
        })
    }

    fn environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        self.client.environment(id).map_err(|e| e.into_api_error())
    }

    fn renew_environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        self.client
            .renew_environment(id)
            .map_err(|e| e.into_api_error())
    }

    fn finish_environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        self.client
            .finish_environment(id)
            .map_err(|e| e.into_api_error())
    }

    fn explain(&self, id: &JobId) -> Result<ExplainResponse, ApiError> {
        self.client.explain_job(id).map_err(|e| e.into_api_error())
    }

    fn fleet(&self) -> Result<FleetOverview, ApiError> {
        self.client.fleet().map_err(|e| e.into_api_error())
    }
}

pub fn run(ctx: &Ctx, args: &McpArgs) -> CliResult<Exit> {
    let credentials = ctx.credentials()?;
    let client = BlockingClient::from_credentials(&credentials)?;
    let roots = if args.roots.is_empty() {
        vec![std::env::current_dir().map_err(|error| {
            CliError::local(format!("cannot read the current directory: {error}"))
        })?]
    } else {
        args.roots.clone()
    };
    let session = SessionInfo {
        id: SessionId::generate(),
        label: None,
        agent: Some("mcp".into()),
        origin_machine: Some(bella_probe::hostname()),
        thread_ref: None,
    };
    let mut options = ServerOptions::new(roots, session);
    options.dashboard_url = Some(credentials.url.to_string());
    if let Some(text) = &args.max_wait {
        let secs = parse_duration_secs(text)
            .map_err(|error| CliError::local(format!("--max-wait: {error}")))?;
        options.max_wait = Duration::from_secs(secs.max(1));
    }
    tracing::info!("serving MCP on standard input and output");
    bella_mcp::serve(
        BufReader::new(std::io::stdin()),
        std::io::stdout(),
        Arc::new(Fleet { client }),
        options,
    )
    .map_err(|error| CliError::local(format!("reading standard input failed: {error}")))?;
    Ok(Exit::Success)
}
