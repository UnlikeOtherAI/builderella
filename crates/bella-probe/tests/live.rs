//! Smoke tests against the machine running them: the public API returns
//! sane values for real hardware, a real OS and whatever tools are present.

use bella_core::BootId;
use bella_core::platform::{CapabilityState, Platform};
use bella_probe::{
    ProbeOptions, assemble_inventory, boot_id, detect_capabilities, hardware, os_info,
};
use std::sync::OnceLock;

/// One detection pass shared by the tests, since it starts real tools.
fn capabilities() -> &'static [bella_core::platform::Capability] {
    static FOUND: OnceLock<Vec<bella_core::platform::Capability>> = OnceLock::new();
    FOUND.get_or_init(|| detect_capabilities(&ProbeOptions::default()))
}

#[test]
fn hardware_is_sane() {
    let hw = hardware();
    assert!(hw.logical_cpus > 0);
    assert!(
        hw.physical_cpus
            .is_none_or(|n| n > 0 && n <= hw.logical_cpus)
    );
    // At least 256 MiB, and below an exbibyte.
    assert!(
        hw.memory_bytes > 256 << 20,
        "memory_bytes = {}",
        hw.memory_bytes
    );
    assert!(hw.memory_bytes < 1 << 60);
    for disk in &hw.disks {
        assert!(!disk.mount.is_empty());
        assert!(disk.total_bytes > 0);
    }
    // Reading twice gives the same answer: nothing volatile is included.
    assert_eq!(hardware(), hw);
}

#[test]
fn os_info_matches_the_build_platform() {
    let os = os_info();
    assert_eq!(Some(os.platform), Platform::current());
    assert!(!os.os_version.is_empty());
    assert!(!os.kernel.is_empty());
    assert!(!os.hostname.is_empty());
    assert_eq!(os.hostname, bella_probe::hostname());
    if cfg!(any(target_os = "macos", windows)) {
        assert!(
            os.os_version.starts_with(|c: char| c.is_ascii_digit()),
            "os_version = {}",
            os.os_version
        );
    }
    if cfg!(windows) {
        // major.minor.build
        assert_eq!(os.os_version.split('.').count(), 3, "{}", os.os_version);
    }
    assert_eq!(os_info(), os);
}

#[test]
fn boot_id_is_stable_within_a_boot() {
    let first = boot_id();
    let second = boot_id();
    assert_eq!(first, second);
    assert!(BootId::parse(first.as_str()).is_ok());
    // A real per-boot value was found, not the fixed fallback.
    assert_ne!(first, BootId::derive(b"bella-probe:unknown-boot"));
}

#[test]
fn detected_capabilities_are_well_formed() {
    let found = capabilities();
    for capability in found {
        assert!(!capability.name.is_empty());
        assert_ne!(capability.state, CapabilityState::Missing, "{capability:?}");
        if capability.state == CapabilityState::Installed && capability.name != "gui-session" {
            let version = capability.version.as_deref().unwrap_or("");
            assert!(
                version.starts_with(|c: char| c.is_ascii_digit()),
                "{capability:?}"
            );
        }
        if capability.state == CapabilityState::Unhealthy {
            assert!(
                capability.attributes.contains_key("error"),
                "{capability:?}"
            );
        }
    }
    // Sorted by name, so equal machines give equal lists.
    let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    // Only the capabilities that may have several versions repeat.
    for pair in found.windows(2) {
        if pair[0].name == pair[1].name {
            assert!(
                ["jdk", "ios-simulator", "msvc"].contains(&pair[0].name.as_str()),
                "duplicate {}",
                pair[0].name
            );
        }
    }
}

/// These tests are run by cargo, so a Rust toolchain is on the machine and
/// must be found with its targets.
#[test]
fn the_toolchain_running_the_tests_is_detected() {
    let found = capabilities();
    let rust = found
        .iter()
        .find(|c| c.name == "rust")
        .expect("rustc is on PATH while cargo runs the tests");
    assert_eq!(rust.state, CapabilityState::Installed, "{rust:?}");
    let targets = rust.attributes.get("targets").expect("targets attribute");
    assert!(targets.split(',').all(|t| t.contains('-')), "{targets}");
    let host = rust.attributes.get("host").expect("host attribute");
    assert!(
        targets.split(',').any(|t| t == host),
        "{targets} lacks {host}"
    );
    let cargo = found
        .iter()
        .find(|c| c.name == "cargo")
        .expect("cargo is on PATH while it runs the tests");
    assert_eq!(cargo.state, CapabilityState::Installed, "{cargo:?}");
}

#[test]
fn platform_only_capabilities_stay_on_their_platform() {
    let found = capabilities();
    let has = |name: &str| found.iter().any(|c| c.name == name);
    if !cfg!(target_os = "macos") {
        assert!(!has("xcode"));
        assert!(!has("ios-simulator"));
    }
    if !cfg!(windows) {
        assert!(!has("msvc"));
    }
}

#[test]
fn inventory_of_this_machine_keeps_its_revision_when_unchanged() {
    let build = |previous| {
        assemble_inventory(
            hardware(),
            os_info(),
            capabilities().to_vec(),
            Vec::new(),
            "0.1.0-test",
            previous,
        )
    };
    let first = build(None);
    assert_eq!(first.revision, 1);
    let second = build(Some(&first));
    assert_eq!(second.revision, 1);
    assert_eq!(second, first);
    let json = serde_json::to_string(&first).expect("serialises");
    let back: bella_core::machine::Inventory = serde_json::from_str(&json).expect("round-trips");
    assert_eq!(back, first);
}
