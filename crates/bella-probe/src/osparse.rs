//! Parsers and normalisation rules for operating-system counters.
//!
//! Pure functions over text or numbers read elsewhere, so the rules that
//! turn raw OS data into [`bella_core::machine::Telemetry`] values are
//! documented in one place and tested on every platform.

use bella_core::machine::{PowerState, PressureLevel, ThermalState};

/// Cumulative CPU time of the whole host in an OS-specific tick unit.
/// Only differences between two readings are meaningful.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Time spent doing work: everything except idle and I/O wait.
    pub busy: u64,
    /// All accounted time, busy and idle.
    pub total: u64,
}

/// Whole-host CPU use between two readings, in milli-CPUs.
///
/// The busy share of the interval is scaled by the number of logical CPUs,
/// so a fully loaded 8-CPU host reports 8000. Returns `None` when no time
/// was accounted between the readings or a counter went backwards.
pub fn cpu_used_milli(previous: CpuTimes, current: CpuTimes, logical_cpus: u32) -> Option<u64> {
    let total = current.total.checked_sub(previous.total)?;
    let busy = current.busy.checked_sub(previous.busy)?;
    if total == 0 {
        return None;
    }
    let capacity = u128::from(logical_cpus) * 1000;
    let used = u128::from(busy.min(total)) * capacity / u128::from(total);
    Some(used as u64)
}

/// CPU use of managed scopes between two samples, in milli-CPUs: the growth
/// of their cumulative CPU time divided by the wall time that passed.
///
/// A cumulative counter that went backwards (the caller dropped a finished
/// scope from its total) counts as no use for that interval. The result is
/// capped at the host's capacity.
pub fn managed_cpu_milli(
    previous_ms: u64,
    current_ms: u64,
    wall_ms: u64,
    logical_cpus: u32,
) -> Option<u64> {
    if wall_ms == 0 {
        return None;
    }
    let delta = u128::from(current_ms.saturating_sub(previous_ms));
    let milli = delta * 1000 / u128::from(wall_ms);
    Some(milli.min(u128::from(logical_cpus) * 1000) as u64)
}

/// Aggregate line of Linux `/proc/stat`:
/// `cpu user nice system idle iowait irq softirq steal guest guest_nice`.
///
/// Idle and I/O wait are idle time. Guest time is already included in user
/// time by the kernel and is not added again.
pub fn proc_stat_cpu(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total - idle,
        total,
    })
}

/// Number of per-CPU lines (`cpu0`, `cpu1`, ...) in Linux `/proc/stat`:
/// the CPUs the kernel is accounting, regardless of this process's
/// affinity or cgroup quota.
pub fn proc_stat_cpu_count(text: &str) -> u32 {
    text.lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count() as u32
}

/// Fields of Linux `/proc/meminfo`, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    /// `MemAvailable`: the kernel's own estimate of memory that can be
    /// handed out without swapping. Absent before Linux 3.14.
    pub available: Option<u64>,
    pub free: u64,
    pub buffers: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    /// `MemAvailable`, or `MemFree + Buffers + Cached` on kernels that do
    /// not report it. The second value is an estimate: `true` in the result
    /// means the kernel's figure was used.
    pub fn available_bytes(&self) -> (u64, bool) {
        match self.available {
            Some(available) => (available.min(self.total), true),
            None => (
                (self.free + self.buffers + self.cached).min(self.total),
                false,
            ),
        }
    }

    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

/// Parse Linux `/proc/meminfo`. `None` without a `MemTotal` line.
pub fn meminfo(text: &str) -> Option<MemInfo> {
    let field = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let mut parts = rest.split_whitespace();
            let value: u64 = parts.next()?.parse().ok()?;
            // Values are in kibibytes; a line without a unit is in bytes.
            Some(match parts.next() {
                Some("kB") => value * 1024,
                _ => value,
            })
        })
    };
    Some(MemInfo {
        total: field("MemTotal")?,
        available: field("MemAvailable"),
        free: field("MemFree").unwrap_or(0),
        buffers: field("Buffers").unwrap_or(0),
        cached: field("Cached").unwrap_or(0),
        swap_total: field("SwapTotal").unwrap_or(0),
        swap_free: field("SwapFree").unwrap_or(0),
    })
}

/// `some avg10` from Linux `/proc/pressure/memory`: the percentage of the
/// last ten seconds in which at least one task was stalled on memory.
pub fn psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// `some avg10` at or above which Linux memory pressure is `Warning`.
pub const PSI_WARNING_PERCENT: f64 = 10.0;
/// `some avg10` at or above which Linux memory pressure is `Critical`.
pub const PSI_CRITICAL_PERCENT: f64 = 40.0;

/// Linux memory pressure from PSI `some avg10`.
///
/// Below [`PSI_WARNING_PERCENT`] (10 %) is `Normal`; from there up to
/// [`PSI_CRITICAL_PERCENT`] (40 %) is `Warning`; at or above it is
/// `Critical`. These are starting values: a host that spends a tenth of its
/// time stalled on reclaim is visibly slow, and one that spends two fifths
/// is close to thrashing.
pub fn pressure_from_psi(some_avg10: f64) -> PressureLevel {
    if some_avg10 >= PSI_CRITICAL_PERCENT {
        PressureLevel::Critical
    } else if some_avg10 >= PSI_WARNING_PERCENT {
        PressureLevel::Warning
    } else {
        PressureLevel::Normal
    }
}

/// macOS `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning,
/// 4 critical. Any other value is `Unknown`.
pub fn pressure_from_macos_level(level: i64) -> PressureLevel {
    match level {
        1 => PressureLevel::Normal,
        2 => PressureLevel::Warning,
        4 => PressureLevel::Critical,
        _ => PressureLevel::Unknown,
    }
}

/// Windows memory pressure, a heuristic because Windows publishes no
/// graded pressure level.
///
/// * `Critical`: the system's low-memory resource notification is
///   signalled, or less than 5 % of physical memory is available, or the
///   commit charge is at or above 95 % of the commit limit.
/// * `Warning`: less than 10 % of physical memory is available, or the
///   commit charge is at or above 90 % of the commit limit.
/// * `Normal` otherwise.
///
/// A commit limit of zero means the commit figures are unknown and only the
/// physical ratio is used.
pub fn windows_pressure(
    available_bytes: u64,
    total_bytes: u64,
    commit_bytes: u64,
    commit_limit_bytes: u64,
    low_memory_signalled: bool,
) -> PressureLevel {
    if total_bytes == 0 {
        return PressureLevel::Unknown;
    }
    let available_percent = u128::from(available_bytes) * 100 / u128::from(total_bytes);
    let commit_percent = if commit_limit_bytes == 0 {
        0
    } else {
        u128::from(commit_bytes) * 100 / u128::from(commit_limit_bytes)
    };
    if low_memory_signalled || available_percent < 5 || commit_percent >= 95 {
        PressureLevel::Critical
    } else if available_percent < 10 || commit_percent >= 90 {
        PressureLevel::Warning
    } else {
        PressureLevel::Normal
    }
}

/// Page counts from macOS `host_statistics64(HOST_VM_INFO64)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MacVmPages {
    /// `free_count`. The kernel includes speculative pages in this number;
    /// `vm_stat` prints "Pages free" as `free_count - speculative_count`.
    pub free_including_speculative: u64,
    pub inactive: u64,
    pub purgeable: u64,
}

impl MacVmPages {
    /// Pages macOS can hand out without swapping: free, speculative,
    /// inactive and purgeable. In `vm_stat` terms that is
    /// "Pages free" + "Pages speculative" + "Pages inactive" +
    /// "Pages purgeable". Not just "free": macOS keeps free pages near zero
    /// by design.
    pub fn available_pages(&self) -> u64 {
        self.free_including_speculative + self.inactive + self.purgeable
    }
}

/// Root-relative fields of one entry in Linux `/sys/class/power_supply`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerSupply {
    /// `type`: `Mains`, `Battery`, `USB`, `UPS`, ...
    pub kind: String,
    /// `online`, for mains-like supplies.
    pub online: Option<bool>,
    /// `status`: `Charging`, `Discharging`, `Full`, `Not charging`.
    pub status: Option<String>,
    /// `capacity`, percent.
    pub capacity: Option<u8>,
    /// `scope`: `Device` marks the battery of a peripheral (mouse,
    /// keyboard), which says nothing about the host.
    pub scope: Option<String>,
}

/// Power state from Linux power-supply entries.
///
/// A host without a system battery reports `on_ac: None` and
/// `battery_percent: None`, the same as every other OS in this crate.
/// With a battery, `on_ac` is `true` when a mains-like supply is online or
/// the battery is charging or full, `false` when it is discharging.
pub fn power_from_supplies(supplies: &[PowerSupply]) -> PowerState {
    let system = |supply: &&PowerSupply| supply.scope.as_deref() != Some("Device");
    let batteries: Vec<&PowerSupply> = supplies
        .iter()
        .filter(system)
        .filter(|supply| supply.kind == "Battery")
        .collect();
    if batteries.is_empty() {
        return PowerState::default();
    }
    let mains_online = supplies
        .iter()
        .filter(system)
        .filter(|supply| supply.kind != "Battery")
        .any(|supply| supply.online == Some(true));
    let status = |wanted: &[&str]| {
        batteries.iter().any(|battery| {
            battery
                .status
                .as_deref()
                .is_some_and(|status| wanted.contains(&status))
        })
    };
    let on_ac = if mains_online {
        Some(true)
    } else if status(&["Discharging"]) {
        Some(false)
    } else if status(&["Charging", "Full"]) {
        Some(true)
    } else {
        None
    };
    let capacities: Vec<u32> = batteries
        .iter()
        .filter_map(|battery| battery.capacity.map(u32::from))
        .collect();
    let battery_percent = (!capacities.is_empty())
        .then(|| (capacities.iter().sum::<u32>() / capacities.len() as u32).min(100) as u8);
    PowerState {
        on_ac,
        battery_percent,
    }
}

/// Power state from macOS `pmset -g batt`.
///
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=1234567)    87%; discharging; 4:12 remaining present: true
/// ```
///
/// A Mac without an internal battery that draws from AC reports
/// `on_ac: None`, like desktops on the other platforms. Drawing from
/// `Battery Power` or `UPS Power` is `on_ac: Some(false)`. Returns `None`
/// when the text has no "Now drawing from" line.
pub fn pmset_batt(text: &str) -> Option<PowerState> {
    let source = text.lines().find_map(|line| {
        let rest = line.split_once("drawing from '")?.1;
        Some(rest.split('\'').next().unwrap_or(rest).to_string())
    })?;
    let battery_percent = text
        .lines()
        .filter(|line| line.contains("InternalBattery"))
        .find_map(|line| {
            let percent = line.find('%')?;
            let digits = line[..percent]
                .rfind(|c: char| !c.is_ascii_digit())
                .map_or(0, |i| i + 1);
            line[digits..percent].parse::<u8>().ok()
        })
        .map(|percent| percent.min(100));
    let on_ac = match source.as_str() {
        "AC Power" => battery_percent.is_some().then_some(true),
        _ => Some(false),
    };
    Some(PowerState {
        on_ac,
        battery_percent,
    })
}

/// Thermal state from macOS `pmset -g therm`.
///
/// The command prints a thermal warning level and CPU power limits once
/// the system has recorded them:
///
/// * `CPU_Speed_Limit` 100 is `Nominal`, 70-99 `Fair`, 40-69 `Serious`,
///   below 40 `Critical`.
/// * `CPU_Scheduler_Limit` below 100 is at least `Fair`.
/// * `Thermal Warning Level` above zero is at least `Serious`, 10 or more
///   `Critical`.
///
/// The worst of these is returned. `None` means nothing was recorded
/// ("No thermal warning level has been recorded", as on Apple silicon
/// most of the time), which is absence of evidence, not a nominal reading.
pub fn pmset_therm(text: &str) -> Option<ThermalState> {
    let value = |name: &str| -> Option<i64> {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name)
                .then(|| value.trim().parse().ok())
                .flatten()
        })
    };
    let mut states = Vec::new();
    if let Some(limit) = value("CPU_Speed_Limit") {
        states.push(match limit {
            100.. => ThermalState::Nominal,
            70..=99 => ThermalState::Fair,
            40..=69 => ThermalState::Serious,
            _ => ThermalState::Critical,
        });
    }
    if let Some(limit) = value("CPU_Scheduler_Limit") {
        states.push(if limit < 100 {
            ThermalState::Fair
        } else {
            ThermalState::Nominal
        });
    }
    if let Some(level) = value("Thermal Warning Level") {
        states.push(match level {
            ..=0 => ThermalState::Nominal,
            1..=9 => ThermalState::Serious,
            _ => ThermalState::Critical,
        });
    }
    states.into_iter().max()
}

/// Thermal state from the macOS thermal pressure level published under the
/// notification name `com.apple.system.thermalpressurelevel`, the source
/// behind `NSProcessInfo.thermalState`: 0 nominal, 1 moderate (`Fair`),
/// 2 heavy (`Serious`), 3 trapping and 4 sleeping (`Critical`).
pub fn thermal_from_macos_pressure_level(level: u64) -> ThermalState {
    match level {
        0 => ThermalState::Nominal,
        1 => ThermalState::Fair,
        2 => ThermalState::Serious,
        _ => ThermalState::Critical,
    }
}

/// Distribution version from Linux `os-release`: `VERSION_ID`, or
/// `BUILD_ID` on rolling distributions that have no version.
pub fn os_release_version(text: &str) -> Option<String> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix('=')?;
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            (!value.is_empty()).then(|| value.to_string())
        })
    };
    field("VERSION_ID").or_else(|| field("BUILD_ID"))
}

/// CPU model from Linux `/proc/cpuinfo`: `model name` on x86, `Model` or
/// `Hardware` on ARM boards. Empty when the kernel prints none.
pub fn cpuinfo_model(text: &str) -> String {
    ["model name", "Model", "Hardware", "cpu model"]
        .iter()
        .find_map(|name| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == *name && !value.trim().is_empty()).then(|| value.trim().to_string())
            })
        })
        .unwrap_or_default()
}

/// One line of Linux `/proc/self/mounts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mount: String,
    pub filesystem: String,
}

/// Parse Linux `/proc/self/mounts`, decoding the octal escapes the kernel
/// uses for spaces and tabs in paths.
pub fn mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Mount {
                device: unescape_mount(fields.next()?),
                mount: unescape_mount(fields.next()?),
                filesystem: fields.next()?.to_string(),
            })
        })
        .collect()
}

fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = (bytes[i] == b'\\' && i + 3 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 4]).ok())
            .flatten()
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a Linux mount is local storage worth listing in the inventory:
/// a block device or a ZFS/Btrfs/overlay root, not a pseudo filesystem,
/// loop image or network share.
pub fn is_storage_mount(mount: &Mount) -> bool {
    const PSEUDO: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "tmpfs",
        "cgroup",
        "cgroup2",
        "pstore",
        "bpf",
        "debugfs",
        "tracefs",
        "securityfs",
        "configfs",
        "fusectl",
        "mqueue",
        "hugetlbfs",
        "autofs",
        "binfmt_misc",
        "rpc_pipefs",
        "nsfs",
        "ramfs",
        "squashfs",
        "efivarfs",
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "9p",
        "fuse.portal",
        "fuse.gvfsd-fuse",
    ];
    if PSEUDO.contains(&mount.filesystem.as_str()) || mount.device.starts_with("/dev/loop") {
        return false;
    }
    mount.device.starts_with("/dev/")
        || matches!(mount.filesystem.as_str(), "zfs" | "btrfs" | "overlay")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT: &str = "cpu  74608 2520 24433 1117073 6176 4054 0 0 0 0\n\
cpu0 17714 607 6088 279303 1541 2026 0 0 0 0\n\
cpu1 18836 642 6219 279196 1547 1007 0 0 0 0\n\
cpu2 19174 638 6061 279251 1538 508 0 0 0 0\n\
cpu3 18884 633 6065 279323 1550 513 0 0 0 0\n\
intr 4208283 23 9 0 0 0 0 0 0 1 0 0 0 144 0 0 0\n\
ctxt 7284334\nbtime 1730000000\nprocesses 28392\nprocs_running 2\nprocs_blocked 0\n";

    #[test]
    fn proc_stat_aggregate_line() {
        let times = proc_stat_cpu(PROC_STAT).unwrap();
        let total = 74608 + 2520 + 24433 + 1117073 + 6176 + 4054;
        assert_eq!(times.total, total);
        assert_eq!(times.busy, total - 1117073 - 6176);
        assert_eq!(proc_stat_cpu_count(PROC_STAT), 4);
        assert_eq!(proc_stat_cpu("intr 1 2 3\n"), None);
        assert_eq!(proc_stat_cpu("cpu  1 2\n"), None);
        // Kernels before 2.6.11 print only four fields.
        assert_eq!(
            proc_stat_cpu("cpu  10 0 5 85\n"),
            Some(CpuTimes {
                busy: 15,
                total: 100
            })
        );
    }

    #[test]
    fn cpu_use_is_the_busy_share_of_all_cpus() {
        let before = CpuTimes {
            busy: 1000,
            total: 10_000,
        };
        let half = CpuTimes {
            busy: 1400,
            total: 10_800,
        };
        assert_eq!(cpu_used_milli(before, half, 8), Some(4000));
        let idle = CpuTimes {
            busy: 1000,
            total: 10_800,
        };
        assert_eq!(cpu_used_milli(before, idle, 8), Some(0));
        let full = CpuTimes {
            busy: 1800,
            total: 10_800,
        };
        assert_eq!(cpu_used_milli(before, full, 8), Some(8000));
        // No time accounted, or a counter that went backwards: no reading.
        assert_eq!(cpu_used_milli(before, before, 8), None);
        assert_eq!(cpu_used_milli(half, before, 8), None);
    }

    #[test]
    fn managed_cpu_is_cpu_time_over_wall_time() {
        // 1500 ms of CPU in one second is one and a half CPUs.
        assert_eq!(managed_cpu_milli(10_000, 11_500, 1000, 8), Some(1500));
        assert_eq!(managed_cpu_milli(10_000, 10_000, 1000, 8), Some(0));
        // A total that shrank is not negative use.
        assert_eq!(managed_cpu_milli(10_000, 4_000, 1000, 8), Some(0));
        // Never more than the machine has.
        assert_eq!(managed_cpu_milli(0, 100_000, 1000, 4), Some(4000));
        assert_eq!(managed_cpu_milli(0, 100, 0, 4), None);
    }

    const MEMINFO: &str = "MemTotal:       16303564 kB\n\
MemFree:          812340 kB\n\
MemAvailable:    9876540 kB\n\
Buffers:          345678 kB\n\
Cached:          7654320 kB\n\
SwapCached:        12345 kB\n\
SwapTotal:       2097148 kB\n\
SwapFree:        1048574 kB\n\
HugePages_Total:       0\n";

    #[test]
    fn meminfo_fields() {
        let info = meminfo(MEMINFO).unwrap();
        assert_eq!(info.total, 16303564 * 1024);
        assert_eq!(info.available, Some(9876540 * 1024));
        assert_eq!(info.available_bytes(), (9876540 * 1024, true));
        assert_eq!(info.swap_used(), (2097148 - 1048574) * 1024);
        assert_eq!(meminfo("Nothing: 1 kB\n"), None);
    }

    #[test]
    fn meminfo_without_memavailable_is_an_estimate() {
        let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n";
        let info = meminfo(old).unwrap();
        assert_eq!(info.available, None);
        assert_eq!(info.available_bytes(), (400 * 1024, false));
        assert_eq!(info.swap_used(), 0);
        // `SwapCached` must not be mistaken for `Cached`.
        let tricky = "MemTotal: 1000 kB\nSwapCached: 999 kB\nCached: 10 kB\n";
        assert_eq!(meminfo(tricky).unwrap().cached, 10 * 1024);
    }

    #[test]
    fn psi_thresholds() {
        let calm = "some avg10=0.00 avg60=0.12 avg300=0.05 total=123456\n\
full avg10=0.00 avg60=0.00 avg300=0.00 total=1234\n";
        assert_eq!(psi_some_avg10(calm), Some(0.0));
        let busy = "some avg10=12.50 avg60=3.00 avg300=1.00 total=99\nfull avg10=4.00 avg60=1.00 avg300=0.50 total=9\n";
        assert_eq!(psi_some_avg10(busy), Some(12.5));
        assert_eq!(psi_some_avg10("full avg10=1.00\n"), None);
        assert_eq!(pressure_from_psi(0.0), PressureLevel::Normal);
        assert_eq!(pressure_from_psi(9.99), PressureLevel::Normal);
        assert_eq!(pressure_from_psi(10.0), PressureLevel::Warning);
        assert_eq!(pressure_from_psi(39.99), PressureLevel::Warning);
        assert_eq!(pressure_from_psi(40.0), PressureLevel::Critical);
    }

    #[test]
    fn macos_pressure_levels() {
        assert_eq!(pressure_from_macos_level(1), PressureLevel::Normal);
        assert_eq!(pressure_from_macos_level(2), PressureLevel::Warning);
        assert_eq!(pressure_from_macos_level(4), PressureLevel::Critical);
        assert_eq!(pressure_from_macos_level(0), PressureLevel::Unknown);
        assert_eq!(pressure_from_macos_level(3), PressureLevel::Unknown);
    }

    #[test]
    fn windows_pressure_heuristic() {
        const GIB: u64 = 1 << 30;
        assert_eq!(
            windows_pressure(20 * GIB, 64 * GIB, 30 * GIB, 80 * GIB, false),
            PressureLevel::Normal
        );
        // Under 10 % available.
        assert_eq!(
            windows_pressure(6 * GIB, 64 * GIB, 30 * GIB, 80 * GIB, false),
            PressureLevel::Warning
        );
        // Commit charge at 90 % of the limit.
        assert_eq!(
            windows_pressure(20 * GIB, 64 * GIB, 72 * GIB, 80 * GIB, false),
            PressureLevel::Warning
        );
        assert_eq!(
            windows_pressure(3 * GIB, 64 * GIB, 30 * GIB, 80 * GIB, false),
            PressureLevel::Critical
        );
        assert_eq!(
            windows_pressure(20 * GIB, 64 * GIB, 76 * GIB, 80 * GIB, false),
            PressureLevel::Critical
        );
        // The OS low-memory signal overrides comfortable ratios.
        assert_eq!(
            windows_pressure(20 * GIB, 64 * GIB, 30 * GIB, 80 * GIB, true),
            PressureLevel::Critical
        );
        // Unknown commit figures leave the physical ratio in charge.
        assert_eq!(
            windows_pressure(20 * GIB, 64 * GIB, 0, 0, false),
            PressureLevel::Normal
        );
        assert_eq!(windows_pressure(0, 0, 0, 0, false), PressureLevel::Unknown);
    }

    #[test]
    fn macos_available_counts_more_than_free() {
        // `vm_stat` on a busy 32 GiB Mac: free 5252, speculative 3096,
        // inactive 347921, purgeable 10 pages of 16 KiB. The kernel's
        // free_count is 5252 + 3096.
        let pages = MacVmPages {
            free_including_speculative: 5252 + 3096,
            inactive: 347_921,
            purgeable: 10,
        };
        assert_eq!(pages.available_pages(), 356_279);
        assert!(pages.available_pages() * 16_384 > 5 * (1 << 30));
    }

    fn supply(
        kind: &str,
        online: Option<bool>,
        status: Option<&str>,
        capacity: Option<u8>,
    ) -> PowerSupply {
        PowerSupply {
            kind: kind.into(),
            online,
            status: status.map(str::to_string),
            capacity,
            scope: None,
        }
    }

    #[test]
    fn linux_power_supplies() {
        // Desktop: nothing, or mains only.
        assert_eq!(power_from_supplies(&[]), PowerState::default());
        assert_eq!(
            power_from_supplies(&[supply("Mains", Some(true), None, None)]),
            PowerState::default()
        );
        // Laptop on the charger.
        assert_eq!(
            power_from_supplies(&[
                supply("Mains", Some(true), None, None),
                supply("Battery", None, Some("Charging"), Some(81)),
            ]),
            PowerState {
                on_ac: Some(true),
                battery_percent: Some(81)
            }
        );
        // Laptop unplugged.
        assert_eq!(
            power_from_supplies(&[
                supply("Mains", Some(false), None, None),
                supply("Battery", None, Some("Discharging"), Some(42)),
            ]),
            PowerState {
                on_ac: Some(false),
                battery_percent: Some(42)
            }
        );
        // No mains entry at all: the battery status decides.
        assert_eq!(
            power_from_supplies(&[supply("Battery", None, Some("Full"), Some(100))]),
            PowerState {
                on_ac: Some(true),
                battery_percent: Some(100)
            }
        );
        assert_eq!(
            power_from_supplies(&[supply("Battery", None, Some("Unknown"), Some(55))]),
            PowerState {
                on_ac: None,
                battery_percent: Some(55)
            }
        );
        // A wireless mouse battery is not the host's battery.
        let mut mouse = supply("Battery", None, Some("Discharging"), Some(12));
        mouse.scope = Some("Device".into());
        assert_eq!(
            power_from_supplies(&[supply("Mains", Some(true), None, None), mouse]),
            PowerState::default()
        );
    }

    #[test]
    fn pmset_battery_report() {
        let desktop = "Now drawing from 'AC Power'\n";
        assert_eq!(pmset_batt(desktop), Some(PowerState::default()));
        let charging = "Now drawing from 'AC Power'\n \
-InternalBattery-0 (id=23855203)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(
            pmset_batt(charging),
            Some(PowerState {
                on_ac: Some(true),
                battery_percent: Some(100)
            })
        );
        let unplugged = "Now drawing from 'Battery Power'\n \
-InternalBattery-0 (id=23855203)\t87%; discharging; 4:12 remaining present: true\n";
        assert_eq!(
            pmset_batt(unplugged),
            Some(PowerState {
                on_ac: Some(false),
                battery_percent: Some(87)
            })
        );
        let ups = "Now drawing from 'UPS Power'\n -Back-UPS ES 700 (id=1234)\t64%; discharging; 0:21 remaining present: true\n";
        assert_eq!(
            pmset_batt(ups),
            Some(PowerState {
                on_ac: Some(false),
                battery_percent: None
            })
        );
        assert_eq!(pmset_batt("pmset: command failed\n"), None);
    }

    #[test]
    fn pmset_thermal_report() {
        let nothing = "Note: No thermal warning level has been recorded\n\
Note: No performance warning level has been recorded\n\
Note: No CPU power status has been recorded\n";
        assert_eq!(pmset_therm(nothing), None);
        let nominal = "Note: No thermal warning level has been recorded\n\
Note: No performance warning level has been recorded\n\
2024-11-01 10:00:00 +0100 CPU Power notify\n\
\tCPU_Scheduler_Limit \t= 100\n\tCPU_Available_CPUs \t= 8\n\tCPU_Speed_Limit \t= 100\n";
        assert_eq!(pmset_therm(nominal), Some(ThermalState::Nominal));
        let throttled = |limit: u32| {
            format!(
                "\tCPU_Scheduler_Limit \t= 100\n\tCPU_Available_CPUs \t= 8\n\tCPU_Speed_Limit \t= {limit}\n"
            )
        };
        assert_eq!(pmset_therm(&throttled(99)), Some(ThermalState::Fair));
        assert_eq!(pmset_therm(&throttled(70)), Some(ThermalState::Fair));
        assert_eq!(pmset_therm(&throttled(69)), Some(ThermalState::Serious));
        assert_eq!(pmset_therm(&throttled(39)), Some(ThermalState::Critical));
        let scheduler = "\tCPU_Scheduler_Limit \t= 80\n\tCPU_Speed_Limit \t= 100\n";
        assert_eq!(pmset_therm(scheduler), Some(ThermalState::Fair));
        let warning = "Thermal Warning Level = 5\n\tCPU_Speed_Limit \t= 100\n";
        assert_eq!(pmset_therm(warning), Some(ThermalState::Serious));
        assert_eq!(
            pmset_therm("Thermal Warning Level = 10\n"),
            Some(ThermalState::Critical)
        );
        assert_eq!(
            pmset_therm("Thermal Warning Level = 0\n"),
            Some(ThermalState::Nominal)
        );
    }

    #[test]
    fn macos_thermal_pressure_levels() {
        assert_eq!(thermal_from_macos_pressure_level(0), ThermalState::Nominal);
        assert_eq!(thermal_from_macos_pressure_level(1), ThermalState::Fair);
        assert_eq!(thermal_from_macos_pressure_level(2), ThermalState::Serious);
        assert_eq!(thermal_from_macos_pressure_level(3), ThermalState::Critical);
        assert_eq!(thermal_from_macos_pressure_level(4), ThermalState::Critical);
    }

    #[test]
    fn os_release_versions() {
        let ubuntu = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n\
VERSION=\"24.04.1 LTS (Noble Numbat)\"\nID=ubuntu\n";
        assert_eq!(os_release_version(ubuntu).as_deref(), Some("24.04"));
        let debian =
            "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nVERSION_ID=\"12\"\nID=debian\n";
        assert_eq!(os_release_version(debian).as_deref(), Some("12"));
        let fedora = "NAME=\"Fedora Linux\"\nVERSION_ID=44\nID=fedora\n";
        assert_eq!(os_release_version(fedora).as_deref(), Some("44"));
        let arch = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        assert_eq!(os_release_version(arch).as_deref(), Some("rolling"));
        assert_eq!(os_release_version("NAME=x\n"), None);
    }

    #[test]
    fn cpuinfo_models() {
        let x86 = "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel\t\t: 97\n\
model name\t: AMD Ryzen 9 7950X 16-Core Processor\nphysical id\t: 0\ncore id\t\t: 0\n";
        assert_eq!(cpuinfo_model(x86), "AMD Ryzen 9 7950X 16-Core Processor");
        let pi = "processor\t: 0\nBogoMIPS\t: 108.00\nCPU implementer\t: 0x41\n\n\
Hardware\t: BCM2835\nModel\t\t: Raspberry Pi 4 Model B Rev 1.4\n";
        assert_eq!(cpuinfo_model(pi), "Raspberry Pi 4 Model B Rev 1.4");
        let vm =
            "processor\t: 0\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd\nCPU implementer\t: 0x61\n";
        assert_eq!(cpuinfo_model(vm), "");
    }

    #[test]
    fn mount_table() {
        let text = "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
/dev/nvme0n1p2 / ext4 rw,relatime 0 0\n\
/dev/sda1 /mnt/My\\040Disk exfat rw 0 0\n\
/dev/loop3 /snap/core22/1621 squashfs ro,nodev,relatime 0 0\n\
tank/data /tank/data zfs rw,xattr,noacl 0 0\n\
server:/export /mnt/nfs nfs4 rw 0 0\n\
overlay / overlay rw,relatime 0 0\n";
        let all = mounts(text);
        assert_eq!(all.len(), 7);
        assert_eq!(all[2].mount, "/mnt/My Disk");
        let kept: Vec<&str> = all
            .iter()
            .filter(|m| is_storage_mount(m))
            .map(|m| m.mount.as_str())
            .collect();
        assert_eq!(kept, ["/", "/mnt/My Disk", "/tank/data", "/"]);
    }
}
