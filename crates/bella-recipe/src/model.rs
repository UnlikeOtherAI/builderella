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
    pub project: ProjectSection,
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
    pub name: String,
    pub description: Option<String>,
    /// In execution order: every step after the steps it needs, otherwise
    /// as declared, cleanup steps last.
    pub steps: Vec<StepSpec>,
    /// Peak simultaneous demand: services summed, other steps maximised,
    /// never below the recipe-level declaration.
    pub envelope: ResourceEnvelope,
    pub compatibility: Compatibility,
    pub priority: Priority,
    pub retry_class: RetryClass,
    pub max_attempts: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub cleanup_timeout_secs: Option<u64>,
    pub cloud: CloudMode,
    pub lan_only: bool,
    pub disconnected_secs: Option<u64>,
    pub artifacts: Vec<String>,
    pub test_reports: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub environment: Option<LeaseSpec>,
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
