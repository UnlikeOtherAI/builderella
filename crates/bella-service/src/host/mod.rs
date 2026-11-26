//! The service managers of the running host.

// The two Unix managers use only files and commands, so they compile on every
// host and are tested against a scripted command runner.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod launchd;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod systemd;
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(all(test, unix))]
mod tests;

use crate::{InstallOptions, Installed, ServiceError, ServiceId, ServiceSpec, ServiceStatus};

pub(crate) trait Backend {
    fn install(
        &self,
        spec: &ServiceSpec,
        options: &InstallOptions,
    ) -> Result<Installed, ServiceError>;
    fn uninstall(&self, id: &ServiceId) -> Result<bool, ServiceError>;
    fn status(&self, id: &ServiceId) -> Result<ServiceStatus, ServiceError>;
    fn start(&self, id: &ServiceId) -> Result<(), ServiceError>;
    fn stop(&self, id: &ServiceId) -> Result<(), ServiceError>;
}

#[cfg(target_os = "linux")]
pub(crate) fn backend() -> Result<Box<dyn Backend>, ServiceError> {
    Ok(Box::new(systemd::Systemd::host()?))
}

#[cfg(target_os = "macos")]
pub(crate) fn backend() -> Result<Box<dyn Backend>, ServiceError> {
    Ok(Box::new(launchd::Launchd::host()?))
}

#[cfg(windows)]
pub(crate) fn backend() -> Result<Box<dyn Backend>, ServiceError> {
    Ok(Box::new(windows::Scm))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn backend() -> Result<Box<dyn Backend>, ServiceError> {
    Err(ServiceError::Unsupported(
        "services can be managed on Linux (systemd), macOS (launchd) and Windows".into(),
    ))
}
