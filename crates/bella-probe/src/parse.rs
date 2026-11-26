//! Parsers for the output of toolchain commands.
//!
//! Everything here is a pure function of captured text, kept apart from
//! process execution so it can be tested against real sample output. A
//! parser returns `None` (or an empty list) when the text does not contain
//! what it looks for; it never guesses a version.

use serde::Deserialize;

/// The leading version-like token of `text`: starts with a digit (after an
/// optional `v`) and ends at whitespace or one of `, ; ( ) "`.
fn version_token(text: &str) -> Option<String> {
    let text = text.trim_start();
    let text = text.strip_prefix('v').unwrap_or(text);
    let end = text
        .find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"'))
        .unwrap_or(text.len());
    let token = &text[..end];
    token
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| token.to_string())
}

/// The version token that follows the first occurrence of `marker`.
fn version_after(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)? + marker.len();
    version_token(&text[start..])
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `git --version`: `git version 2.47.1.windows.2` gives `2.47.1.windows.2`.
pub fn git(stdout: &str) -> Option<String> {
    version_after(stdout, "git version ")
}

/// `git lfs version`: `git-lfs/3.7.1 (GitHub; darwin arm64; go 1.25.3)`.
pub fn git_lfs(stdout: &str) -> Option<String> {
    version_after(stdout, "git-lfs/")
}

/// Release and host triple reported by `rustc -vV`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rustc {
    pub version: String,
    pub host: Option<String>,
}

/// `rustc -vV`. Falls back to the first line (`rustc 1.89.0 (...)`) when the
/// `release:` field is missing.
pub fn rustc_verbose(stdout: &str) -> Option<Rustc> {
    let field = |name: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(non_empty)
    };
    let version = field("release:")
        .and_then(|release| version_token(&release))
        .or_else(|| version_after(stdout, "rustc "))?;
    Some(Rustc {
        version,
        host: field("host:"),
    })
}

/// `rustup target list --installed`: one target triple per line.
pub fn rustup_targets(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|target| target.contains('-'))
        .map(str::to_string)
        .collect()
}

/// `cargo --version`: `cargo 1.89.0 (c24e10642 2025-06-23)`.
pub fn cargo(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("cargo "))
        .and_then(version_token)
}

/// A tool that prints only its version, optionally prefixed with `v`
/// (`node`, `npm`, `pnpm`, `yarn`, `bun`, `ninja`). Lines that are not a
/// lone version, such as warnings, are skipped.
pub fn bare(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let line = line.trim();
        let token = version_token(line)?;
        let lone = line.strip_prefix('v').unwrap_or(line) == token;
        let plain = token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'));
        (lone && plain).then_some(token)
    })
}

/// `python --version`: `Python 3.12.7`. Python 2 prints it on standard
/// error, so pass both streams.
pub fn python(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Python "))
        .and_then(version_token)
}

/// `go version`: `go version go1.23.2 darwin/arm64` gives `1.23.2`.
pub fn go(stdout: &str) -> Option<String> {
    version_after(stdout, "go version go")
}

/// One Java runtime as reported by `java -XshowSettings:properties -version`
/// or plain `java -version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Java {
    /// Comparable version. The legacy `1.8.0_392` scheme is rewritten to
    /// `8.0.392` so that `>=8` and `>=17` compare as intended.
    pub version: String,
    /// The version string exactly as the runtime printed it.
    pub raw_version: String,
    /// The `... Runtime Environment ...` line, when present.
    pub runtime: Option<String>,
    /// `java.home`, when the properties were printed.
    pub home: Option<String>,
    /// `java.vendor`, when the properties were printed.
    pub vendor: Option<String>,
}

/// Parse `java -version`, which prints on standard error. Accepts the
/// combined output with or without the `-XshowSettings:properties` block.
pub fn java(text: &str) -> Option<Java> {
    let raw_version = text.lines().find_map(|line| {
        let start = line.find(" version \"")? + " version \"".len();
        let rest = &line[start..];
        let end = rest.find('"')?;
        non_empty(&rest[..end])
    })?;
    let property = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| non_empty(value)).flatten()
        })
    };
    Some(Java {
        version: normalise_java_version(&raw_version),
        runtime: text
            .lines()
            .find(|line| line.contains("Runtime Environment"))
            .and_then(non_empty),
        home: property("java.home"),
        vendor: property("java.vendor"),
        raw_version,
    })
}

fn normalise_java_version(raw: &str) -> String {
    match raw.strip_prefix("1.") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.replacen('_', ".", 1),
        _ => raw.to_string(),
    }
}

/// Home directories listed by macOS `/usr/libexec/java_home -V`, which
/// prints on standard error, in the order given.
pub fn java_home_list(text: &str) -> Vec<String> {
    let mut homes = Vec::new();
    for line in text.lines() {
        // Entries are indented; the line without indentation at the end is
        // the default home again.
        if !line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(quote) = line.rfind('"') else {
            continue;
        };
        let rest = &line[quote + 1..];
        if let Some(slash) = rest.find('/') {
            let home = rest[slash..].trim().to_string();
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
    homes
}

/// `gradle --version`: the `Gradle 8.10.2` line.
pub fn gradle(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Gradle "))
        .and_then(version_token)
}

/// `kotlinc -version` (`info: kotlinc-jvm 2.0.21 (JRE ...)`, on standard
/// error) or `kotlin -version` (`Kotlin version 2.0.21-release-482 (...)`).
pub fn kotlin(text: &str) -> Option<String> {
    version_after(text, "kotlinc-jvm ")
        .or_else(|| version_after(text, "kotlinc-native "))
        .or_else(|| version_after(text, "Kotlin version "))
}

/// `swift --version`: `Apple Swift version 6.0.2 (...)` or
/// `Swift version 5.10.1 (swift-5.10.1-RELEASE)`. Some releases print the
/// line on standard error, so pass both streams.
pub fn swift(text: &str) -> Option<String> {
    version_after(text, "Swift version ")
}

/// The `Target: <triple>` line printed by `swift --version` and
/// `clang --version`.
pub fn target_line(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Target:"))
        .and_then(non_empty)
}

/// Version and build of the selected Xcode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xcode {
    pub version: String,
    pub build: Option<String>,
}

/// `xcodebuild -version`, two lines: `Xcode 16.1` and `Build version 16B40`.
pub fn xcodebuild_version(stdout: &str) -> Option<Xcode> {
    let version = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Xcode "))
        .and_then(version_token)?;
    let build = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build version "))
        .and_then(non_empty);
    Some(Xcode { version, build })
}

/// SDK identifiers from `xcodebuild -showsdks` (the word after `-sdk`), in
/// the order printed, without duplicates.
pub fn xcode_sdks(stdout: &str) -> Vec<String> {
    let mut sdks = Vec::new();
    for line in stdout.lines() {
        let Some((_, rest)) = line.split_once("-sdk ") else {
            continue;
        };
        if let Some(sdk) = rest.split_whitespace().next()
            && !sdks.iter().any(|known| known == sdk)
        {
            sdks.push(sdk.to_string());
        }
    }
    sdks
}

/// One simulator runtime from `xcrun simctl list runtimes --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimRuntime {
    /// `iOS 18.1`.
    pub name: String,
    /// `18.1`.
    pub version: String,
    /// `com.apple.CoreSimulator.SimRuntime.iOS-18-1`.
    pub identifier: String,
    pub build: String,
    pub available: bool,
    /// `iOS`, `tvOS`, `watchOS`, `xrOS`; derived from the name on Xcode
    /// versions that do not print it.
    pub platform: String,
}

#[derive(Deserialize)]
struct RawSimctl {
    #[serde(default)]
    runtimes: Vec<RawRuntime>,
}

#[derive(Deserialize)]
struct RawRuntime {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    buildversion: String,
    #[serde(default, rename = "isAvailable")]
    is_available: Option<bool>,
    /// Xcode 10 and earlier: `(available)` or a reason in parentheses.
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

/// `xcrun simctl list runtimes --json`. `None` when the text is not the
/// expected JSON document.
pub fn simctl_runtimes(json: &str) -> Option<Vec<SimRuntime>> {
    let raw: RawSimctl = serde_json::from_str(json).ok()?;
    Some(
        raw.runtimes
            .into_iter()
            .map(|runtime| {
                let platform = runtime
                    .platform
                    .filter(|p| !p.is_empty())
                    .or_else(|| runtime.name.split_whitespace().next().map(str::to_string))
                    .unwrap_or_default();
                let available = runtime
                    .is_available
                    .unwrap_or_else(|| runtime.availability.as_deref() == Some("(available)"));
                SimRuntime {
                    name: runtime.name,
                    version: runtime.version,
                    identifier: runtime.identifier,
                    build: runtime.buildversion,
                    available,
                    platform,
                }
            })
            .collect(),
    )
}

/// One Visual Studio installation from `vswhere -format json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VsInstance {
    /// `installationVersion`, e.g. `17.11.35327.3`.
    pub version: String,
    pub path: String,
    pub display_name: Option<String>,
    pub complete: bool,
    pub prerelease: bool,
}

#[derive(Deserialize)]
struct RawVsInstance {
    #[serde(default, rename = "installationVersion")]
    installation_version: String,
    #[serde(default, rename = "installationPath")]
    installation_path: String,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default, rename = "isComplete")]
    is_complete: Option<bool>,
    #[serde(default, rename = "isPrerelease")]
    is_prerelease: Option<bool>,
}

/// `vswhere -format json -utf8`. `None` when the text is not a JSON array.
pub fn vswhere(json: &str) -> Option<Vec<VsInstance>> {
    // vswhere writes a UTF-8 byte order mark in some versions.
    let json = json.trim_start_matches('\u{feff}').trim();
    let raw: Vec<RawVsInstance> = serde_json::from_str(json).ok()?;
    Some(
        raw.into_iter()
            .map(|instance| VsInstance {
                version: instance.installation_version,
                path: instance.installation_path,
                display_name: instance.display_name.filter(|name| !name.is_empty()),
                complete: instance.is_complete.unwrap_or(true),
                prerelease: instance.is_prerelease.unwrap_or(false),
            })
            .collect(),
    )
}

/// SDK versions from `dotnet --list-sdks` (`8.0.403 [C:\Program Files\dotnet\sdk]`),
/// in the order printed, which is ascending.
pub fn dotnet_sdks(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| line.contains('['))
        .filter_map(version_token)
        .collect()
}

/// `cmake --version`: `cmake version 3.30.5`.
pub fn cmake(stdout: &str) -> Option<String> {
    version_after(stdout, "cmake version ")
}

/// `make --version`: `GNU Make 4.3`. Other makes are not recognised.
pub fn make(stdout: &str) -> Option<String> {
    version_after(stdout, "GNU Make ")
}

/// What `clang --version` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clang {
    pub version: String,
    pub target: Option<String>,
    /// Apple's clang numbers its releases differently from LLVM's.
    pub apple: bool,
}

/// `clang --version`: `Apple clang version 16.0.0 (clang-1600.0.26.4)`,
/// `Ubuntu clang version 18.1.3 (1ubuntu1)`, `clang version 19.1.0`.
pub fn clang(stdout: &str) -> Option<Clang> {
    let line = stdout
        .lines()
        .find(|line| line.contains("clang version "))?;
    Some(Clang {
        version: version_after(line, "clang version ")?,
        target: target_line(stdout),
        apple: line.trim_start().starts_with("Apple"),
    })
}

/// `gcc --version`: the version after the parenthesised distribution tag,
/// e.g. `gcc (Ubuntu 13.2.0-23ubuntu4) 13.2.0` gives `13.2.0`. Returns
/// `None` when the command is clang answering to the name `gcc`.
pub fn gcc(stdout: &str) -> Option<String> {
    if stdout.contains("clang") {
        return None;
    }
    let line = stdout.lines().find(|line| !line.trim().is_empty())?;
    let rest = match line.find('(') {
        Some(open) => {
            let mut depth = 0usize;
            let mut end = line.len();
            for (index, c) in line[open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + index + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            &line[end..]
        }
        None => line,
    };
    rest.split_whitespace().find_map(version_token)
}

/// `docker --version`: `Docker version 27.3.1, build ce12230`.
pub fn docker_client(stdout: &str) -> Option<String> {
    version_after(stdout, "Docker version ")
}

/// `Pkg.Revision` from an Android SDK package's `source.properties`.
pub fn source_properties_revision(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "Pkg.Revision")
            .then(|| non_empty(value))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions() {
        assert_eq!(
            git("git version 2.54.0 (Apple Git-157)\n").as_deref(),
            Some("2.54.0")
        );
        assert_eq!(
            git("git version 2.47.1.windows.2\n").as_deref(),
            Some("2.47.1.windows.2")
        );
        assert_eq!(git("git version 2.43.0\n").as_deref(), Some("2.43.0"));
        assert_eq!(git("fatal: not a thing\n"), None);
        assert_eq!(git(""), None);
    }

    #[test]
    fn git_lfs_versions() {
        assert_eq!(
            git_lfs("git-lfs/3.7.1 (GitHub; darwin arm64; go 1.25.3)\n").as_deref(),
            Some("3.7.1")
        );
        assert_eq!(
            git_lfs("git-lfs/3.5.1 (GitHub; windows amd64; go 1.21.7; git e237bb3a)\n").as_deref(),
            Some("3.5.1")
        );
        assert_eq!(
            git_lfs("git: 'lfs' is not a git command. See 'git --help'.\n"),
            None
        );
    }

    const RUSTC_VV: &str = "rustc 1.89.0 (29483883e 2025-08-04)\n\
binary: rustc\n\
commit-hash: 29483883eed69d5fb4db01964cdf2af4d86e9cb2\n\
commit-date: 2025-08-04\n\
host: x86_64-pc-windows-msvc\n\
release: 1.89.0\n\
LLVM version: 20.1.7\n";

    #[test]
    fn rustc_verbose_output() {
        assert_eq!(
            rustc_verbose(RUSTC_VV),
            Some(Rustc {
                version: "1.89.0".into(),
                host: Some("x86_64-pc-windows-msvc".into()),
            })
        );
        let nightly = "rustc 1.91.0-nightly (425a9c0a0 2025-08-17)\nbinary: rustc\n\
host: aarch64-apple-darwin\nrelease: 1.91.0-nightly\nLLVM version: 21.1.0\n";
        let parsed = rustc_verbose(nightly).unwrap();
        assert_eq!(parsed.version, "1.91.0-nightly");
        assert_eq!(parsed.host.as_deref(), Some("aarch64-apple-darwin"));
        // A distribution build without the verbose block still has line one.
        let short = rustc_verbose("rustc 1.98.1 (48a229cea 2026-09-01) (Homebrew)\n").unwrap();
        assert_eq!(short.version, "1.98.1");
        assert_eq!(short.host, None);
        assert_eq!(
            rustc_verbose("error: toolchain 'stable' is not installed\n"),
            None
        );
    }

    #[test]
    fn rustup_installed_targets() {
        let out = "aarch64-apple-darwin\naarch64-apple-ios\nwasm32-unknown-unknown\n\n";
        assert_eq!(
            rustup_targets(out),
            [
                "aarch64-apple-darwin",
                "aarch64-apple-ios",
                "wasm32-unknown-unknown"
            ]
        );
        assert!(rustup_targets("").is_empty());
        assert!(rustup_targets("error: no default toolchain\n").is_empty());
    }

    #[test]
    fn cargo_versions() {
        assert_eq!(
            cargo("cargo 1.89.0 (c24e10642 2025-06-23)\n").as_deref(),
            Some("1.89.0")
        );
        assert_eq!(
            cargo("cargo 1.98.1 (797e8a9bc 2026-08-05) (Homebrew)\n").as_deref(),
            Some("1.98.1")
        );
        assert_eq!(cargo("error: no such command\n"), None);
    }

    #[test]
    fn bare_versions_skip_noise() {
        assert_eq!(bare("v22.11.0\n").as_deref(), Some("22.11.0"));
        assert_eq!(bare("10.9.0\n").as_deref(), Some("10.9.0"));
        assert_eq!(bare("1.22.22\r\n").as_deref(), Some("1.22.22"));
        assert_eq!(bare("1.1.34\n").as_deref(), Some("1.1.34"));
        assert_eq!(
            bare("1.11.1.git.kitware.jobserver-1\n").as_deref(),
            Some("1.11.1.git.kitware.jobserver-1")
        );
        // A warning ahead of the version line.
        let noisy = "npm warn Unknown user config \"python\". This will stop working.\n10.8.2\n";
        assert_eq!(bare(noisy).as_deref(), Some("10.8.2"));
        // A sentence that merely starts with a digit is not a version.
        assert_eq!(bare("2 packages are looking for funding\n"), None);
        assert_eq!(bare("Usage: tool [options]\n"), None);
        assert_eq!(bare(""), None);
    }

    #[test]
    fn python_versions() {
        assert_eq!(python("Python 3.12.7\n").as_deref(), Some("3.12.7"));
        assert_eq!(python("Python 2.7.18\n").as_deref(), Some("2.7.18"));
        assert_eq!(python("Python 3.13.0rc2\n").as_deref(), Some("3.13.0rc2"));
        // The Windows app execution alias when Python is not installed.
        let stub = "Python was not found; run without arguments to install from the \
Microsoft Store, or disable this shortcut from Settings > Apps > Advanced app settings > \
App execution aliases.\n";
        assert_eq!(python(stub), None);
    }

    #[test]
    fn go_versions() {
        assert_eq!(
            go("go version go1.23.2 darwin/arm64\n").as_deref(),
            Some("1.23.2")
        );
        assert_eq!(
            go("go version go1.21rc3 linux/amd64\n").as_deref(),
            Some("1.21rc3")
        );
        assert_eq!(go("go: unknown command\n"), None);
    }

    /// `java -version` writes everything to standard error.
    const JAVA_21_STDERR: &str = "openjdk version \"21.0.4\" 2024-07-16 LTS\n\
OpenJDK Runtime Environment Temurin-21.0.4+7 (build 21.0.4+7-LTS)\n\
OpenJDK 64-Bit Server VM Temurin-21.0.4+7 (build 21.0.4+7-LTS, mixed mode, sharing)\n";

    const JAVA_8_STDERR: &str = "java version \"1.8.0_392\"\n\
Java(TM) SE Runtime Environment (build 1.8.0_392-b08)\n\
Java HotSpot(TM) 64-Bit Server VM (build 25.392-b08, mixed mode)\n";

    const JAVA_17_WITH_PROPERTIES: &str = r#"Property settings:
    file.encoding = UTF-8
    java.class.version = 61.0
    java.home = /Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home
    java.io.tmpdir = /var/folders/xx/T/
    java.library.path = /Library/Java/Extensions
        /usr/lib/java
    java.runtime.name = OpenJDK Runtime Environment
    java.vendor = Eclipse Adoptium
    java.vendor.url = https://adoptium.net/
    java.version = 17.0.12
    os.name = Mac OS X

openjdk version "17.0.12" 2024-07-16
OpenJDK Runtime Environment Temurin-17.0.12+7 (build 17.0.12+7)
OpenJDK 64-Bit Server VM Temurin-17.0.12+7 (build 17.0.12+7, mixed mode)
"#;

    #[test]
    fn java_version_from_stderr() {
        let modern = java(JAVA_21_STDERR).unwrap();
        assert_eq!(modern.version, "21.0.4");
        assert_eq!(modern.raw_version, "21.0.4");
        assert_eq!(
            modern.runtime.as_deref(),
            Some("OpenJDK Runtime Environment Temurin-21.0.4+7 (build 21.0.4+7-LTS)")
        );
        assert_eq!(modern.home, None);

        let legacy = java(JAVA_8_STDERR).unwrap();
        assert_eq!(legacy.raw_version, "1.8.0_392");
        assert_eq!(legacy.version, "8.0.392");
        assert!(bella_core::version::satisfies(&legacy.version, ">=8, <9"));

        let early = java("openjdk version \"22-ea\" 2024-03-19\n").unwrap();
        assert_eq!(early.version, "22-ea");
    }

    #[test]
    fn java_properties_give_home_and_vendor() {
        let parsed = java(JAVA_17_WITH_PROPERTIES).unwrap();
        assert_eq!(parsed.version, "17.0.12");
        assert_eq!(
            parsed.home.as_deref(),
            Some("/Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home")
        );
        assert_eq!(parsed.vendor.as_deref(), Some("Eclipse Adoptium"));
    }

    #[test]
    fn java_stub_without_a_runtime_has_no_version() {
        let stub = "The operation couldn\u{2019}t be completed. Unable to locate a Java Runtime.\n\
Please visit http://www.java.com for information on installing Java.\n";
        assert_eq!(java(stub), None);
        assert_eq!(java(""), None);
    }

    #[test]
    fn java_home_listing() {
        let listing = r#"Matching Java Virtual Machines (3):
    21.0.4 (arm64) "Eclipse Adoptium" - "OpenJDK 21.0.4" /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home
    17.0.12 (arm64) "Eclipse Adoptium" - "OpenJDK 17.0.12" /Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home
    1.8.0_292 (x86_64) "AdoptOpenJDK" - "AdoptOpenJDK 8" /Library/Java/JavaVirtualMachines/adoptopenjdk-8.jdk/Contents/Home
/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home
"#;
        assert_eq!(
            java_home_list(listing),
            [
                "/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home",
                "/Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home",
                "/Library/Java/JavaVirtualMachines/adoptopenjdk-8.jdk/Contents/Home",
            ]
        );
        // Older macOS releases separate the fields with tabs.
        let old = "Matching Java Virtual Machines (1):\n\x20   1.8.0_292, x86_64:\t\"AdoptOpenJDK 8\"\t\
/Library/Java/JavaVirtualMachines/adoptopenjdk-8.jdk/Contents/Home\n\n\
/Library/Java/JavaVirtualMachines/adoptopenjdk-8.jdk/Contents/Home\n";
        assert_eq!(
            java_home_list(old),
            ["/Library/Java/JavaVirtualMachines/adoptopenjdk-8.jdk/Contents/Home"]
        );
        let none = "The operation couldn\u{2019}t be completed. Unable to locate a Java Runtime.\n";
        assert!(java_home_list(none).is_empty());
    }

    #[test]
    fn gradle_versions() {
        let out = "\n------------------------------------------------------------\n\
Gradle 8.10.2\n\
------------------------------------------------------------\n\n\
Build time:    2024-09-23 21:28:39 UTC\n\
Revision:      415adb9e06a516c44b391edff552fd42139443f7\n\n\
Kotlin:        1.9.24\nGroovy:        3.0.22\n\
Launcher JVM:  21.0.4 (Eclipse Adoptium 21.0.4+7-LTS)\n\
OS:            Mac OS X 15.1 aarch64\n";
        assert_eq!(gradle(out).as_deref(), Some("8.10.2"));
        assert_eq!(
            gradle("Welcome to Gradle 9.0.0-rc-1!\n\nGradle 9.0.0-rc-1\n").as_deref(),
            Some("9.0.0-rc-1")
        );
        assert_eq!(gradle("ERROR: JAVA_HOME is not set\n"), None);
    }

    #[test]
    fn kotlin_versions() {
        assert_eq!(
            kotlin("info: kotlinc-jvm 2.0.21 (JRE 21.0.4+7-LTS)\n").as_deref(),
            Some("2.0.21")
        );
        assert_eq!(
            kotlin("Kotlin version 2.0.21-release-482 (JRE 21.0.4+7-LTS)\n").as_deref(),
            Some("2.0.21-release-482")
        );
        assert_eq!(kotlin("error: no java\n"), None);
    }

    #[test]
    fn swift_versions() {
        let apple = "swift-driver version: 1.168.6 Apple Swift version 6.4.2 \
(swiftlang-6.4.2.1.9 clang-2100.4.1.1)\nTarget: arm64-apple-macosx27.0.0\n";
        assert_eq!(swift(apple).as_deref(), Some("6.4.2"));
        assert_eq!(
            target_line(apple).as_deref(),
            Some("arm64-apple-macosx27.0.0")
        );
        let linux =
            "Swift version 5.10.1 (swift-5.10.1-RELEASE)\nTarget: x86_64-unknown-linux-gnu\n";
        assert_eq!(swift(linux).as_deref(), Some("5.10.1"));
        assert_eq!(swift("xcrun: error: invalid active developer path\n"), None);
    }

    #[test]
    fn xcodebuild_two_line_version() {
        assert_eq!(
            xcodebuild_version("Xcode 27.2\nBuild version 27B52\n"),
            Some(Xcode {
                version: "27.2".into(),
                build: Some("27B52".into()),
            })
        );
        assert_eq!(
            xcodebuild_version("Xcode 14.3.1\nBuild version 14E300c\n")
                .unwrap()
                .version,
            "14.3.1"
        );
        let tools_only = "xcode-select: error: tool 'xcodebuild' requires Xcode, but active \
developer directory '/Library/Developer/CommandLineTools' is a command line tools instance\n";
        assert_eq!(xcodebuild_version(tools_only), None);
    }

    #[test]
    fn xcode_sdk_identifiers() {
        let out = "DriverKit SDKs:\n\tDriverKit 27.2                \t-sdk driverkit27.2\n\n\
iOS SDKs:\n\tiOS 27.2                      \t-sdk iphoneos27.2\n\n\
iOS Simulator SDKs:\n\tSimulator - iOS 27.2          \t-sdk iphonesimulator27.2\n\n\
macOS SDKs:\n\tmacOS 27.2                    \t-sdk macosx27.2\n\
\tmacOS 27.2                    \t-sdk macosx27.2\n\n";
        assert_eq!(
            xcode_sdks(out),
            [
                "driverkit27.2",
                "iphoneos27.2",
                "iphonesimulator27.2",
                "macosx27.2"
            ]
        );
        assert!(xcode_sdks("").is_empty());
    }

    #[test]
    fn simctl_runtime_list() {
        let json = r#"{
  "runtimes" : [
    {
      "isAvailable" : true,
      "version" : "26.5",
      "isInternal" : false,
      "buildversion" : "23F77",
      "supportedArchitectures" : [ "arm64" ],
      "supportedDeviceTypes" : [
        { "bundlePath" : "\/Library\/Developer\/x.simdevicetype", "name" : "iPhone 17 Pro",
          "productFamily" : "iPhone", "identifier" : "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro" }
      ],
      "identifier" : "com.apple.CoreSimulator.SimRuntime.iOS-26-5",
      "platform" : "iOS",
      "bundlePath" : "\/Library\/Developer\/CoreSimulator\/Volumes\/iOS_23F77\/iOS 26.5.simruntime",
      "name" : "iOS 26.5"
    },
    {
      "isAvailable" : false,
      "version" : "11.0",
      "buildversion" : "22R349",
      "identifier" : "com.apple.CoreSimulator.SimRuntime.watchOS-11-0",
      "platform" : "watchOS",
      "name" : "watchOS 11.0"
    }
  ]
}"#;
        let runtimes = simctl_runtimes(json).unwrap();
        assert_eq!(runtimes.len(), 2);
        assert_eq!(runtimes[0].platform, "iOS");
        assert_eq!(runtimes[0].version, "26.5");
        assert_eq!(runtimes[0].build, "23F77");
        assert_eq!(
            runtimes[0].identifier,
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5"
        );
        assert!(runtimes[0].available);
        assert_eq!(runtimes[1].platform, "watchOS");
        assert!(!runtimes[1].available);
    }

    #[test]
    fn simctl_runtime_list_from_old_xcode() {
        let json = r#"{"runtimes":[{"buildversion":"16B91","availability":"(available)",
"name":"iOS 12.1","identifier":"com.apple.CoreSimulator.SimRuntime.iOS-12-1","version":"12.1"},
{"buildversion":"15A372","availability":"(unavailable, runtime profile not found)",
"name":"iOS 11.0","identifier":"com.apple.CoreSimulator.SimRuntime.iOS-11-0","version":"11.0"}]}"#;
        let runtimes = simctl_runtimes(json).unwrap();
        assert_eq!(runtimes[0].platform, "iOS");
        assert!(runtimes[0].available);
        assert!(!runtimes[1].available);
        assert_eq!(simctl_runtimes("{\"runtimes\":[]}"), Some(vec![]));
        assert_eq!(
            simctl_runtimes("xcrun: error: unable to find utility \"simctl\""),
            None
        );
    }

    #[test]
    fn vswhere_instances() {
        let json = "\u{feff}[\r\n  {\r\n    \"instanceId\": \"a1b2c3d4\",\r\n\
    \"installDate\": \"2024-08-20T09:12:44Z\",\r\n\
    \"installationName\": \"VisualStudio/17.11.5+35327.3\",\r\n\
    \"installationPath\": \"C:\\\\Program Files\\\\Microsoft Visual Studio\\\\2022\\\\Community\",\r\n\
    \"installationVersion\": \"17.11.35327.3\",\r\n\
    \"productId\": \"Microsoft.VisualStudio.Product.Community\",\r\n\
    \"isComplete\": true,\r\n    \"isLaunchable\": true,\r\n    \"isPrerelease\": false,\r\n\
    \"displayName\": \"Visual Studio Community 2022\",\r\n\
    \"catalog\": { \"productLineVersion\": \"2022\" }\r\n  },\r\n  {\r\n\
    \"installationPath\": \"C:\\\\Program Files (x86)\\\\Microsoft Visual Studio\\\\2022\\\\BuildTools\",\r\n\
    \"installationVersion\": \"17.12.35506.116\",\r\n\
    \"isComplete\": false,\r\n    \"isPrerelease\": true\r\n  }\r\n]\r\n";
        let instances = vswhere(json).unwrap();
        assert_eq!(
            instances[0],
            VsInstance {
                version: "17.11.35327.3".into(),
                path: "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community".into(),
                display_name: Some("Visual Studio Community 2022".into()),
                complete: true,
                prerelease: false,
            }
        );
        assert!(!instances[1].complete);
        assert!(instances[1].prerelease);
        assert_eq!(instances[1].display_name, None);
        assert_eq!(vswhere("[]"), Some(vec![]));
        assert_eq!(vswhere("Error 0x57: The parameter is incorrect."), None);
    }

    #[test]
    fn dotnet_sdk_list() {
        let out = "6.0.428 [C:\\Program Files\\dotnet\\sdk]\r\n\
8.0.403 [C:\\Program Files\\dotnet\\sdk]\r\n\
9.0.100-rc.2.24474.11 [C:\\Program Files\\dotnet\\sdk]\r\n";
        assert_eq!(
            dotnet_sdks(out),
            ["6.0.428", "8.0.403", "9.0.100-rc.2.24474.11"]
        );
        assert!(dotnet_sdks("").is_empty());
        assert!(dotnet_sdks("No .NET SDKs were found.\n").is_empty());
    }

    #[test]
    fn cmake_and_make_versions() {
        assert_eq!(
            cmake("cmake version 3.30.5\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).\n")
                .as_deref(),
            Some("3.30.5")
        );
        assert_eq!(
            cmake("cmake version 3.31.0-rc2\n").as_deref(),
            Some("3.31.0-rc2")
        );
        assert_eq!(
            make("GNU Make 3.81\nCopyright (C) 2006  Free Software Foundation, Inc.\n").as_deref(),
            Some("3.81")
        );
        assert_eq!(
            make("GNU Make 4.4.1\nBuilt for x86_64-pc-linux-gnu\n").as_deref(),
            Some("4.4.1")
        );
        assert_eq!(make("usage: make [-BeikNnqrSstWwX]\n"), None);
    }

    #[test]
    fn clang_versions() {
        let apple = "Apple clang version 21.0.0 (clang-2100.4.1.1)\n\
Target: arm64-apple-darwin27.0.0\nThread model: posix\n\
InstalledDir: /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin\n";
        assert_eq!(
            clang(apple),
            Some(Clang {
                version: "21.0.0".into(),
                target: Some("arm64-apple-darwin27.0.0".into()),
                apple: true,
            })
        );
        let ubuntu = "Ubuntu clang version 18.1.3 (1ubuntu1)\nTarget: x86_64-pc-linux-gnu\n\
Thread model: posix\nInstalledDir: /usr/bin\n";
        let parsed = clang(ubuntu).unwrap();
        assert_eq!(parsed.version, "18.1.3");
        assert!(!parsed.apple);
        let windows = "clang version 19.1.0\nTarget: x86_64-pc-windows-msvc\nThread model: posix\n\
InstalledDir: C:\\Program Files\\LLVM\\bin\n";
        assert_eq!(clang(windows).unwrap().version, "19.1.0");
        assert_eq!(clang("clang: error: no input files\n"), None);
    }

    #[test]
    fn gcc_versions() {
        assert_eq!(
            gcc("gcc (Ubuntu 13.2.0-23ubuntu4) 13.2.0\nCopyright (C) 2023 Free Software Foundation, Inc.\n")
                .as_deref(),
            Some("13.2.0")
        );
        assert_eq!(
            gcc("gcc (GCC) 14.2.1 20240912 (Red Hat 14.2.1-3)\n").as_deref(),
            Some("14.2.1")
        );
        assert_eq!(
            gcc("gcc.exe (Rev3, Built by MSYS2 project) 14.1.0\n").as_deref(),
            Some("14.1.0")
        );
        assert_eq!(
            gcc("gcc (Debian 12.2.0-14+deb12u1) 12.2.0\n").as_deref(),
            Some("12.2.0")
        );
        assert_eq!(
            gcc("gcc (crosstool-NG 1.26.0 (custom)) 13.2.0\n").as_deref(),
            Some("13.2.0")
        );
        // On macOS `gcc` is clang.
        let masquerade =
            "Apple clang version 21.0.0 (clang-2100.4.1.1)\nTarget: arm64-apple-darwin27.0.0\n";
        assert_eq!(gcc(masquerade), None);
        assert_eq!(gcc(""), None);
    }

    #[test]
    fn docker_client_versions() {
        assert_eq!(
            docker_client("Docker version 27.3.1, build ce12230\n").as_deref(),
            Some("27.3.1")
        );
        assert_eq!(
            docker_client("Docker version 20.10.24+dfsg1, build 297e128\n").as_deref(),
            Some("20.10.24+dfsg1")
        );
        assert_eq!(docker_client("podman version 5.2.2\n"), None);
    }

    #[test]
    fn android_source_properties() {
        let text = "Pkg.UserSrc=false\nPkg.Revision=35.0.2\nPkg.Path=platform-tools\n\
Pkg.Desc=Android SDK Platform-Tools\n";
        assert_eq!(source_properties_revision(text).as_deref(), Some("35.0.2"));
        assert_eq!(
            source_properties_revision("Pkg.Revision = 34.0.0 rc3\r\n").as_deref(),
            Some("34.0.0 rc3")
        );
        assert_eq!(source_properties_revision("Pkg.Desc=x\n"), None);
    }
}
