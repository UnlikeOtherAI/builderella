//! Turning a working tree and a recipe (or a bare command) into a job:
//! recipe resolution, capture, upload, snapshot registration and the
//! idempotent submission. `bella submit`, `bella plan` and the MCP bridge
//! all go through here.
//!
//! Capture writes into a local spool store first, then asks the controller
//! which objects it lacks and uploads only those. An interrupted upload
//! therefore resumes on the next attempt: the controller keeps what arrived,
//! the next capture of the same tree has the same digests, and only the rest
//! is sent.

use bella_capture::{CapturePolicy, CaptureResult, RootSpec};
use bella_client::{BlockingClient, ClientError};
use bella_core::api::{
    JobView, PlanRequest, PlanResponse, ProjectPolicy, RegisterSnapshotRequest, SessionInfo,
    SnapshotView, SubmitJobRequest,
};
use bella_core::job::{CommandSpec, JobSpec};
use bella_core::platform::Platform;
use bella_core::resources::ResourceVector;
use bella_core::snapshot::Entry;
use bella_core::store::ObjectStore;
use bella_core::{ApiError, Digest, ErrorCode};
use bella_recipe::{Issue, LoadError, Overrides, PolicyLimits, RecipeFile};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Reservation of a bare command when the caller names none.
pub const DEFAULT_AD_HOC_CPU_MILLI: u64 = 1000;
pub const DEFAULT_AD_HOC_MEMORY: u64 = 1 << 30;

/// What to run, from where.
#[derive(Clone, Debug, Default)]
pub struct WorkRequest {
    /// The directory the work is for. The recipe file is looked for here and
    /// above.
    pub workspace: PathBuf,
    pub recipe: Option<String>,
    pub command: Option<CommandSpec>,
    pub project: Option<String>,
    pub overrides: Overrides,
    /// Reservation of a bare command. Dimensions left at zero take the
    /// default.
    pub resources: Option<ResourceVector>,
}

/// Why preparing or submitting work failed.
#[derive(Debug)]
pub enum WorkError {
    /// Something about the request or the local tree.
    Local(String),
    /// The recipe file or the resolution has errors.
    Recipe {
        file: Option<PathBuf>,
        issues: Vec<Issue>,
    },
    /// The controller refused or could not be reached.
    Client(ClientError),
}

impl From<ClientError> for WorkError {
    fn from(error: ClientError) -> Self {
        WorkError::Client(error)
    }
}

impl WorkError {
    /// Human text: recipe problems one per line, with the file's name in
    /// front of each position.
    pub fn describe(&self) -> String {
        match self {
            WorkError::Local(message) => message.clone(),
            WorkError::Client(error) => error.to_string(),
            WorkError::Recipe { file, issues } => {
                let prefix = file
                    .as_ref()
                    .map(|file| format!("{}: ", file.display()))
                    .unwrap_or_default();
                let mut lines = vec!["the job cannot be built from this recipe:".to_string()];
                lines.extend(issues.iter().map(|issue| format!("{prefix}{issue}")));
                lines.join("\n")
            }
        }
    }

    /// The REST error shape, for the MCP tools.
    pub fn into_api_error(self) -> ApiError {
        match self {
            WorkError::Client(error) => error.into_api_error(),
            other => ApiError::new(ErrorCode::BadRequest, other.describe()),
        }
    }
}

/// A resolved job, not yet captured.
pub struct Prepared {
    pub project: String,
    pub spec: JobSpec,
    /// Recipe warnings, formatted with their positions.
    pub warnings: Vec<String>,
    pub recipe_file: Option<PathBuf>,
    roots: Vec<RootSpec>,
    pub capture_policy: CapturePolicy,
}

/// A job that exists on the controller.
pub struct Submitted {
    pub job: JobView,
    pub snapshot: SnapshotView,
    pub capture: CaptureResult,
    pub uploaded_objects: u64,
    pub uploaded_bytes: u64,
    pub warnings: Vec<String>,
    /// The policy the capture ran with, for reporting what it left out.
    pub capture_policy: CapturePolicy,
}

fn origin_platform() -> Result<Platform, WorkError> {
    Platform::current().ok_or_else(|| {
        WorkError::Local("this operating system or architecture is not a supported platform".into())
    })
}

fn load_recipe_file(workspace: &Path) -> Result<Option<(PathBuf, RecipeFile)>, WorkError> {
    let Some(path) = bella_recipe::find_recipe_file(workspace) else {
        return Ok(None);
    };
    match bella_recipe::load(&path) {
        Ok(file) => Ok(Some((path, file))),
        Err(LoadError::Invalid { path, issues }) => Err(WorkError::Recipe {
            file: Some(path),
            issues,
        }),
        Err(error) => Err(WorkError::Local(format!(
            "{error}: {}",
            std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default()
        ))),
    }
}

/// Resolve the recipe or command against the project's policy. Reads the
/// recipe file and asks the controller for the project's policy; uploads
/// nothing.
pub fn prepare(client: &BlockingClient, request: &WorkRequest) -> Result<Prepared, WorkError> {
    let workspace = std::path::absolute(&request.workspace).map_err(|error| {
        WorkError::Local(format!(
            "cannot use {} as the workspace: {error}",
            request.workspace.display()
        ))
    })?;
    if !workspace.is_dir() {
        return Err(WorkError::Local(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    let found = load_recipe_file(&workspace)?;

    let recipe = match (&request.recipe, &request.command, &found) {
        (Some(_), Some(_), _) => {
            return Err(WorkError::Local(
                "give either a recipe name or a command after `--`, not both".into(),
            ));
        }
        (Some(name), None, None) => {
            return Err(WorkError::Local(format!(
                "no {} in {} or any directory above it, so there is no recipe `{name}`",
                bella_recipe::RECIPE_FILE_NAME,
                workspace.display()
            )));
        }
        (Some(name), None, Some(_)) => Some(name.clone()),
        (None, Some(_), _) => None,
        (None, None, None) => {
            return Err(WorkError::Local(format!(
                "no {} in {} or above; name a command after `--` to run one without a recipe",
                bella_recipe::RECIPE_FILE_NAME,
                workspace.display()
            )));
        }
        (None, None, Some((path, file))) => {
            let names: Vec<&str> = file.recipe_names().collect();
            match names.as_slice() {
                [only] => Some((*only).to_string()),
                [] => {
                    return Err(WorkError::Local(format!(
                        "{} defines no recipes; name a command after `--` instead",
                        path.display()
                    )));
                }
                several => {
                    return Err(WorkError::Local(format!(
                        "{} defines several recipes ({}); name one",
                        path.display(),
                        several.join(", ")
                    )));
                }
            }
        }
    };

    let project = match (&request.project, &found) {
        (Some(project), _) => project.clone(),
        (None, Some((_, file))) => file.project.name.clone(),
        (None, None) => workspace
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                WorkError::Local(
                    "cannot name the project after this directory; pass --project".into(),
                )
            })?,
    };

    let policy = client
        .projects()?
        .into_iter()
        .find(|view| view.name == project)
        .map(|view| view.policy)
        .unwrap_or_else(ProjectPolicy::default);
    let limits = PolicyLimits::for_project(&policy);

    let recipe_file = found.as_ref().map(|(path, _)| path.clone());
    let resolved = match (&recipe, &request.command) {
        (Some(name), _) => {
            if request.resources.is_some() {
                return Err(WorkError::Local(format!(
                    "the reservation of recipe `{name}` cannot be overridden per job; change \
                     the recipe, or run a command instead of a recipe to choose cpu and memory"
                )));
            }
            let Some((_, file)) = &found else {
                return Err(WorkError::Local("no recipe file".into()));
            };
            bella_recipe::resolve(file, name, &request.overrides, &limits)
        }
        (None, Some(command)) => {
            let mut resources = request.resources.clone().unwrap_or_default();
            if resources.cpu_milli == 0 {
                resources.cpu_milli = DEFAULT_AD_HOC_CPU_MILLI;
            }
            if resources.memory_bytes == 0 {
                resources.memory_bytes = DEFAULT_AD_HOC_MEMORY;
            }
            bella_recipe::ad_hoc(command.clone(), resources, &request.overrides, &limits)
        }
        (None, None) => return Err(WorkError::Local("nothing to run".into())),
    };
    let resolved = resolved.map_err(|issues| WorkError::Recipe {
        file: recipe.as_ref().and(recipe_file.clone()),
        issues,
    })?;

    let prefix = recipe_file
        .as_ref()
        .map(|file| format!("{}: ", file.display()))
        .unwrap_or_default();
    let mut warnings: Vec<String> = resolved
        .warnings
        .iter()
        .map(|issue| format!("{prefix}{issue}"))
        .collect();
    if let Some((_, file)) = &found {
        for issue in &file.warnings {
            let text = format!("{prefix}{issue}");
            if !warnings.contains(&text) {
                warnings.push(text);
            }
        }
    }

    // The snapshot is of the project: the recipe file's directory when there
    // is one, otherwise the workspace itself.
    let base = recipe_file
        .as_ref()
        .and_then(|file| file.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| workspace.clone());
    let mut roots = vec![RootSpec::single(base.clone())];
    let mut capture_policy = CapturePolicy::default();
    if let Some((_, file)) = &found {
        let capture = &file.capture;
        for (mount, dir) in &capture.roots {
            roots.push(RootSpec::mounted(base.join(dir), mount.clone()));
        }
        capture_policy
            .exclude
            .extend(capture.exclude.iter().cloned());
        capture_policy
            .include_ignored
            .extend(capture.include_ignored.iter().cloned());
        capture_policy
            .secret_patterns
            .extend(capture.secrets.iter().cloned());
        capture_policy
            .allow_secrets
            .extend(capture.allow_secrets.iter().cloned());
    }

    Ok(Prepared {
        project,
        spec: resolved.spec,
        warnings,
        recipe_file,
        roots,
        capture_policy,
    })
}

/// Ask the controller where the work could run. Nothing is captured.
pub fn plan(
    client: &BlockingClient,
    request: &WorkRequest,
) -> Result<(Prepared, PlanResponse), WorkError> {
    let prepared = prepare(client, request)?;
    let response = client.plan(&PlanRequest {
        project: prepared.project.clone(),
        spec: prepared.spec.clone(),
        origin_platform: origin_platform()?,
    })?;
    Ok((prepared, response))
}

/// Prepare, capture, upload, register and submit. Returns once the job
/// exists; it does not wait for it.
///
/// Must be called from a plain thread (not one of a Tokio runtime): the
/// object store blocks its caller.
pub fn submit(
    client: &BlockingClient,
    request: &WorkRequest,
    idempotency_key: &str,
    session: Option<SessionInfo>,
) -> Result<Submitted, WorkError> {
    let prepared = prepare(client, request)?;
    let origin = origin_platform()?;

    let spool = tempfile::Builder::new()
        .prefix("bella-capture-")
        .tempdir()
        .map_err(|error| WorkError::Local(format!("cannot create a capture spool: {error}")))?;
    let local = bella_store::FsStore::open(spool.path().join("objects"))
        .map_err(|error| WorkError::Local(format!("cannot open the capture spool: {error}")))?;
    let captured = bella_capture::capture(&prepared.roots, &prepared.capture_policy, &local, None)
        .map_err(|error| WorkError::Local(format!("capture failed: {error}")))?;

    let mut digests = Vec::new();
    let mut seen = BTreeSet::new();
    for root in &captured.manifest.roots {
        for entry in &root.entries {
            if let Entry::File { digest, .. } = entry
                && seen.insert(digest.clone())
            {
                digests.push(digest.clone());
            }
        }
    }
    let remote = client.object_store();
    let missing = remote.missing(&digests).map_err(store_error)?;
    let mut uploaded_bytes = 0;
    for digest in &missing {
        uploaded_bytes += upload(&local, &remote, digest)?;
    }
    tracing::info!(
        files = captured.stats.files,
        bytes = captured.stats.bytes,
        uploaded = missing.len(),
        uploaded_bytes,
        "snapshot captured and uploaded"
    );

    let snapshot = client.register_snapshot(&RegisterSnapshotRequest {
        project: prepared.project.clone(),
        manifest: captured.manifest.clone(),
        meta: captured.meta.clone(),
    })?;
    let job = client.submit_job(
        &SubmitJobRequest {
            project: prepared.project.clone(),
            snapshot_id: snapshot.id.clone(),
            spec: prepared.spec.clone(),
            origin_platform: origin,
            session,
        },
        idempotency_key,
    )?;
    let mut warnings = prepared.warnings;
    warnings.extend(
        captured
            .warnings
            .iter()
            .map(|warning| format!("warning: capture: {warning}")),
    );
    Ok(Submitted {
        job,
        snapshot,
        uploaded_objects: missing.len() as u64,
        uploaded_bytes,
        capture: captured,
        warnings,
        capture_policy: prepared.capture_policy,
    })
}

fn upload(
    local: &bella_store::FsStore,
    remote: &dyn ObjectStore,
    digest: &Digest,
) -> Result<u64, WorkError> {
    let mut reader = local.open(digest).map_err(store_error)?;
    let (_, size) = remote.put(&mut reader, Some(digest)).map_err(store_error)?;
    Ok(size)
}

fn store_error(error: bella_core::store::StoreError) -> WorkError {
    match error {
        bella_core::store::StoreError::Unavailable(message) => {
            WorkError::Client(ClientError::Transport { message })
        }
        other => WorkError::Local(format!("uploading the snapshot failed: {other}")),
    }
}
