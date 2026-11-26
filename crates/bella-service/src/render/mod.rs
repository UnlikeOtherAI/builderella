//! Pure rendering of definitions. Nothing here reads or changes the host, so
//! every operating system's definition can be produced on any of them.

pub(crate) mod launchd;
pub(crate) mod systemd;
pub(crate) mod windows;

use crate::{Os, ServiceError, ServiceSpec};
use std::path::PathBuf;

/// A definition as it would be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The file that would be written; `~` stands for the installing user's
    /// home directory. For Windows, the service's registry key.
    pub path: PathBuf,
    /// The file contents. For Windows, a description of the entry the Service
    /// Control Manager would hold.
    pub contents: String,
}

/// Produce the definition of `spec` for `platform`, or say why it cannot be
/// expressed there.
pub fn render(spec: &ServiceSpec, platform: Os) -> Result<Rendered, ServiceError> {
    match platform {
        Os::Linux => systemd::render(spec),
        Os::Macos => launchd::render(spec),
        Os::Windows => windows::render(spec),
    }
}
