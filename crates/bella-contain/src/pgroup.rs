//! Process-group adapter for macOS and as the Linux fallback. Advisory limits.

use crate::{ContainError, Containment, Limits, Scope};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::EnforcementReport;

pub struct ProcessGroups;

impl ProcessGroups {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self
    }
}

impl Containment for ProcessGroups {
    fn kind(&self) -> ScopeKind {
        ScopeKind::UnixProcessGroup
    }
    fn mechanism(&self) -> &'static str {
        "process-group"
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
