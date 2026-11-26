//! Fixtures: one simulated host whose observations the tests set directly.

use bella_core::job::{Priority, RetryClass};
use bella_core::machine::{
    Hardware, LocalControl, MachinePolicy, MachineProfile, MeasurementQuality, PowerState,
    PressureLevel, Telemetry, ThermalState,
};
use bella_core::resources::ResourceVector;
use bella_core::{AttemptId, UnixMillis};
use bella_protect::{Action, Budget, Grant, History, Input, ProtectConfig, Rejection};

pub use bella_core::defaults::{GIB, MIB};

/// Start of every scenario.
pub const T0: UnixMillis = UnixMillis(1_800_000_000_000);

pub fn hardware(logical_cpus: u32, memory_gib: u64) -> Hardware {
    Hardware {
        logical_cpus,
        physical_cpus: None,
        cpu_model: String::new(),
        memory_bytes: memory_gib * GIB,
        disks: Vec::new(),
        gpus: Vec::new(),
    }
}

/// A quiet host: nothing here limits admission.
pub fn healthy(at: UnixMillis, memory_gib: u64) -> Telemetry {
    Telemetry {
        sampled_at: at,
        cpu_used_milli: 500,
        cpu_managed_milli: 0,
        memory_total_bytes: memory_gib * GIB,
        memory_available_bytes: (memory_gib - 2) * GIB,
        memory_managed_bytes: 0,
        swap_used_bytes: 0,
        memory_pressure: PressureLevel::Normal,
        disk_total_bytes: 500 * GIB,
        disk_free_bytes: 300 * GIB,
        power: PowerState::default(),
        thermal: ThermalState::Nominal,
        quality: MeasurementQuality::Good,
    }
}

pub fn id(n: u32) -> AttemptId {
    AttemptId::derive(&n.to_be_bytes())
}

/// An accepted offer that has not started.
pub fn offered(n: u32, cpu_milli: u64, memory_bytes: u64) -> Grant {
    Grant {
        attempt_id: id(n),
        reserved: ResourceVector::new(cpu_milli, memory_bytes, 0),
        memory_used_bytes: None,
        cpu_used_milli: None,
        disk_used_bytes: None,
        started: false,
        priority: Priority::Normal,
        retry_class: RetryClass::Pure,
        started_at: None,
        suspended: false,
    }
}

/// A started attempt using `memory_used` of its reservation.
pub fn running(n: u32, cpu_milli: u64, memory_bytes: u64, memory_used: u64) -> Grant {
    Grant {
        memory_used_bytes: Some(memory_used),
        started: true,
        started_at: Some(T0),
        ..offered(n, cpu_milli, memory_bytes)
    }
}

pub fn request(cpu_milli: u64, memory_bytes: u64) -> ResourceVector {
    ResourceVector::new(cpu_milli, memory_bytes, 0)
}

pub struct Host {
    pub policy: MachinePolicy,
    pub hardware: Hardware,
    pub telemetry: Option<Telemetry>,
    pub now: UnixMillis,
    pub grants: Vec<Grant>,
    pub control: LocalControl,
    pub config: ProtectConfig,
}

impl Host {
    /// The worked example: a protected workstation with 10 logical CPUs
    /// and 32 GiB, quiet, with a fresh sample.
    pub fn workstation() -> Self {
        Self::with(MachineProfile::ProtectedWorkstation, 10, 32)
    }

    /// The same machine without the one-attempt limit, for scenarios with
    /// several grants.
    pub fn shared_workstation() -> Self {
        let mut host = Self::workstation();
        host.policy.max_concurrent_attempts = None;
        host
    }

    pub fn with(profile: MachineProfile, logical_cpus: u32, memory_gib: u64) -> Self {
        Self {
            policy: MachinePolicy::defaults_for(profile),
            hardware: hardware(logical_cpus, memory_gib),
            telemetry: Some(healthy(T0, memory_gib)),
            now: T0,
            grants: Vec::new(),
            control: LocalControl::default(),
            config: ProtectConfig::default(),
        }
    }

    pub fn input(&self) -> Input<'_> {
        Input {
            policy: &self.policy,
            hardware: &self.hardware,
            telemetry: self.telemetry.as_ref(),
            now: self.now,
            grants: &self.grants,
            control: self.control,
            config: &self.config,
        }
    }

    pub fn budget(&self) -> Budget {
        bella_protect::budget(&self.input())
    }

    pub fn admit(&self, request: &ResourceVector) -> Result<(), Rejection> {
        bella_protect::admit(&self.input(), request)
    }

    pub fn sample(&mut self) -> &mut Telemetry {
        self.telemetry.as_mut().expect("this host has telemetry")
    }

    pub fn set_pressure(&mut self, level: PressureLevel) {
        self.sample().memory_pressure = level;
    }

    pub fn set_available(&mut self, bytes: u64) {
        self.sample().memory_available_bytes = bytes;
    }

    /// Foreground CPU use, with managed work using `managed` on top.
    pub fn set_cpu(&mut self, external: u64, managed: u64) {
        let sample = self.sample();
        sample.cpu_used_milli = external + managed;
        sample.cpu_managed_milli = managed;
    }

    /// One turn of the protection loop one second later: take a sample,
    /// decide, apply everything.
    pub fn tick(&mut self, history: &mut History) -> Vec<Action> {
        self.now = self.now.plus_secs(1);
        if let Some(sample) = self.telemetry.as_mut() {
            sample.sampled_at = self.now;
        }
        history.observe(&self.input());
        let todo = bella_protect::actions(&self.input(), history);
        history.record(&todo, self.now);
        todo
    }

    /// Seconds since the start of the scenario.
    pub fn elapsed(&self) -> u64 {
        self.now.since(T0).as_secs()
    }
}

pub fn evicted(actions: &[Action]) -> Vec<AttemptId> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Evict { attempt_id, .. } => Some(attempt_id.clone()),
            _ => None,
        })
        .collect()
}

pub fn cpu_limits(actions: &[Action]) -> Vec<(AttemptId, u64)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::SetCpuLimit {
                attempt_id,
                cpu_milli,
            } => Some((attempt_id.clone(), *cpu_milli)),
            _ => None,
        })
        .collect()
}

pub fn stops(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::StopAdmission { .. }))
        .count()
}

pub fn resumes(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::ResumeAdmission))
        .count()
}
