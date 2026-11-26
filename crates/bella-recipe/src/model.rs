//! A parsed and validated `builderella.toml`.

use crate::issue::{Issue, Pos};
use bella_core::job::{CloudMode, LeaseSpec, Priority, RetryClass, StepSpec};
use bella_core::platform::Compatibility;
use bella_core::resources::ResourceEnvelope;
use std::collections::BTreeMap;

/// A recipe file that passed validation. Every recipe in it can be resolved
/// into a job; only overrides and policy limits can still be refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeFile {
    /// `[project]`.
    pub project: ProjectSection,
    /// `[capture]`; empty when the file has none.
    pub capture: CaptureSection,
    /// Recipes by name, with `[defaults]` already merged in.
    pub recipes: BTreeMap<String, Recipe>,
    /// Every warning in the file, ordered by position.
    pub warnings: Vec<Issue>,
    /// Position of each value that was read, by key path, so problems found
    /// while resolving can still point into the file.
    pub(crate) positions: BTreeMap<String, Pos>,
}

/// `[project]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSection {
    /// The project's name on the fleet.
    pub name: String,
}

/// `[capture]`: what goes into a snapshot of the working tree. Plain data;
/// the caller maps it onto the capture request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureSection {
    /// Patterns for paths left out of the snapshot.
    pub exclude: Vec<String>,
    /// Git-ignored paths that are captured anyway.
    pub include_ignored: Vec<String>,
    /// Secret patterns in addition to the built-in ones.
    pub secrets: Vec<String>,
    /// Paths that match a secret pattern and are captured regardless.
    pub allow_secrets: Vec<String>,
    /// Additional source roots: mount path inside the snapshot to a
    /// directory, relative to the recipe file.
    pub roots: BTreeMap<String, String>,
}

/// One `[recipes.<name>]` table after validation.
///
/// Limits that the fleet may lower (`timeout_secs`, `max_attempts`, ...)
/// keep `None` when the file did not set them, so a default that is lowered
/// by policy is not reported as if the author had asked for more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// The key of the recipe's table: `[a-z0-9_-]+`.
    pub name: String,
    /// One line for listings.
    pub description: Option<String>,
    /// In execution order: every step after the steps it needs, otherwise
    /// as declared, cleanup steps last.
    pub steps: Vec<StepSpec>,
    /// Peak simultaneous demand: services summed, other steps maximised,
    /// never below the recipe-level declaration.
    pub envelope: ResourceEnvelope,
    /// `hosts`, `target`, `test_runtime`, `requires` and `isolation`,
    /// exactly as declared. Empty `build_hosts` means no `hosts` key: the
    /// job is pinned to the platform that submits it.
    pub compatibility: Compatibility,
    pub priority: Priority,
    /// `retry`.
    pub retry_class: RetryClass,
    /// Total attempts including the first.
    pub max_attempts: Option<u32>,
    /// Wall-clock limit for the whole job.
    pub timeout_secs: Option<u64>,
    pub cleanup_timeout_secs: Option<u64>,
    pub cloud: CloudMode,
    /// The source may not leave the LAN.
    pub lan_only: bool,
    /// `disconnected`: how long an attempt may continue without the
    /// controller.
    pub disconnected_secs: Option<u64>,
    /// Globs relative to the source root; they never leave it.
    pub artifacts: Vec<String>,
    /// Globs for test reports, under the same rule as `artifacts`.
    pub test_reports: Vec<String>,
    /// Environment for every step.
    pub env: BTreeMap<String, String>,
    /// `Some` keeps the services alive after the other steps finish. Only
    /// present when the recipe has at least one service.
    pub environment: Option<LeaseSpec>,
    /// Suggested build parallelism, at least 1.
    pub parallelism: Option<u32>,
    /// Warnings about this recipe, ordered by position.
    pub warnings: Vec<Issue>,
}

impl RecipeFile {
    /// Recipe names in alphabetical order.
    pub fn recipe_names(&self) -> impl Iterator<Item = &str> {
        self.recipes.keys().map(String::as_str)
    }

    pub(crate) fn position(&self, path: &str) -> Option<Pos> {
        self.positions.get(path).copied()
    }
}
