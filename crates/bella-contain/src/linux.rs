//! cgroup v2 adapter. Requires a delegated cgroup subtree.

use crate::{ContainError, Containment, Limits, Scope};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::EnforcementReport;

pub struct CgroupV2;

impl CgroupV2 {
    /// `Err(Unavailable)` when no writable, delegated cgroup v2 subtree exists.
    pub fn detect() -> Result<Self, ContainError> {
        Err(ContainError::Unavailable("cgroup v2 adapter is not implemented yet".into()))
    }
}

impl Containment for CgroupV2 {
    fn kind(&self) -> ScopeKind {
        ScopeKind::LinuxCgroup
    }
    fn mechanism(&self) -> &'static str {
        "cgroup-v2"
    }
    fn enforcement(&self) -> EnforcementReport {
        EnforcementReport::none()
    }
    fn self_test(&self) -> Result<EnforcementReport, ContainError> {
        Err(ContainError::Unsupported("self_test"))
    }
    fn create(&self, _name: &str, _limits: &Limits) -> Result<Box<dyn Scope>, ContainError> {
        Err(ContainError::Unsupported("create"))
    }
    fn adopt(&self, _identity: &ScopeIdentity) -> Result<Option<Box<dyn Scope>>, ContainError> {
        Err(ContainError::Unsupported("adopt"))
    }
}
