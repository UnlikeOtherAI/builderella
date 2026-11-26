//! Windows: Win32 system information, the registry and Terminal Services.

use super::{GuiSession, MemoryReading, Origin};
use crate::osparse::{self, CpuTimes};
use bella_core::machine::{DiskInfo, Hardware, PowerState, PressureLevel, ThermalState};
use bella_core::platform::CapabilityState;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, FILETIME, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{
    ENUM_PAGE_FILE_INFORMATION, K32EnumPageFilesW, K32GetPerformanceInfo, PERFORMANCE_INFORMATION,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SESSION, WTSActive, WTSConnectState, WTSFreeMemory,
    WTSQuerySessionInformationW,
};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNameDnsHostname, GetComputerNameExW, GetLogicalProcessorInformationEx,
    GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcessId, GetSystemTimes,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, SYSTEM_TIMEOFDAY_INFORMATION};
use windows_sys::core::{BOOL, PCWSTR};

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Handles one sampler keeps open.
pub(crate) struct SamplerState {
    /// The system's low-memory resource notification, or null.
    low_memory: HANDLE,
}

// SAFETY: the only field is a kernel handle to a notification object, which
// may be queried and closed from any thread.
unsafe impl Send for SamplerState {}

impl SamplerState {
    pub fn new() -> Self {
        // SAFETY: the call takes a plain enumeration value and returns a
        // handle or null.
        let low_memory = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
        Self { low_memory }
    }

    /// Whether Windows itself currently signals low physical memory.
    fn low_memory_signalled(&self) -> Option<bool> {
        if self.low_memory.is_null() {
            return None;
        }
        let mut signalled: BOOL = 0;
        // SAFETY: the handle is a live notification handle owned by `self`
        // and `signalled` is a live out-parameter.
        let ok = unsafe { QueryMemoryResourceNotification(self.low_memory, &mut signalled) };
        (ok != 0).then_some(signalled != 0)
    }
}

impl Drop for SamplerState {
    fn drop(&mut self) {
        if !self.low_memory.is_null() {
            // SAFETY: the handle came from CreateMemoryResourceNotification
            // and is closed exactly once.
            unsafe {
                CloseHandle(self.low_memory);
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn filetime(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn reg_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut buffer = [0u16; 512];
    let mut bytes = (buffer.len() * 2) as u32;
    // SAFETY: both names are NUL-terminated, `buffer` is writable for
    // `bytes` bytes, and RRF_RT_REG_SZ makes the call fail for any other
    // value type. The registry is only read.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let text = from_wide(&buffer).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut data: u32 = 0;
    let mut bytes = size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated, `data` is writable for the
    // four bytes announced, and RRF_RT_REG_DWORD rejects other value types.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut bytes,
        )
    };
    (rc == ERROR_SUCCESS).then_some(data)
}

pub(crate) fn cpu_times(_state: &mut SamplerState) -> Option<CpuTimes> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut idle, mut kernel, mut user) = (zero, zero, zero);
    // SAFETY: the three pointers are live, writable FILETIME values.
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    // Kernel time includes idle time.
    let total = filetime(kernel) + filetime(user);
    Some(CpuTimes {
        busy: total.saturating_sub(filetime(idle)),
        total,
    })
}

pub(crate) fn logical_cpus() -> u32 {
    // SAFETY: the call takes a group number and has no other preconditions.
    let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    if count > 0 {
        count
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
    }
}

fn performance_info() -> Option<PERFORMANCE_INFORMATION> {
    let mut info = PERFORMANCE_INFORMATION::default();
    let size = size_of::<PERFORMANCE_INFORMATION>() as u32;
    info.cb = size;
    // SAFETY: `info` is a live PERFORMANCE_INFORMATION of the size passed.
    let ok = unsafe { K32GetPerformanceInfo(&mut info, size) };
    (ok != 0).then_some(info)
}

/// Adds the pages in use of one page file to the `u64` behind `context`.
unsafe extern "system" fn add_page_file_use(
    context: *mut c_void,
    info: *mut ENUM_PAGE_FILE_INFORMATION,
    _name: PCWSTR,
) -> BOOL {
    if context.is_null() || info.is_null() {
        return 0;
    }
    // SAFETY: K32EnumPageFilesW passes back the context pointer given to
    // it, which `page_file_bytes_in_use` points at a live `u64`, and a
    // valid page-file record for the duration of the call.
    unsafe {
        *context.cast::<u64>() += (*info).TotalInUse as u64;
    }
    1
}

/// Bytes currently written to page files, which is what "swap used" means
/// on Windows. This is not the commit charge.
fn page_file_bytes_in_use(page_size: u64) -> Option<u64> {
    let mut pages: u64 = 0;
    // SAFETY: the callback matches PENUM_PAGE_FILE_CALLBACKW and receives
    // `pages`, which outlives the call, as its context.
    let ok = unsafe { K32EnumPageFilesW(Some(add_page_file_use), (&raw mut pages).cast()) };
    (ok != 0).then(|| pages.saturating_mul(page_size))
}

pub(crate) fn memory() -> Option<MemoryReading> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a live MEMORYSTATUSEX with `dwLength` set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return None;
    }
    let performance = performance_info();
    let page_size = performance.map_or(4096, |info| info.PageSize as u64);
    Some(MemoryReading {
        total: status.ullTotalPhys,
        available: status.ullAvailPhys.min(status.ullTotalPhys),
        available_exact: true,
        swap_used: page_file_bytes_in_use(page_size),
        commit: performance.map(|info| {
            (
                (info.CommitTotal as u64).saturating_mul(page_size),
                (info.CommitLimit as u64).saturating_mul(page_size),
            )
        }),
    })
}

/// Windows publishes no graded memory pressure, so the level is derived by
/// [`osparse::windows_pressure`] and reported as an estimate.
pub(crate) fn pressure(
    state: &SamplerState,
    memory: Option<&MemoryReading>,
) -> (PressureLevel, Origin) {
    let Some(memory) = memory else {
        return (PressureLevel::Unknown, Origin::Unavailable);
    };
    let (commit, limit) = memory.commit.unwrap_or((0, 0));
    let level = osparse::windows_pressure(
        memory.available,
        memory.total,
        commit,
        limit,
        state.low_memory_signalled().unwrap_or(false),
    );
    (level, Origin::Estimated)
}

/// Size of the volume holding `path` and the space this account may still
/// use on it (quotas included).
pub(crate) fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `path` is NUL-terminated and the out-parameters are live; the
    // third out-parameter is optional.
    let ok =
        unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((total, free))
}

pub(crate) fn power() -> Option<PowerState> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a live SYSTEM_POWER_STATUS.
    let ok = unsafe { GetSystemPowerStatus(&mut status) };
    if ok == 0 {
        return None;
    }
    // Flag 128 means the system has no battery; such machines report no
    // power state at all, like desktops on the other platforms.
    const NO_SYSTEM_BATTERY: u8 = 128;
    const UNKNOWN: u8 = 255;
    if status.BatteryFlag != UNKNOWN && status.BatteryFlag & NO_SYSTEM_BATTERY != 0 {
        return Some(PowerState::default());
    }
    Some(PowerState {
        on_ac: match status.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        battery_percent: (status.BatteryLifePercent <= 100).then_some(status.BatteryLifePercent),
    })
}

/// Windows has no cheap, documented thermal-throttling state.
pub(crate) fn thermal_fast(_state: &SamplerState) -> Option<ThermalState> {
    None
}

pub(crate) fn thermal_slow() -> Option<ThermalState> {
    None
}

pub(crate) fn hardware() -> Hardware {
    Hardware {
        logical_cpus: logical_cpus(),
        physical_cpus: physical_cpus(),
        cpu_model: reg_string(
            r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "ProcessorNameString",
        )
        .unwrap_or_default(),
        memory_bytes: memory().map_or(0, |m| m.total),
        disks: disks(),
        gpus: gpus(),
    }
}

/// Number of processor cores, counted from the core relationship records.
fn physical_cpus() -> Option<u32> {
    let mut bytes: u32 = 0;
    // SAFETY: a null buffer with length zero asks only for the size needed.
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut bytes);
    }
    if bytes == 0 {
        return None;
    }
    // u64 elements give the records the alignment they need.
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
    // SAFETY: `buffer` is writable for at least `bytes` bytes and suitably
    // aligned for the records.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if ok == 0 {
        return None;
    }
    let base = buffer.as_ptr().cast::<u8>();
    let (mut offset, mut cores) = (0usize, 0u32);
    while offset + size_of::<u32>() * 2 <= bytes as usize {
        // SAFETY: the system wrote consecutive records into the first
        // `bytes` bytes; each starts with its relationship and size, and
        // `offset` stays within that range.
        let record = unsafe {
            &*base
                .add(offset)
                .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
        };
        if record.Size == 0 {
            break;
        }
        if record.Relationship == RelationProcessorCore {
            cores += 1;
        }
        offset += record.Size as usize;
    }
    (cores > 0).then_some(cores)
}

/// Fixed local drives.
fn disks() -> Vec<DiskInfo> {
    // SAFETY: the call has no parameters.
    let mask = unsafe { GetLogicalDrives() };
    let mut disks = Vec::new();
    for index in 0..26u8 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let root = format!("{}:\\", char::from(b'A' + index));
        let root_wide = wide(&root);
        // SAFETY: `root_wide` is a NUL-terminated drive root.
        if unsafe { GetDriveTypeW(root_wide.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let Some((total, _)) = disk_usage(Path::new(&root)) else {
            continue;
        };
        let mut filesystem = [0u16; 64];
        // SAFETY: `root_wide` is NUL-terminated; only the file system name
        // is requested, into a buffer of the length passed.
        unsafe {
            GetVolumeInformationW(
                root_wide.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            );
        }
        disks.push(DiskInfo {
            mount: root,
            total_bytes: total,
            filesystem: from_wide(&filesystem),
        });
    }
    disks
}

/// Display adapters from the display device class in the registry, which
/// is readable from services and SSH sessions that have no desktop.
fn gpus() -> Vec<String> {
    const DISPLAY_CLASS: &str =
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let mut gpus: Vec<String> = Vec::new();
    for index in 0..32 {
        let key = format!("{DISPLAY_CLASS}\\{index:04}");
        if let Some(name) = reg_string(&key, "DriverDesc")
            && !gpus.contains(&name)
        {
            gpus.push(name);
        }
    }
    gpus
}

/// `major.minor.build`, e.g. `10.0.26100`.
pub(crate) fn os_version() -> String {
    let major = reg_dword(CURRENT_VERSION_KEY, "CurrentMajorVersionNumber");
    let minor = reg_dword(CURRENT_VERSION_KEY, "CurrentMinorVersionNumber");
    let build = reg_string(CURRENT_VERSION_KEY, "CurrentBuildNumber");
    match (major, minor, build) {
        (Some(major), Some(minor), Some(build)) => format!("{major}.{minor}.{build}"),
        _ => String::new(),
    }
}

/// The OS version with the update build revision, e.g. `10.0.26100.2314`.
pub(crate) fn kernel() -> String {
    let version = os_version();
    match reg_dword(CURRENT_VERSION_KEY, "UBR") {
        Some(revision) if !version.is_empty() => format!("{version}.{revision}"),
        _ => version,
    }
}

pub(crate) fn hostname() -> String {
    let mut buffer = [0u16; 256];
    let mut len = buffer.len() as u32;
    // SAFETY: `buffer` is writable for `len` characters.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return std::env::var("COMPUTERNAME").unwrap_or_default();
    }
    from_wide(&buffer)
}

/// The kernel's boot time with the clock-adjustment bias removed.
///
/// `SYSTEM_TIMEOFDAY_INFORMATION` starts with `BootTime` and carries
/// `BootTimeBias` at byte 32. When the clock is set, Windows moves both by
/// the same amount, so their difference stays the same for the whole boot,
/// unlike a boot time computed from the current time and the uptime.
pub(crate) fn boot_seed() -> Option<Vec<u8>> {
    let mut info = SYSTEM_TIMEOFDAY_INFORMATION { Reserved1: [0; 48] };
    let mut written: u32 = 0;
    // SAFETY: `info` is writable for the size passed and `written` is a
    // live out-parameter. The call only reads system information.
    let status = unsafe {
        NtQuerySystemInformation(
            SystemTimeOfDayInformation,
            (&raw mut info).cast(),
            size_of::<SYSTEM_TIMEOFDAY_INFORMATION>() as u32,
            &mut written,
        )
    };
    if status < 0 || written < 40 {
        return None;
    }
    let field = |at: usize| -> Option<u64> {
        Some(u64::from_ne_bytes(
            info.Reserved1[at..at + 8].try_into().ok()?,
        ))
    };
    let boot = field(0)?.wrapping_sub(field(32)?);
    Some(format!("windows-boot-time:{boot}").into_bytes())
}

/// An interactive session this process runs in. Services and SSH logins
/// run in session 0, which has no desktop a user can see, so they report
/// no GUI session even when someone is logged in at the console.
pub(crate) fn gui_session() -> Option<GuiSession> {
    let mut session: u32 = 0;
    // SAFETY: `session` is a live out-parameter; the process id is ours.
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    if ok == 0 || session == 0 {
        return None;
    }
    let mut buffer: *mut u16 = std::ptr::null_mut();
    let mut bytes: u32 = 0;
    // SAFETY: a null server handle means this machine; `buffer` and `bytes`
    // are live out-parameters. The buffer is released below.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            std::ptr::null_mut(),
            WTS_CURRENT_SESSION,
            WTSConnectState,
            &mut buffer,
            &mut bytes,
        )
    };
    let mut state = CapabilityState::Unverified;
    if ok != 0 && !buffer.is_null() {
        if bytes as usize >= size_of::<i32>() {
            // SAFETY: for WTSConnectState the buffer holds one
            // WTS_CONNECTSTATE_CLASS value of at least four bytes.
            let connect_state = unsafe { buffer.cast::<i32>().read_unaligned() };
            if connect_state == WTSActive {
                state = CapabilityState::Installed;
            }
        }
        // SAFETY: the buffer was allocated by WTSQuerySessionInformationW
        // and is freed exactly once.
        unsafe {
            WTSFreeMemory(buffer.cast());
        }
    }
    Some(GuiSession {
        kind: "windows",
        state,
    })
}

/// CPU time used so far by this process.
#[cfg(test)]
pub(crate) fn process_cpu_time() -> std::time::Duration {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of the current process is always valid and
    // the four out-parameters are live FILETIME values.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return std::time::Duration::ZERO;
    }
    // FILETIME counts 100-nanosecond intervals.
    std::time::Duration::from_nanos((filetime(kernel) + filetime(user)) * 100)
}
