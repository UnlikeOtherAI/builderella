//! `builderella.toml`: parsing, validation and resolution into a frozen
//! [`JobSpec`](bella_core::job::JobSpec).
//!
//! The file format is described in `docs/recipes.md`. The flow is:
//!
//! 1. [`find_recipe_file`] locates `builderella.toml` above a directory.
//! 2. [`load`] (or [`parse`] on text) validates it and reports every
//!    problem at once, with positions.
//! 3. [`resolve`] combines one recipe with per-job [`Overrides`] and the
//!    [`PolicyLimits`] in force and returns the job specification. A bare
//!    command without a recipe goes through [`ad_hoc`].
//!
//! Nothing here performs I/O except [`find_recipe_file`] and [`load`].

mod graph;
mod issue;
mod model;
mod parse;
mod resolve;
mod text;
pub mod units;

pub use issue::{Issue, Severity};
pub use model::{CaptureSection, ProjectSection, Recipe, RecipeFile};
pub use parse::{
    DEFAULT_READY_INTERVAL_MS, DEFAULT_READY_TIMEOUT_SECS, MAIN_STEP, MAX_STEPS, parse,
};
pub use resolve::{
    AD_HOC_RECIPE, DEFAULT_MAX_ATTEMPTS, Overrides, PolicyLimits, Resolved, ad_hoc, resolve,
};

use bella_core::platform::{CapabilityRequirement, PlatformSelector};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The name of the recipe file.
pub const RECIPE_FILE_NAME: &str = "builderella.toml";

/// A recipe file larger than this is refused unread.
pub const MAX_RECIPE_FILE_BYTES: u64 = 1024 * 1024;

/// Find the recipe file for work done in `start_dir`: `builderella.toml` in
/// that directory or the nearest directory above it.
///
/// A relative `start_dir` is taken relative to the current directory. Only
/// regular files count; a directory of that name is skipped.
pub fn find_recipe_file(start_dir: &Path) -> Option<PathBuf> {
    let start = std::path::absolute(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    start
        .ancestors()
        .map(|dir| dir.join(RECIPE_FILE_NAME))
        .find(|candidate| candidate.is_file())
}

/// Why a recipe file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read `{}`", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`{}` is larger than {} bytes", .path.display(), MAX_RECIPE_FILE_BYTES)]
    TooLarge { path: PathBuf },
    /// The file was read but is not a valid recipe file. `issues` holds
    /// every problem, errors and warnings, ordered by position.
    #[error("`{}` is not a valid recipe file", .path.display())]
    Invalid { path: PathBuf, issues: Vec<Issue> },
}

/// Read and validate the recipe file at `path`.
pub fn load(path: &Path) -> Result<RecipeFile, LoadError> {
    let io = |source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(io)?;
    // Read one byte past the limit so an over-long file is noticed without
    // trusting its reported size.
    let mut bytes = Vec::new();
    file.take(MAX_RECIPE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_RECIPE_FILE_BYTES {
        return Err(LoadError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the file is not valid UTF-8",
        ))
    })?;
    parse(&text).map_err(|issues| LoadError::Invalid {
        path: path.to_path_buf(),
        issues,
    })
}

/// Parse a platform the way `hosts` entries are written: `linux`,
/// `macos-aarch64`, `any-x86_64`, `any`. For command-line flags that take
/// the same text as the file.
pub fn parse_platform(text: &str) -> Result<PlatformSelector, String> {
    text::selector_from_str(text)
}

/// Parse a requirement the way `requires` entries are written: `rust`,
/// `rust>=1.85`, `node=22`.
pub fn parse_requirement(text: &str) -> Result<CapabilityRequirement, String> {
    text::requirement_from_str(text)
}
