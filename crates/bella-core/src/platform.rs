//! Platform and toolchain model.
//!
//! Four separate decisions are never collapsed into one platform string:
//! the build host, the artifact target, the test runtime, and additional
//! execution requirements. Cross-compiling successfully does not prove the
//! output runs on its target.

use crate::version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Linux,
    Windows,
    Macos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
            Os::Macos => "macos",
        }
    }
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }
}

impl FromStr for Os {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "linux" => Ok(Os::Linux),
            "windows" => Ok(Os::Windows),
            "macos" | "darwin" => Ok(Os::Macos),
            other => Err(format!("unknown operating system `{other}`")),
        }
    }
}

impl FromStr for Arch {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "x86_64" | "amd64" | "x64" => Ok(Arch::X86_64),
            "aarch64" | "arm64" => Ok(Arch::Aarch64),
            other => Err(format!("unknown architecture `{other}`")),
        }
    }
}

/// A concrete OS/architecture pair, written `linux-x86_64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// The platform this binary was compiled for, when it is a supported one.
    pub fn current() -> Option<Self> {
        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            return None;
        };
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            return None;
        };
        Some(Self { os, arch })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os.as_str(), self.arch.as_str())
    }
}

impl FromStr for Platform {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (os, arch) = s.split_once('-').ok_or_else(|| format!("expected `os-arch`, got `{s}`"))?;
        Ok(Self { os: os.parse()?, arch: arch.parse()? })
    }
}

/// A set of acceptable platforms. Unset fields match anything.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<Arch>,
    /// Requirement on the OS version, e.g. `>=13` (see [`crate::version`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
}

impl PlatformSelector {
    pub fn os(os: Os) -> Self {
        Self { os: Some(os), ..Self::default() }
    }

    pub fn exact(platform: Platform) -> Self {
        Self { os: Some(platform.os), arch: Some(platform.arch), os_version: None }
    }

    pub fn matches(&self, platform: Platform, os_version: &str) -> bool {
        self.os.is_none_or(|os| os == platform.os)
            && self.arch.is_none_or(|arch| arch == platform.arch)
            && self.os_version.as_deref().is_none_or(|req| version::satisfies(os_version, req))
    }
}

impl fmt::Display for PlatformSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let os = self.os.map(Os::as_str).unwrap_or("any");
        let arch = self.arch.map(Arch::as_str).unwrap_or("any");
        write!(f, "{os}-{arch}")?;
        if let Some(v) = &self.os_version {
            write!(f, " ({v})")?;
        }
        Ok(())
    }
}

/// A capability is more than present or absent; the scheduler tells these
/// states apart when explaining why a machine was excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Installed,
    Missing,
    Unhealthy,
    Occupied,
    Unauthorized,
    Unverified,
}

/// A discovered toolchain, SDK, runtime or test capability with its exact
/// version. Names are lowercase and stable: `rust`, `cargo`, `node`, `pnpm`,
/// `jdk`, `gradle`, `xcode`, `swift`, `msvc`, `cmake`, `ninja`, `docker`,
/// `android-sdk`, `ios-simulator`, `gui-session`, `git`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub state: CapabilityState,
    /// Extra facts, e.g. `targets = "x86_64-pc-windows-msvc,wasm32-unknown-unknown"`,
    /// `path`, `sdk`, `runtime`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub name: String,
    /// Version requirement (see [`crate::version`]); `None` accepts any version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Each entry must appear in the capability's comma-separated attribute
    /// of the same name, e.g. `targets = "aarch64-apple-ios"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

/// Why a capability requirement is not met.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapabilityMismatch {
    Missing { name: String },
    NotUsable { name: String, state: CapabilityState },
    WrongVersion { name: String, have: String, need: String },
    MissingAttribute { name: String, attribute: String, need: String },
}

impl fmt::Display for CapabilityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { name } => write!(f, "`{name}` is not installed"),
            Self::NotUsable { name, state } => write!(f, "`{name}` is {state:?}"),
            Self::WrongVersion { name, have, need } => {
                write!(f, "`{name}` {have} does not satisfy `{need}`")
            }
            Self::MissingAttribute { name, attribute, need } => {
                write!(f, "`{name}` lacks {attribute} `{need}`")
            }
        }
    }
}

impl CapabilityRequirement {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into(), version: None, attributes: BTreeMap::new() }
    }

    /// Check this requirement against a machine's capability inventory.
    pub fn check(&self, capabilities: &[Capability]) -> Result<(), CapabilityMismatch> {
        let candidates: Vec<&Capability> =
            capabilities.iter().filter(|c| c.name == self.name).collect();
        if candidates.is_empty() {
            return Err(CapabilityMismatch::Missing { name: self.name.clone() });
        }
        let mut last = CapabilityMismatch::Missing { name: self.name.clone() };
        for cap in candidates {
            match self.check_one(cap) {
                Ok(()) => return Ok(()),
                Err(mismatch) => last = mismatch,
            }
        }
        Err(last)
    }

    fn check_one(&self, cap: &Capability) -> Result<(), CapabilityMismatch> {
        if cap.state != CapabilityState::Installed {
            return Err(CapabilityMismatch::NotUsable { name: cap.name.clone(), state: cap.state });
        }
        if let Some(need) = &self.version {
            let have = cap.version.clone().unwrap_or_default();
            if !version::satisfies(&have, need) {
                return Err(CapabilityMismatch::WrongVersion {
                    name: cap.name.clone(),
                    have,
                    need: need.clone(),
                });
            }
        }
        for (attribute, need) in &self.attributes {
            let have = cap.attributes.get(attribute).map(String::as_str).unwrap_or("");
            if !have.split(',').any(|item| item.trim() == need) {
                return Err(CapabilityMismatch::MissingAttribute {
                    name: cap.name.clone(),
                    attribute: attribute.clone(),
                    need: need.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Isolation an attempt requires from its executor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Native processes in a managed scope. For trusted code.
    #[default]
    Native,
    Container,
    Vm,
}

/// The four-field compatibility declaration of a recipe.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    /// Where the compiler and build tools may run. Empty means the recipe
    /// declared no portability, so the job is pinned to the platform of the
    /// machine that submitted it.
    #[serde(default)]
    pub build_hosts: Vec<PlatformSelector>,
    /// What the output is built for, e.g. `aarch64-apple-ios`. Informational
    /// for scheduling; part of cache and artifact identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_target: Option<String>,
    /// Where validation must execute. When set, test steps run only on a
    /// matching machine, and a job is never split across hosts in v1, so the
    /// whole job needs a machine matching both this and `build_hosts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_runtime: Option<PlatformSelector>,
    #[serde(default)]
    pub requires: Vec<CapabilityRequirement>,
    #[serde(default)]
    pub isolation: IsolationLevel,
}

/// The user-facing platform choice for one submission.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlacementChoice {
    /// Any combination the recipe declares valid.
    #[default]
    Any,
    /// Use it when feasible; otherwise fall back within the recipe's set.
    Prefer { platform: PlatformSelector },
    /// Hard constraint; never silently substituted.
    Require { platform: PlatformSelector },
    /// A machine id or its unique display name.
    Machine { machine: String },
    Pool { pool: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str, version: &str) -> Capability {
        Capability {
            name: name.into(),
            version: Some(version.into()),
            state: CapabilityState::Installed,
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn platform_text_round_trips() {
        let p: Platform = "macos-arm64".parse().unwrap();
        assert_eq!(p, Platform { os: Os::Macos, arch: Arch::Aarch64 });
        assert_eq!(p.to_string(), "macos-aarch64");
        assert!("plan9-x86_64".parse::<Platform>().is_err());
    }

    #[test]
    fn selector_checks_os_version() {
        let sel = PlatformSelector { os: Some(Os::Macos), arch: None, os_version: Some(">=14".into()) };
        let mac = Platform { os: Os::Macos, arch: Arch::X86_64 };
        assert!(!sel.matches(mac, "12.7.6"));
        assert!(sel.matches(mac, "15.1"));
    }

    #[test]
    fn requirement_explains_the_mismatch() {
        let caps = vec![cap("xcode", "14.2"), cap("rust", "1.89.0")];
        let need = CapabilityRequirement { name: "xcode".into(), version: Some(">=16".into()), attributes: BTreeMap::new() };
        assert!(matches!(need.check(&caps), Err(CapabilityMismatch::WrongVersion { .. })));
        assert!(matches!(
            CapabilityRequirement::named("jdk").check(&caps),
            Err(CapabilityMismatch::Missing { .. })
        ));
        assert!(CapabilityRequirement::named("rust").check(&caps).is_ok());
    }

    #[test]
    fn any_installed_version_can_satisfy() {
        let caps = vec![cap("jdk", "17.0.9"), cap("jdk", "21.0.4")];
        let need = CapabilityRequirement { name: "jdk".into(), version: Some("21".into()), attributes: BTreeMap::new() };
        assert!(need.check(&caps).is_ok());
    }
}
