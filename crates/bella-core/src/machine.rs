//! Machines: profile, protection policy, inventory and live telemetry.
//!
//! One physical host has one resource budget. A VM, WSL environment or
//! container on that host divides the budget; it never adds to it.

use crate::defaults::GIB;
use crate::ids::ProjectId;
use crate::platform::{Capability, IsolationLevel, Platform};
use crate::resources::{EnforcementReport, ResourceVector};
use crate::time::UnixMillis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineProfile {
    /// Someone works on this machine. Foreground reserve, low-priority
    /// builds, conservative concurrency.
    ProtectedWorkstation,
    /// High sustained utilisation within thermal and disk limits.
    DedicatedBuilder,
    /// Builder rules plus power, battery and sleep restrictions.
    PortableBuilder,
    /// Controller and services only; build admission disabled.
    InfrastructureOnly,
    /// Fleet-created cloud worker with a fixed expiry.
    EphemeralCloud,
}

/// Machine-owner policy. A project recipe can never lower these floors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachinePolicy {
    /// Bumped on every change; workers and offers carry it.
    pub revision: u64,
    pub profile: MachineProfile,
    pub accept_builds: bool,
    /// Share of logical CPU capacity withheld for the foreground and OS.
    pub foreground_cpu_percent: u8,
    /// Share of RAM withheld for the foreground and OS...
    pub foreground_memory_percent: u8,
    /// ...but never less than this many bytes.
    pub foreground_memory_min_bytes: u64,
    /// Instantaneous safety floor on available memory. Checked separately
    /// from the entitlement above; the two are not added together.
    pub available_memory_floor_bytes: u64,
    pub disk_free_floor_percent: u8,
    pub disk_free_floor_min_bytes: u64,
    /// Explicit cap on managed resources, applied on top of the floors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_managed: Option<ResourceVector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_attempts: Option<u32>,
    /// Run managed work at background priority where the OS supports it.
    pub low_priority: bool,
    /// Accept new work only on external power.
    pub require_ac_power: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_battery_percent: Option<u8>,
    /// `None` allows every project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_projects: Option<Vec<ProjectId>>,
    /// Declared countable capacity: simulators, GUI sessions, service quotas.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, u64>,
    #[serde(default)]
    pub pools: Vec<String>,
    /// Longest an attempt may keep running without controller contact,
    /// regardless of what its recipe asks for.
    pub max_disconnected_secs: u64,
    pub cleanup_grace_secs: u64,
}

impl MachinePolicy {
    /// Starting policy for a profile. These are defaults to validate on the
    /// actual machine, not evidence of measured capacity.
    pub fn defaults_for(profile: MachineProfile) -> Self {
        let base = Self {
            revision: 1,
            profile,
            accept_builds: true,
            foreground_cpu_percent: 10,
            foreground_memory_percent: 10,
            foreground_memory_min_bytes: 2 * GIB,
            available_memory_floor_bytes: GIB,
            disk_free_floor_percent: 10,
            disk_free_floor_min_bytes: 10 * GIB,
            max_managed: None,
            max_concurrent_attempts: None,
            low_priority: false,
            require_ac_power: false,
            min_battery_percent: None,
            allowed_projects: None,
            tokens: BTreeMap::new(),
            pools: Vec::new(),
            max_disconnected_secs: 15 * 60,
            cleanup_grace_secs: 10,
        };
        match profile {
            MachineProfile::ProtectedWorkstation => Self {
                foreground_cpu_percent: 20,
                foreground_memory_percent: 20,
                foreground_memory_min_bytes: 4 * GIB,
                available_memory_floor_bytes: 2 * GIB,
                disk_free_floor_percent: 15,
                disk_free_floor_min_bytes: 20 * GIB,
                max_concurrent_attempts: Some(1),
                low_priority: true,
                ..base
            },
            MachineProfile::DedicatedBuilder => base,
            MachineProfile::PortableBuilder => Self {
                foreground_cpu_percent: 20,
                foreground_memory_percent: 20,
                foreground_memory_min_bytes: 4 * GIB,
                available_memory_floor_bytes: 2 * GIB,
                max_concurrent_attempts: Some(2),
                low_priority: true,
                require_ac_power: true,
                min_battery_percent: Some(50),
                max_disconnected_secs: 5 * 60,
                ..base
            },
            MachineProfile::InfrastructureOnly => Self { accept_builds: false, ..base },
            MachineProfile::EphemeralCloud => Self {
                foreground_cpu_percent: 5,
                foreground_memory_percent: 5,
                foreground_memory_min_bytes: GIB,
                available_memory_floor_bytes: GIB / 2,
                max_disconnected_secs: 2 * 60,
                ..base
            },
        }
    }

    /// The static upper bound on managed work: hardware minus the foreground
    /// entitlement, then capped by `max_managed`. Live pressure, foreground
    /// use and disk floors reduce it further at admission time.
    pub fn managed_ceiling(&self, hardware: &Hardware) -> ResourceVector {
        let total_cpu = u64::from(hardware.logical_cpus) * 1000;
        let cpu = total_cpu * (100 - u64::from(self.foreground_cpu_percent.min(100))) / 100;
        let reserve = (hardware.memory_bytes / 100 * u64::from(self.foreground_memory_percent.min(100)))
            .max(self.foreground_memory_min_bytes);
        let memory = hardware.memory_bytes.saturating_sub(reserve);
        let mut ceiling = ResourceVector {
            cpu_milli: cpu,
            memory_bytes: memory,
            disk_bytes: u64::MAX,
            tokens: self.tokens.clone(),
        };
        if let Some(cap) = &self.max_managed {
            ceiling.cpu_milli = ceiling.cpu_milli.min(cap.cpu_milli);
            ceiling.memory_bytes = ceiling.memory_bytes.min(cap.memory_bytes);
            if cap.disk_bytes > 0 {
                ceiling.disk_bytes = cap.disk_bytes;
            }
        }
        ceiling
    }

    /// Bytes that must stay free on a volume of the given size.
    pub fn disk_floor_bytes(&self, volume_total_bytes: u64) -> u64 {
        (volume_total_bytes / 100 * u64::from(self.disk_free_floor_percent.min(100)))
            .max(self.disk_free_floor_min_bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    pub mount: String,
    pub total_bytes: u64,
    #[serde(default)]
    pub filesystem: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub logical_cpus: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cpus: Option<u32>,
    #[serde(default)]
    pub cpu_model: String,
    pub memory_bytes: u64,
    #[serde(default)]
    pub disks: Vec<DiskInfo>,
    #[serde(default)]
    pub gpus: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInfo {
    pub platform: Platform,
    /// e.g. `12.7.6`, `10.0.26100`, `24.04`.
    pub os_version: String,
    #[serde(default)]
    pub kernel: String,
    /// Display label only. Never an identity.
    #[serde(default)]
    pub hostname: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorInfo {
    pub isolation: IsolationLevel,
    /// `cgroup-v2`, `job-object`, `process-group`, `docker`, ...
    pub mechanism: String,
    pub enforcement: EnforcementReport,
}

/// Relatively static facts about a machine. Refreshed on enrollment, boot
/// and change; `revision` increases whenever the content changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub revision: u64,
    pub hardware: Hardware,
    pub os: OsInfo,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub executors: Vec<ExecutorInfo>,
    pub worker_version: String,
}

impl Inventory {
    /// Enforcement of the native executor, or "none" if there is none.
    pub fn native_enforcement(&self) -> EnforcementReport {
        self.executors
            .iter()
            .find(|e| e.isolation == IsolationLevel::Native)
            .map(|e| e.enforcement.clone())
            .unwrap_or_else(EnforcementReport::none)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    #[default]
    Unknown,
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalState {
    #[default]
    Unknown,
    Nominal,
    Fair,
    Serious,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementQuality {
    Good,
    /// Some counters are estimated or unavailable on this OS.
    Degraded,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerState {
    /// `None` when the machine has no battery or the state is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ac: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_percent: Option<u8>,
}

/// One observation of a physical host. Measured usage and reserved capacity
/// are different things; this is only the former.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Telemetry {
    pub sampled_at: UnixMillis,
    /// Whole-host CPU use in milli-CPUs, averaged over the sample interval.
    pub cpu_used_milli: u64,
    /// The part of `cpu_used_milli` attributable to managed scopes.
    pub cpu_managed_milli: u64,
    pub memory_total_bytes: u64,
    /// Memory the OS could hand out without swapping, by its own accounting.
    pub memory_available_bytes: u64,
    /// Memory charged to managed scopes.
    pub memory_managed_bytes: u64,
    pub swap_used_bytes: u64,
    pub memory_pressure: PressureLevel,
    /// Work volume.
    pub disk_total_bytes: u64,
    pub disk_free_bytes: u64,
    pub power: PowerState,
    pub thermal: ThermalState,
    pub quality: MeasurementQuality,
}

impl Telemetry {
    /// Host activity that is not managed work: the browser, the IDE,
    /// unrelated commands.
    pub fn cpu_external_milli(&self) -> u64 {
        self.cpu_used_milli.saturating_sub(self.cpu_managed_milli)
    }

    pub fn memory_external_bytes(&self) -> u64 {
        self.memory_total_bytes
            .saturating_sub(self.memory_available_bytes)
            .saturating_sub(self.memory_managed_bytes)
    }
}

/// Independent conditions that limit a machine. A machine can be reachable
/// yet unavailable for a particular job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityFlag {
    Draining,
    UserPaused,
    PressureLimited,
    OnBattery,
    Overheated,
    DiskLow,
    Sleeping,
    /// No heartbeat within the freshness window: no new offers.
    Stale,
    /// No contact for longer still. Existing reservations stay held.
    Unreachable,
    /// Cleanup could not be verified.
    Quarantined,
    Upgrading,
    BuildsDisabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineLifecycle {
    Enrolling,
    Probing,
    Ready,
    Retired,
    Revoked,
}

/// Controls the machine owner sets locally. They work while the controller
/// is offline and are reported when it reconnects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalControl {
    /// Accept no new work; running work continues.
    pub paused: bool,
    /// Accept no new work and report when idle.
    pub draining: bool,
    /// Temporary use of spare capacity up to the protected ceiling. Never
    /// lowers the foreground floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boost_until: Option<UnixMillis>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(cpus: u32, gib: u64) -> Hardware {
        Hardware {
            logical_cpus: cpus,
            physical_cpus: None,
            cpu_model: String::new(),
            memory_bytes: gib * GIB,
            disks: vec![],
            gpus: vec![],
        }
    }

    #[test]
    fn workstation_keeps_a_fifth() {
        let policy = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
        let ceiling = policy.managed_ceiling(&hw(10, 32));
        assert_eq!(ceiling.cpu_milli, 8000);
        // 20% of 32 GiB is 6.4 GiB, above the 4 GiB minimum.
        assert_eq!(ceiling.memory_bytes, 32 * GIB - 32 * GIB / 100 * 20);
    }

    #[test]
    fn small_machine_uses_the_absolute_memory_minimum() {
        let policy = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
        let ceiling = policy.managed_ceiling(&hw(4, 8));
        assert_eq!(ceiling.memory_bytes, 4 * GIB);
    }

    #[test]
    fn infrastructure_machines_refuse_builds() {
        assert!(!MachinePolicy::defaults_for(MachineProfile::InfrastructureOnly).accept_builds);
    }

    #[test]
    fn disk_floor_is_the_larger_rule() {
        let policy = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
        assert_eq!(policy.disk_floor_bytes(100 * GIB), 20 * GIB);
        assert_eq!(policy.disk_floor_bytes(1000 * GIB), 150 * GIB);
    }
}
