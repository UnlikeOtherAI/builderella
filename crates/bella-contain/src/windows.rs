//! Job Object adapter for Windows.

use crate::{ContainError, Containment, Limits, Scope};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::EnforcementReport;

pub struct JobObjects;

impl JobObjects {
    pub fn new() -> Result<Self, ContainError> {
        Ok(Self)
    }
}

impl Containment for JobObjects {
    fn kind(&self) -> ScopeKind {
        ScopeKind::WindowsJob
    }
    fn mechanism(&self) -> &'static str {
        "job-object"
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
