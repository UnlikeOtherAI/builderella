//! Operating-system access. One module per OS implements the same set of
//! functions; everything above this module is portable.
//!
//! Each OS module provides:
//!
//! * `SamplerState::new()`: OS handles and counters a sampler keeps.
//! * `cpu_times(&mut SamplerState) -> Option<CpuTimes>`
//! * `logical_cpus() -> u32`
//! * `memory() -> Option<MemoryReading>`
//! * `pressure(&SamplerState, Option<&MemoryReading>) -> (PressureLevel, Origin)`
//! * `disk_usage(&Path) -> Option<(total, free)>`
//! * `power() -> Option<PowerState>`
//! * `thermal_fast(&SamplerState) -> Option<ThermalState>`: cheap, read on
//!   every sample.
//! * `thermal_slow() -> Option<ThermalState>`: may run a command; cached.
//! * `hardware() -> Hardware`, `os_version()`, `kernel()`, `hostname()`
//! * `boot_seed() -> Option<Vec<u8>>`: bytes that identify this boot.
//! * `gui_session() -> Option<GuiSession>`
//! * `process_cpu_time() -> Duration` (tests only)

use bella_core::platform::CapabilityState;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
pub(crate) mod os;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
pub(crate) mod os;

#[cfg(windows)]
#[path = "windows.rs"]
pub(crate) mod os;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("bella-probe supports Linux, macOS and Windows only");

/// Where a telemetry value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A counter the OS reports directly.
    Measured,
    /// Derived by a rule of this crate from other counters.
    #[cfg_attr(not(windows), allow(dead_code))]
    Estimated,
    /// Could not be read.
    Unavailable,
}

/// Physical memory and swap, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MemoryReading {
    pub total: u64,
    pub available: u64,
    /// `false` when `available` is an estimate rather than the OS's figure.
    pub available_exact: bool,
    pub swap_used: Option<u64>,
    /// System commit charge and commit limit, where the OS has the concept.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub commit: Option<(u64, u64)>,
}

/// A graphical session on this machine, as seen from this process.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GuiSession {
    /// `aqua`, `windows`, `x11` or `wayland`.
    pub kind: &'static str,
    pub state: CapabilityState,
}

/// CPU time used so far by this process and by the children it has waited
/// for.
#[cfg(all(test, unix))]
pub(crate) fn unix_process_cpu_time() -> std::time::Duration {
    let mut total = std::time::Duration::ZERO;
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        // SAFETY: all-zero bytes are a valid `rusage`, which is plain integers.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is a live, writable `rusage`.
        if unsafe { libc::getrusage(who, &mut usage) } != 0 {
            continue;
        }
        for time in [usage.ru_utime, usage.ru_stime] {
            total += std::time::Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1000);
        }
    }
    total
}
