//! Machine inventory and live telemetry.
//!
//! This crate answers two questions about the machine it runs on, for
//! Linux, macOS and Windows:
//!
//! * **What is it and what does it have?** [`hardware`], [`os_info`],
//!   [`detect_capabilities`] and [`inventory`] build a
//!   [`bella_core::machine::Inventory`]; [`boot_id`] names the current boot.
//! * **What is it doing?** [`Sampler`] produces
//!   [`bella_core::machine::Telemetry`].
//!
//! # Guarantees
//!
//! * **Read-only.** Nothing here changes `PATH`, the selected Xcode, or any
//!   installation. Tools are only asked for their version, from a neutral
//!   working directory, with standard input closed and with settings that
//!   stop them from installing or downloading anything as a side effect.
//!   What a tool writes to its own cache when asked for its version is
//!   outside this crate's control.
//! * **Bounded.** Every external command has a hard timeout
//!   ([`ProbeOptions::timeout`], 5 s by default) after which it is killed
//!   together with the processes it started. A probe that hangs or fails
//!   affects only its own capability.
//! * **Honest.** A tool that is present but cannot report a version, or
//!   whose daemon is down, is listed as `Unhealthy` with an `error`
//!   attribute; an absent tool is not listed. Telemetry carries a
//!   [`MeasurementQuality`](bella_core::machine::MeasurementQuality) that
//!   is `Good` only when every resource counter was read from the OS.
//!
//! # Capabilities
//!
//! | Name | Version | Attributes |
//! | --- | --- | --- |
//! | `git`, `cargo`, `node`, `npm`, `pnpm`, `yarn`, `bun`, `python`, `go`, `gradle`, `kotlin`, `cmake`, `ninja`, `make`, `gcc` | as printed by the tool | `path` |
//! | `git-lfs` | `git lfs version` | `path` when `git-lfs` is on `PATH` |
//! | `rust` | `rustc -vV` release | `targets` (comma separated), `host`, `path` |
//! | `jdk` (one per JDK) | `java -version`; `1.8.0_392` becomes `8.0.392` | `path` (the JDK home), `vendor`, `runtime`, `raw_version` |
//! | `swift`, `clang` | as printed | `target`, `path`; `vendor=apple` for Apple clang |
//! | `xcode` | `xcodebuild -version` | `build`, `sdks`, `path` (`xcode-select -p`) |
//! | `ios-simulator` (one per iOS runtime) | runtime version | `runtime`, `build` |
//! | `msvc` (one per Visual Studio with C++ tools) | `installationVersion` | `path`, `display_name`, `tools_version`, `prerelease` |
//! | `dotnet` | newest installed SDK | `sdks`, `path` |
//! | `docker` | client version; `Unhealthy` when `docker info` fails | `server_version`, `path` |
//! | `android-sdk` | platform-tools revision | `path`, `build_tools`, `platforms`, `ndk` |
//! | `gui-session` | none | `session` = `aqua`, `windows`, `x11` or `wayland` |
//!
//! `gui-session` means: on macOS, a user is logged in at the console
//! (`Unauthorized` when it is a different user than this process's); on
//! Windows, this process runs in an interactive session, which services and
//! SSH logins do not (`Unverified` when that session is disconnected); on
//! Linux, `WAYLAND_DISPLAY` or `DISPLAY` is set.
//!
//! # Sampling cost
//!
//! One sample reads a few OS counters and starts no process. The only
//! subprocess on the sampling path is macOS `pmset` for power and thermal
//! state, cached for [`Sampler::SLOW_REFRESH`]. The budget is 1 % of one
//! CPU at one sample per second, that is 10 ms of CPU time per sample.
//!
//! The ignored test `sampling_cost_of_100_samples` measures the CPU time
//! of this process and its children over 100 samples at a simulated 1 Hz.
//! Results from debug builds:
//!
//! | Host | CPU time per sample | Share of one CPU at 1 Hz |
//! | --- | --- | --- |
//! | macOS, Apple silicon (includes ten `pmset` refreshes) | 1.01 ms | 0.10 % |
//! | Windows 11, x86_64 (process times tick at 15.6 ms) | 0.47 ms | 0.05 % |
//! | Linux 6.12, aarch64, in a container | 0.03 ms | 0.003 % |

mod capabilities;
mod exec;
pub mod osparse;
pub mod parse;
mod sys;
mod telemetry;

pub use exec::ProbeOptions;
pub use telemetry::{CounterReport, CounterSource, ManagedUsage, Sampler};

use bella_core::BootId;
use bella_core::machine::{ExecutorInfo, Hardware, Inventory, OsInfo};
use bella_core::platform::{Arch, Capability, Os, Platform};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("bella-probe supports x86_64 and aarch64 only");

/// CPUs, memory, local disks and graphics adapters of this machine.
///
/// `logical_cpus` counts the CPUs of the host, not the share this process
/// may use under an affinity mask or a cgroup quota. Fields the OS does not
/// reveal are left empty rather than guessed. On macOS the graphics
/// adapters come from `system_profiler`, which runs under the default
/// probe timeout.
pub fn hardware() -> Hardware {
    sys::os::hardware()
}

/// Platform, OS version, kernel and host name.
///
/// * `platform` is [`Platform::current`].
/// * `os_version` is the macOS product version (`15.1`), the Windows
///   version with build (`10.0.26100`), or the Linux distribution's
///   `VERSION_ID` (`24.04`; `BUILD_ID` on rolling distributions).
/// * `kernel` is `Darwin <release>`, `Linux <release>`, or the Windows
///   version including the update revision (`10.0.26100.2314`).
/// * `hostname` is a display label, see [`hostname`].
pub fn os_info() -> OsInfo {
    OsInfo {
        platform: current_platform(),
        os_version: sys::os::os_version(),
        kernel: sys::os::kernel(),
        hostname: hostname(),
    }
}

/// The platform this crate was compiled for. The `compile_error!` guards
/// in this crate restrict it to combinations [`Platform::current`] knows.
fn current_platform() -> Platform {
    Platform::current().unwrap_or(Platform {
        os: if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        },
        arch: if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        },
    })
}

/// The machine's host name, for display only. It is never an identity:
/// two machines can share a name and a machine can be renamed.
pub fn hostname() -> String {
    sys::os::hostname()
}

/// An identifier of the current boot: the same on every call until the
/// machine restarts, different afterwards.
///
/// It is derived from a value the kernel fixes at boot:
///
/// * Linux: `/proc/sys/kernel/random/boot_id`.
/// * macOS: `kern.bootsessionuuid`, with `kern.boottime` as the fallback.
/// * Windows: the kernel's boot time with its clock-adjustment bias
///   removed, so setting the clock does not change it. A shutdown with
///   Fast Startup resumes the same kernel session and keeps the id.
///
/// If the OS refuses all of these, a fixed id is returned. That errs on
/// the safe side: an unchanged boot id never proves that old processes are
/// gone.
pub fn boot_id() -> BootId {
    match sys::os::boot_seed() {
        Some(seed) => BootId::derive(&seed),
        None => {
            tracing::warn!("the OS reported no boot identifier; using a fixed one");
            BootId::derive(b"bella-probe:unknown-boot")
        }
    }
}

/// Discover the toolchains, SDKs and runtimes on this machine.
///
/// Each capability has its exact version and a state; see the crate
/// documentation for the list. The result is sorted by name, then version,
/// so two passes over an unchanged machine return equal lists. Detection
/// takes as long as the slowest tool, bounded by `options.timeout` per
/// command.
pub fn detect_capabilities(options: &ProbeOptions) -> Vec<Capability> {
    capabilities::detect(options)
}

/// Probe this machine and build its inventory with default options.
///
/// `revision` is `previous.revision + 1` when anything differs from
/// `previous`, the same revision when nothing does, and 1 without a
/// previous inventory.
pub fn inventory(
    worker_version: &str,
    executors: Vec<ExecutorInfo>,
    previous: Option<&Inventory>,
) -> Inventory {
    inventory_with(
        &ProbeOptions::default(),
        worker_version,
        executors,
        previous,
    )
}

/// [`inventory`] with explicit probe options.
pub fn inventory_with(
    options: &ProbeOptions,
    worker_version: &str,
    executors: Vec<ExecutorInfo>,
    previous: Option<&Inventory>,
) -> Inventory {
    assemble_inventory(
        hardware(),
        os_info(),
        detect_capabilities(options),
        executors,
        worker_version,
        previous,
    )
}

/// Build an inventory from already collected parts and number it against
/// `previous`: the revision grows by one only when the content differs.
pub fn assemble_inventory(
    hardware: Hardware,
    os: OsInfo,
    capabilities: Vec<Capability>,
    executors: Vec<ExecutorInfo>,
    worker_version: &str,
    previous: Option<&Inventory>,
) -> Inventory {
    let mut inventory = Inventory {
        revision: previous.map_or(1, |previous| previous.revision),
        hardware,
        os,
        capabilities,
        executors,
        worker_version: worker_version.to_string(),
    };
    if previous.is_some_and(|previous| *previous != inventory) {
        inventory.revision = inventory.revision.saturating_add(1);
    }
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::machine::DiskInfo;
    use bella_core::platform::{CapabilityState, IsolationLevel};
    use bella_core::resources::EnforcementReport;
    use std::collections::BTreeMap;

    fn sample_hardware() -> Hardware {
        Hardware {
            logical_cpus: 8,
            physical_cpus: Some(4),
            cpu_model: "Test CPU".into(),
            memory_bytes: 16 << 30,
            disks: vec![DiskInfo {
                mount: "/".into(),
                total_bytes: 500 << 30,
                filesystem: "ext4".into(),
            }],
            gpus: vec![],
        }
    }

    fn sample_os() -> OsInfo {
        OsInfo {
            platform: current_platform(),
            os_version: "24.04".into(),
            kernel: "Linux 6.8.0".into(),
            hostname: "builder".into(),
        }
    }

    fn cap(name: &str, version: &str) -> Capability {
        Capability {
            name: name.into(),
            version: Some(version.into()),
            state: CapabilityState::Installed,
            attributes: BTreeMap::new(),
        }
    }

    fn executor() -> ExecutorInfo {
        ExecutorInfo {
            isolation: IsolationLevel::Native,
            mechanism: "process-group".into(),
            enforcement: EnforcementReport::none(),
        }
    }

    fn build(
        capabilities: Vec<Capability>,
        version: &str,
        previous: Option<&Inventory>,
    ) -> Inventory {
        assemble_inventory(
            sample_hardware(),
            sample_os(),
            capabilities,
            vec![executor()],
            version,
            previous,
        )
    }

    #[test]
    fn first_inventory_is_revision_one() {
        let first = build(vec![cap("rust", "1.89.0")], "0.1.0", None);
        assert_eq!(first.revision, 1);
        assert_eq!(first.worker_version, "0.1.0");
        assert_eq!(first.executors, vec![executor()]);
    }

    #[test]
    fn revision_stays_when_nothing_changed() {
        let first = build(vec![cap("rust", "1.89.0")], "0.1.0", None);
        let again = build(vec![cap("rust", "1.89.0")], "0.1.0", Some(&first));
        assert_eq!(again.revision, 1);
        assert_eq!(again, first);
        let third = build(vec![cap("rust", "1.89.0")], "0.1.0", Some(&again));
        assert_eq!(third.revision, 1);
    }

    #[test]
    fn revision_grows_by_one_per_change() {
        let first = build(vec![cap("rust", "1.89.0")], "0.1.0", None);

        let upgraded = build(vec![cap("rust", "1.90.0")], "0.1.0", Some(&first));
        assert_eq!(upgraded.revision, 2);

        let added = build(
            vec![cap("rust", "1.90.0"), cap("node", "22.11.0")],
            "0.1.0",
            Some(&upgraded),
        );
        assert_eq!(added.revision, 3);

        let new_worker = build(
            vec![cap("rust", "1.90.0"), cap("node", "22.11.0")],
            "0.2.0",
            Some(&added),
        );
        assert_eq!(new_worker.revision, 4);

        let mut unhealthy = cap("node", "22.11.0");
        unhealthy.state = CapabilityState::Unhealthy;
        let degraded = build(
            vec![cap("rust", "1.90.0"), unhealthy],
            "0.2.0",
            Some(&new_worker),
        );
        assert_eq!(degraded.revision, 5);

        let settled = build(degraded.capabilities.clone(), "0.2.0", Some(&degraded));
        assert_eq!(settled.revision, 5);
    }

    #[test]
    fn revision_continues_from_the_previous_number() {
        let mut old = build(vec![cap("rust", "1.89.0")], "0.1.0", None);
        old.revision = 41;
        let same = build(vec![cap("rust", "1.89.0")], "0.1.0", Some(&old));
        assert_eq!(same.revision, 41);
        let changed = build(vec![], "0.1.0", Some(&old));
        assert_eq!(changed.revision, 42);
    }

    #[test]
    fn hardware_and_executor_changes_count_as_changes() {
        let first = build(vec![], "0.1.0", None);
        let mut more_memory = sample_hardware();
        more_memory.memory_bytes = 32 << 30;
        let grown = assemble_inventory(
            more_memory,
            sample_os(),
            vec![],
            vec![executor()],
            "0.1.0",
            Some(&first),
        );
        assert_eq!(grown.revision, 2);
        let no_executor = assemble_inventory(
            sample_hardware(),
            sample_os(),
            vec![],
            vec![],
            "0.1.0",
            Some(&first),
        );
        assert_eq!(no_executor.revision, 2);
    }
}
