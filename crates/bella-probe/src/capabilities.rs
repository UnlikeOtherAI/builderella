//! Toolchain and runtime discovery.
//!
//! Each detector locates a tool, asks it for its version through
//! [`Runner`], and turns the answer into [`Capability`] entries with the
//! parsers in [`crate::parse`]. Detectors only read: nothing is installed,
//! selected, or added to `PATH`.

use crate::exec::{CommandOutput, ProbeOptions, Runner};
use crate::parse;
use crate::sys;
use bella_core::platform::{Capability, CapabilityState};
use bella_core::version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Longest diagnostic kept in an `error` attribute.
const MAX_ERROR_CHARS: usize = 200;

/// Detectors run at most this many at a time, so a pass does not start
/// dozens of tools at once on a machine that is busy building.
const MAX_PARALLEL_PROBES: usize = 8;

/// Shared state of one detection pass.
pub(crate) struct Context {
    pub runner: Runner,
    /// macOS: the active developer directory, looked up once.
    developer_dir: OnceLock<Option<String>>,
}

impl Context {
    pub fn new(runner: Runner) -> Self {
        Self {
            runner,
            developer_dir: OnceLock::new(),
        }
    }

    /// macOS: the directory printed by `xcode-select -p`, when one is
    /// selected and exists. `None` on other systems.
    fn developer_dir(&self) -> Option<&str> {
        self.developer_dir
            .get_or_init(|| {
                if !cfg!(target_os = "macos") {
                    return None;
                }
                let output = self
                    .runner
                    .run(Path::new("/usr/bin/xcode-select"), &["-p"])
                    .ok()?;
                let dir = output.stdout.trim();
                (output.success && Path::new(dir).is_dir()).then(|| dir.to_string())
            })
            .as_deref()
    }

    /// Whether `program` is one of the macOS `/usr/bin` developer-tool
    /// stubs while no developer directory is installed. Running such a
    /// stub would open the "install the command line developer tools"
    /// dialog, so it counts as an absent tool instead.
    fn is_unusable_macos_stub(&self, program: &Path) -> bool {
        cfg!(target_os = "macos")
            && program.parent() == Some(Path::new("/usr/bin"))
            && program
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| MACOS_DEVELOPER_STUBS.contains(&name))
            && self.developer_dir().is_none()
    }
}

/// `/usr/bin` commands that macOS forwards to the selected developer
/// directory.
const MACOS_DEVELOPER_STUBS: &[&str] = &[
    "git",
    "clang",
    "gcc",
    "make",
    "swift",
    "python3",
    "xcodebuild",
    "xcrun",
];

type Detector = fn(&Context) -> Vec<Capability>;

const DETECTORS: &[Detector] = &[
    detect_git,
    detect_git_lfs,
    detect_rust,
    detect_cargo,
    detect_node,
    detect_npm,
    detect_pnpm,
    detect_yarn,
    detect_bun,
    detect_python,
    detect_go,
    detect_jdk,
    detect_gradle,
    detect_kotlin,
    detect_swift,
    detect_xcode,
    detect_msvc,
    detect_dotnet,
    detect_cmake,
    detect_ninja,
    detect_make,
    detect_clang,
    detect_gcc,
    detect_docker,
    detect_android_sdk,
    detect_gui_session,
];

/// Run every detector and return the capabilities in a stable order.
pub(crate) fn detect(options: &ProbeOptions) -> Vec<Capability> {
    let context = Context::new(Runner::new(options));
    let mut found = run_detectors(&context, DETECTORS);
    sort(&mut found);
    found
}

/// Run `detectors` on a small pool of threads. A detector that panics
/// loses only its own entries.
fn run_detectors(context: &Context, detectors: &[Detector]) -> Vec<Capability> {
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    let work = || {
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(detector) = detectors.get(index) else {
                break;
            };
            let run = std::panic::AssertUnwindSafe(|| detector(context));
            match std::panic::catch_unwind(run) {
                Ok(capabilities) => {
                    if let Ok(mut held) = found.lock() {
                        held.extend(capabilities);
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        index,
                        "a capability detector panicked; its entries are missing"
                    );
                }
            }
        }
    };
    let workers = detectors.len().min(MAX_PARALLEL_PROBES);
    std::thread::scope(|scope| {
        for _ in 1..workers {
            // A helper that cannot be started is not needed: the calling
            // thread below drains whatever is left.
            let _ = std::thread::Builder::new()
                .name("bella-probe-detect".into())
                .spawn_scoped(scope, work);
        }
        work();
    });
    found
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Order by name, then version (newest last), then install path.
fn sort(capabilities: &mut [Capability]) {
    capabilities.sort_by(|a, b| {
        let version = |c: &Capability| c.version.clone().unwrap_or_default();
        let path = |c: &Capability| c.attributes.get("path").cloned().unwrap_or_default();
        a.name
            .cmp(&b.name)
            .then_with(|| version::compare(&version(a), &version(b)))
            .then_with(|| version(a).cmp(&version(b)))
            .then_with(|| path(a).cmp(&path(b)))
    });
}

fn capability(name: &str, version: Option<String>, state: CapabilityState) -> Capability {
    Capability {
        name: name.to_string(),
        version,
        state,
        attributes: BTreeMap::new(),
    }
}

fn installed(name: &str, version: String) -> Capability {
    capability(name, Some(version), CapabilityState::Installed)
}

/// A tool that is present but did not prove usable, with the reason.
fn unhealthy(name: &str, version: Option<String>, reason: &str) -> Capability {
    let mut entry = capability(name, version, CapabilityState::Unhealthy);
    set(&mut entry, "error", reason);
    entry
}

fn set(entry: &mut Capability, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let value = if key == "error" {
        let line = value
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(value);
        line.trim().chars().take(MAX_ERROR_CHARS).collect()
    } else {
        value.to_string()
    };
    entry.attributes.insert(key.to_string(), value);
}

fn set_path(entry: &mut Capability, path: &Path) {
    set(entry, "path", &path.to_string_lossy());
}

/// What the output of a failed or unparsable command says went wrong.
fn failure_reason(output: &CommandOutput) -> String {
    let text = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    match text.lines().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim().to_string(),
        None if output.success => "no version in the output".to_string(),
        None => output.status_text(),
    }
}

/// A program that exists by name but stands for "not installed": the
/// Windows app execution alias for a Store app that is not installed.
fn is_absent_alias(program: &Path) -> bool {
    cfg!(windows)
        && program
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case("WindowsApps"))
}

/// Detect one command-line tool.
///
/// The first of `programs` found on the search path is run with `args`.
/// `parse` extracts the version from its output. The result is `Installed`
/// when the command succeeded and printed a version, `Unhealthy` (with an
/// `error` attribute) when it is present but failed, hung or printed no
/// version, and empty when no program was found.
fn tool(
    context: &Context,
    name: &str,
    programs: &[&str],
    args: &[&str],
    parse: impl Fn(&CommandOutput) -> Option<String>,
) -> Vec<Capability> {
    for program in programs {
        let Some(path) = context.runner.find(program) else {
            continue;
        };
        if context.is_unusable_macos_stub(&path) {
            continue;
        }
        let mut entry = match context.runner.run(&path, args) {
            Ok(output) => match parse(&output) {
                Some(version) if output.success => installed(name, version),
                Some(version) => unhealthy(name, Some(version), &output.status_text()),
                None if is_absent_alias(&path) => continue,
                None => unhealthy(name, None, &failure_reason(&output)),
            },
            Err(failure) => unhealthy(name, None, &failure.to_string()),
        };
        set_path(&mut entry, &path);
        return vec![entry];
    }
    Vec::new()
}

fn detect_git(context: &Context) -> Vec<Capability> {
    tool(context, "git", &["git"], &["--version"], |o| {
        parse::git(&o.stdout)
    })
}

/// Git LFS is asked through `git lfs`, because installers such as Git for
/// Windows keep `git-lfs` in git's own directory rather than on `PATH`.
fn detect_git_lfs(context: &Context) -> Vec<Capability> {
    let Some(git) = context.runner.find("git") else {
        return Vec::new();
    };
    if context.is_unusable_macos_stub(&git) {
        return Vec::new();
    }
    let standalone = context.runner.find("git-lfs");
    match context.runner.run(&git, &["lfs", "version"]) {
        Ok(output) => match parse::git_lfs(&output.stdout) {
            Some(version) => {
                let mut entry = installed("git-lfs", version);
                if let Some(path) = &standalone {
                    set_path(&mut entry, path);
                }
                vec![entry]
            }
            // "git: 'lfs' is not a git command": not installed.
            None if standalone.is_none() => Vec::new(),
            None => {
                let mut entry = unhealthy("git-lfs", None, &failure_reason(&output));
                if let Some(path) = &standalone {
                    set_path(&mut entry, path);
                }
                vec![entry]
            }
        },
        Err(failure) => vec![unhealthy("git-lfs", None, &failure.to_string())],
    }
}

fn detect_rust(context: &Context) -> Vec<Capability> {
    let Some(rustc) = context.runner.find("rustc") else {
        return Vec::new();
    };
    let output = match context.runner.run(&rustc, &["-vV"]) {
        Ok(output) => output,
        Err(failure) => {
            let mut entry = unhealthy("rust", None, &failure.to_string());
            set_path(&mut entry, &rustc);
            return vec![entry];
        }
    };
    let Some(info) = parse::rustc_verbose(&output.stdout).filter(|_| output.success) else {
        let mut entry = unhealthy("rust", None, &failure_reason(&output));
        set_path(&mut entry, &rustc);
        return vec![entry];
    };
    let mut entry = installed("rust", info.version);
    set_path(&mut entry, &rustc);
    let targets = rust_targets(context, &rustc, info.host.as_deref());
    set(&mut entry, "targets", &targets.join(","));
    if let Some(host) = &info.host {
        set(&mut entry, "host", host);
    }
    vec![entry]
}

/// Targets whose standard library is installed for the `rustc` on `PATH`.
///
/// When that compiler belongs to a rustup toolchain, rustup is asked with
/// `rustup target list --installed` for exactly that toolchain. A compiler
/// from elsewhere (a distribution package, Homebrew) is not described by
/// rustup, so its own sysroot is listed instead. The host triple is the
/// last resort.
fn rust_targets(context: &Context, rustc: &Path, host: Option<&str>) -> Vec<String> {
    let sysroot = context
        .runner
        .run(rustc, &["--print", "sysroot"])
        .ok()
        .filter(|output| output.success)
        .map(|output| PathBuf::from(output.stdout.trim()))
        .filter(|path| path.is_dir());
    if let (Some(sysroot), Some(rustup)) = (&sysroot, context.runner.find("rustup")) {
        let in_rustup_home = sysroot
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == "toolchains");
        let toolchain = sysroot.file_name().and_then(|name| name.to_str());
        if let (true, Some(toolchain)) = (in_rustup_home, toolchain) {
            let listed = context
                .runner
                .run(
                    &rustup,
                    &["target", "list", "--installed", "--toolchain", toolchain],
                )
                .ok()
                .filter(|output| output.success)
                .map(|output| parse::rustup_targets(&output.stdout))
                .unwrap_or_default();
            if !listed.is_empty() {
                return listed;
            }
        }
    }
    let from_sysroot = sysroot
        .map(|sysroot| sysroot_targets(&sysroot))
        .unwrap_or_default();
    if !from_sysroot.is_empty() {
        return from_sysroot;
    }
    host.map(|host| vec![host.to_string()]).unwrap_or_default()
}

/// Target directories under `<sysroot>/lib/rustlib` that hold a standard
/// library.
fn sysroot_targets(sysroot: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sysroot.join("lib").join("rustlib")) else {
        return Vec::new();
    };
    let mut targets: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().join("lib").is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.contains('-'))
        .collect();
    targets.sort();
    targets
}

fn detect_cargo(context: &Context) -> Vec<Capability> {
    tool(context, "cargo", &["cargo"], &["--version"], |o| {
        parse::cargo(&o.stdout)
    })
}

fn detect_node(context: &Context) -> Vec<Capability> {
    tool(context, "node", &["node"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_npm(context: &Context) -> Vec<Capability> {
    tool(context, "npm", &["npm"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_pnpm(context: &Context) -> Vec<Capability> {
    tool(context, "pnpm", &["pnpm"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_yarn(context: &Context) -> Vec<Capability> {
    tool(context, "yarn", &["yarn"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_bun(context: &Context) -> Vec<Capability> {
    tool(context, "bun", &["bun"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_python(context: &Context) -> Vec<Capability> {
    tool(
        context,
        "python",
        &["python3", "python"],
        &["--version"],
        |o| parse::python(&o.combined()),
    )
}

fn detect_go(context: &Context) -> Vec<Capability> {
    tool(context, "go", &["go"], &["version"], |o| {
        parse::go(&o.stdout)
    })
}

/// One entry per JDK: `JAVA_HOME`, the homes macOS `java_home -V` lists,
/// and the `java` on `PATH`. The same home reached twice is listed once.
fn detect_jdk(context: &Context) -> Vec<Capability> {
    let java_name = if cfg!(windows) { "java.exe" } else { "java" };
    let mut found: Vec<Capability> = Vec::new();
    let mut seen_homes: Vec<PathBuf> = Vec::new();
    let mut seen_binaries: Vec<PathBuf> = Vec::new();

    let mut candidates: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    if let Some(home) = std::env::var_os("JAVA_HOME").filter(|home| !home.is_empty()) {
        let home = PathBuf::from(home);
        let binary = home.join("bin").join(java_name);
        if binary.is_file() {
            candidates.push((binary, Some(home)));
        } else {
            let mut entry = unhealthy("jdk", None, "JAVA_HOME has no bin/java");
            set_path(&mut entry, &home);
            found.push(entry);
        }
    }
    if cfg!(target_os = "macos") {
        // `java_home -V` prints its list on standard error and never asks
        // to install anything.
        if let Ok(output) = context
            .runner
            .run(Path::new("/usr/libexec/java_home"), &["-V"])
        {
            for home in parse::java_home_list(&output.combined()) {
                let home = PathBuf::from(home);
                candidates.push((home.join("bin").join(java_name), Some(home)));
            }
        }
    }
    if let Some(binary) = context.runner.find("java") {
        // On macOS `/usr/bin/java` only forwards to a home that
        // `java_home` already listed, and without one it may prompt.
        let macos_stub = cfg!(target_os = "macos") && binary == Path::new("/usr/bin/java");
        if !macos_stub {
            candidates.push((binary, None));
        }
    }

    for (binary, declared_home) in candidates {
        let resolved = std::fs::canonicalize(&binary).unwrap_or_else(|_| binary.clone());
        if seen_binaries.contains(&resolved) {
            continue;
        }
        seen_binaries.push(resolved);
        let Some((mut entry, reported_home)) = probe_java(context, &binary) else {
            continue;
        };
        let home = reported_home.map(PathBuf::from).or(declared_home);
        if let Some(home) = home {
            let canonical = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
            if seen_homes.contains(&canonical) {
                continue;
            }
            seen_homes.push(canonical);
            set_path(&mut entry, &home);
        } else {
            set_path(&mut entry, &binary);
        }
        found.push(entry);
    }
    found
}

/// Run one `java` binary. Returns the capability and the `java.home` it
/// reported, or `None` when the binary is a launcher with no runtime
/// behind it.
fn probe_java(context: &Context, binary: &Path) -> Option<(Capability, Option<String>)> {
    // `java -version` prints on standard error. The settings flag adds
    // `java.home` and the vendor; runtimes that reject it are asked again
    // without it.
    let attempts: [&[&str]; 2] = [&["-XshowSettings:properties", "-version"], &["-version"]];
    let mut last_failure = String::new();
    for args in attempts {
        match context.runner.run(binary, args) {
            Ok(output) => {
                let text = output.combined();
                if text.contains("Unable to locate a Java Runtime") {
                    return None;
                }
                if let Some(java) = parse::java(&text) {
                    let mut entry = if output.success {
                        installed("jdk", java.version)
                    } else {
                        unhealthy("jdk", Some(java.version), &output.status_text())
                    };
                    if java.raw_version != entry.version.clone().unwrap_or_default() {
                        set(&mut entry, "raw_version", &java.raw_version);
                    }
                    if let Some(vendor) = &java.vendor {
                        set(&mut entry, "vendor", vendor);
                    }
                    if let Some(runtime) = &java.runtime {
                        set(&mut entry, "runtime", runtime);
                    }
                    return Some((entry, java.home));
                }
                if is_absent_alias(binary) {
                    return None;
                }
                last_failure = failure_reason(&output);
            }
            Err(failure) => {
                last_failure = failure.to_string();
                break;
            }
        }
    }
    Some((unhealthy("jdk", None, &last_failure), None))
}

fn detect_gradle(context: &Context) -> Vec<Capability> {
    tool(context, "gradle", &["gradle"], &["--version"], |o| {
        parse::gradle(&o.stdout)
    })
}

fn detect_kotlin(context: &Context) -> Vec<Capability> {
    tool(
        context,
        "kotlin",
        &["kotlinc", "kotlin"],
        &["-version"],
        |o| parse::kotlin(&o.combined()),
    )
}

fn detect_swift(context: &Context) -> Vec<Capability> {
    let Some(swift) = context.runner.find("swift") else {
        return Vec::new();
    };
    if context.is_unusable_macos_stub(&swift) {
        return Vec::new();
    }
    let mut entry = match context.runner.run(&swift, &["--version"]) {
        Ok(output) => {
            // Depending on the release the version line is on either stream.
            let text = output.combined();
            match parse::swift(&text) {
                Some(version) => {
                    let mut entry = if output.success {
                        installed("swift", version)
                    } else {
                        unhealthy("swift", Some(version), &output.status_text())
                    };
                    if let Some(target) = parse::target_line(&text) {
                        set(&mut entry, "target", &target);
                    }
                    entry
                }
                None => unhealthy("swift", None, &failure_reason(&output)),
            }
        }
        Err(failure) => unhealthy("swift", None, &failure.to_string()),
    };
    set_path(&mut entry, &swift);
    vec![entry]
}

/// Xcode itself and, when it works, one `ios-simulator` entry per iOS
/// simulator runtime.
fn detect_xcode(context: &Context) -> Vec<Capability> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(developer_dir) = context.developer_dir() else {
        return Vec::new();
    };
    let Some(xcodebuild) = context.runner.find("xcodebuild") else {
        return Vec::new();
    };
    let output = match context.runner.run(&xcodebuild, &["-version"]) {
        Ok(output) => output,
        Err(failure) => {
            let mut entry = unhealthy("xcode", None, &failure.to_string());
            set(&mut entry, "path", developer_dir);
            return vec![entry];
        }
    };
    let Some(xcode) = parse::xcodebuild_version(&output.stdout) else {
        // Only the command line tools are installed: there is no Xcode.
        if output.combined().contains("requires Xcode") {
            return Vec::new();
        }
        let mut entry = unhealthy("xcode", None, &failure_reason(&output));
        set(&mut entry, "path", developer_dir);
        return vec![entry];
    };
    let mut entry = if output.success {
        installed("xcode", xcode.version)
    } else {
        unhealthy("xcode", Some(xcode.version), &output.status_text())
    };
    set(&mut entry, "path", developer_dir);
    if let Some(build) = &xcode.build {
        set(&mut entry, "build", build);
    }
    match context.runner.run(&xcodebuild, &["-showsdks"]) {
        Ok(sdks) if sdks.success => {
            set(
                &mut entry,
                "sdks",
                &parse::xcode_sdks(&sdks.stdout).join(","),
            );
        }
        Ok(sdks) => {
            // A version without SDKs usually means the licence was not
            // accepted or first-launch setup has not run.
            entry.state = CapabilityState::Unhealthy;
            set(&mut entry, "error", &failure_reason(&sdks));
        }
        Err(failure) => {
            entry.state = CapabilityState::Unhealthy;
            set(&mut entry, "error", &failure.to_string());
        }
    }
    let usable = entry.state == CapabilityState::Installed;
    let mut found = vec![entry];
    if usable {
        found.extend(detect_ios_simulators(context));
    }
    found
}

fn detect_ios_simulators(context: &Context) -> Vec<Capability> {
    let Some(xcrun) = context.runner.find("xcrun") else {
        return Vec::new();
    };
    let output = match context
        .runner
        .run(&xcrun, &["simctl", "list", "runtimes", "--json"])
    {
        Ok(output) => output,
        Err(failure) => return vec![unhealthy("ios-simulator", None, &failure.to_string())],
    };
    let Some(runtimes) = parse::simctl_runtimes(&output.stdout).filter(|_| output.success) else {
        return vec![unhealthy("ios-simulator", None, &failure_reason(&output))];
    };
    runtimes
        .into_iter()
        .filter(|runtime| runtime.platform == "iOS" && !runtime.version.is_empty())
        .map(|runtime| {
            let mut entry = if runtime.available {
                installed("ios-simulator", runtime.version)
            } else {
                unhealthy(
                    "ios-simulator",
                    Some(runtime.version),
                    "the runtime is installed but not available to this Xcode",
                )
            };
            set(&mut entry, "runtime", &runtime.identifier);
            set(&mut entry, "build", &runtime.build);
            entry
        })
        .collect()
}

/// Visual Studio installations with the C++ toolset, through `vswhere`.
fn detect_msvc(context: &Context) -> Vec<Capability> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let program_files = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let vswhere = program_files
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.is_file() {
        return Vec::new();
    }
    let args = [
        "-all",
        "-prerelease",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "Microsoft.VisualStudio.Component.VC.Tools.ARM64",
        "-requiresAny",
        "-format",
        "json",
        "-utf8",
        "-nologo",
    ];
    let output = match context.runner.run(&vswhere, &args) {
        Ok(output) => output,
        Err(failure) => return vec![unhealthy("msvc", None, &failure.to_string())],
    };
    let Some(instances) = parse::vswhere(&output.stdout).filter(|_| output.success) else {
        return vec![unhealthy("msvc", None, &failure_reason(&output))];
    };
    instances
        .into_iter()
        .filter(|instance| !instance.version.is_empty())
        .map(|instance| {
            let mut entry = if instance.complete {
                installed("msvc", instance.version)
            } else {
                unhealthy(
                    "msvc",
                    Some(instance.version),
                    "the installation is incomplete",
                )
            };
            set(&mut entry, "path", &instance.path);
            if let Some(name) = &instance.display_name {
                set(&mut entry, "display_name", name);
            }
            if instance.prerelease {
                set(&mut entry, "prerelease", "true");
            }
            let toolset = Path::new(&instance.path)
                .join("VC")
                .join("Auxiliary")
                .join("Build")
                .join("Microsoft.VCToolsVersion.default.txt");
            if let Ok(version) = std::fs::read_to_string(toolset) {
                set(&mut entry, "tools_version", &version);
            }
            entry
        })
        .collect()
}

/// The newest installed .NET SDK. `--list-sdks` is answered by the host
/// executable itself, so it does not trigger the SDK's first-run setup.
fn detect_dotnet(context: &Context) -> Vec<Capability> {
    let Some(dotnet) = context.runner.find("dotnet") else {
        return Vec::new();
    };
    let mut entry = match context.runner.run(&dotnet, &["--list-sdks"]) {
        Ok(output) => {
            let sdks = parse::dotnet_sdks(&output.stdout);
            let newest = sdks
                .iter()
                .max_by(|a, b| version::compare(a, b).then_with(|| a.cmp(b)))
                .cloned();
            match newest {
                Some(newest) if output.success => {
                    let mut entry = installed("dotnet", newest);
                    set(&mut entry, "sdks", &sdks.join(","));
                    entry
                }
                Some(newest) => unhealthy("dotnet", Some(newest), &output.status_text()),
                None if output.success => {
                    unhealthy("dotnet", None, "no SDK is installed, only the runtime host")
                }
                None => unhealthy("dotnet", None, &failure_reason(&output)),
            }
        }
        Err(failure) => unhealthy("dotnet", None, &failure.to_string()),
    };
    set_path(&mut entry, &dotnet);
    vec![entry]
}

fn detect_cmake(context: &Context) -> Vec<Capability> {
    tool(context, "cmake", &["cmake"], &["--version"], |o| {
        parse::cmake(&o.stdout)
    })
}

fn detect_ninja(context: &Context) -> Vec<Capability> {
    tool(context, "ninja", &["ninja"], &["--version"], |o| {
        parse::bare(&o.stdout)
    })
}

fn detect_make(context: &Context) -> Vec<Capability> {
    tool(context, "make", &["make", "gmake"], &["--version"], |o| {
        parse::make(&o.stdout)
    })
}

fn detect_clang(context: &Context) -> Vec<Capability> {
    let Some(clang) = context.runner.find("clang") else {
        return Vec::new();
    };
    if context.is_unusable_macos_stub(&clang) {
        return Vec::new();
    }
    let mut entry = match context.runner.run(&clang, &["--version"]) {
        Ok(output) => match parse::clang(&output.stdout) {
            Some(info) => {
                let mut entry = if output.success {
                    installed("clang", info.version)
                } else {
                    unhealthy("clang", Some(info.version), &output.status_text())
                };
                if let Some(target) = &info.target {
                    set(&mut entry, "target", target);
                }
                if info.apple {
                    set(&mut entry, "vendor", "apple");
                }
                entry
            }
            None => unhealthy("clang", None, &failure_reason(&output)),
        },
        Err(failure) => unhealthy("clang", None, &failure.to_string()),
    };
    set_path(&mut entry, &clang);
    vec![entry]
}

/// GCC proper. A `gcc` command that is clang under another name (macOS)
/// is not listed.
fn detect_gcc(context: &Context) -> Vec<Capability> {
    let Some(gcc) = context.runner.find("gcc") else {
        return Vec::new();
    };
    if context.is_unusable_macos_stub(&gcc) {
        return Vec::new();
    }
    let mut entry = match context.runner.run(&gcc, &["--version"]) {
        Ok(output) if output.combined().contains("clang") => return Vec::new(),
        Ok(output) => match parse::gcc(&output.stdout) {
            Some(version) if output.success => installed("gcc", version),
            Some(version) => unhealthy("gcc", Some(version), &output.status_text()),
            None => unhealthy("gcc", None, &failure_reason(&output)),
        },
        Err(failure) => unhealthy("gcc", None, &failure.to_string()),
    };
    set_path(&mut entry, &gcc);
    vec![entry]
}

/// The Docker client version; `Unhealthy` when `docker info` cannot reach
/// a daemon.
fn detect_docker(context: &Context) -> Vec<Capability> {
    let mut found = tool(context, "docker", &["docker"], &["--version"], |o| {
        parse::docker_client(&o.stdout)
    });
    let Some(entry) = found
        .first_mut()
        .filter(|entry| entry.state == CapabilityState::Installed)
    else {
        return found;
    };
    let Some(docker) = entry.attributes.get("path").map(PathBuf::from) else {
        return found;
    };
    match context
        .runner
        .run(&docker, &["info", "--format", "{{.ServerVersion}}"])
    {
        Ok(output) if output.success && !output.stdout.trim().is_empty() => {
            set(entry, "server_version", &output.stdout);
        }
        Ok(output) => {
            entry.state = CapabilityState::Unhealthy;
            let reason = failure_reason(&output);
            set(entry, "error", &format!("docker info failed: {reason}"));
        }
        Err(failure) => {
            entry.state = CapabilityState::Unhealthy;
            set(entry, "error", &format!("docker info: {failure}"));
        }
    }
    found
}

/// The Android SDK named by `ANDROID_HOME` or `ANDROID_SDK_ROOT`.
fn detect_android_sdk(_context: &Context) -> Vec<Capability> {
    let root = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from);
    match root {
        Some(root) => vec![android_sdk_at(&root)],
        None => Vec::new(),
    }
}

/// Describe the Android SDK at `root`.
///
/// The SDK has no version of its own, so the version is the revision of
/// its platform-tools package, or of the command-line tools when
/// platform-tools is not installed.
pub(crate) fn android_sdk_at(root: &Path) -> Capability {
    if !root.is_dir() {
        let mut entry = unhealthy("android-sdk", None, "the SDK directory does not exist");
        set_path(&mut entry, root);
        return entry;
    }
    let revision = [
        root.join("platform-tools"),
        root.join("cmdline-tools").join("latest"),
    ]
    .iter()
    .filter_map(|package| std::fs::read_to_string(package.join("source.properties")).ok())
    .find_map(|text| parse::source_properties_revision(&text));
    let mut entry = match revision {
        Some(revision) => installed("android-sdk", revision),
        None => unhealthy(
            "android-sdk",
            None,
            "neither platform-tools nor cmdline-tools reports a revision",
        ),
    };
    set_path(&mut entry, root);
    for (attribute, directory) in [
        ("build_tools", "build-tools"),
        ("platforms", "platforms"),
        ("ndk", "ndk"),
    ] {
        let mut names = subdirectories(&root.join(directory));
        names.sort_by(|a, b| version::compare(a, b).then_with(|| a.cmp(b)));
        set(&mut entry, attribute, &names.join(","));
    }
    entry
}

fn subdirectories(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

/// A graphical session programs started by this process could use. It has
/// no version; the `session` attribute names its kind.
fn detect_gui_session(_context: &Context) -> Vec<Capability> {
    match sys::os::gui_session() {
        Some(session) => {
            let mut entry = capability("gui-session", None, session.state);
            set(&mut entry, "session", session.kind);
            vec![entry]
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A directory with one fake tool that runs `body` as a shell script.
    ///
    /// The timeout is generous: the first run of a freshly written script
    /// can take over a second on a busy or older machine.
    fn fake_tool(name: &str, body: &str) -> (tempfile::TempDir, Context) {
        fake_tool_with_timeout(name, body, Duration::from_secs(20))
    }

    fn fake_tool_with_timeout(
        name: &str,
        body: &str,
        timeout: Duration,
    ) -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        if cfg!(windows) {
            let script = format!("@echo off\r\n{body}\r\n");
            std::fs::write(dir.path().join(format!("{name}.cmd")), script).unwrap();
        } else {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let runner = Runner::with_dirs(timeout, vec![dir.path().to_path_buf()]);
        (dir, Context::new(runner))
    }

    fn probe(context: &Context) -> Vec<Capability> {
        tool(context, "faketool", &["faketool"], &["--version"], |o| {
            parse::cmake(&o.stdout)
        })
    }

    #[test]
    fn a_tool_that_reports_its_version_is_installed() {
        let (dir, context) = fake_tool("faketool", "echo cmake version 3.30.5");
        let found = probe(&context);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "faketool");
        assert_eq!(found[0].version.as_deref(), Some("3.30.5"));
        assert_eq!(found[0].state, CapabilityState::Installed);
        let path = PathBuf::from(&found[0].attributes["path"]);
        assert_eq!(path.parent(), Some(dir.path()));
        assert!(!found[0].attributes.contains_key("error"));
    }

    #[test]
    fn an_absent_tool_is_not_listed() {
        let (_dir, context) = fake_tool("othertool", "echo cmake version 3.30.5");
        assert!(probe(&context).is_empty());
    }

    #[test]
    fn a_tool_without_a_version_is_unhealthy() {
        let (_dir, context) = fake_tool("faketool", "echo something went wrong 1>&2\nexit 3");
        let found = probe(&context);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, CapabilityState::Unhealthy);
        assert_eq!(found[0].version, None);
        assert_eq!(found[0].attributes["error"], "something went wrong");
    }

    #[test]
    fn a_version_from_a_failing_command_is_unhealthy_but_kept() {
        let (_dir, context) = fake_tool("faketool", "echo cmake version 3.30.5\nexit 2");
        let found = probe(&context);
        assert_eq!(found[0].state, CapabilityState::Unhealthy);
        assert_eq!(found[0].version.as_deref(), Some("3.30.5"));
        assert_eq!(found[0].attributes["error"], "exit status 2");
    }

    #[test]
    fn a_hung_tool_is_unhealthy_within_the_timeout() {
        let hang = if cfg!(windows) {
            "ping -n 60 127.0.0.1 >NUL"
        } else {
            "sleep 60"
        };
        let (_dir, context) = fake_tool_with_timeout("faketool", hang, Duration::from_millis(1500));
        let started = Instant::now();
        let found = probe(&context);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, CapabilityState::Unhealthy);
        assert!(found[0].attributes["error"].contains("timed out"));
    }

    /// One detector failing, hanging or panicking leaves the others' entries
    /// intact.
    #[test]
    fn a_bad_detector_does_not_lose_the_others() {
        fn good(_: &Context) -> Vec<Capability> {
            vec![installed("good", "1.0".into())]
        }
        fn broken(context: &Context) -> Vec<Capability> {
            tool(context, "faketool", &["faketool"], &[], |o| {
                parse::cmake(&o.stdout)
            })
        }
        fn panics(_: &Context) -> Vec<Capability> {
            panic!("detector failure under test")
        }
        let (_dir, context) = fake_tool("faketool", "exit 1");
        let mut found = run_detectors(&context, &[good, broken, panics, good]);
        sort(&mut found);
        let names: Vec<(&str, CapabilityState)> =
            found.iter().map(|c| (c.name.as_str(), c.state)).collect();
        assert_eq!(
            names,
            [
                ("faketool", CapabilityState::Unhealthy),
                ("good", CapabilityState::Installed),
                ("good", CapabilityState::Installed),
            ]
        );
    }

    #[test]
    fn capabilities_sort_by_name_then_numeric_version() {
        let mut list = vec![
            installed("jdk", "21.0.4".into()),
            installed("ios-simulator", "18.1".into()),
            installed("jdk", "8.0.392".into()),
            installed("ios-simulator", "9.3".into()),
            capability("gui-session", None, CapabilityState::Installed),
            installed("jdk", "17.0.12".into()),
        ];
        sort(&mut list);
        let order: Vec<String> = list
            .iter()
            .map(|c| format!("{} {}", c.name, c.version.as_deref().unwrap_or("-")))
            .collect();
        assert_eq!(
            order,
            [
                "gui-session -",
                "ios-simulator 9.3",
                "ios-simulator 18.1",
                "jdk 8.0.392",
                "jdk 17.0.12",
                "jdk 21.0.4",
            ]
        );
    }

    #[test]
    fn error_attributes_are_one_bounded_line() {
        let long = format!("\n\nfirst line {}\nsecond line", "x".repeat(500));
        let entry = unhealthy("tool", None, &long);
        let error = &entry.attributes["error"];
        assert!(error.starts_with("first line"));
        assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
        assert!(!error.contains('\n'));
    }

    #[test]
    fn android_sdk_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("platform-tools")).unwrap();
        std::fs::write(
            root.join("platform-tools").join("source.properties"),
            "Pkg.UserSrc=false\nPkg.Revision=35.0.2\nPkg.Path=platform-tools\n",
        )
        .unwrap();
        for version in ["34.0.0", "35.0.0", "9.0.0"] {
            std::fs::create_dir_all(root.join("build-tools").join(version)).unwrap();
        }
        std::fs::create_dir_all(root.join("platforms").join("android-35")).unwrap();
        let entry = android_sdk_at(root);
        assert_eq!(entry.name, "android-sdk");
        assert_eq!(entry.state, CapabilityState::Installed);
        assert_eq!(entry.version.as_deref(), Some("35.0.2"));
        assert_eq!(entry.attributes["build_tools"], "9.0.0,34.0.0,35.0.0");
        assert_eq!(entry.attributes["platforms"], "android-35");
        assert!(!entry.attributes.contains_key("ndk"));
        assert_eq!(PathBuf::from(&entry.attributes["path"]), root);
    }

    #[test]
    fn android_sdk_that_is_missing_or_empty_is_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let empty = android_sdk_at(dir.path());
        assert_eq!(empty.state, CapabilityState::Unhealthy);
        assert_eq!(empty.version, None);
        let missing = android_sdk_at(&dir.path().join("gone"));
        assert_eq!(missing.state, CapabilityState::Unhealthy);
        assert!(missing.attributes["error"].contains("does not exist"));
    }

    #[test]
    fn sysroot_targets_are_the_directories_with_a_library() {
        let dir = tempfile::tempdir().unwrap();
        let rustlib = dir.path().join("lib").join("rustlib");
        for target in ["aarch64-apple-darwin", "wasm32-unknown-unknown"] {
            std::fs::create_dir_all(rustlib.join(target).join("lib")).unwrap();
        }
        // Not targets: component metadata and a target without a library.
        std::fs::create_dir_all(rustlib.join("etc")).unwrap();
        std::fs::create_dir_all(rustlib.join("src").join("rust")).unwrap();
        std::fs::create_dir_all(rustlib.join("x86_64-unknown-none")).unwrap();
        assert_eq!(
            sysroot_targets(dir.path()),
            ["aarch64-apple-darwin", "wasm32-unknown-unknown"]
        );
        assert!(sysroot_targets(&dir.path().join("missing")).is_empty());
    }
}
