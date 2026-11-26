//! Job Object adapter for Windows.
//!
//! One scope is one **named** job object, `Local\bella-<scope name>`. Every
//! process started with [`Scope::spawn`] is created suspended, assigned to
//! the job and only then resumed, so it is a member before it runs any code.
//! Descendants inherit the job and cannot leave it: `CREATE_BREAKAWAY_FROM_JOB`
//! is refused because the job never sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK`.
//!
//! # Lifetime and adoption
//!
//! The job does **not** set `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. If the
//! worker crashes, the attempt's processes keep running inside the named job
//! so that the restarted worker can [`Containment::adopt`] it by name,
//! account for what is left and stop it. Cleanup is therefore always
//! explicit: [`Scope::kill`], then [`Scope::destroy`]. Dropping a
//! [`JobScope`] closes this handle and stops no member.
//!
//! A kernel object keeps its name only while some handle to it is open; the
//! processes in a job do not count. A crashed worker would therefore leave a
//! job that still runs but can no longer be opened. To prevent that, every
//! scope has a *keeper*: a `cmd.exe` process that is created suspended and
//! never resumed, holding a query-only handle to the job. It runs no code,
//! sits in a job of its own (`Local\bella-<scope name>.keeper`) rather than
//! in the scope, and is ended when the scope is destroyed or dropped while
//! empty. A scope dropped with live members keeps its keeper, which is what
//! makes it adoptable.
//!
//! `adopt` returns `Ok(None)` when the job can no longer be opened. With the
//! keeper in place that happens only once the scope was destroyed or dropped
//! empty, so it is evidence that nothing is left. The one way to defeat it is
//! for something else to kill the keeper while no worker holds the job.
//! Identity is the object name, never a process id, so a reused PID is never
//! mistaken for a member (process ids reported by the job are only acted on
//! after [`IsProcessInJob`] confirmed them on an open handle).
//!
//! `Local\` is the session namespace: a worker has to restart in the same
//! logon session (for a service that is session 0) to find its jobs again.
//!
//! # What is enforced
//!
//! | `Limits` field | Job Object control | Notes |
//! | --- | --- | --- |
//! | `memory_bytes` | `JOB_OBJECT_LIMIT_JOB_MEMORY` | A limit on **committed** memory of all members together, not a physical-RAM reservation. Allocations beyond it fail; nothing is killed. |
//! | `max_processes` | `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` | The console host (`conhost.exe`) Windows starts for a console program is a member and is killed with the job, but Windows does not count it against this limit. |
//! | `cpu_milli` | CPU rate control, hard cap | Share of **all** logical processors. A request at or above the whole machine removes the cap. |
//! | `low_priority` | `JOB_OBJECT_LIMIT_PRIORITY_CLASS` | Below-normal priority class. A scheduling preference, not a limit. |
//!
//! Disk use is neither limited nor measured.
//!
//! Hitting the memory or process limit is an ordinary failure inside the
//! process that hit it. [`JobScope::limit_events`] reports those hits from
//! the job's completion port so a worker can call the result resource
//! exhaustion instead of a plain non-zero exit.
//!
//! # Starting processes
//!
//! * The environment block is exactly [`ProcessSpec::env`], sorted
//!   case-insensitively. Windows adds nothing: `SystemRoot`, `PATH`, `TEMP`
//!   and the rest exist only if the caller passes them (most programs that
//!   use sockets need `SystemRoot`).
//! * The program is looked up without consulting the worker: a name with a
//!   path separator is taken relative to `cwd`; a bare name is searched in
//!   the `PATH` of `env`, then the system directory, then the Windows
//!   directory. `.exe` and `.com` are tried when the name has no extension.
//! * Arguments are quoted with the MSVC runtime rules ([`command_line`]).
//!   `cmd.exe` does not use those rules, so for `cmd.exe … /c <text>` (or
//!   `/k`) everything after the switch is joined with spaces and handed over
//!   verbatim as `/s /c "<text>"`, which `cmd` runs exactly as written.
//! * `.bat` and `.cmd` files are refused: they would be run through an
//!   implicit `cmd.exe` whose quoting cannot be made safe from an argv. Pass
//!   an explicit shell instead: `["cmd.exe", "/d", "/c", "build.cmd release"]`.
//! * Standard input is `NUL`. Standard output and error are anonymous pipes;
//!   only those three handles are inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
//!   A pipe reaches end-of-file when the last process holding its write end
//!   exits, which can be a background descendant rather than the root.
//! * Each root gets its own windowless console (`CREATE_NO_WINDOW`) and its
//!   own process group.
//! * While [`Scope::terminate`] signals a scope, this process is attached to
//!   that scope's console for a moment. Anything else the same process
//!   starts in that window with an inherited console lands on the scope's
//!   console and dies with it, so every other spawn in a worker must use
//!   `CREATE_NO_WINDOW` or `DETACHED_PROCESS`.
//!
//! # Stopping
//!
//! Windows has no `SIGTERM`. [`Scope::terminate`] generates `CTRL_BREAK_EVENT`
//! for the process groups of the scope. Because every root has a private
//! console, the caller has to attach to that console for a moment
//! ([`JobScope::send_ctrl_break`]). That is only done when it cannot damage
//! the calling process: none of its standard handles may be a console handle,
//! and if it has a console of its own, another process must share it so it
//! can re-attach. A service qualifies; a worker printing to an interactive
//! terminal does not, and then `terminate` sends nothing and the grace period
//! simply runs out before [`Scope::kill`]. Processes without a console (GUI
//! programs, detached processes) never get a polite signal. After adoption
//! the group ids of roots that already exited are unknown, so their surviving
//! descendants are not signalled either.
//!
//! `kill` is `TerminateJobObject`: every member, including descendants in
//! nested jobs, ends with exit code [`KILL_EXIT_CODE`].
//!
//! # Nested jobs
//!
//! When the calling process is itself in a job (some SSH servers, CI runners
//! and service hosts do this), scopes are nested inside that job. They are
//! bounded by its limits as well as their own, and they end with it if it is
//! closed with kill-on-close. [`Containment::enforcement`] says whether that
//! is the case.
//!
//! # Known escape hatches
//!
//! Resource containment is not a security sandbox. A process created on a
//! member's behalf by something outside the job is outside the job: WMI
//! (`Win32_Process.Create`), the Task Scheduler, a Windows service, a COM
//! local server. The job object is also openable by name by any process of
//! the same user, members included, which could change its limits.

use crate::{
    Child, ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind, UsageSummary};
use bella_core::resources::{Enforcement, EnforcementReport};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Read};
use std::mem::{MaybeUninit, offset_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, GENERIC_READ,
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    SetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
    GetConsoleMode, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOBOBJECTINFOCLASS, JobObjectAssociateCompletionPortInformation,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetActiveProcessorCount, GetCurrentProcess, GetExitCodeProcess,
    INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    NORMAL_PRIORITY_CLASS, OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Exit code of every process ended by [`Scope::kill`].
pub const KILL_EXIT_CODE: u32 = 1;

/// Exit status of a process ended by the default `CTRL_BREAK_EVENT` handler
/// (`STATUS_CONTROL_C_EXIT`), as it appears in [`ExitInfo::code`].
pub const CTRL_BREAK_EXIT_CODE: i32 = 0xC000_013A_u32 as i32;

const OBJECT_NAME_PREFIX: &str = r"Local\bella-";
/// Appended to a scope's object name for its keeper job. Scope names cannot
/// contain a dot, so this never collides with another scope.
const KEEPER_SUFFIX: &str = ".keeper";
const MAX_SCOPE_NAME_LEN: usize = 200;
/// Longest command line `CreateProcessW` accepts, without the terminator.
const MAX_COMMAND_LINE_UNITS: usize = 32_766;

// winnt.h values that windows-sys only exposes behind its very large
// `Win32_System_SystemServices` feature.
const JOB_OBJECT_ALL_ACCESS: u32 = 0x001F_003F;
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT: u32 = 3;
const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
const JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10;

const SPAWN_FLAGS: u32 = CREATE_SUSPENDED
    | CREATE_NEW_PROCESS_GROUP
    | CREATE_UNICODE_ENVIRONMENT
    | CREATE_NO_WINDOW
    | EXTENDED_STARTUPINFO_PRESENT;

const STD_SLOTS: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];

/// A process attaches to one console at a time, so transient attachments
/// from different scopes must not interleave.
static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

static SELF_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Small OS helpers
// ---------------------------------------------------------------------------

/// The calling thread's last OS error, wrapped with what was being done.
/// Call it directly after the failing function.
fn win_err(what: &str) -> ContainError {
    let error = io::Error::last_os_error();
    ContainError::Other(format!("{what}: {error}"))
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

/// Takes ownership of a handle returned by the OS.
///
/// # Safety
/// `handle` must be a valid, open handle that nothing else will close.
unsafe fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: guaranteed by the caller.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Result<Vec<u16>, ContainError> {
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return Err(ContainError::Other(format!(
            "path `{}` contains a NUL character",
            path.display()
        )));
    }
    units.push(0);
    Ok(units)
}

fn system_directory() -> Result<PathBuf, ContainError> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the buffer is writable for `buffer.len()` UTF-16 units.
        let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if len == 0 {
            return Err(win_err("GetSystemDirectoryW"));
        }
        if len < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..len])));
        }
        buffer = vec![0; len + 1];
    }
}

fn logical_cpus() -> u32 {
    // SAFETY: no pointers are involved.
    unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1)
}

// ---------------------------------------------------------------------------
// Job information
// ---------------------------------------------------------------------------

/// Reads one fixed-size job information class. A null `job` means the job of
/// the calling process.
///
/// # Safety
/// `T` must be the structure Windows defines for `class` and must be valid
/// for every bit pattern.
unsafe fn query_job<T>(
    job: HANDLE,
    class: JOBOBJECTINFOCLASS,
    what: &str,
) -> Result<T, ContainError> {
    let mut info = MaybeUninit::<T>::zeroed();
    // SAFETY: the buffer is writable for `size_of::<T>()` bytes and the
    // caller guarantees that this is the layout of `class`.
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            class,
            info.as_mut_ptr().cast(),
            size_of::<T>() as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(win_err(what));
    }
    // SAFETY: zero-initialised and then filled by the kernel; the caller
    // guarantees every bit pattern is a valid `T`.
    Ok(unsafe { info.assume_init() })
}

/// Writes one fixed-size job information class.
///
/// # Safety
/// `T` must be the structure Windows defines for `class`.
unsafe fn set_job<T>(
    job: HANDLE,
    class: JOBOBJECTINFOCLASS,
    info: &T,
    what: &str,
) -> Result<(), ContainError> {
    // SAFETY: `info` is readable for `size_of::<T>()` bytes and the caller
    // guarantees that this is the layout of `class`.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            class,
            std::ptr::from_ref(info).cast(),
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(win_err(what));
    }
    Ok(())
}

fn accounting(job: HANDLE) -> Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, ContainError> {
    // SAFETY: the type is the plain-data structure of this class.
    unsafe {
        query_job(
            job,
            JobObjectBasicAccountingInformation,
            "QueryInformationJobObject(accounting)",
        )
    }
}

fn extended_limits(job: HANDLE) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION, ContainError> {
    // SAFETY: the type is the plain-data structure of this class.
    unsafe {
        query_job(
            job,
            JobObjectExtendedLimitInformation,
            "QueryInformationJobObject(limits)",
        )
    }
}

fn cpu_rate_enabled(job: HANDLE) -> bool {
    // SAFETY: the type is the plain-data structure of this class.
    let info: Result<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, _> = unsafe {
        query_job(
            job,
            JobObjectCpuRateControlInformation,
            "QueryInformationJobObject(cpu rate)",
        )
    };
    info.is_ok_and(|info| info.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0)
}

/// `CpuRate` for a hard cap of `cpu_milli`: hundredths of a percent of all
/// logical processors, rounded down so the cap never exceeds the request.
/// `None` when the request covers the whole machine and no cap is needed.
fn cpu_rate(cpu_milli: u64, logical_cpus: u32) -> Option<u32> {
    let machine_milli = u64::from(logical_cpus.max(1)) * 1000;
    if cpu_milli >= machine_milli {
        return None;
    }
    Some((cpu_milli.saturating_mul(10_000) / machine_milli).clamp(1, 9_999) as u32)
}

fn validate_limits(limits: &Limits) -> Result<(), ContainError> {
    let zero = |what: &str| ContainError::Other(format!("{what} limit must be greater than zero"));
    if limits.cpu_milli == Some(0) {
        return Err(zero("cpu"));
    }
    if limits.memory_bytes == Some(0) {
        return Err(zero("memory"));
    }
    if limits.max_processes == Some(0) {
        return Err(zero("process count"));
    }
    Ok(())
}

/// Makes the job's limits exactly `limits`, replacing whatever was set.
fn apply_limits(job: HANDLE, limits: &Limits, logical_cpus: u32) -> Result<(), ContainError> {
    validate_limits(limits)?;
    let current = extended_limits(job)?;

    let mut flags = 0;
    if limits.memory_bytes.is_some() {
        flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
    }
    if limits.max_processes.is_some() {
        flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    }
    let job_memory = limits
        .memory_bytes
        .map_or(0, |bytes| usize::try_from(bytes).unwrap_or(usize::MAX));
    let build = |flags, priority_class| JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: flags,
            ActiveProcessLimit: limits.max_processes.unwrap_or(0),
            PriorityClass: priority_class,
            ..Default::default()
        },
        JobMemoryLimit: job_memory,
        ..Default::default()
    };
    let set = |info: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION| {
        // SAFETY: the type is the structure of this class.
        unsafe {
            set_job(
                job,
                JobObjectExtendedLimitInformation,
                info,
                "SetInformationJobObject(limits)",
            )
        }
    };

    if limits.low_priority {
        set(&build(
            flags | JOB_OBJECT_LIMIT_PRIORITY_CLASS,
            BELOW_NORMAL_PRIORITY_CLASS,
        ))?;
    } else {
        let had_priority =
            current.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_PRIORITY_CLASS != 0;
        if had_priority {
            // Clearing the flag leaves current members at the lowered class,
            // so move them back to normal before letting go of it.
            set(&build(
                flags | JOB_OBJECT_LIMIT_PRIORITY_CLASS,
                NORMAL_PRIORITY_CLASS,
            ))?;
        }
        set(&build(flags, 0))?;
    }

    let rate = limits
        .cpu_milli
        .and_then(|milli| cpu_rate(milli, logical_cpus));
    let control = match rate {
        Some(rate) => Some(JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: rate },
        }),
        None if cpu_rate_enabled(job) => Some(JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: 0,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 0 },
        }),
        None => None,
    };
    if let Some(control) = control {
        // SAFETY: the type is the structure of this class.
        unsafe {
            set_job(
                job,
                JobObjectCpuRateControlInformation,
                &control,
                "SetInformationJobObject(cpu rate)",
            )?;
        }
    }
    Ok(())
}

/// Process ids the job currently lists. A listed id is only a hint: use
/// [`open_member`] before acting on one.
fn process_ids(job: HANDLE) -> Result<Vec<u32>, ContainError> {
    let word = size_of::<usize>();
    let first = offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList).div_ceil(word);
    let mut capacity = 64usize;
    loop {
        // `usize` storage keeps the buffer aligned for the structure.
        let mut buffer = vec![0usize; first + capacity];
        // SAFETY: the buffer is writable for the stated number of bytes and
        // is aligned for JOBOBJECT_BASIC_PROCESS_ID_LIST.
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * word) as u32,
                null_mut(),
            )
        };
        let truncated = ok == 0;
        if truncated {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(ContainError::Other(format!(
                    "QueryInformationJobObject(process ids): {error}"
                )));
            }
        }
        // SAFETY: the buffer is at least as large as the fixed part of the
        // structure, aligned for it, and initialised (zeroed, then written).
        let (assigned, listed) = unsafe {
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            (
                (*list).NumberOfAssignedProcesses as usize,
                (*list).NumberOfProcessIdsInList as usize,
            )
        };
        if (truncated || listed < assigned) && capacity < (1 << 20) {
            capacity = capacity.max(assigned) * 2;
            continue;
        }
        let listed = listed.min(capacity);
        return Ok(buffer[first..first + listed]
            .iter()
            .map(|pid| *pid as u32)
            .collect());
    }
}

/// Opens `pid` and returns the handle only if that process is a member of
/// `job`. While the handle is open the id cannot be reused, so it is safe to
/// act on `pid` for as long as the returned handle lives.
fn open_member(job: HANDLE, pid: u32) -> Option<OwnedHandle> {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: no pointers are involved; failure is reported as null.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a non-null handle from OpenProcess is ours to close.
    let handle = unsafe { owned(handle) };
    let mut in_job = 0;
    // SAFETY: both handles are open and `in_job` is a valid out pointer.
    let ok = unsafe { IsProcessInJob(raw(&handle), job, &mut in_job) };
    (ok != 0 && in_job != 0).then_some(handle)
}

fn image_path(process: HANDLE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: the buffer is writable for `len` UTF-16 units and `len` is a
    // valid in/out pointer.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len)
    };
    (ok != 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
}

/// Private committed bytes of one process.
fn private_commit(process: HANDLE) -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: `counters` is writable for `size` bytes; the extended structure
    // starts with the base structure the signature names.
    let ok = unsafe { K32GetProcessMemoryInfo(process, (&raw mut counters).cast(), size) };
    (ok != 0).then_some(counters.PrivateUsage as u64)
}

fn create_job_object(object_name: Option<&str>) -> io::Result<(OwnedHandle, bool)> {
    let name = object_name.map(wide);
    let name_ptr = name.as_ref().map_or(null(), |name| name.as_ptr());
    // SAFETY: `name_ptr` is null or a NUL-terminated string that outlives the
    // call; default security attributes give a non-inheritable handle.
    let (handle, error) = unsafe {
        SetLastError(0);
        let handle = CreateJobObjectW(null(), name_ptr);
        (handle, GetLastError())
    };
    if handle.is_null() {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: a non-null handle from CreateJobObjectW is ours to close.
    Ok((unsafe { owned(handle) }, error == ERROR_ALREADY_EXISTS))
}

/// Opens a named job. `Ok(None)` when no object has that name.
fn open_job_object(object_name: &str) -> Result<Option<OwnedHandle>, ContainError> {
    let name = wide(object_name);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_ALL_ACCESS, 0, name.as_ptr()) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            return Ok(None);
        }
        return Err(ContainError::Other(format!(
            "OpenJobObjectW({object_name}): {error}"
        )));
    }
    // SAFETY: a non-null handle from OpenJobObjectW is ours to close.
    Ok(Some(unsafe { owned(handle) }))
}

fn set_completion_port(job: HANDLE, port: HANDLE) -> Result<(), ContainError> {
    let info = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: null_mut(),
        CompletionPort: port,
    };
    // SAFETY: the type is the structure of this class.
    unsafe {
        set_job(
            job,
            JobObjectAssociateCompletionPortInformation,
            &info,
            "SetInformationJobObject(completion port)",
        )
    }
}

/// Creates a completion port and makes it the job's notification port.
fn associate_port(job: HANDLE) -> Result<OwnedHandle, ContainError> {
    // SAFETY: creates a new port that is not bound to any file handle.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
    if port.is_null() {
        return Err(win_err("CreateIoCompletionPort"));
    }
    // SAFETY: a non-null handle from CreateIoCompletionPort is ours to close.
    let port = unsafe { owned(port) };
    set_completion_port(job, raw(&port))?;
    Ok(port)
}

/// Starts the process that keeps both job names alive: `cmd.exe`, created
/// suspended and never resumed, placed in `keeper_job` and given query-only
/// handles to both jobs. Names of kernel objects last only while a handle is
/// open, so without it a job whose worker died could not be found again.
fn start_keeper(job: HANDLE, keeper_job: HANDLE) -> Result<(), ContainError> {
    let system = system_directory()?;
    let application = wide_path(&system.join("cmd.exe"))?;
    let mut line = wide("cmd.exe");
    let cwd = wide_path(&system)?;
    let environment = [0u16; 2];
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: every pointer refers to a live, NUL-terminated or correctly
    // sized buffer and `line` is mutable. No handle is inherited.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        return Err(win_err("CreateProcessW(keeper)"));
    }
    // SAFETY: CreateProcessW succeeded, so both handles are ours to close.
    // The thread handle is closed without ever resuming the thread.
    let (process, _thread) = unsafe { (owned(info.hProcess), owned(info.hThread)) };
    let place = || {
        // SAFETY: both handles are open.
        if unsafe { AssignProcessToJobObject(keeper_job, raw(&process)) } == 0 {
            return Err(win_err("AssignProcessToJobObject(keeper)"));
        }
        for handle in [job, keeper_job] {
            let mut remote = null_mut();
            // SAFETY: all handles are open and `remote` is a valid out
            // pointer. The copy lives in the keeper's handle table.
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    handle,
                    raw(&process),
                    &mut remote,
                    JOB_OBJECT_QUERY,
                    0,
                    0,
                )
            };
            if ok == 0 {
                return Err(win_err("DuplicateHandle(keeper)"));
            }
        }
        Ok(())
    };
    let placed = place();
    if placed.is_err() {
        // SAFETY: the process handle is open. The process never ran.
        unsafe { TerminateProcess(raw(&process), 0) };
    }
    placed
}

// ---------------------------------------------------------------------------
// Command line, environment and program lookup
// ---------------------------------------------------------------------------

fn has_nul(text: &str) -> bool {
    text.contains('\0')
}

/// Appends `arg` quoted so that the MSVC runtime (and `CommandLineToArgvW`)
/// parses it back to exactly `arg`.
fn append_quoted(arg: &str, line: &mut String) {
    let plain = !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0B' | '"'));
    if plain {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, then the quote is escaped.
                line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes are doubled so they do not escape the closing quote.
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// One argument quoted with the MSVC runtime rules.
pub fn quote_arg(arg: &str) -> String {
    let mut quoted = String::new();
    append_quoted(arg, &mut quoted);
    quoted
}

fn is_cmd_exe(program: &str) -> bool {
    let name = program.rsplit(['\\', '/', ':']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd")
}

/// The `CreateProcessW` command line for `argv`.
///
/// Arguments are quoted with the MSVC runtime rules, which nearly every
/// Windows program uses to split its command line. The program name is
/// parsed by a simpler rule that has no escape for a double quote, so a
/// quote in `argv[0]` is an error.
///
/// `cmd.exe` is the exception: when `argv[0]` is `cmd` or `cmd.exe` and a
/// separate `/c` or `/k` argument is present, the arguments after it are
/// joined with single spaces and passed verbatim inside one pair of quotes,
/// with `/s` added so that `cmd` strips exactly that pair. The text is shell
/// code and is run as such.
pub fn command_line(argv: &[String]) -> Result<String, ContainError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| ContainError::Other("argv is empty".into()))?;
    if program.is_empty() {
        return Err(ContainError::Other("argv[0] is empty".into()));
    }
    if argv.iter().any(|arg| has_nul(arg)) {
        return Err(ContainError::Other(
            "an argument contains a NUL character".into(),
        ));
    }
    if program.contains('"') {
        return Err(ContainError::Other(format!(
            "program name `{program}` contains a double quote"
        )));
    }

    let mut line = String::new();
    if program.contains([' ', '\t']) {
        line.push('"');
        line.push_str(program);
        line.push('"');
    } else {
        line.push_str(program);
    }

    let is_run_switch =
        |arg: &String| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k");
    let shell_split = is_cmd_exe(program)
        .then(|| args.iter().position(is_run_switch))
        .flatten();
    if let Some(position) = shell_split {
        let (switches, rest) = args.split_at(position);
        for switch in switches {
            line.push(' ');
            append_quoted(switch, &mut line);
        }
        if !switches.iter().any(|s| s.eq_ignore_ascii_case("/s")) {
            line.push_str(" /s");
        }
        line.push(' ');
        line.push_str(&rest[0]);
        line.push_str(" \"");
        line.push_str(&rest[1..].join(" "));
        line.push('"');
    } else {
        for arg in args {
            line.push(' ');
            append_quoted(arg, &mut line);
        }
    }

    let units = line.encode_utf16().count();
    if units > MAX_COMMAND_LINE_UNITS {
        return Err(ContainError::Other(format!(
            "command line is {units} UTF-16 units; Windows allows {MAX_COMMAND_LINE_UNITS}"
        )));
    }
    Ok(line)
}

/// The Unicode environment block for `env`: `NAME=value\0` entries sorted by
/// upper-cased name, closed by an extra `\0`. Nothing is added to `env`.
fn environment_block(env: &BTreeMap<String, String>) -> Result<Vec<u16>, ContainError> {
    let mut entries: Vec<(Vec<u16>, &String, &String)> = Vec::with_capacity(env.len());
    for (name, value) in env {
        // Windows keeps per-drive current directories in variables whose
        // names start with `=`; anywhere else `=` ends the name.
        if name.is_empty() || name.chars().skip(1).any(|c| c == '=') || has_nul(name) {
            return Err(ContainError::Other(format!(
                "environment variable name `{}` is empty or contains `=` or NUL",
                name.escape_default()
            )));
        }
        if has_nul(value) {
            return Err(ContainError::Other(format!(
                "value of environment variable `{name}` contains a NUL character"
            )));
        }
        entries.push((name.to_uppercase().encode_utf16().collect(), name, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(pair) = entries.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(ContainError::Other(format!(
            "environment variables `{}` and `{}` differ only in case",
            pair[0].1, pair[1].1
        )));
    }

    let mut block = Vec::new();
    for (_, name, value) in &entries {
        block.extend(name.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.encode_utf16());
        block.push(0);
    }
    if entries.is_empty() {
        // An empty block still needs its two terminators.
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn is_batch_file(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    // Win32 ignores trailing dots and spaces in a file name.
    let name = name.to_string_lossy().to_ascii_lowercase();
    let name = name.trim_end_matches(['.', ' ']);
    name.ends_with(".bat") || name.ends_with(".cmd")
}

fn batch_error(program: &str, found: &Path) -> ContainError {
    ContainError::Other(format!(
        "`{program}` is a batch file ({}); batch files are not started directly. \
         Run it through an explicit shell argv, for example \
         [\"cmd.exe\", \"/d\", \"/c\", \"<command line>\"]",
        found.display()
    ))
}

/// The first existing image for `base`: the exact name when it has an
/// extension, otherwise `.exe`, `.com`, then the bare name. A batch file with
/// the same stem is remembered so the error can say why it was not used.
fn first_image(base: &Path, batch: &mut Option<PathBuf>) -> Option<PathBuf> {
    if base.extension().is_some() {
        return base.is_file().then(|| base.to_path_buf());
    }
    for extension in ["exe", "com"] {
        let candidate = base.with_extension(extension);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    if batch.is_none() {
        *batch = ["cmd", "bat"]
            .iter()
            .map(|extension| base.with_extension(extension))
            .find(|candidate| candidate.is_file());
    }
    None
}

/// Directories searched for a bare program name: `PATH` of the process
/// environment, then the system and Windows directories. The worker's own
/// `PATH`, current directory and install directory are never consulted.
fn search_directories(env: &BTreeMap<String, String>) -> Result<Vec<PathBuf>, ContainError> {
    let mut directories: Vec<PathBuf> = env
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.as_str())
        .unwrap_or_default()
        .split(';')
        .map(|entry| PathBuf::from(entry.trim().trim_matches('"')))
        .filter(|entry| entry.is_absolute())
        .collect();
    let system = system_directory()?;
    let windows = system.parent().map(Path::to_path_buf);
    directories.push(system);
    directories.extend(windows);
    Ok(directories)
}

/// Resolves `argv[0]` to the image file passed to `CreateProcessW`.
fn resolve_program(
    program: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf, ContainError> {
    let path = Path::new(program);
    let mut batch = None;
    let found = if program.contains(['\\', '/', ':']) {
        let base = if path.is_absolute() {
            path.to_path_buf()
        } else if matches!(path.components().next(), Some(Component::Prefix(_))) {
            return Err(ContainError::Other(format!(
                "program path `{program}` is relative to a drive's current directory; \
                 use an absolute path or one relative to the working directory"
            )));
        } else {
            cwd.join(path)
        };
        first_image(&base, &mut batch)
    } else {
        search_directories(env)?
            .iter()
            .find_map(|directory| first_image(&directory.join(path), &mut batch))
    };
    match (found, batch) {
        (Some(found), _) if is_batch_file(&found) => Err(batch_error(program, &found)),
        (Some(found), _) => Ok(found),
        (None, Some(batch)) => Err(batch_error(program, &batch)),
        (None, None) => Err(ContainError::Spawn {
            program: program.to_owned(),
            source: io::Error::new(
                io::ErrorKind::NotFound,
                "program not found in the working directory, the PATH of the process \
                 environment or the system directory",
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Handles for a new process
// ---------------------------------------------------------------------------

fn inheritable() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    }
}

/// An inheritable read handle to the `NUL` device.
fn open_nul() -> Result<OwnedHandle, ContainError> {
    let name = wide("NUL");
    let attributes = inheritable();
    // SAFETY: `name` is NUL-terminated and `attributes` is a valid structure;
    // both outlive the call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(win_err("CreateFileW(NUL)"));
    }
    // SAFETY: a valid handle from CreateFileW is ours to close.
    Ok(unsafe { owned(handle) })
}

/// An anonymous pipe as `(read, write)`. Only the write end is inheritable,
/// and only for the short time until the child has been created; it is
/// inherited by name through the handle list, never by accident.
fn pipe() -> Result<(OwnedHandle, OwnedHandle), ContainError> {
    let (mut read, mut write) = (null_mut(), null_mut());
    // SAFETY: both out pointers are valid; null attributes give
    // non-inheritable handles.
    if unsafe { CreatePipe(&mut read, &mut write, null(), 0) } == 0 {
        return Err(win_err("CreatePipe"));
    }
    // SAFETY: CreatePipe succeeded, so both handles are ours to close.
    let (read, write) = unsafe { (owned(read), owned(write)) };
    // SAFETY: `write` is an open handle.
    if unsafe { SetHandleInformation(raw(&write), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(win_err("SetHandleInformation"));
    }
    Ok((read, write))
}

/// A `PROC_THREAD_ATTRIBUTE_LIST` holding the handles a child may inherit.
struct AttributeList {
    storage: Vec<usize>,
    /// The list keeps a pointer to this array until it is deleted.
    _handles: Box<[HANDLE]>,
}

impl AttributeList {
    fn with_handles(handles: &[HANDLE]) -> Result<Self, ContainError> {
        let mut size = 0usize;
        // SAFETY: a null list with a size out pointer asks for the required
        // size; the call is expected to fail with ERROR_INSUFFICIENT_BUFFER.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(win_err("InitializeProcThreadAttributeList(size)"));
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `storage` is writable for at least `size` bytes.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut size)
        } == 0
        {
            return Err(win_err("InitializeProcThreadAttributeList"));
        }
        let handles: Box<[HANDLE]> = handles.into();
        // SAFETY: the list was initialised above; `handles` is readable for
        // the stated size and stays alive, unmoved, as long as the list.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                storage.as_mut_ptr().cast(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(&*handles),
                null_mut(),
                null(),
            )
        };
        if ok == 0 {
            let error = win_err("UpdateProcThreadAttribute(handle list)");
            // SAFETY: the list was initialised and is not used again.
            unsafe { DeleteProcThreadAttributeList(storage.as_mut_ptr().cast()) };
            return Err(error);
        }
        Ok(Self {
            storage,
            _handles: handles,
        })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised in `with_handles` and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// Read end of a child's output pipe. End-of-file is reported once every
/// process holding the write end has exited.
struct PipeReader(OwnedHandle);

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let mut read = 0u32;
        // SAFETY: `buffer` is writable for `len` bytes, `read` is a valid out
        // pointer and the handle is a synchronous pipe handle.
        let ok = unsafe {
            ReadFile(
                raw(&self.0),
                buffer.as_mut_ptr(),
                len,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(error);
        }
        Ok(read as usize)
    }
}

struct JobChild {
    pid: u32,
    process: Arc<OwnedHandle>,
}

impl JobChild {
    fn poll(&self, timeout_ms: u32) -> io::Result<Option<ExitInfo>> {
        // SAFETY: the process handle is open for the lifetime of `self`.
        match unsafe { WaitForSingleObject(raw(&self.process), timeout_ms) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: the handle is open and `code` is a valid out pointer.
                if unsafe { GetExitCodeProcess(raw(&self.process), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitInfo {
                    code: Some(code as i32),
                    signal: None,
                }))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

impl Child for JobChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitInfo>> {
        self.poll(0)
    }

    fn wait(&mut self) -> io::Result<ExitInfo> {
        loop {
            if let Some(exit) = self.poll(INFINITE)? {
                return Ok(exit);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Console attachment for CTRL_BREAK
// ---------------------------------------------------------------------------

fn console_processes() -> Vec<u32> {
    let mut list = vec![0u32; 64];
    loop {
        // SAFETY: `list` is writable for `list.len()` entries.
        let count = unsafe { GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) } as usize;
        if count <= list.len() {
            list.truncate(count);
            return list;
        }
        list = vec![0; count * 2];
    }
}

fn is_console_handle(handle: HANDLE) -> bool {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut mode = 0;
    // SAFETY: `mode` is a valid out pointer; a handle that is not a console
    // handle makes the call fail.
    unsafe { GetConsoleMode(handle, &mut mode) != 0 }
}

/// The standard handles of this process as they were before a transient
/// console attachment.
struct StdSlots([HANDLE; 3]);

impl StdSlots {
    fn capture() -> Self {
        // SAFETY: no pointers are involved.
        Self(STD_SLOTS.map(|slot| unsafe { GetStdHandle(slot) }))
    }

    /// Puts the captured handles back and returns the handles that had
    /// replaced them. `AttachConsole` installs console handles in slots it
    /// considers unset.
    fn restore(&self) -> Vec<HANDLE> {
        let mut installed = Vec::new();
        for (slot, saved) in STD_SLOTS.iter().zip(self.0) {
            // SAFETY: no pointers are involved.
            let current = unsafe { GetStdHandle(*slot) };
            if current == saved {
                continue;
            }
            // SAFETY: `saved` is the value this slot held before.
            unsafe { SetStdHandle(*slot, saved) };
            let usable = !current.is_null() && current != INVALID_HANDLE_VALUE;
            if usable && !self.0.contains(&current) && !installed.contains(&current) {
                installed.push(current);
            }
        }
        installed
    }
}

/// A second handle to the object behind `handle`, used to tell later whether
/// `handle` still refers to that object.
fn duplicate(handle: HANDLE) -> Option<OwnedHandle> {
    let mut copy = null_mut();
    // SAFETY: the pseudo handle of the current process is always valid,
    // `copy` is a valid out pointer, and a stale `handle` only makes the call fail.
    let ok = unsafe {
        let process = GetCurrentProcess();
        DuplicateHandle(
            process,
            handle,
            process,
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    // SAFETY: on success `copy` is a new handle that is ours to close.
    (ok != 0 && !copy.is_null()).then(|| unsafe { owned(copy) })
}

/// Whether this process may leave its console for a moment, and how to come
/// back to it.
struct ConsolePlan {
    /// Other processes on this process's own console; empty without one.
    return_to: Vec<u32>,
    has_console: bool,
    slots: StdSlots,
}

fn console_plan() -> Result<ConsolePlan, String> {
    let slots = StdSlots::capture();
    if slots.0.iter().any(|handle| is_console_handle(*handle)) {
        return Err(
            "a standard handle of this process is a console handle, which detaching from \
             the console would invalidate"
                .into(),
        );
    }
    let me = std::process::id();
    let own = console_processes();
    let return_to: Vec<u32> = own.iter().copied().filter(|pid| *pid != me).collect();
    if !own.is_empty() && return_to.is_empty() {
        return Err(
            "this process is the only one on its console and could not re-attach to it".into(),
        );
    }
    Ok(ConsolePlan {
        return_to,
        has_console: !own.is_empty(),
        slots,
    })
}

/// What [`JobScope::send_ctrl_break`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtrlBreakReport {
    /// Consoles on which the event was generated for at least one group.
    pub consoles: u32,
    /// Processes attached to those consoles.
    pub processes: u32,
    /// Why nothing was sent, when the calling process could not safely
    /// attach to another console.
    pub skipped: Option<String>,
    /// Set when the calling process had a console and could not return to it.
    pub console_lost: bool,
}

/// Limit hits recorded for a scope since it was created or adopted.
///
/// Windows posts these notifications on a best-effort basis: a count above
/// zero proves a hit, a zero does not prove there was none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitEvents {
    /// Allocations refused because the job's commit limit was reached.
    pub memory_limit_hits: u64,
    /// Process creations refused by the active-process limit.
    pub process_limit_hits: u64,
    /// Members that ended with an unhandled exception or similar abnormal status.
    pub abnormal_exits: u64,
}

/// One live member of a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobProcess {
    pub pid: u32,
    /// Full image path; `None` when the process could not be opened or had
    /// already exited.
    pub image: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

struct Root {
    pid: u32,
    /// Keeps the process object alive so that `pid`, which is also the id of
    /// the process group, cannot be reused while the scope exists.
    process: Arc<OwnedHandle>,
}

/// A containment scope backed by one named job object.
pub struct JobScope {
    name: String,
    object_name: String,
    job: OwnedHandle,
    /// Job holding only the keeper process.
    keeper_job: OwnedHandle,
    /// Completion port receiving the job's notifications. `None` on an
    /// adopted scope whose port could not be replaced.
    port: Option<OwnedHandle>,
    events: Mutex<LimitEvents>,
    roots: Vec<Root>,
    logical_cpus: u32,
}

impl JobScope {
    fn handle(&self) -> HANDLE {
        raw(&self.job)
    }

    /// Name of the job object in the kernel object namespace.
    pub fn object_name(&self) -> &str {
        &self.object_name
    }

    /// Starts a keeper if there is none, for instance because another handle
    /// to this scope was dropped while the scope was empty.
    fn ensure_keeper(&self) -> Result<(), ContainError> {
        if accounting(raw(&self.keeper_job))?.ActiveProcesses == 0 {
            start_keeper(self.handle(), raw(&self.keeper_job))?;
        }
        Ok(())
    }

    /// Moves queued job notifications into the counters. Also keeps the
    /// queue short: the job posts a message for every process start and exit.
    fn drain_notifications(&self) -> Result<Option<LimitEvents>, ContainError> {
        let Some(port) = &self.port else {
            return Ok(None);
        };
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let (mut message, mut key, mut detail) = (0u32, 0usize, null_mut());
            // SAFETY: all three out pointers are valid and the port is open.
            // A zero timeout makes this a poll.
            let ok = unsafe {
                GetQueuedCompletionStatus(raw(port), &mut message, &mut key, &mut detail, 0)
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    return Ok(Some(*events));
                }
                return Err(ContainError::Other(format!(
                    "GetQueuedCompletionStatus: {error}"
                )));
            }
            match message {
                JOB_OBJECT_MSG_JOB_MEMORY_LIMIT | JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT => {
                    events.memory_limit_hits += 1;
                }
                JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT => events.process_limit_hits += 1,
                JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => events.abnormal_exits += 1,
                _ => {}
            }
        }
    }

    /// Limit hits observed so far. `Unsupported` on an adopted scope whose
    /// notifications still belong to the worker process that created it.
    pub fn limit_events(&self) -> Result<LimitEvents, ContainError> {
        self.drain_notifications()?.ok_or(ContainError::Unsupported(
            "limit notifications on this adopted scope",
        ))
    }

    /// Live members with their image paths, for reporting what is still
    /// running. Only processes confirmed as members are given an image.
    pub fn processes(&self) -> Result<Vec<JobProcess>, ContainError> {
        Ok(process_ids(self.handle())?
            .into_iter()
            .map(|pid| JobProcess {
                pid,
                image: open_member(self.handle(), pid).and_then(|member| image_path(raw(&member))),
            })
            .collect())
    }

    /// Generates `CTRL_BREAK_EVENT` for the scope's process groups.
    ///
    /// Each root has its own console, so this process attaches to every
    /// member console in turn, generates the event for the groups that can
    /// be on it (every live member in case it leads a group, and the roots
    /// that exited and may have left descendants), and detaches again. Events are always addressed to a group,
    /// never to a whole console, so only processes of this scope receive
    /// them. See the module documentation for when nothing is sent.
    pub fn send_ctrl_break(&mut self) -> Result<CtrlBreakReport, ContainError> {
        let mut report = CtrlBreakReport::default();
        let members = process_ids(self.handle())?;
        if members.is_empty() {
            return Ok(report);
        }
        let _console = CONSOLE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let plan = match console_plan() {
            Ok(plan) => plan,
            Err(reason) => {
                report.skipped = Some(reason);
                return Ok(report);
            }
        };
        // SAFETY: no pointers are involved; this process re-attaches below.
        if plan.has_console && unsafe { FreeConsole() } == 0 {
            report.skipped = Some(format!(
                "could not detach from the console: {}",
                io::Error::last_os_error()
            ));
            return Ok(report);
        }

        let me = std::process::id();
        let exited_roots: Vec<u32> = self
            .roots
            .iter()
            // SAFETY: the root's process handle is open.
            .filter(|root| unsafe { WaitForSingleObject(raw(&root.process), 0) } == WAIT_OBJECT_0)
            .map(|root| root.pid)
            .collect();
        let mut visited = BTreeSet::new();
        for pid in &members {
            if !visited.insert(*pid) {
                continue;
            }
            // Holding the handle pins the id until the console is released.
            let Some(_member) = open_member(self.handle(), *pid) else {
                continue;
            };
            // SAFETY: no pointers are involved. Fails for a process without
            // a console or one that just exited.
            if unsafe { AttachConsole(*pid) } == 0 {
                continue;
            }
            let installed: Vec<(HANDLE, OwnedHandle)> = plan
                .slots
                .restore()
                .into_iter()
                .filter_map(|handle| duplicate(handle).map(|copy| (handle, copy)))
                .collect();

            let attached = console_processes();
            // A live root shows up in `attached`; a root that exited may
            // still have its group on any console.
            let mut groups: BTreeSet<u32> = exited_roots.iter().copied().collect();
            for other in attached.iter().filter(|other| **other != me) {
                visited.insert(*other);
                if members.contains(other) {
                    groups.insert(*other);
                }
            }
            let mut sent = false;
            for group in groups {
                // SAFETY: no pointers are involved. An id that is not a
                // process group on this console makes the call fail.
                sent |= unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) } != 0;
            }
            if sent {
                report.consoles += 1;
                report.processes += attached.iter().filter(|other| **other != me).count() as u32;
            }

            // SAFETY: no pointers are involved.
            unsafe { FreeConsole() };
            plan.slots.restore();
            for (handle, copy) in installed {
                // SAFETY: `copy` is open. `handle` is closed here only if it
                // still names the object AttachConsole created for this
                // process, i.e. FreeConsole did not already close it.
                unsafe {
                    if CompareObjectHandles(handle, raw(&copy)) != 0 {
                        CloseHandle(handle);
                    }
                }
            }
        }

        if plan.has_console {
            // SAFETY: no pointers are involved.
            let back = plan
                .return_to
                .iter()
                .any(|pid| unsafe { AttachConsole(*pid) } != 0)
                || unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
            plan.slots.restore();
            report.console_lost = !back;
        }
        Ok(report)
    }
}

impl Scope for JobScope {
    fn identity(&self) -> ScopeIdentity {
        ScopeIdentity {
            kind: ScopeKind::WindowsJob,
            name: self.name.clone(),
            detail: self.object_name.clone(),
        }
    }

    fn spawn(&mut self, spec: &ProcessSpec) -> Result<Spawned, ContainError> {
        let mut line = wide(&command_line(&spec.argv)?);
        let program = spec.argv[0].clone();
        self.ensure_keeper()?;
        if !spec.cwd.is_absolute() {
            return Err(ContainError::Other(format!(
                "working directory `{}` is not absolute",
                spec.cwd.display()
            )));
        }
        let application = wide_path(&resolve_program(&program, &spec.cwd, &spec.env)?)?;
        let environment = environment_block(&spec.env)?;
        let cwd = wide_path(&spec.cwd)?;

        let stdin = open_nul()?;
        let (stdout_read, stdout_write) = pipe()?;
        let (stderr_read, stderr_write) = pipe()?;
        let mut attributes =
            AttributeList::with_handles(&[raw(&stdin), raw(&stdout_write), raw(&stderr_write)])?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: raw(&stdin),
                hStdOutput: raw(&stdout_write),
                hStdError: raw(&stderr_write),
                ..Default::default()
            },
            lpAttributeList: attributes.as_mut_ptr(),
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every pointer refers to a live, NUL-terminated or correctly
        // sized buffer; `line` is mutable as CreateProcessW requires; the
        // startup structure is the extended form announced by
        // EXTENDED_STARTUPINFO_PRESENT and its attribute list is initialised.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                SPAWN_FLAGS,
                environment.as_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        };
        if created == 0 {
            return Err(ContainError::Spawn {
                program,
                source: io::Error::last_os_error(),
            });
        }
        // SAFETY: CreateProcessW succeeded, so both handles are ours to close.
        let (process, thread) = unsafe { (Arc::new(owned(info.hProcess)), owned(info.hThread)) };
        // The child has its own copies; ours must go so the pipes can reach
        // end-of-file.
        drop((attributes, stdin, stdout_write, stderr_write));

        // The process is still suspended: it becomes a member before it runs.
        // SAFETY: both handles are open.
        let assigned = unsafe { AssignProcessToJobObject(self.handle(), raw(&process)) } != 0;
        // SAFETY: the thread handle is open.
        let failure = if !assigned {
            Some(("AssignProcessToJobObject", io::Error::last_os_error()))
        } else if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
            Some(("ResumeThread", io::Error::last_os_error()))
        } else {
            None
        };
        if let Some((step, error)) = failure {
            // SAFETY: the process handle is open. The process never ran.
            unsafe { TerminateProcess(raw(&process), KILL_EXIT_CODE) };
            return Err(ContainError::Other(format!(
                "`{program}` was not started in job object {}: {step}: {error}",
                self.object_name
            )));
        }

        self.roots.push(Root {
            pid: info.dwProcessId,
            process: Arc::clone(&process),
        });
        Ok(Spawned {
            child: Box::new(JobChild {
                pid: info.dwProcessId,
                process,
            }),
            stdout: Box::new(PipeReader(stdout_read)),
            stderr: Box::new(PipeReader(stderr_read)),
        })
    }

    /// CPU time covers every process that was ever a member. Peak memory is
    /// the job's peak commit charge; at the limit it can include the size of
    /// the allocation that was refused. Current memory is the sum of the private
    /// commit of the members that could be opened, the same quantity the
    /// memory limit applies to. Disk use is not measured.
    fn usage(&self) -> Result<UsageSummary, ContainError> {
        self.drain_notifications()?;
        let accounting = accounting(self.handle())?;
        let limits = extended_limits(self.handle())?;
        let memory_current_bytes: u64 = process_ids(self.handle())?
            .into_iter()
            .filter_map(|pid| open_member(self.handle(), pid))
            .filter_map(|member| private_commit(raw(&member)))
            .sum();
        // Job times are in 100 ns units.
        let cpu_100ns =
            accounting.TotalUserTime.max(0) as u64 + accounting.TotalKernelTime.max(0) as u64;
        Ok(UsageSummary {
            cpu_time_ms: cpu_100ns / 10_000,
            memory_current_bytes,
            memory_peak_bytes: limits.PeakJobMemoryUsed as u64,
            processes: accounting.ActiveProcesses,
            disk_bytes: 0,
        })
    }

    /// A process stops counting when its exit begins. Its handles, and with
    /// them locks on files in the workspace, are released a moment later.
    fn members(&self) -> Result<Membership, ContainError> {
        self.drain_notifications()?;
        Ok(match accounting(self.handle())?.ActiveProcesses {
            0 => Membership::Empty,
            alive => Membership::Alive(alive),
        })
    }

    fn set_limits(&mut self, limits: &Limits) -> Result<(), ContainError> {
        apply_limits(self.handle(), limits, self.logical_cpus)
    }

    fn terminate(&mut self) -> Result<(), ContainError> {
        self.send_ctrl_break().map(|_| ())
    }

    fn kill(&mut self) -> Result<(), ContainError> {
        // SAFETY: the job handle is open.
        if unsafe { TerminateJobObject(self.handle(), KILL_EXIT_CODE) } == 0 {
            return Err(win_err("TerminateJobObject"));
        }
        Ok(())
    }

    /// On `NotEmpty` this handle is closed like on any drop; the job, its
    /// processes and its keeper remain and can be adopted again by identity.
    fn destroy(self: Box<Self>) -> Result<String, ContainError> {
        let accounting = accounting(self.handle())?;
        if accounting.ActiveProcesses > 0 {
            return Err(ContainError::NotEmpty(self.name.clone()));
        }
        let object_name = self.object_name.clone();
        // Dropping an empty scope ends its keeper and closes this handle.
        drop(self);
        // The name goes once the keeper's handles are closed, which the
        // kernel does a moment after the keeper is terminated.
        let started = Instant::now();
        let name_state = loop {
            match open_job_object(&object_name) {
                Ok(None) => break "handle closed, keeper ended, name released",
                Ok(Some(_)) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(Some(_)) => break "handle closed, name still held by another handle",
                Err(_) => break "handle closed",
            }
        };
        Ok(format!(
            "job object {object_name}: ActiveProcesses=0 (TotalProcesses={}, \
             TotalTerminatedProcesses={}); {name_state}",
            accounting.TotalProcesses, accounting.TotalTerminatedProcesses
        ))
    }
}

impl Drop for JobScope {
    /// An empty scope has nothing to adopt, so its keeper goes with the
    /// handle and the job disappears. A scope with members keeps its keeper.
    fn drop(&mut self) {
        if accounting(self.handle()).is_ok_and(|accounting| accounting.ActiveProcesses == 0) {
            // SAFETY: the keeper job handle is open; it holds only the keeper.
            unsafe { TerminateJobObject(raw(&self.keeper_job), 0) };
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// The job object name for a scope name, after checking the name.
fn object_name(name: &str) -> Result<String, ContainError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SCOPE_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !valid {
        return Err(ContainError::Other(format!(
            "scope name `{}` must be 1 to {MAX_SCOPE_NAME_LEN} characters of [a-z0-9_-]",
            name.escape_default()
        )));
    }
    Ok(format!("{OBJECT_NAME_PREFIX}{name}"))
}

/// Describes the job this process already runs in, if any.
fn outer_job_note() -> Option<String> {
    let mut in_job = 0;
    // SAFETY: the pseudo handle of the current process is always valid, a
    // null job means "any job", and `in_job` is a valid out pointer.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) };
    if ok == 0 || in_job == 0 {
        return None;
    }
    let flags = extended_limits(null_mut())
        .map(|limits| limits.BasicLimitInformation.LimitFlags)
        .ok();
    let detail = match flags {
        Some(flags) if flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0 => {
            format!(" (limit flags {flags:#x}, kill-on-close)")
        }
        Some(flags) => format!(" (limit flags {flags:#x})"),
        None => String::new(),
    };
    Some(format!(
        "nested: this process already runs inside a job object{detail}; scopes nest in it, \
         are bounded by its limits too and end with it"
    ))
}

fn polite_stop_note() -> String {
    match console_plan() {
        Ok(plan) if plan.has_console => {
            "terminate: CTRL_BREAK is sent by leaving this process's console for a moment \
             and attaching to each member console"
                .into()
        }
        Ok(_) => "terminate: CTRL_BREAK is sent by attaching to each member console \
                  (this process has no console)"
            .into(),
        Err(reason) => format!("terminate: no polite signal is sent, only kill works: {reason}"),
    }
}

/// Job Object containment. Stateless apart from what was detected at
/// construction; scopes hold their own job handles.
pub struct JobObjects {
    logical_cpus: u32,
    detected: EnforcementReport,
}

impl JobObjects {
    /// Detects what job objects accept here by setting each limit on a
    /// throw-away, empty job. `Unavailable` when no job can be created.
    pub fn new() -> Result<Self, ContainError> {
        let logical_cpus = logical_cpus();
        let (probe, _) = create_job_object(None)
            .map_err(|error| ContainError::Unavailable(format!("CreateJobObjectW: {error}")))?;
        let mut notes = Vec::new();
        let mut accepted =
            |what: &str, limits: Limits| match apply_limits(raw(&probe), &limits, logical_cpus) {
                Ok(()) => Enforcement::Hard,
                Err(error) => {
                    notes.push(format!("{what} limit is not accepted: {error}"));
                    Enforcement::None
                }
            };
        let memory = accepted(
            "memory",
            Limits {
                memory_bytes: Some(1 << 30),
                ..Limits::default()
            },
        );
        let process_count = accepted(
            "process count",
            Limits {
                max_processes: Some(64),
                ..Limits::default()
            },
        );
        let cpu = accepted(
            "cpu",
            Limits {
                cpu_milli: Some(500),
                ..Limits::default()
            },
        );
        notes.push(
            "memory: job-wide commit limit, not a physical-RAM reservation; \
             allocations beyond it fail"
                .into(),
        );
        notes.push(format!(
            "cpu: hard cap as a share of all {logical_cpus} logical processors"
        ));
        notes.push(
            "process count: console hosts (conhost.exe) are members but Windows does not \
             count them against the limit"
                .into(),
        );
        notes.push("disk: not limited".into());
        notes.extend(outer_job_note());
        notes.push(polite_stop_note());
        Ok(Self {
            logical_cpus,
            detected: EnforcementReport {
                cpu,
                memory,
                process_tree: Enforcement::Hard,
                process_count,
                disk: Enforcement::None,
                verified: false,
                notes,
            },
        })
    }

    /// [`Containment::create`] with the concrete scope type, which also
    /// offers [`JobScope::limit_events`] and [`JobScope::processes`].
    pub fn create_job(&self, name: &str, limits: &Limits) -> Result<JobScope, ContainError> {
        let object_name = object_name(name)?;
        let (job, existed) = create_job_object(Some(&object_name)).map_err(|error| {
            ContainError::Other(format!("CreateJobObjectW({object_name}): {error}"))
        })?;
        if existed {
            // Never take over an object somebody else created under this name.
            return Err(ContainError::Other(format!(
                "job object {object_name} already exists; scope names are unique per attempt \
                 (adopt the existing scope instead)"
            )));
        }
        let keeper_name = format!("{object_name}{KEEPER_SUFFIX}");
        let (keeper_job, existed) = create_job_object(Some(&keeper_name)).map_err(|error| {
            ContainError::Other(format!("CreateJobObjectW({keeper_name}): {error}"))
        })?;
        if existed {
            return Err(ContainError::Other(format!(
                "job object {keeper_name} is left over from an earlier scope of this name"
            )));
        }
        apply_limits(raw(&job), limits, self.logical_cpus)?;
        let port = associate_port(raw(&job))?;
        // Last, so that an error above leaves no process behind.
        start_keeper(raw(&job), raw(&keeper_job))?;
        Ok(JobScope {
            name: name.to_owned(),
            object_name,
            job,
            keeper_job,
            port: Some(port),
            events: Mutex::new(LimitEvents::default()),
            roots: Vec::new(),
            logical_cpus: self.logical_cpus,
        })
    }

    /// [`Containment::adopt`] with the concrete scope type.
    ///
    /// The adopting handle takes over the job's notifications, so an earlier
    /// handle to the same scope stops receiving them. Hits that happened
    /// while no worker was running are not recovered.
    pub fn adopt_job(&self, identity: &ScopeIdentity) -> Result<Option<JobScope>, ContainError> {
        if identity.kind != ScopeKind::WindowsJob {
            return Err(ContainError::Other(format!(
                "scope `{}` is not a Windows job object scope",
                identity.name
            )));
        }
        let object_name = object_name(&identity.name)?;
        if !identity.detail.is_empty() && identity.detail != object_name {
            return Err(ContainError::Other(format!(
                "scope `{}` records job object `{}`, expected `{object_name}`",
                identity.name, identity.detail
            )));
        }
        let Some(job) = open_job_object(&object_name)? else {
            return Ok(None);
        };
        let port = associate_port(raw(&job))
            .or_else(|_| {
                set_completion_port(raw(&job), null_mut())?;
                associate_port(raw(&job))
            })
            .ok();
        let keeper_name = format!("{object_name}{KEEPER_SUFFIX}");
        let keeper_job = match open_job_object(&keeper_name)? {
            Some(keeper_job) => keeper_job,
            None => create_job_object(Some(&keeper_name))
                .map(|(keeper_job, _)| keeper_job)
                .map_err(|error| {
                    ContainError::Other(format!("CreateJobObjectW({keeper_name}): {error}"))
                })?,
        };
        let scope = JobScope {
            name: identity.name.clone(),
            object_name,
            job,
            keeper_job,
            port,
            events: Mutex::new(LimitEvents::default()),
            roots: Vec::new(),
            logical_cpus: self.logical_cpus,
        };
        scope.ensure_keeper()?;
        Ok(Some(scope))
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
        self.detected.clone()
    }

    fn self_test(&self) -> Result<EnforcementReport, ContainError> {
        let test = SelfTest::new(self)?;
        let mut report = EnforcementReport {
            cpu: Enforcement::None,
            memory: Enforcement::None,
            process_tree: Enforcement::None,
            process_count: Enforcement::None,
            disk: Enforcement::None,
            verified: true,
            notes: Vec::new(),
        };
        // A dimension is `Hard` only if its check passed on this machine.
        let mut record = |dimension: &str, result: Result<String, String>| match result {
            Ok(note) => {
                report.notes.push(format!("{dimension}: {note}"));
                Enforcement::Hard
            }
            Err(reason) => {
                report
                    .notes
                    .push(format!("{dimension}: not verified: {reason}"));
                Enforcement::None
            }
        };
        let process_tree = record("process tree", test.process_tree());
        let process_count = record("process count", test.process_count());
        let memory = record("memory", test.memory());
        let cpu = record("cpu", test.cpu());
        report.process_tree = process_tree;
        report.process_count = process_count;
        report.memory = memory;
        report.cpu = cpu;
        report.notes.push("disk: not limited".into());
        report.notes.extend(outer_job_note());
        report.notes.push(polite_stop_note());
        Ok(report)
    }

    fn create(&self, name: &str, limits: &Limits) -> Result<Box<dyn Scope>, ContainError> {
        Ok(Box::new(self.create_job(name, limits)?))
    }

    fn adopt(&self, identity: &ScopeIdentity) -> Result<Option<Box<dyn Scope>>, ContainError> {
        Ok(self
            .adopt_job(identity)?
            .map(|scope| Box::new(scope) as Box<dyn Scope>))
    }
}

// ---------------------------------------------------------------------------
// Commissioning self-test
// ---------------------------------------------------------------------------

const SELF_TEST_STEP: Duration = Duration::from_secs(30);

/// Runs real processes from the system directory inside throw-away scopes.
struct SelfTest<'a> {
    adapter: &'a JobObjects,
    system: PathBuf,
    env: BTreeMap<String, String>,
}

impl<'a> SelfTest<'a> {
    fn new(adapter: &'a JobObjects) -> Result<Self, ContainError> {
        let system = system_directory()?;
        let windows = system.parent().unwrap_or(&system).to_path_buf();
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let temp = text(&std::env::temp_dir());
        let env = BTreeMap::from([
            ("SystemRoot".to_owned(), text(&windows)),
            ("windir".to_owned(), text(&windows)),
            ("ComSpec".to_owned(), text(&system.join("cmd.exe"))),
            (
                "PATH".to_owned(),
                format!(
                    "{};{}",
                    text(&system),
                    text(&system.join(r"WindowsPowerShell\v1.0"))
                ),
            ),
            ("TEMP".to_owned(), temp.clone()),
            ("TMP".to_owned(), temp),
        ]);
        Ok(Self {
            adapter,
            system,
            env,
        })
    }

    fn spec(&self, argv: &[&str]) -> ProcessSpec {
        ProcessSpec {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: self.system.clone(),
            env: self.env.clone(),
        }
    }

    /// Runs `check` in a fresh scope and always empties and destroys the
    /// scope afterwards. A scope that cannot be emptied fails the check.
    fn in_scope(
        &self,
        tag: &str,
        limits: &Limits,
        check: impl FnOnce(&mut JobScope) -> Result<String, String>,
    ) -> Result<String, String> {
        let name = format!(
            "selftest-{tag}-{}-{}",
            std::process::id(),
            SELF_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut scope = self
            .adapter
            .create_job(&name, limits)
            .map_err(|error| error.to_string())?;
        let outcome = check(&mut scope);
        let cleanup = empty_and_destroy(scope);
        match (outcome, cleanup) {
            (Ok(note), Ok(_)) => Ok(note),
            (Err(reason), Ok(_)) => Err(reason),
            (_, Err(cleanup)) => Err(format!("scope could not be cleaned up: {cleanup}")),
        }
    }

    /// The root exits while a child started with `start /b` keeps running:
    /// the child must still be a member, and killing the job must end it.
    fn process_tree(&self) -> Result<String, String> {
        self.in_scope("tree", &Limits::default(), |scope| {
            let command = "start /b ping -n 60 127.0.0.1 >NUL 2>&1 & exit 0";
            let root = run(scope, &self.spec(&["cmd.exe", "/d", "/c", command]))?;
            if root != 0 {
                return Err(format!("the root shell exited with {root}"));
            }
            let descendant = wait_for(SELF_TEST_STEP, || {
                let processes = scope.processes().map_err(|error| error.to_string())?;
                Ok(processes
                    .into_iter()
                    .find(|process| is_image(process, "ping.exe")))
            })?
            .ok_or("a child started with `start /b` was not a member after its parent exited")?;
            let pinned = open_member(scope.handle(), descendant.pid)
                .ok_or("the descendant left the job before it could be checked")?;
            scope.kill().map_err(|error| error.to_string())?;
            // SAFETY: `pinned` is an open process handle.
            if unsafe { WaitForSingleObject(raw(&pinned), 30_000) } != WAIT_OBJECT_0 {
                return Err("the descendant survived TerminateJobObject".into());
            }
            Ok(
                "a descendant that outlived its root stayed in the job and was killed with it"
                    .into(),
            )
        })
    }

    /// A chain of nested shells fits without a limit and fails with one.
    fn process_count(&self) -> Result<String, String> {
        const LIMIT: u32 = 3;
        let chain = "cmd /c cmd /c cmd /c cmd /c cmd /c cmd /c exit 0";
        let spec = self.spec(&["cmd.exe", "/d", "/c", chain]);
        let unlimited = self.in_scope("count-free", &Limits::default(), |scope| {
            run(scope, &spec).map(|code| code.to_string())
        })?;
        if unlimited != "0" {
            return Err(format!(
                "the control run without a limit exited with {unlimited}"
            ));
        }
        let limits = Limits {
            max_processes: Some(LIMIT),
            ..Limits::default()
        };
        self.in_scope("count", &limits, |scope| {
            let code = run(scope, &spec)?;
            let hits = scope
                .limit_events()
                .map(|events| events.process_limit_hits)
                .unwrap_or(0);
            if code == 0 {
                return Err(format!(
                    "seven nested shells all started under a limit of {LIMIT} processes"
                ));
            }
            Ok(format!(
                "a limit of {LIMIT} stopped a chain of seven shells (exit code {code}, \
                 {hits} limit notifications)"
            ))
        })
    }

    /// PowerShell starts within the limit, then fails to allocate past it.
    fn memory(&self) -> Result<String, String> {
        const LIMIT: u64 = 512 << 20;
        const REFUSED: i32 = 42;
        let limits = Limits {
            memory_bytes: Some(LIMIT),
            ..Limits::default()
        };
        let script = "try { $block = [byte[]]::new(1073741824); exit 3 } catch { exit 42 }";
        let spec = self.spec(&[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]);
        self.in_scope("memory", &limits, |scope| {
            let code = run(scope, &spec)?;
            let hits = scope
                .limit_events()
                .map(|events| events.memory_limit_hits)
                .unwrap_or(0);
            match code {
                REFUSED => Ok(format!(
                    "a 1 GiB allocation was refused under a 512 MiB commit limit \
                     ({hits} limit notifications); the limit is on committed memory"
                )),
                3 => Err("a 1 GiB allocation succeeded under a 512 MiB commit limit".into()),
                other => Err(format!(
                    "the allocation probe exited with {other} instead of reporting a result"
                )),
            }
        })
    }

    /// A single-threaded busy loop under a 250 mCPU hard cap must use well
    /// under one processor.
    fn cpu(&self) -> Result<String, String> {
        const CAP_MILLI: u64 = 250;
        const WINDOW: Duration = Duration::from_millis(2000);
        let limits = Limits {
            cpu_milli: Some(CAP_MILLI),
            ..Limits::default()
        };
        if cpu_rate(CAP_MILLI, self.adapter.logical_cpus).is_none() {
            return Err("the machine is too small for the probe".into());
        }
        let spec = self.spec(&["cmd.exe", "/d", "/c", "for /l %i in () do @rem"]);
        self.in_scope("cpu", &limits, |scope| {
            let mut spawned = scope.spawn(&spec).map_err(|error| error.to_string())?;
            let started = Instant::now();
            std::thread::sleep(WINDOW);
            let usage = scope.usage().map_err(|error| error.to_string())?;
            let wall_ms = started.elapsed().as_millis() as u64;
            if let Some(exit) = spawned
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                return Err(format!("the busy loop ended early with {:?}", exit.code));
            }
            let cpu_ms = usage.cpu_time_ms;
            // Uncapped, the loop uses about one processor. The cap allows a
            // quarter of one; anything under 60% shows the cap is biting.
            if cpu_ms < 50 {
                return Err(format!("the busy loop used only {cpu_ms} ms of CPU"));
            }
            if cpu_ms * 10 > wall_ms * 6 {
                return Err(format!(
                    "a busy loop used {cpu_ms} ms of CPU in {wall_ms} ms under a \
                     {CAP_MILLI} mCPU hard cap"
                ));
            }
            Ok(format!(
                "a busy loop used {cpu_ms} ms of CPU in {wall_ms} ms under a \
                 {CAP_MILLI} mCPU hard cap"
            ))
        })
    }
}

fn is_image(process: &JobProcess, file_name: &str) -> bool {
    process
        .image
        .as_deref()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case(file_name))
}

/// Polls `probe` until it yields a value or `limit` passes.
fn wait_for<T>(
    limit: Duration,
    mut probe: impl FnMut() -> Result<Option<T>, String>,
) -> Result<Option<T>, String> {
    let started = Instant::now();
    loop {
        if let Some(value) = probe()? {
            return Ok(Some(value));
        }
        if started.elapsed() >= limit {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts `spec` and returns the root's exit code. Output is discarded.
fn run(scope: &mut JobScope, spec: &ProcessSpec) -> Result<i32, String> {
    let Spawned { mut child, .. } = scope.spawn(spec).map_err(|error| error.to_string())?;
    let exit = wait_for(SELF_TEST_STEP, || {
        child.try_wait().map_err(|error| error.to_string())
    })?
    .ok_or_else(|| format!("`{}` did not exit in time", spec.argv[0]))?;
    exit.code
        .ok_or_else(|| "the process reported no exit code".to_owned())
}

fn empty_and_destroy(mut scope: JobScope) -> Result<String, String> {
    scope.kill().map_err(|error| error.to_string())?;
    let emptied = wait_for(SELF_TEST_STEP, || {
        let members = scope.members().map_err(|error| error.to_string())?;
        Ok((members == Membership::Empty).then_some(()))
    })?;
    if emptied.is_none() {
        return Err("members were still alive after TerminateJobObject".into());
    }
    Box::new(scope).destroy().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(argv: &[&str]) -> String {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        command_line(&argv).unwrap()
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quote_arg("abc"), "abc");
        assert_eq!(quote_arg(r"C:\dir\file.txt"), r"C:\dir\file.txt");
        assert_eq!(quote_arg(r"trailing\"), r"trailing\");
        assert_eq!(quote_arg("a&b|c<d>e^f%PATH%"), "a&b|c<d>e^f%PATH%");
    }

    #[test]
    fn empty_and_whitespace_arguments_are_quoted() {
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg("a b"), r#""a b""#);
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
        assert_eq!(quote_arg("a\nb"), "\"a\nb\"");
    }

    #[test]
    fn quotes_and_backslashes_follow_the_msvc_rules() {
        // A quote is escaped with a backslash.
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        // Backslashes not followed by a quote stay as they are.
        assert_eq!(quote_arg(r"a\\b c"), r#""a\\b c""#);
        // Backslashes before a quote are doubled, plus one for the quote.
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg(r#"a\\"b"#), r#""a\\\\\"b""#);
        // Trailing backslashes are doubled only when the argument is quoted.
        assert_eq!(quote_arg(r"dir with space\"), r#""dir with space\\""#);
        assert_eq!(quote_arg(r"two at end\\"), r#""two at end\\\\""#);
        assert_eq!(quote_arg(r#"""#), r#""\"""#);
    }

    #[test]
    fn command_line_joins_quoted_arguments() {
        assert_eq!(
            line(&["tool.exe", "one", "two words", "", r#"q"q"#]),
            r#"tool.exe one "two words" "" "q\"q""#
        );
        assert_eq!(
            line(&[r"C:\Program Files\tool.exe", "x"]),
            r#""C:\Program Files\tool.exe" x"#
        );
    }

    #[test]
    fn command_line_rejects_what_cannot_be_represented() {
        let argv = |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).into()).collect() };
        assert!(command_line(&[]).is_err());
        assert!(command_line(&argv(&[""])).is_err());
        assert!(command_line(&argv(&["a\"b.exe"])).is_err());
        assert!(command_line(&argv(&["tool.exe", "nul\0inside"])).is_err());
        let huge = "x".repeat(MAX_COMMAND_LINE_UNITS);
        assert!(command_line(&argv(&["tool.exe", &huge])).is_err());
    }

    #[test]
    fn cmd_command_text_is_passed_verbatim() {
        assert_eq!(
            line(&["cmd.exe", "/d", "/c", r#"echo "a b" & exit 3"#]),
            r#"cmd.exe /d /s /c "echo "a b" & exit 3""#
        );
        // Several arguments after the switch are joined with spaces.
        assert_eq!(
            line(&["CMD", "/C", "echo", "hello world"]),
            r#"CMD /s /C "echo hello world""#
        );
        // An existing /s is not repeated.
        assert_eq!(
            line(&[r"C:\Windows\System32\cmd.exe", "/S", "/k", "ver"]),
            r#"C:\Windows\System32\cmd.exe /S /k "ver""#
        );
        // Without /c or /k the ordinary rules apply.
        assert_eq!(line(&["cmd.exe", "/?", "a b"]), r#"cmd.exe /? "a b""#);
        // Other programs named like a switch user are not special.
        assert_eq!(line(&["tool.exe", "/c", "a b"]), r#"tool.exe /c "a b""#);
    }

    #[test]
    fn environment_block_is_sorted_and_double_terminated() {
        let env = BTreeMap::from([
            ("b".to_owned(), "2".to_owned()),
            ("A".to_owned(), "1".to_owned()),
            ("C".to_owned(), "x=y".to_owned()),
        ]);
        let block = String::from_utf16(&environment_block(&env).unwrap()).unwrap();
        assert_eq!(block, "A=1\0b=2\0C=x=y\0\0");
        let empty = environment_block(&BTreeMap::new()).unwrap();
        assert_eq!(empty, [0, 0]);
    }

    #[test]
    fn environment_block_rejects_ambiguous_names() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        assert!(environment_block(&env(&[("", "x")])).is_err());
        assert!(environment_block(&env(&[("A=B", "x")])).is_err());
        assert!(environment_block(&env(&[("A", "x\0y")])).is_err());
        assert!(environment_block(&env(&[("Path", "x"), ("PATH", "y")])).is_err());
    }

    #[test]
    fn cpu_rate_is_a_share_of_the_whole_machine() {
        // One of four processors is 25.00%.
        assert_eq!(cpu_rate(1000, 4), Some(2500));
        assert_eq!(cpu_rate(250, 32), Some(78));
        // Never zero, which Windows rejects.
        assert_eq!(cpu_rate(1, 64), Some(1));
        // The whole machine or more needs no cap.
        assert_eq!(cpu_rate(4000, 4), None);
        assert_eq!(cpu_rate(9000, 4), None);
        assert_eq!(cpu_rate(3999, 4), Some(9997));
    }

    #[test]
    fn batch_files_are_recognised_despite_trailing_dots_and_spaces() {
        assert!(is_batch_file(Path::new(r"C:\x\build.bat")));
        assert!(is_batch_file(Path::new(r"C:\x\BUILD.CMD")));
        assert!(is_batch_file(Path::new(r"C:\x\build.cmd. .")));
        assert!(!is_batch_file(Path::new(r"C:\x\build.exe")));
        assert!(!is_batch_file(Path::new(r"C:\x\cmd")));
    }

    #[test]
    fn scope_names_are_restricted() {
        assert_eq!(
            object_name("attempt-1_a").unwrap(),
            r"Local\bella-attempt-1_a"
        );
        let too_long = "x".repeat(MAX_SCOPE_NAME_LEN + 1);
        for bad in ["", "Upper", r"a\b", "a b", "ä", too_long.as_str()] {
            assert!(object_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn zero_limits_are_rejected() {
        for limits in [
            Limits {
                cpu_milli: Some(0),
                ..Limits::default()
            },
            Limits {
                memory_bytes: Some(0),
                ..Limits::default()
            },
            Limits {
                max_processes: Some(0),
                ..Limits::default()
            },
        ] {
            assert!(validate_limits(&limits).is_err());
        }
    }
}
