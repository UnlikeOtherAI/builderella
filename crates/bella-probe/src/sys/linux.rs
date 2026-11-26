//! Linux: `/proc`, `/sys` and `statvfs`.

use super::{GuiSession, MemoryReading, Origin};
use crate::osparse::{self, CpuTimes, PowerSupply};
use bella_core::machine::{DiskInfo, Hardware, PowerState, PressureLevel, ThermalState};
use bella_core::platform::CapabilityState;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Linux needs no per-sampler state.
pub(crate) struct SamplerState;

impl SamplerState {
    pub fn new() -> Self {
        SamplerState
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// First line of a small text file, trimmed. `None` when it cannot be read
/// or is empty.
fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

pub(crate) fn cpu_times(_state: &mut SamplerState) -> Option<CpuTimes> {
    osparse::proc_stat_cpu(&read("/proc/stat")?)
}

/// CPUs the kernel accounts in `/proc/stat`. Unlike
/// `available_parallelism` this ignores the affinity mask and cgroup quota
/// of this process, which describe the worker, not the host.
pub(crate) fn logical_cpus() -> u32 {
    let counted = read("/proc/stat")
        .map(|stat| osparse::proc_stat_cpu_count(&stat))
        .unwrap_or(0);
    if counted > 0 {
        counted
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
    }
}

pub(crate) fn memory() -> Option<MemoryReading> {
    let info = osparse::meminfo(&read("/proc/meminfo")?)?;
    let (available, available_exact) = info.available_bytes();
    Some(MemoryReading {
        total: info.total,
        available,
        available_exact,
        swap_used: Some(info.swap_used()),
        commit: None,
    })
}

/// Pressure stall information. Kernels built without PSI, or booted with
/// `psi=0`, have no file to read and report `Unknown`.
pub(crate) fn pressure(
    _state: &SamplerState,
    _memory: Option<&MemoryReading>,
) -> (PressureLevel, Origin) {
    match read("/proc/pressure/memory").and_then(|text| osparse::psi_some_avg10(&text)) {
        Some(avg10) => (osparse::pressure_from_psi(avg10), Origin::Measured),
        None => (PressureLevel::Unknown, Origin::Unavailable),
    }
}

/// Size of the volume holding `path` and the space an unprivileged process
/// may still use on it.
pub(crate) fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: all-zero bytes are a valid `statvfs`, which is plain integers.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string and `stat` is a live,
    // writable `statvfs`.
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // The field widths differ between C libraries and architectures.
    #[allow(clippy::unnecessary_cast)]
    let (unit, blocks, available) = (
        stat.f_frsize as u64,
        stat.f_blocks as u64,
        stat.f_bavail as u64,
    );
    Some((blocks.saturating_mul(unit), available.saturating_mul(unit)))
}

/// `None` when `/sys/class/power_supply` cannot be listed, which also
/// happens inside containers that do not mount it.
pub(crate) fn power() -> Option<PowerState> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let supplies: Vec<PowerSupply> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            Some(PowerSupply {
                kind: read_trimmed(dir.join("type"))?,
                online: read_trimmed(dir.join("online")).map(|v| v == "1"),
                status: read_trimmed(dir.join("status")),
                capacity: read_trimmed(dir.join("capacity")).and_then(|v| v.parse().ok()),
                scope: read_trimmed(dir.join("scope")),
            })
        })
        .collect();
    Some(osparse::power_from_supplies(&supplies))
}

/// Linux thermal zones report temperatures without a common meaning for
/// "throttling", so no thermal state is derived from them.
pub(crate) fn thermal_fast(_state: &SamplerState) -> Option<ThermalState> {
    None
}

pub(crate) fn thermal_slow() -> Option<ThermalState> {
    None
}

pub(crate) fn hardware() -> Hardware {
    let cpuinfo = read("/proc/cpuinfo").unwrap_or_default();
    Hardware {
        logical_cpus: logical_cpus(),
        physical_cpus: physical_cpus(),
        cpu_model: osparse::cpuinfo_model(&cpuinfo),
        memory_bytes: memory().map_or(0, |m| m.total),
        disks: disks(),
        gpus: gpus(),
    }
}

/// Distinct (package, core) pairs in the CPU topology.
fn physical_cpus() -> Option<u32> {
    let mut cores = BTreeSet::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(|n| n.strip_prefix("cpu")) else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let topology = entry.path().join("topology");
        if let Some(core) = read_trimmed(topology.join("core_id")) {
            let package = read_trimmed(topology.join("physical_package_id")).unwrap_or_default();
            cores.insert((package, core));
        }
    }
    (!cores.is_empty()).then_some(cores.len() as u32)
}

fn disks() -> Vec<DiskInfo> {
    let mut disks: Vec<DiskInfo> = Vec::new();
    let mut seen = BTreeSet::new();
    let table = read("/proc/self/mounts").unwrap_or_default();
    for mount in osparse::mounts(&table) {
        if !osparse::is_storage_mount(&mount) || !Path::new(&mount.mount).is_dir() {
            continue;
        }
        // The same device mounted in several places is one disk.
        if !seen.insert(mount.device.clone()) {
            continue;
        }
        if let Some((total, _)) = disk_usage(Path::new(&mount.mount)) {
            if total > 0 {
                disks.push(DiskInfo {
                    mount: mount.mount,
                    total_bytes: total,
                    filesystem: mount.filesystem,
                });
            }
        }
    }
    disks.sort_by(|a, b| a.mount.cmp(&b.mount));
    disks
}

/// Graphics adapters known to DRM: `vendor:device (driver)` from the PCI
/// id, or the driver name alone on platforms without PCI.
fn gpus() -> Vec<String> {
    let mut gpus = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // `card0` is an adapter; `card0-HDMI-A-1` is one of its outputs.
        let is_card = name
            .strip_prefix("card")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if !is_card {
            continue;
        }
        let Ok(uevent) = std::fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let field = |key: &str| {
            uevent
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let label = match (field("PCI_ID"), field("DRIVER")) {
            (Some(id), Some(driver)) => format!("{} ({driver})", id.to_ascii_lowercase()),
            (Some(id), None) => id.to_ascii_lowercase(),
            (None, Some(driver)) => driver.to_string(),
            (None, None) => continue,
        };
        gpus.insert(label);
    }
    gpus.into_iter().collect()
}

pub(crate) fn os_version() -> String {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .filter_map(|path| read(path))
        .find_map(|text| osparse::os_release_version(&text))
        .unwrap_or_default()
}

pub(crate) fn kernel() -> String {
    read_trimmed("/proc/sys/kernel/osrelease")
        .map(|release| format!("Linux {release}"))
        .unwrap_or_default()
}

pub(crate) fn hostname() -> String {
    read_trimmed("/proc/sys/kernel/hostname").unwrap_or_default()
}

/// The kernel's random per-boot UUID, or the boot time from `/proc/stat`
/// on kernels that hide it.
pub(crate) fn boot_seed() -> Option<Vec<u8>> {
    if let Some(id) = read_trimmed("/proc/sys/kernel/random/boot_id") {
        return Some(format!("linux-boot-id:{id}").into_bytes());
    }
    let stat = read("/proc/stat")?;
    let btime = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
    Some(format!("linux-btime:{}", btime.trim()).into_bytes())
}

/// A display server this process was told about. The variables are what a
/// program started by this process would use to open a window.
pub(crate) fn gui_session() -> Option<GuiSession> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let kind = if set("WAYLAND_DISPLAY") {
        "wayland"
    } else if set("DISPLAY") {
        "x11"
    } else {
        return None;
    };
    Some(GuiSession {
        kind,
        state: CapabilityState::Installed,
    })
}

/// CPU time used so far by this process and the children it waited for.
#[cfg(test)]
pub(crate) fn process_cpu_time() -> std::time::Duration {
    super::unix_process_cpu_time()
}
