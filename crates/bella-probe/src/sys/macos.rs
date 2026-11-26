//! macOS: `sysctl`, Mach host statistics, `statfs` and `pmset`.

use super::{GuiSession, MemoryReading, Origin};
use crate::exec::{ProbeOptions, run_command};
use crate::osparse::{self, CpuTimes, MacVmPages};
use bella_core::machine::{DiskInfo, Hardware, PowerState, PressureLevel, ThermalState};
use bella_core::platform::CapabilityState;
use std::ffi::{CStr, CString, c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Limit for the `pmset` calls made on the sampling path.
const PMSET_TIMEOUT: Duration = Duration::from_secs(2);

// The notify API of libSystem (`<notify.h>`), which the `libc` crate does
// not bind. It publishes the thermal pressure level without a subprocess.
unsafe extern "C" {
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state64: *mut u64) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

const NOTIFY_STATUS_OK: u32 = 0;
const THERMAL_PRESSURE_NAME: &CStr = c"com.apple.system.thermalpressurelevel";

/// Counters and handles one sampler keeps between samples.
pub(crate) struct SamplerState {
    /// Token for the thermal pressure notification, when registered.
    thermal_token: Option<c_int>,
    /// Last raw tick counters, summed over CPUs. They are 32-bit and wrap
    /// after some weeks of uptime, so deltas are accumulated in 64 bits.
    last_ticks: Option<[u32; 4]>,
    busy: u64,
    total: u64,
}

impl SamplerState {
    pub fn new() -> Self {
        let mut token: c_int = 0;
        // SAFETY: the name is a NUL-terminated string and `token` is a live
        // out-parameter.
        let status = unsafe { notify_register_check(THERMAL_PRESSURE_NAME.as_ptr(), &mut token) };
        Self {
            thermal_token: (status == NOTIFY_STATUS_OK).then_some(token),
            last_ticks: None,
            busy: 0,
            total: 0,
        }
    }
}

impl Drop for SamplerState {
    fn drop(&mut self) {
        if let Some(token) = self.thermal_token.take() {
            // SAFETY: the token came from `notify_register_check` and is
            // cancelled exactly once.
            unsafe {
                notify_cancel(token);
            }
        }
    }
}

/// The host port for statistics calls, fetched once.
fn host_port() -> libc::mach_port_t {
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    *PORT.get_or_init(|| {
        // SAFETY: `mach_host_self` has no preconditions; it returns a send
        // right that this process keeps for its lifetime.
        #[allow(deprecated)]
        let port = unsafe { libc::mach_host_self() };
        port
    })
}

/// Read a sysctl value into `buffer`; returns the number of bytes written.
fn sysctl_into(name: &CStr, buffer: &mut [u8]) -> Option<usize> {
    let mut len = buffer.len();
    // SAFETY: `name` is NUL-terminated, `buffer` is writable for `len`
    // bytes, and no new value is supplied, so nothing is changed.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len <= buffer.len()).then_some(len)
}

fn sysctl_int(name: &CStr) -> Option<i64> {
    let mut buffer = [0u8; 8];
    match sysctl_into(name, &mut buffer)? {
        4 => Some(i64::from(i32::from_ne_bytes(buffer[..4].try_into().ok()?))),
        8 => Some(i64::from_ne_bytes(buffer)),
        _ => None,
    }
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buffer = [0u8; 256];
    let len = sysctl_into(name, &mut buffer)?;
    let text = &buffer[..len];
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    let text = String::from_utf8_lossy(&text[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Per-CPU tick counters from `host_processor_info`, summed over CPUs.
///
/// `host_statistics(HOST_CPU_LOAD_INFO)` would be simpler, but the kernel
/// serves it to third-party processes from a snapshot it refreshes only a
/// few times per second for the whole system, so two samples can receive
/// identical counters. `host_processor_info` is not rate-limited.
fn cpu_ticks() -> Option<[u32; 4]> {
    let mut cpus: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    // SAFETY: the three out-parameters are live. On success the kernel
    // maps an array of `count` integers into this task and stores its
    // address in `info`.
    let rc = unsafe {
        libc::host_processor_info(
            host_port(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut cpus,
            &mut info,
            &mut count,
        )
    };
    if rc != libc::KERN_SUCCESS || info.is_null() {
        return None;
    }
    let states = libc::CPU_STATE_MAX as usize;
    // SAFETY: `info` points at `count` integers mapped by the call above,
    // which stay valid until they are deallocated below.
    let values = unsafe { std::slice::from_raw_parts(info, count as usize) };
    let mut ticks = [0u32; 4];
    for cpu in values.chunks_exact(states) {
        for (sum, &value) in ticks.iter_mut().zip(cpu) {
            // Tick counters are unsigned and wrap; the sum wraps with them.
            *sum = sum.wrapping_add(value as u32);
        }
    }
    // SAFETY: the address and size describe exactly the region the kernel
    // mapped for this call, and `values` is not used afterwards.
    #[allow(deprecated)]
    unsafe {
        libc::vm_deallocate(
            libc::mach_task_self(),
            info as libc::vm_address_t,
            count as usize * size_of::<libc::integer_t>(),
        );
    }
    Some(ticks)
}

pub(crate) fn cpu_times(state: &mut SamplerState) -> Option<CpuTimes> {
    let ticks = cpu_ticks()?;
    if let Some(last) = state.last_ticks {
        for (index, (&now, &before)) in ticks.iter().zip(last.iter()).enumerate() {
            let delta = u64::from(now.wrapping_sub(before));
            state.total += delta;
            if index != libc::CPU_STATE_IDLE as usize {
                state.busy += delta;
            }
        }
    }
    state.last_ticks = Some(ticks);
    Some(CpuTimes {
        busy: state.busy,
        total: state.total,
    })
}

pub(crate) fn logical_cpus() -> u32 {
    sysctl_int(c"hw.logicalcpu")
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u32))
}

fn page_size() -> u64 {
    sysctl_int(c"hw.pagesize")
        .and_then(|n| u64::try_from(n).ok())
        .filter(|&n| n > 0)
        .unwrap_or(4096)
}

/// Memory from `host_statistics64`. The kernel may answer third-party
/// processes from a snapshot up to a second old; that is accurate enough
/// for memory, which is why only the CPU counters avoid this call.
pub(crate) fn memory() -> Option<MemoryReading> {
    let total = u64::try_from(sysctl_int(c"hw.memsize")?).ok()?;
    // SAFETY: all-zero bytes are a valid `vm_statistics64`, which is plain
    // integers.
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `vm` is a live `vm_statistics64` and `count` is its size in
    // `integer_t` units; a kernel with a shorter structure fills less.
    let rc = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            (&raw mut vm).cast(),
            &mut count,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return None;
    }
    let pages = MacVmPages {
        free_including_speculative: u64::from(vm.free_count),
        inactive: u64::from(vm.inactive_count),
        purgeable: u64::from(vm.purgeable_count),
    };
    Some(MemoryReading {
        total,
        available: pages
            .available_pages()
            .saturating_mul(page_size())
            .min(total),
        available_exact: true,
        swap_used: swap_used(),
        commit: None,
    })
}

/// `vm.swapusage` is an `xsw_usage`: total, available and used bytes as
/// three 64-bit integers, followed by the page size and a flag.
fn swap_used() -> Option<u64> {
    let mut buffer = [0u8; size_of::<libc::xsw_usage>()];
    let len = sysctl_into(c"vm.swapusage", &mut buffer)?;
    if len < 24 {
        return None;
    }
    Some(u64::from_ne_bytes(buffer[16..24].try_into().ok()?))
}

pub(crate) fn pressure(
    _state: &SamplerState,
    _memory: Option<&MemoryReading>,
) -> (PressureLevel, Origin) {
    match sysctl_int(c"kern.memorystatus_vm_pressure_level").map(osparse::pressure_from_macos_level)
    {
        Some(PressureLevel::Unknown) | None => (PressureLevel::Unknown, Origin::Unavailable),
        Some(level) => (level, Origin::Measured),
    }
}

fn statfs(path: &Path) -> Option<libc::statfs> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: all-zero bytes are a valid `statfs`: integers and byte arrays.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a live, writable `statfs`.
    let rc = unsafe { libc::statfs(path.as_ptr(), &mut stat) };
    (rc == 0).then_some(stat)
}

/// Size of the volume holding `path` and the space an unprivileged process
/// may still use on it. Purgeable space that APFS could reclaim is not
/// counted as free.
pub(crate) fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let stat = statfs(path)?;
    let unit = u64::from(stat.f_bsize);
    Some((
        stat.f_blocks.saturating_mul(unit),
        stat.f_bavail.saturating_mul(unit),
    ))
}

fn pmset(argument: &str) -> Option<String> {
    let mut command = Command::new("/usr/bin/pmset");
    command.args(["-g", argument]);
    let output = run_command(command, PMSET_TIMEOUT).ok()?;
    output.success.then_some(output.stdout)
}

pub(crate) fn power() -> Option<PowerState> {
    osparse::pmset_batt(&pmset("batt")?)
}

/// The thermal pressure level the OS publishes for
/// `NSProcessInfo.thermalState`.
pub(crate) fn thermal_fast(state: &SamplerState) -> Option<ThermalState> {
    let token = state.thermal_token?;
    let mut level: u64 = 0;
    // SAFETY: `token` is a live registration owned by `state` and `level`
    // is a live out-parameter.
    let status = unsafe { notify_get_state(token, &mut level) };
    (status == NOTIFY_STATUS_OK).then(|| osparse::thermal_from_macos_pressure_level(level))
}

pub(crate) fn thermal_slow() -> Option<ThermalState> {
    osparse::pmset_therm(&pmset("therm")?)
}

pub(crate) fn hardware() -> Hardware {
    Hardware {
        logical_cpus: logical_cpus(),
        physical_cpus: sysctl_int(c"hw.physicalcpu")
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0),
        cpu_model: sysctl_string(c"machdep.cpu.brand_string").unwrap_or_default(),
        memory_bytes: sysctl_int(c"hw.memsize")
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0),
        disks: disks(),
        gpus: gpus(),
    }
}

fn c_chars_to_string(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The boot volume and the writable volumes a user would see in Finder.
/// The APFS helper volumes of the boot container are hidden mounts and are
/// skipped, as are read-only disk images.
fn disks() -> Vec<DiskInfo> {
    // SAFETY: a null buffer with size zero asks only for the mount count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let Ok(capacity) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for mounts that appear between the two calls.
    let capacity = capacity + 8;
    // SAFETY: all-zero bytes are a valid `statfs`: integers and byte arrays.
    let mut mounts: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacity];
    let Ok(bytes) = c_int::try_from(capacity * size_of::<libc::statfs>()) else {
        return Vec::new();
    };
    // SAFETY: `mounts` is writable for `bytes` bytes, which is exactly its
    // length in `statfs` entries.
    let filled = unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let filled = usize::try_from(filled).unwrap_or(0).min(capacity);

    let mut disks: Vec<DiskInfo> = mounts[..filled]
        .iter()
        .filter_map(|stat| {
            let mount = c_chars_to_string(&stat.f_mntonname);
            let filesystem = c_chars_to_string(&stat.f_fstypename);
            let local = stat.f_flags & libc::MNT_LOCAL as u32 != 0;
            let hidden = stat.f_flags & libc::MNT_DONTBROWSE as u32 != 0;
            let read_only = stat.f_flags & libc::MNT_RDONLY as u32 != 0;
            let pseudo = matches!(filesystem.as_str(), "devfs" | "autofs");
            let total = stat.f_blocks.saturating_mul(u64::from(stat.f_bsize));
            // Besides the sealed system volume, a read-only volume is a
            // mounted disk image or installer, which comes and goes.
            let listed = local && !pseudo && total > 0 && (mount == "/" || !(hidden || read_only));
            listed.then_some(DiskInfo {
                mount,
                total_bytes: total,
                filesystem,
            })
        })
        .collect();
    disks.sort_by(|a, b| a.mount.cmp(&b.mount));
    disks
}

/// Graphics adapters from `system_profiler`, which is the supported way to
/// list them without IOKit bindings.
fn gpus() -> Vec<String> {
    let mut command = Command::new("/usr/sbin/system_profiler");
    command.args(["SPDisplaysDataType", "-json"]);
    let Ok(output) = run_command(command, ProbeOptions::DEFAULT_TIMEOUT) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let mut gpus: Vec<String> = Vec::new();
    for adapter in value["SPDisplaysDataType"].as_array().into_iter().flatten() {
        let name = adapter["sppci_model"]
            .as_str()
            .or_else(|| adapter["_name"].as_str())
            .unwrap_or_default();
        if !name.is_empty() && !gpus.iter().any(|known| known == name) {
            gpus.push(name.to_string());
        }
    }
    gpus
}

pub(crate) fn os_version() -> String {
    sysctl_string(c"kern.osproductversion").unwrap_or_default()
}

pub(crate) fn kernel() -> String {
    sysctl_string(c"kern.osrelease")
        .map(|release| format!("Darwin {release}"))
        .unwrap_or_default()
}

pub(crate) fn hostname() -> String {
    let mut buffer = [0 as c_char; 256];
    // SAFETY: `buffer` is writable for the length passed, which leaves room
    // for the terminating NUL.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) };
    if rc != 0 {
        return String::new();
    }
    let name = c_chars_to_string(&buffer);
    name.strip_suffix(".local").unwrap_or(&name).to_string()
}

/// `kern.bootsessionuuid` is a UUID the kernel generates at every boot.
/// `kern.boottime` is the fallback; it identifies a boot too but moves when
/// the clock is set.
pub(crate) fn boot_seed() -> Option<Vec<u8>> {
    if let Some(uuid) = sysctl_string(c"kern.bootsessionuuid") {
        return Some(format!("macos-boot-session:{uuid}").into_bytes());
    }
    // A `timeval`: 64-bit seconds followed by 32-bit microseconds.
    let mut buffer = [0u8; 16];
    let len = sysctl_into(c"kern.boottime", &mut buffer)?;
    if len < 12 {
        return None;
    }
    let seconds = i64::from_ne_bytes(buffer[..8].try_into().ok()?);
    let micros = i32::from_ne_bytes(buffer[8..12].try_into().ok()?);
    Some(format!("macos-boottime:{seconds}.{micros:06}").into_bytes())
}

/// The owner of `/dev/console` is the user logged in at the screen; root
/// owns it while the login window is showing. A session of this process's
/// own user (or any session, for root) can show windows; someone else's
/// cannot be used.
pub(crate) fn gui_session() -> Option<GuiSession> {
    let console_user = std::fs::metadata("/dev/console").ok()?.uid();
    if console_user == 0 {
        return None;
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let state = if me == console_user || me == 0 {
        CapabilityState::Installed
    } else {
        CapabilityState::Unauthorized
    };
    Some(GuiSession {
        kind: "aqua",
        state,
    })
}

/// CPU time used so far by this process and the children it waited for.
#[cfg(test)]
pub(crate) fn process_cpu_time() -> Duration {
    super::unix_process_cpu_time()
}
