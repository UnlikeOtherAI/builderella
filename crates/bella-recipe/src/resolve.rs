//! Resolving a recipe, per-job overrides and policy limits into a frozen
//! [`JobSpec`].

use crate::issue::{Issue, Pos, Severity, sort_issues};
use crate::model::RecipeFile;
use crate::parse::MAIN_STEP;
use crate::text::{self, closest, one_of};
use crate::units::{format_bytes, format_cpus, format_duration_secs};
use bella_core::defaults;
use bella_core::ids::Digest;
use bella_core::job::{
    CloudMode, CommandSpec, JobSpec, LeaseSpec, OutputSpec, Priority, RetryClass, StepKind,
    StepSpec,
};
use bella_core::platform::{Compatibility, PlacementChoice};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use serde::Serialize;
use std::collections::BTreeMap;

/// The `recipe` name of a job built by [`ad_hoc`]. It contains characters a
/// recipe name cannot, so it never collides with one.
pub const AD_HOC_RECIPE: &str = "(ad-hoc)";

/// Attempts a job gets when neither the recipe nor policy says otherwise:
/// one, so nothing runs twice unless the author allowed it.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 1;

/// Version of the canonical form hashed into `recipe_digest`. Changing what
/// the digest covers changes this number, and with it every digest.
const DIGEST_FORMAT: u32 = 1;

/// What one submission may change about a recipe. Everything else is fixed
/// by the recipe file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Replaces the recipe's priority.
    pub priority: Option<Priority>,
    /// Where the job should run, within what the recipe allows. `None`
    /// means any machine the recipe accepts.
    pub placement: Option<PlacementChoice>,
    /// Added to the recipe's `env`; a variable set here wins.
    pub env: BTreeMap<String, String>,
    /// Replaces the recipe's suggested build parallelism.
    pub parallelism: Option<u32>,
    /// Replaces the recipe's cloud mode, still below the policy ceiling.
    pub cloud: Option<CloudMode>,
    /// Replaces the recipe's timeout, still below the policy limit.
    pub timeout_secs: Option<u64>,
    /// Keep the source on the LAN for this job. Can only tighten: `false`
    /// leaves the recipe's own setting in force.
    pub lan_only: bool,
}

/// Fleet, project and machine policy as it applies to one submission.
///
/// Limits only ever narrow a job: nothing in a recipe or in [`Overrides`]
/// can raise one. `Default` is the cautious choice: no urgent priority and
/// no paid capacity. `None` means "no limit of this kind".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyLimits {
    /// Longer job timeouts are lowered to this.
    pub max_timeout_secs: Option<u64>,
    /// Longer cleanup timeouts are lowered to this.
    pub max_cleanup_timeout_secs: Option<u64>,
    /// More attempts are lowered to this. Never below one.
    pub max_attempts: Option<u32>,
    /// Whether `urgent` priority may be used at all.
    pub allow_urgent: bool,
    /// The largest reservation a job may hold. Every dimension is a
    /// ceiling as in [`ResourceVector::fits_within`]: a token that is not
    /// listed here is not available at all.
    pub max_envelope: Option<ResourceVector>,
    /// Source must stay on the LAN regardless of recipe and overrides.
    pub forced_lan_only: bool,
    /// The most cloud use allowed. A job gets the part of its requested
    /// mode that the ceiling also grants.
    pub cloud_ceiling: CloudMode,
    /// Longer disconnected continuation is lowered to this.
    pub max_disconnected_secs: Option<u64>,
    /// Environments may not outlive this.
    pub max_environment_lifetime_secs: Option<u64>,
}

impl PolicyLimits {
    /// No limits at all: for a local, single-user fleet and for tests.
    pub fn unrestricted() -> Self {
        Self {
            allow_urgent: true,
            cloud_ceiling: CloudMode::Hybrid,
            ..Self::default()
        }
    }
}

/// A job specification ready to submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub spec: JobSpec,
    /// Warnings about the recipe and about values that policy lowered.
    pub warnings: Vec<Issue>,
}

/// Resolve the recipe named `recipe` into a frozen [`JobSpec`].
///
/// Overrides are applied first, then limits. Values above a limit are
/// lowered with a warning; `urgent` priority without permission and a
/// reservation above `max_envelope` are errors, because lowering those
/// would run a different job than the one asked for. On error the list
/// holds every problem found, warnings included.
///
/// The result's `compatibility` is exactly what the recipe declares: a
/// recipe without `hosts` has empty `build_hosts`, which pins the job to
/// the platform that submits it.
pub fn resolve(
    file: &RecipeFile,
    recipe: &str,
    overrides: &Overrides,
    limits: &PolicyLimits,
) -> Result<Resolved, Vec<Issue>> {
    let Some(found) = file.recipes.get(recipe) else {
        let names: Vec<&str> = file.recipe_names().collect();
        let message = if names.is_empty() {
            format!("no recipe named `{recipe}`: this file defines no recipes")
        } else {
            let hint = match closest(recipe, &names) {
                Some(name) => format!("; did you mean `{name}`?"),
                None => String::new(),
            };
            format!(
                "no recipe named `{recipe}`{hint} (this file has {})",
                one_of(&names)
            )
        };
        let pos = file.position("recipes");
        return Err(vec![Issue::new(Severity::Error, "recipes", pos, message)]);
    };

    let recipe_path = format!("recipes.{recipe}");
    // Where a key of this recipe was written: in the recipe, else in
    // `[defaults]`, else the nearest enclosing table that exists.
    let locate = |key: &str| -> (String, Option<Pos>) {
        let mut candidate = key;
        loop {
            for path in [
                format!("{recipe_path}.{candidate}"),
                format!("defaults.{candidate}"),
            ] {
                if let Some(pos) = file.position(&path) {
                    return (path, Some(pos));
                }
            }
            match candidate.rsplit_once('.') {
                Some((parent, _)) => candidate = parent,
                None => return (recipe_path.clone(), file.position(&recipe_path)),
            }
        }
    };

    let draft = Draft {
        recipe: found.name.clone(),
        steps: found.steps.clone(),
        envelope: found.envelope.clone(),
        compatibility: found.compatibility.clone(),
        priority: found.priority,
        retry_class: found.retry_class,
        max_attempts: found.max_attempts,
        timeout_secs: found.timeout_secs,
        cleanup_timeout_secs: found.cleanup_timeout_secs,
        cloud: found.cloud,
        lan_only: found.lan_only,
        disconnected_secs: found.disconnected_secs,
        outputs: OutputSpec {
            artifacts: found.artifacts.clone(),
            test_reports: found.test_reports.clone(),
            ..OutputSpec::default()
        },
        environment: found.environment,
        env: found.env.clone(),
        parallelism: found.parallelism,
        warnings: found.warnings.clone(),
    };
    finalize(draft, overrides, limits, &locate)
}

/// Build the specification of a bare command, with no recipe file.
///
/// The job has one step named `main` and reserves exactly `resources`. It
/// declares no build hosts, so it is pinned to the platform that submits
/// it. Overrides and limits apply as in [`resolve`]. `[defaults]` of a
/// recipe file do not apply.
pub fn ad_hoc(
    command: CommandSpec,
    resources: ResourceVector,
    overrides: &Overrides,
    limits: &PolicyLimits,
) -> Result<Resolved, Vec<Issue>> {
    let problem = match &command {
        CommandSpec::Argv { argv } => argv
            .first()
            .is_none_or(|program| program.is_empty())
            .then_some("the command is empty: give the program followed by its arguments"),
        CommandSpec::Shell { script, shell } => {
            if script.trim().is_empty() {
                Some("the command is empty")
            } else if shell
                .as_ref()
                .is_some_and(|argv| argv.first().is_none_or(|program| program.is_empty()))
            {
                Some("the shell is empty: give the program that runs the script")
            } else {
                None
            }
        }
    };
    if let Some(problem) = problem {
        return Err(vec![Issue::new(Severity::Error, "command", None, problem)]);
    }

    let mut resources = resources;
    resources.tokens.retain(|_, count| *count > 0);
    let locate = |_: &str| ("resources".to_string(), None);
    let draft = Draft {
        recipe: AD_HOC_RECIPE.to_string(),
        steps: vec![StepSpec {
            name: MAIN_STEP.to_string(),
            kind: StepKind::Task,
            command,
            cwd: None,
            env: BTreeMap::new(),
            depends_on: Vec::new(),
            readiness: None,
            timeout_secs: None,
            resources: None,
            ports: Vec::new(),
        }],
        envelope: ResourceEnvelope::fixed(resources),
        compatibility: Compatibility::default(),
        priority: Priority::default(),
        retry_class: RetryClass::default(),
        max_attempts: None,
        timeout_secs: None,
        cleanup_timeout_secs: None,
        cloud: CloudMode::default(),
        lan_only: false,
        disconnected_secs: None,
        outputs: OutputSpec::default(),
        environment: None,
        env: BTreeMap::new(),
        parallelism: None,
        warnings: Vec::new(),
    };
    finalize(draft, overrides, limits, &locate)
}

/// A job before overrides and limits. `None` in a limited field means the
/// author did not ask for a value, so lowering the default is not news.
struct Draft {
    recipe: String,
    steps: Vec<StepSpec>,
    envelope: ResourceEnvelope,
    compatibility: Compatibility,
    priority: Priority,
    retry_class: RetryClass,
    max_attempts: Option<u32>,
    timeout_secs: Option<u64>,
    cleanup_timeout_secs: Option<u64>,
    cloud: CloudMode,
    lan_only: bool,
    disconnected_secs: Option<u64>,
    outputs: OutputSpec,
    environment: Option<LeaseSpec>,
    env: BTreeMap<String, String>,
    parallelism: Option<u32>,
    warnings: Vec<Issue>,
}

type Locate<'a> = &'a dyn Fn(&str) -> (String, Option<Pos>);

struct Report<'a> {
    issues: Vec<Issue>,
    locate: Locate<'a>,
}

impl Report<'_> {
    fn error(&mut self, key: &str, message: String) {
        let (path, pos) = (self.locate)(key);
        self.issues
            .push(Issue::new(Severity::Error, path, pos, message));
    }

    fn warn(&mut self, key: &str, message: String) {
        let (path, pos) = (self.locate)(key);
        self.issues
            .push(Issue::new(Severity::Warning, path, pos, message));
    }

    /// An issue about an override, which has no place in the file.
    fn error_in_override(&mut self, field: &str, message: String) {
        let path = format!("overrides.{field}");
        self.issues
            .push(Issue::new(Severity::Error, path, None, message));
    }

    fn warn_in_override(&mut self, field: &str, message: String) {
        let path = format!("overrides.{field}");
        self.issues
            .push(Issue::new(Severity::Warning, path, None, message));
    }

    /// Lower `value` to `limit` and say so, unless the value is only a
    /// default (`source` is `None`).
    fn lower(
        &mut self,
        value: u64,
        limit: Option<u64>,
        source: Option<Source<'_>>,
        what: &str,
        show: &dyn Fn(u64) -> String,
    ) -> u64 {
        let Some(limit) = limit.filter(|limit| value > *limit) else {
            return value;
        };
        let message = format!(
            "{what} of {} is above the limit of {}; the job will use {}",
            show(value),
            show(limit),
            show(limit)
        );
        match source {
            Some(Source::Override(field)) => self.warn_in_override(field, message),
            Some(Source::Recipe(key)) => self.warn(key, message),
            None => {}
        }
        limit
    }
}

/// Who asked for a value.
#[derive(Clone, Copy)]
enum Source<'a> {
    /// A field of [`Overrides`].
    Override(&'a str),
    /// A key of the recipe.
    Recipe(&'a str),
}

fn finalize(
    draft: Draft,
    overrides: &Overrides,
    limits: &PolicyLimits,
    locate: Locate<'_>,
) -> Result<Resolved, Vec<Issue>> {
    let mut report = Report {
        issues: draft.warnings.clone(),
        locate,
    };

    // Overrides: check them, then let them replace the recipe's values.
    let mut env = draft.env;
    for (name, value) in &overrides.env {
        match text::check_env_name(name).and_then(|()| text::check_env_value(value)) {
            Ok(()) => {
                env.insert(name.clone(), value.clone());
            }
            Err(message) => report.error_in_override("env", message),
        }
    }
    if overrides.parallelism == Some(0) {
        report.error_in_override("parallelism", "parallelism must be at least 1".to_string());
    }
    if overrides.timeout_secs == Some(0) {
        report.error_in_override(
            "timeout_secs",
            "the timeout must be longer than zero".to_string(),
        );
    }
    let placement = match &overrides.placement {
        Some(choice) => checked_placement(choice).unwrap_or_else(|message| {
            report.error_in_override("placement", message);
            PlacementChoice::default()
        }),
        None => PlacementChoice::default(),
    };

    let priority = overrides.priority.unwrap_or(draft.priority);
    if priority == Priority::Urgent && !limits.allow_urgent {
        let message = "`urgent` priority is not allowed by policy here; use `interactive` or lower"
            .to_string();
        if overrides.priority.is_some() {
            report.error_in_override("priority", message);
        } else {
            report.error("priority", message);
        }
    }

    let timeout_source = if overrides.timeout_secs.is_some() {
        Some(Source::Override("timeout_secs"))
    } else {
        draft.timeout_secs.map(|_| Source::Recipe("timeout"))
    };
    let timeout_secs = report.lower(
        overrides
            .timeout_secs
            .or(draft.timeout_secs)
            .unwrap_or(defaults::JOB_TIMEOUT.as_secs()),
        limits.max_timeout_secs,
        timeout_source,
        "a timeout",
        &format_duration_secs,
    );
    let cleanup_timeout_secs = report.lower(
        draft
            .cleanup_timeout_secs
            .unwrap_or(defaults::CLEANUP_TIMEOUT.as_secs()),
        limits.max_cleanup_timeout_secs,
        draft
            .cleanup_timeout_secs
            .map(|_| Source::Recipe("cleanup_timeout")),
        "a cleanup timeout",
        &format_duration_secs,
    );
    let disconnected_continuation_secs = report.lower(
        draft.disconnected_secs.unwrap_or(0),
        limits.max_disconnected_secs,
        draft
            .disconnected_secs
            .map(|_| Source::Recipe("disconnected")),
        "disconnected continuation",
        &format_duration_secs,
    );
    let max_attempts = report.lower(
        u64::from(draft.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS)),
        limits.max_attempts.map(|limit| u64::from(limit.max(1))),
        draft.max_attempts.map(|_| Source::Recipe("max_attempts")),
        "an attempt count",
        &|attempts| attempts.to_string(),
    );
    // Lowered from a `u32` or to a `u32` limit, so it still fits.
    let max_attempts = u32::try_from(max_attempts).unwrap_or(u32::MAX);

    let requested_cloud = overrides.cloud.unwrap_or(draft.cloud);
    let cloud = cloud_within(requested_cloud, limits.cloud_ceiling);
    if cloud != requested_cloud {
        let message = format!(
            "cloud mode `{}` is more than policy allows (`{}`); the job will use `{}`",
            cloud_name(requested_cloud),
            cloud_name(limits.cloud_ceiling),
            cloud_name(cloud)
        );
        if overrides.cloud.is_some() {
            report.warn_in_override("cloud", message);
        } else {
            report.warn("cloud", message);
        }
    }

    let source_lan_only = draft.lan_only || overrides.lan_only || limits.forced_lan_only;

    let mut environment = draft.environment;
    if let (Some(lease), Some(limit)) = (&mut environment, limits.max_environment_lifetime_secs)
        && lease.max_lifetime_secs > limit
    {
        report.warn(
            "environment.max_lifetime",
            format!(
                "an environment lifetime of {} is above the limit of {}; it will end after {}",
                format_duration_secs(lease.max_lifetime_secs),
                format_duration_secs(limit),
                format_duration_secs(limit)
            ),
        );
        lease.max_lifetime_secs = limit;
        lease.lease_secs = lease.lease_secs.min(limit);
        lease.idle_timeout_secs = lease.idle_timeout_secs.min(limit);
    }

    let mut envelope = draft.envelope;
    if let Some(limit) = &limits.max_envelope {
        limit_envelope(&mut envelope, limit, &mut report);
    }

    let mut issues = report.issues;
    sort_issues(&mut issues);
    if issues.iter().any(Issue::is_error) {
        return Err(issues);
    }

    let parallelism = overrides.parallelism.or(draft.parallelism);
    let recipe_digest = digest(&DigestInput {
        format: DIGEST_FORMAT,
        steps: &draft.steps,
        env: &env,
        compatibility: &draft.compatibility,
        envelope: &envelope,
        outputs: &draft.outputs,
        timeout_secs,
        cleanup_timeout_secs,
        environment: environment.as_ref(),
        parallelism,
    });
    let Some(recipe_digest) = recipe_digest else {
        let message = "the job specification could not be serialized for its digest";
        issues.push(Issue::new(Severity::Error, "", None, message));
        return Err(issues);
    };

    Ok(Resolved {
        spec: JobSpec {
            recipe: draft.recipe,
            recipe_digest,
            steps: draft.steps,
            envelope,
            compatibility: draft.compatibility,
            placement,
            priority,
            retry_class: draft.retry_class,
            max_attempts,
            timeout_secs,
            cleanup_timeout_secs,
            cloud,
            source_lan_only,
            outputs: draft.outputs,
            environment,
            env,
            disconnected_continuation_secs,
            parallelism,
        },
        warnings: issues,
    })
}

/// Everything that can change what a job produces or whether it passes.
///
/// Priority, placement and cloud mode decide where and when a job runs, not
/// what it does, and are left out together with the other scheduling terms:
/// the recipe's name, LAN-only, retry class, attempt count and disconnected
/// continuation. Environment variables and parallelism are in, because the
/// steps can read them.
///
/// Fields serialize in declaration order and maps are ordered, so the JSON
/// is canonical without a separate canonicalization step.
#[derive(Serialize)]
struct DigestInput<'a> {
    format: u32,
    steps: &'a [StepSpec],
    env: &'a BTreeMap<String, String>,
    compatibility: &'a Compatibility,
    envelope: &'a ResourceEnvelope,
    outputs: &'a OutputSpec,
    timeout_secs: u64,
    cleanup_timeout_secs: u64,
    environment: Option<&'a LeaseSpec>,
    parallelism: Option<u32>,
}

fn digest(input: &DigestInput<'_>) -> Option<Digest> {
    let bytes = serde_json::to_vec(input).ok()?;
    Some(Digest::of_bytes(&bytes))
}

/// The part of `requested` that `ceiling` also grants. The modes are sets of
/// two permissions (a person may authorise cloud capacity for a job;
/// standing policy may add it automatically), so the result is their
/// intersection and never exceeds the ceiling.
fn cloud_within(requested: CloudMode, ceiling: CloudMode) -> CloudMode {
    let parts = |mode: CloudMode| match mode {
        CloudMode::ExistingOnly => (false, false),
        CloudMode::Manual => (true, false),
        CloudMode::AutoOverflow => (false, true),
        CloudMode::Hybrid => (true, true),
    };
    let (manual, automatic) = parts(requested);
    let (manual_allowed, automatic_allowed) = parts(ceiling);
    match (manual && manual_allowed, automatic && automatic_allowed) {
        (false, false) => CloudMode::ExistingOnly,
        (true, false) => CloudMode::Manual,
        (false, true) => CloudMode::AutoOverflow,
        (true, true) => CloudMode::Hybrid,
    }
}

/// The spelling used in recipe files.
fn cloud_name(mode: CloudMode) -> &'static str {
    match mode {
        CloudMode::ExistingOnly => "existing-only",
        CloudMode::Manual => "manual",
        CloudMode::AutoOverflow => "auto-overflow",
        CloudMode::Hybrid => "hybrid",
    }
}

fn checked_placement(choice: &PlacementChoice) -> Result<PlacementChoice, String> {
    let mut choice = choice.clone();
    match &mut choice {
        PlacementChoice::Any => {}
        PlacementChoice::Prefer { platform } | PlacementChoice::Require { platform } => {
            if let Some(version) = &platform.os_version {
                platform.os_version = Some(text::version_requirement(version)?);
            }
        }
        PlacementChoice::Machine { machine } => {
            if machine.trim().is_empty() {
                return Err("the machine name is empty".to_string());
            }
        }
        PlacementChoice::Pool { pool } => {
            if pool.trim().is_empty() {
                return Err("the pool name is empty".to_string());
            }
        }
    }
    Ok(choice)
}

/// Refuse a reservation above the limit; lower growth room that exceeds it.
fn limit_envelope(
    envelope: &mut ResourceEnvelope,
    limit: &ResourceVector,
    report: &mut Report<'_>,
) {
    let memory = |bytes: u64| format!("{} of memory", format_bytes(bytes));
    let disk = |bytes: u64| format!("{} of disk", format_bytes(bytes));
    let dimensions: [(&str, &str, u64, &mut u64, u64, &dyn Fn(u64) -> String); 3] = [
        (
            "cpu",
            "max_cpu",
            envelope.peak.cpu_milli,
            &mut envelope.maximum.cpu_milli,
            limit.cpu_milli,
            &format_cpus,
        ),
        (
            "memory",
            "max_memory",
            envelope.peak.memory_bytes,
            &mut envelope.maximum.memory_bytes,
            limit.memory_bytes,
            &memory,
        ),
        (
            "disk",
            "max_disk",
            envelope.peak.disk_bytes,
            &mut envelope.maximum.disk_bytes,
            limit.disk_bytes,
            &disk,
        ),
    ];
    for (key, max_key, peak, maximum, limit, show) in dimensions {
        if peak > limit {
            report.error(
                key,
                format!(
                    "the job needs {} at its peak, above the limit of {}",
                    show(peak),
                    show(limit)
                ),
            );
        } else if *maximum > limit {
            report.warn(
                max_key,
                format!(
                    "growth up to {} is above the limit of {}; the job may grow to {}",
                    show(*maximum),
                    show(limit),
                    show(limit)
                ),
            );
            *maximum = limit;
        }
    }
    for (name, needed) in &envelope.peak.tokens {
        let available = limit.token(name);
        if *needed > available {
            let message = if available == 0 {
                format!("the job needs {needed} `{name}`, which policy does not make available")
            } else {
                format!("the job needs {needed} `{name}`, above the limit of {available}")
            };
            report.error("tokens", message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_ceiling_is_an_intersection_of_permissions() {
        use CloudMode::*;
        let modes = [ExistingOnly, Manual, AutoOverflow, Hybrid];
        for requested in modes {
            // Nothing exceeds its ceiling, and a mode fits under itself.
            assert_eq!(cloud_within(requested, Hybrid), requested);
            assert_eq!(cloud_within(requested, requested), requested);
            assert_eq!(cloud_within(requested, ExistingOnly), ExistingOnly);
            for ceiling in modes {
                let granted = cloud_within(requested, ceiling);
                assert_eq!(cloud_within(granted, ceiling), granted);
                assert_eq!(cloud_within(granted, requested), granted);
            }
        }
        assert_eq!(cloud_within(Hybrid, Manual), Manual);
        assert_eq!(cloud_within(Hybrid, AutoOverflow), AutoOverflow);
        // Neither grants what the other does: a person's say-so is not
        // standing policy and the reverse.
        assert_eq!(cloud_within(Manual, AutoOverflow), ExistingOnly);
        assert_eq!(cloud_within(AutoOverflow, Manual), ExistingOnly);
    }
}
