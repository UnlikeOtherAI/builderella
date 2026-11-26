//! Rule 1 and 2: per-machine blockers and the job's reason.

use crate::support::*;
use bella_core::job::{LeaseSpec, Priority, QueueReasonCode as Code};
use bella_core::machine::AvailabilityFlag as Flag;
use bella_core::platform::{
    CapabilityRequirement, CapabilityState, IsolationLevel, Os, PlacementChoice, PlatformSelector,
};
use bella_core::resources::ResourceVector;
use bella_core::{JobId, UnixMillis};
use bella_scheduler::{
    MachineState, QueuedJob, effective_capacity, evaluate, explain, job_blocker, schedule,
};

fn mac(name: &str) -> MachineState {
    let mut m = machine(name, 8, 32);
    m.platform = MAC;
    m.os_version = "15.1".to_string();
    m
}

fn windows(name: &str) -> MachineState {
    let mut m = machine(name, 8, 32);
    m.platform = WINDOWS;
    m.os_version = "10.0.26100".to_string();
    m.case_sensitive_fs = false;
    m
}

/// Schedule one job against one machine and return the machine's blocker.
#[track_caller]
fn sole_blocker(m: MachineState, queued: QueuedJob) -> bella_core::job::MachineBlocker {
    let p = project("app");
    let name = m.name.clone();
    let state = fleet(vec![m], vec![p], vec![queued.clone()]);
    let plan = schedule(&state);
    blocker_on(reason_for(&plan, &queued), &name).clone()
}

#[test]
fn a_recipe_without_build_hosts_is_pinned_to_the_submitting_platform() {
    let p = project("app");
    let mut pinned = spec(2, 4);
    pinned.compatibility.build_hosts.clear();
    let queued = job("pinned", &p, 0, pinned);
    let linux = machine("linux-box", 8, 32);
    let state = fleet(
        vec![mac("mac-box"), linux.clone()],
        vec![p],
        vec![queued.clone()],
    );

    let plan = schedule(&state);
    assert_eq!(offered_machine(&plan, &queued), linux.id);

    let verdicts = evaluate(&state, &queued);
    let on_mac = verdicts
        .iter()
        .find(|(id, _)| *id != linux.id)
        .and_then(|(_, verdict)| verdict.clone().err())
        .expect("the mac is blocked");
    assert_eq!(on_mac.code, Code::WrongPlatform);
    assert!(on_mac.permanent);
    assert!(
        on_mac.message.contains("linux-x86_64"),
        "{}",
        on_mac.message
    );
    assert!(
        on_mac.message.contains("macos-aarch64"),
        "{}",
        on_mac.message
    );
}

#[test]
fn build_hosts_must_match_one_selector_including_the_os_version() {
    let p = project("app");
    let mut needs_new_mac = spec(2, 4);
    needs_new_mac.compatibility.build_hosts = vec![PlatformSelector {
        os: Some(Os::Macos),
        arch: None,
        os_version: Some(">=14".to_string()),
    }];
    let queued = job("ios", &p, 0, needs_new_mac);

    let mut old = mac("old-mac");
    old.os_version = "12.7.6".to_string();
    let found = sole_blocker(old, queued.clone());
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
    assert!(found.message.contains(">=14"), "{}", found.message);
    assert!(found.message.contains("12.7.6"), "{}", found.message);

    let new = mac("new-mac");
    let plan = schedule(&fleet(vec![new.clone()], vec![p], vec![queued.clone()]));
    assert_eq!(offered_machine(&plan, &queued), new.id);
}

/// A cross-compiled binary is never "validated" on the wrong host: when the
/// recipe names a test runtime, a machine that could build but is not that
/// runtime gets no offer.
#[test]
fn a39_cross_compiled_work_is_not_validated_on_the_wrong_host() {
    let p = project("app");
    let mut cross = spec(2, 4);
    cross.compatibility.build_hosts = vec![
        PlatformSelector::os(Os::Linux),
        PlatformSelector::os(Os::Windows),
    ];
    cross.compatibility.artifact_target = Some("x86_64-pc-windows-msvc".to_string());
    cross.compatibility.test_runtime = Some(PlatformSelector::os(Os::Windows));
    let queued = job("cross", &p, 0, cross);

    // Only the Linux builder exists: it can compile, but nothing is offered.
    let linux = machine("linux-box", 16, 64);
    let alone = fleet(vec![linux.clone()], vec![p.clone()], vec![queued.clone()]);
    let plan = schedule(&alone);
    assert!(plan.offers.is_empty());
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    let found = blocker_on(reason, "linux-box");
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
    assert!(
        found.message.contains("tests must run on windows-any"),
        "{}",
        found.message
    );
    assert!(found.message.contains("linux-x86_64"), "{}", found.message);

    // With a Windows machine it goes there, even though Linux has more room.
    let win = windows("win-box");
    let both = fleet(vec![linux, win.clone()], vec![p], vec![queued.clone()]);
    assert_eq!(offered_machine(&schedule(&both), &queued), win.id);
}

#[test]
fn require_is_a_hard_constraint_and_prefer_is_not() {
    let p = project("app");
    let linux = machine("linux-box", 8, 32);

    let mut required = spec(2, 4);
    required.placement = PlacementChoice::Require {
        platform: PlatformSelector::os(Os::Macos),
    };
    let must = job("must", &p, 0, required);
    let found = sole_blocker(linux.clone(), must);
    assert_eq!(found.code, Code::PlacementConstraint);
    assert!(found.permanent);
    assert!(
        found.message.contains("requires macos-any"),
        "{}",
        found.message
    );

    let mut preferred = spec(2, 4);
    preferred.placement = PlacementChoice::Prefer {
        platform: PlatformSelector::os(Os::Macos),
    };
    let may = job("may", &p, 0, preferred);
    let plan = schedule(&fleet(vec![linux.clone()], vec![p], vec![may.clone()]));
    assert_eq!(offered_machine(&plan, &may), linux.id);
}

#[test]
fn a_machine_choice_matches_the_id_or_a_unique_name() {
    let p = project("app");
    let a = machine("alpha", 8, 32);
    let b = machine("beta", 8, 32);

    let by_name = {
        let mut s = spec(2, 4);
        s.placement = PlacementChoice::Machine {
            machine: "beta".to_string(),
        };
        job("by-name", &p, 0, s)
    };
    let by_id = {
        let mut s = spec(2, 4);
        s.placement = PlacementChoice::Machine {
            machine: a.id.to_string(),
        };
        job("by-id", &p, 0, s)
    };
    let state = fleet(
        vec![a.clone(), b.clone()],
        vec![p.clone()],
        vec![by_name.clone(), by_id.clone()],
    );
    let plan = schedule(&state);
    assert_eq!(offered_machine(&plan, &by_name), b.id);
    assert_eq!(offered_machine(&plan, &by_id), a.id);

    let other = evaluate(&state, &by_name)
        .into_iter()
        .find(|(id, _)| *id == a.id)
        .and_then(|(_, verdict)| verdict.err())
        .expect("alpha is excluded");
    assert_eq!(other.code, Code::PlacementConstraint);
    assert!(other.permanent);
    assert!(
        other.message.contains("requires machine `beta`"),
        "{}",
        other.message
    );

    // Two machines called "beta": the name no longer identifies one.
    let mut twin = machine("beta-2", 8, 32);
    twin.name = "beta".to_string();
    let ambiguous = fleet(vec![a, b, twin], vec![p], vec![by_name.clone()]);
    let plan = schedule(&ambiguous);
    let reason = reason_for(&plan, &by_name);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    assert!(
        reason
            .blockers
            .iter()
            .any(|blocker| blocker.message.contains("a name 2 machines share")),
        "{reason:?}"
    );
}

#[test]
fn a_pool_choice_matches_the_machine_policy() {
    let p = project("app");
    let mut pooled = spec(2, 4);
    pooled.placement = PlacementChoice::Pool {
        pool: "release".to_string(),
    };
    let queued = job("pooled", &p, 0, pooled);

    let outside = machine("outside", 8, 32);
    let mut inside = machine("inside", 8, 32);
    inside.policy.pools = vec!["release".to_string()];

    let found = sole_blocker(outside.clone(), queued.clone());
    assert_eq!(found.code, Code::PlacementConstraint);
    assert!(found.permanent);
    assert!(
        found.message.contains("requires pool `release`"),
        "{}",
        found.message
    );

    let plan = schedule(&fleet(
        vec![outside, inside.clone()],
        vec![p],
        vec![queued.clone()],
    ));
    assert_eq!(offered_machine(&plan, &queued), inside.id);
}

/// A job requiring `xcode >=16` offered to a machine with Xcode 14.2 is
/// rejected, naming both versions.
#[test]
fn a38_a_toolchain_version_mismatch_names_both_versions() {
    let p = project("ios");
    let mut needs_xcode = spec(4, 8);
    needs_xcode.compatibility.requires = vec![CapabilityRequirement {
        name: "xcode".to_string(),
        version: Some(">=16".to_string()),
        attributes: Default::default(),
    }];
    let queued = job("archive", &p, 0, needs_xcode);

    let mut old = mac("old-mac");
    old.capabilities = vec![capability("xcode", "14.2")];
    let state = fleet(vec![old.clone()], vec![p.clone()], vec![queued.clone()]);
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    let found = blocker_on(reason, "old-mac");
    assert_eq!(found.code, Code::MissingCapability);
    assert!(found.permanent);
    assert!(found.message.contains("14.2"), "{}", found.message);
    assert!(found.message.contains(">=16"), "{}", found.message);
    assert!(reason.message.contains("14.2") && reason.message.contains(">=16"));

    let mut current = mac("new-mac");
    current.capabilities = vec![capability("xcode", "16.2")];
    let plan = schedule(&fleet(
        vec![old, current.clone()],
        vec![p],
        vec![queued.clone()],
    ));
    assert_eq!(offered_machine(&plan, &queued), current.id);
}

/// A job that needs a GUI session is not offered to a headless machine.
#[test]
fn a40_a_gui_session_requirement_excludes_headless_machines() {
    let p = project("app");
    let mut ui_tests = spec(2, 4);
    ui_tests.compatibility.requires = vec![CapabilityRequirement::named("gui-session")];
    let queued = job("ui-tests", &p, 0, ui_tests);

    let headless = machine("headless", 16, 64);
    let found = sole_blocker(headless.clone(), queued.clone());
    assert_eq!(found.code, Code::MissingCapability);
    assert!(found.permanent);
    assert!(
        found.message.contains("`gui-session` is not installed"),
        "{}",
        found.message
    );

    let mut desktop = machine("desktop", 8, 16);
    desktop.capabilities = vec![capability("gui-session", "1")];
    let plan = schedule(&fleet(
        vec![headless, desktop.clone()],
        vec![p],
        vec![queued.clone()],
    ));
    assert_eq!(offered_machine(&plan, &queued), desktop.id);
}

#[test]
fn an_occupied_capability_is_a_temporary_blocker() {
    let p = project("ios");
    let mut needs_sim = spec(2, 4);
    needs_sim.compatibility.requires = vec![CapabilityRequirement::named("ios-simulator")];
    let queued = job("sim-tests", &p, 0, needs_sim);

    let mut busy = mac("mac-box");
    let mut simulator = capability("ios-simulator", "18.1");
    simulator.state = CapabilityState::Occupied;
    busy.capabilities = vec![simulator];

    let plan = schedule(&fleet(vec![busy], vec![p], vec![queued.clone()]));
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::MissingCapability);
    let found = blocker_on(reason, "mac-box");
    assert!(!found.permanent);
    assert!(found.message.contains("Occupied"), "{}", found.message);
}

#[test]
fn isolation_above_native_is_not_offered_in_v1() {
    let p = project("app");
    let mut contained = spec(2, 4);
    contained.compatibility.isolation = IsolationLevel::Container;
    let found = sole_blocker(machine("box", 8, 32), job("contained", &p, 0, contained));
    assert_eq!(found.code, Code::MissingCapability);
    assert!(found.permanent);
    assert!(
        found.message.contains("container isolation"),
        "{}",
        found.message
    );
    assert!(
        found.message.contains("native execution only"),
        "{}",
        found.message
    );
}

#[test]
fn a_snapshot_is_matched_against_the_machine_filesystem() {
    let p = project("app");

    let mut reserved_name = job("aux", &p, 0, spec(2, 4));
    reserved_name.snapshot_windows_incompatible = Some("`src/aux.rs` is a reserved name".into());
    let found = sole_blocker(windows("win-box"), reserved_name.clone());
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
    assert!(
        found.message.contains("`src/aux.rs` is a reserved name"),
        "{}",
        found.message
    );
    // The same snapshot is fine on Linux.
    let linux = machine("linux-box", 8, 32);
    let plan = schedule(&fleet(
        vec![linux.clone()],
        vec![p.clone()],
        vec![reserved_name.clone()],
    ));
    assert_eq!(offered_machine(&plan, &reserved_name), linux.id);

    let mut collides = job("case", &p, 0, spec(2, 4));
    collides.snapshot_case_sensitive_only = true;
    let found = sole_blocker(mac_insensitive("mac-box"), collides);
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
    assert!(
        found.message.contains("case-insensitive"),
        "{}",
        found.message
    );

    let mut links = job("links", &p, 0, spec(2, 4));
    links.snapshot_has_symlinks = true;
    let mut no_links = machine("no-links", 8, 32);
    no_links.supports_symlinks = false;
    let found = sole_blocker(no_links, links);
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
    assert!(
        found.message.contains("symbolic links"),
        "{}",
        found.message
    );
}

fn mac_insensitive(name: &str) -> MachineState {
    let mut m = mac(name);
    m.case_sensitive_fs = false;
    m
}

#[test]
fn machine_policy_can_refuse_builds_and_projects() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(2, 4));

    let mut infra = machine("infra", 8, 32);
    infra.policy.accept_builds = false;
    let found = sole_blocker(infra, queued.clone());
    assert_eq!(found.code, Code::BuildsDisabled);
    assert!(found.permanent);

    let mut flagged = machine("flagged", 8, 32);
    flagged.flags = vec![Flag::BuildsDisabled];
    assert_eq!(
        sole_blocker(flagged, queued.clone()).code,
        Code::BuildsDisabled
    );

    let other = project("other");
    let mut private = machine("private", 8, 32);
    private.policy.allowed_projects = Some(vec![other.id.clone()]);
    let found = sole_blocker(private.clone(), queued.clone());
    assert_eq!(found.code, Code::ProjectNotAllowed);
    assert!(found.permanent);
    assert!(found.message.contains("project `app`"), "{}", found.message);

    let allowed = job("allowed", &other, 0, spec(2, 4));
    let plan = schedule(&fleet(
        vec![private.clone()],
        vec![p, other],
        vec![allowed.clone()],
    ));
    assert_eq!(offered_machine(&plan, &allowed), private.id);
}

/// Source marked LAN-only never goes to a machine off the LAN, whatever
/// the priority or placement choice says.
#[test]
fn a55_lan_only_source_never_leaves_the_lan() {
    let mut remote = machine("cloud-1", 64, 256);
    remote.on_lan = false;
    let local = machine("local-1", 4, 8);

    // The project forbids it; the job is urgent and names the machine.
    let mut p = project("secret");
    p.policy.source_lan_only = true;
    p.policy.allow_urgent = true;
    let mut pushy = spec(2, 4);
    pushy.priority = Priority::Urgent;
    pushy.placement = PlacementChoice::Machine {
        machine: "cloud-1".to_string(),
    };
    let queued = job("pushy", &p, 3_600_000, pushy);
    let state = fleet(
        vec![remote.clone(), local.clone()],
        vec![p.clone()],
        vec![queued.clone()],
    );
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    let found = blocker_on(reason, "cloud-1");
    assert_eq!(found.code, Code::SourceLanOnly);
    assert!(found.permanent);
    assert!(
        found.message.contains("project `secret`"),
        "{}",
        found.message
    );

    // The recipe alone forbids it too, and the job runs on the LAN instead
    // even though the remote machine has far more room.
    let open = project("open");
    let mut lan_only = spec(2, 4);
    lan_only.source_lan_only = true;
    let queued = job("lan-only", &open, 0, lan_only);
    let state = fleet(
        vec![remote, local.clone()],
        vec![open],
        vec![queued.clone()],
    );
    let plan = schedule(&state);
    assert_eq!(offered_machine(&plan, &queued), local.id);
    let off_lan = evaluate(&state, &queued)
        .into_iter()
        .find(|(id, _)| *id != local.id)
        .and_then(|(_, verdict)| verdict.err())
        .expect("the remote machine is blocked");
    assert_eq!(off_lan.code, Code::SourceLanOnly);
    assert!(
        off_lan.message.contains("the recipe"),
        "{}",
        off_lan.message
    );
}

#[test]
fn a_request_above_the_ceiling_names_requested_and_grantable() {
    let p = project("app");
    let found = sole_blocker(machine("small", 8, 10), job("big", &p, 0, spec(2, 18)));
    assert_eq!(found.code, Code::MachineTooSmall);
    assert!(found.permanent);
    assert_eq!(
        found.message,
        "needs 18 GiB memory; this machine can grant at most 10 GiB"
    );

    let found = sole_blocker(machine("small", 8, 64), job("wide", &p, 0, spec(12, 4)));
    assert_eq!(found.code, Code::MachineTooSmall);
    assert_eq!(
        found.message,
        "needs 12 CPUs; this machine can grant at most 8 CPUs"
    );

    let mut needs_gpu = spec(2, 4);
    needs_gpu.envelope.peak.tokens.insert("gpu".to_string(), 1);
    let found = sole_blocker(machine("no-gpu", 8, 64), job("train", &p, 0, needs_gpu));
    assert_eq!(found.code, Code::MachineTooSmall);
    assert_eq!(
        found.message,
        "needs 1 `gpu` token; this machine can grant at most 0"
    );
}

#[test]
fn each_temporary_condition_has_its_own_code() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(2, 4));
    type Tweak = fn(&mut MachineState);
    let cases: Vec<(&str, Tweak, Code, &str)> = vec![
        (
            "offline",
            |m| m.connected = false,
            Code::MachineUnreachable,
            "not connected",
        ),
        (
            "unreachable",
            |m| m.flags = vec![Flag::Unreachable],
            Code::MachineUnreachable,
            "out of contact",
        ),
        (
            "asleep",
            |m| m.flags = vec![Flag::Sleeping],
            Code::MachineUnreachable,
            "asleep",
        ),
        (
            "stale",
            |m| m.telemetry_age_ms = Some(20_000),
            Code::StaleTelemetry,
            "20 s old; offers need it newer than 15 s",
        ),
        (
            "stale-flag",
            |m| m.flags = vec![Flag::Stale],
            Code::StaleTelemetry,
            "heartbeat is overdue",
        ),
        (
            "no-telemetry",
            |m| m.telemetry_age_ms = None,
            Code::StaleTelemetry,
            "has not reported telemetry",
        ),
        (
            "no-budget",
            |m| m.effective_budget = None,
            Code::StaleTelemetry,
            "has not reported a protection budget",
        ),
        (
            "unreconciled",
            |m| m.reconciled = false,
            Code::CleanupPending,
            "still being reconciled",
        ),
        (
            "quarantined",
            |m| m.flags = vec![Flag::Quarantined],
            Code::Quarantined,
            "could not be verified",
        ),
        (
            "upgrading",
            |m| m.flags = vec![Flag::Upgrading],
            Code::MachineUpgrading,
            "being upgraded",
        ),
        (
            "paused",
            |m| m.flags = vec![Flag::UserPaused],
            Code::MachinePaused,
            "paused",
        ),
        (
            "draining",
            |m| m.flags = vec![Flag::Draining],
            Code::MachineDraining,
            "draining",
        ),
        (
            "battery",
            |m| m.flags = vec![Flag::OnBattery],
            Code::OnBattery,
            "on battery",
        ),
        (
            "hot",
            |m| m.flags = vec![Flag::Overheated],
            Code::Overheated,
            "overheated",
        ),
        (
            "pressure",
            |m| m.flags = vec![Flag::PressureLimited],
            Code::MemoryPressure,
            "memory pressure",
        ),
        (
            "disk",
            |m| m.flags = vec![Flag::DiskLow],
            Code::DiskLow,
            "free-space floor",
        ),
        (
            "not-admitting",
            |m| m.admitting = false,
            Code::ForegroundProtection,
            "not admitting new work",
        ),
    ];
    for (name, tweak, code, text) in cases {
        let mut m = machine(name, 8, 32);
        tweak(&mut m);
        let state = fleet(vec![m], vec![p.clone()], vec![queued.clone()]);
        let plan = schedule(&state);
        assert!(plan.offers.is_empty(), "{name}: nothing may be offered");
        let reason = reason_for(&plan, &queued);
        assert_eq!(reason.code, code, "{name}");
        let found = blocker_on(reason, name);
        assert_eq!(found.code, code, "{name}");
        assert!(!found.permanent, "{name} can clear by waiting");
        assert!(found.message.contains(text), "{name}: {}", found.message);
        assert!(
            reason.message.starts_with(&format!("{name}: ")),
            "{}",
            reason.message
        );
    }
}

#[test]
fn telemetry_exactly_at_the_threshold_is_still_fresh() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(2, 4));
    let mut m = machine("box", 8, 32);
    m.telemetry_age_ms = Some(15_000);
    let plan = schedule(&fleet(vec![m.clone()], vec![p], vec![queued.clone()]));
    assert_eq!(offered_machine(&plan, &queued), m.id);
}

#[test]
fn a_recent_rejection_keeps_the_job_off_that_machine_until_it_expires() {
    let p = project("app");
    let a = machine("alpha", 8, 32);
    let b = machine("beta", 4, 8);
    let mut queued = job("build", &p, 0, spec(2, 4));
    queued.recently_rejected_by = vec![(a.id.clone(), UnixMillis(NOW + 30_000))];

    // Alpha has more headroom and would win, but it declined this job.
    let state = fleet(
        vec![a.clone(), b.clone()],
        vec![p.clone()],
        vec![queued.clone()],
    );
    assert_eq!(offered_machine(&schedule(&state), &queued), b.id);

    let alone = fleet(vec![a.clone()], vec![p.clone()], vec![queued.clone()]);
    let plan = schedule(&alone);
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::OfferRejected);
    let found = blocker_on(reason, "alpha");
    assert!(!found.permanent);
    assert!(found.message.contains("again in 30 s"), "{}", found.message);

    // Once the avoidance window has passed the machine is offered again.
    queued.recently_rejected_by = vec![(a.id.clone(), UnixMillis(NOW))];
    let state = fleet(vec![a.clone(), b], vec![p], vec![queued.clone()]);
    assert_eq!(offered_machine(&schedule(&state), &queued), a.id);
}

#[test]
fn a_full_machine_reports_the_concurrency_limit() {
    let p = project("app");
    let mut m = machine("workstation", 8, 32);
    m.policy.max_concurrent_attempts = Some(1);
    m.held_attempts = 1;
    m.reserved = vector(1, 1);
    let found = sole_blocker(m, job("build", &p, 0, spec(2, 4)));
    assert_eq!(found.code, Code::MachineConcurrencyLimit);
    assert!(!found.permanent);
    assert_eq!(found.message, "runs 1 of at most 1 concurrent attempts");
}

#[test]
fn waiting_for_capacity_names_the_short_dimension() {
    let p = project("app");

    let mut tight_memory = machine("box", 8, 16);
    tight_memory.reserved = vector(2, 10);
    tight_memory.held_attempts = 1;
    let found = sole_blocker(tight_memory, job("build", &p, 0, spec(2, 8)));
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert!(!found.permanent);
    assert_eq!(
        found.message,
        "needs 8 GiB memory; 6 GiB free (10 GiB of 16 GiB reserved)"
    );

    let mut tight_cpu = machine("box", 8, 64);
    tight_cpu.reserved = vector(6, 4);
    let found = sole_blocker(tight_cpu, job("build", &p, 0, spec(4, 8)));
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert_eq!(
        found.message,
        "needs 4 CPUs; 2 CPUs free (6 CPUs of 8 CPUs reserved)"
    );

    let mut one_simulator = machine("box", 8, 64);
    one_simulator
        .managed_ceiling
        .tokens
        .insert("ios-simulator".into(), 1);
    one_simulator.effective_budget = Some(one_simulator.managed_ceiling.clone());
    one_simulator.reserved = vector(1, 1).with_token("ios-simulator", 1);
    let mut needs_sim = spec(1, 1);
    needs_sim
        .envelope
        .peak
        .tokens
        .insert("ios-simulator".into(), 1);
    let found = sole_blocker(one_simulator, job("sim", &p, 0, needs_sim));
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert_eq!(
        found.message,
        "needs 1 `ios-simulator` token; 0 free (1 of 1 reserved)"
    );
}

#[test]
fn the_protection_budget_narrows_the_ceiling() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(2, 8));

    // 32 GiB ceiling, but the worker currently grants only 6 GiB.
    let mut squeezed = machine("workstation", 8, 32);
    squeezed.effective_budget = Some(vector(8, 6));
    let found = sole_blocker(squeezed.clone(), queued.clone());
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert!(
        !found.permanent,
        "the budget can recover; the ceiling is large enough"
    );
    assert_eq!(
        found.message,
        "needs 8 GiB memory; 6 GiB free (0 B of 6 GiB reserved); \
         the worker currently grants 6 GiB of its 32 GiB ceiling"
    );

    // A budget above the ceiling grants nothing beyond the ceiling.
    let mut generous = machine("builder", 8, 16);
    generous.effective_budget = Some(vector(64, 256));
    generous.reserved = vector(0, 10);
    let found = sole_blocker(generous, queued);
    assert_eq!(found.code, Code::WaitingForCapacity);
    assert!(
        found.message.contains("of 16 GiB reserved"),
        "{}",
        found.message
    );
}

#[test]
fn effective_capacity_is_the_smaller_of_ceiling_and_budget() {
    let ceiling = ResourceVector::new(8000, 32 * GIB, 100 * GIB)
        .with_token("gpu", 2)
        .with_token("ios-simulator", 4);
    assert_eq!(effective_capacity(&ceiling, None), ceiling);

    let budget = ResourceVector::new(6000, 64 * GIB, 40 * GIB)
        .with_token("gpu", 1)
        .with_token("undeclared", 9);
    let capacity = effective_capacity(&ceiling, Some(&budget));
    assert_eq!(capacity.cpu_milli, 6000);
    assert_eq!(capacity.memory_bytes, 32 * GIB);
    assert_eq!(capacity.disk_bytes, 40 * GIB);
    // Narrowed where the budget names the token. A worker's budget names
    // every token it declares, so one it is silent about is not granted.
    assert_eq!(capacity.token("gpu"), 1);
    assert_eq!(capacity.token("ios-simulator"), 0);
    // A token the ceiling does not declare is never granted.
    assert_eq!(capacity.token("undeclared"), 0);
    assert_eq!(capacity, ceiling.min(&budget));
}

#[test]
fn a_permanent_blocker_is_reported_before_a_temporary_one() {
    let p = project("app");
    let mut pinned = spec(2, 4);
    pinned.compatibility.build_hosts = vec![PlatformSelector::os(Os::Macos)];
    let queued = job("mac-only", &p, 0, pinned);

    // Offline *and* the wrong platform: waiting for it to return is pointless.
    let mut offline_linux = machine("linux-box", 8, 32);
    offline_linux.connected = false;
    let plan = schedule(&fleet(vec![offline_linux], vec![p], vec![queued.clone()]));
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    assert!(
        reason
            .message
            .contains("none of the 1 enrolled machine can run"),
        "{}",
        reason.message
    );
    let found = blocker_on(reason, "linux-box");
    assert_eq!(found.code, Code::WrongPlatform);
    assert!(found.permanent);
}

#[test]
fn the_job_reason_is_the_temporary_blocker_closest_to_clearing() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(4, 8));

    let wrong = mac("mac-box");
    let mut offline = machine("offline", 16, 64);
    offline.connected = false;
    let mut paused = machine("paused", 16, 64);
    paused.flags = vec![Flag::UserPaused];
    let mut busy = machine("busy", 16, 64);
    busy.reserved = vector(14, 60);

    let mut needs_linux = (*queued.spec).clone();
    needs_linux.compatibility.build_hosts = vec![PlatformSelector::os(Os::Linux)];
    let queued = QueuedJob {
        spec: std::sync::Arc::new(needs_linux),
        ..queued
    };

    let state = fleet(
        vec![wrong, offline, paused, busy],
        vec![p],
        vec![queued.clone()],
    );
    let plan = schedule(&state);
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::WaitingForCapacity);
    assert!(
        reason
            .message
            .starts_with("busy: needs 4 CPUs; 2 CPUs free"),
        "{}",
        reason.message
    );
    assert!(
        reason
            .message
            .contains("2 other compatible machines cannot take it right now either"),
        "{}",
        reason.message
    );
    // Every machine's blocker is attached, permanent ones included.
    assert_eq!(reason.blockers.len(), 4);
    assert_eq!(blocker_on(reason, "mac-box").code, Code::WrongPlatform);
    assert_eq!(blocker_on(reason, "offline").code, Code::MachineUnreachable);
    assert_eq!(blocker_on(reason, "paused").code, Code::MachinePaused);
    assert_eq!(blocker_on(reason, "busy").code, Code::WaitingForCapacity);
}

#[test]
fn an_empty_fleet_explains_itself() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(1, 1));
    let plan = schedule(&fleet(Vec::new(), vec![p], vec![queued.clone()]));
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::NoCompatibleMachine);
    assert_eq!(reason.message, "no machines are enrolled in this fleet");
}

#[test]
fn a_job_with_an_unresolved_attempt_is_not_offered_again() {
    let p = project("app");
    let mut queued = job("retry", &p, 0, spec(1, 1));
    queued.has_unresolved_attempt = true;
    let state = fleet(vec![machine("box", 8, 32)], vec![p], vec![queued.clone()]);
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    assert_eq!(
        reason_for(&plan, &queued).code,
        Code::PreviousAttemptUnresolved
    );
    assert_eq!(
        job_blocker(&state, &queued).map(|reason| reason.code),
        Some(Code::PreviousAttemptUnresolved)
    );
}

#[test]
fn a_project_at_its_attempt_limit_waits() {
    let mut p = project("app");
    p.policy.max_concurrent_attempts = Some(2);
    p.held_attempts = 2;
    let queued = job("third", &p, 0, spec(1, 1));
    let state = fleet(vec![machine("box", 8, 32)], vec![p], vec![queued.clone()]);
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    let reason = reason_for(&plan, &queued);
    assert_eq!(reason.code, Code::ProjectConcurrencyLimit);
    assert_eq!(
        reason.message,
        "project `app` runs 2 of at most 2 concurrent attempts"
    );
}

#[test]
fn a_project_at_its_environment_limit_starts_no_new_environment() {
    let mut p = project("app");
    p.policy.max_environments = Some(1);
    p.environments = 1;
    let mut with_env = spec(1, 1);
    with_env.environment = Some(LeaseSpec::default());
    let keeps_running = job("dev-server", &p, 10, with_env);
    let plain = job("unit-tests", &p, 0, spec(1, 1));
    let state = fleet(
        vec![machine("box", 8, 32)],
        vec![p],
        vec![keeps_running.clone(), plain.clone()],
    );
    let plan = schedule(&state);
    let reason = reason_for(&plan, &keeps_running);
    assert_eq!(reason.code, Code::EnvironmentQuota);
    assert!(
        reason.message.contains("1 of at most 1 environments"),
        "{}",
        reason.message
    );
    // Work that keeps no environment is unaffected.
    assert!(offer_for(&plan, &plain).is_some());
}

#[test]
fn nothing_is_offered_while_the_controller_reconciles() {
    let p = project("app");
    let a = job("a", &p, 10, spec(1, 1));
    let b = job("b", &p, 0, spec(1, 1));
    let mut state = fleet(
        vec![machine("box", 8, 32)],
        vec![p],
        vec![a.clone(), b.clone()],
    );
    state.reconciling = true;
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    assert_eq!(reason_for(&plan, &a).code, Code::ControllerReconciling);
    assert_eq!(reason_for(&plan, &b).code, Code::ControllerReconciling);
    assert_eq!(
        job_blocker(&state, &a).map(|reason| reason.code),
        Some(Code::ControllerReconciling)
    );
}

#[test]
fn a_job_of_an_unknown_project_is_never_offered() {
    let known = project("known");
    let ghost = project("ghost");
    let queued = job("orphan", &ghost, 0, spec(1, 1));
    let state = fleet(
        vec![machine("box", 8, 32)],
        vec![known],
        vec![queued.clone()],
    );
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    assert_eq!(reason_for(&plan, &queued).code, Code::ProjectNotAllowed);
    let verdicts = evaluate(&state, &queued);
    let found = verdicts[0].1.clone().expect_err("no policy, no placement");
    assert_eq!(found.code, Code::ProjectNotAllowed);
}

#[test]
fn explain_answers_for_offered_waiting_and_unknown_jobs() {
    let p = project("app");
    let fits = job("fits", &p, 10, spec(2, 4));
    let too_big_now = job("waits", &p, 0, spec(8, 30));
    let m = machine("box", 8, 32);
    let state = fleet(
        vec![m],
        vec![p.clone()],
        vec![fits.clone(), too_big_now.clone()],
    );

    let ready = explain(&state, &fits.id).expect("the job is queued");
    assert_eq!(ready.code, Code::Schedulable);
    assert!(
        ready.message.contains("about to be offered to box"),
        "{}",
        ready.message
    );

    // The second job would fit an idle machine, but not after the first.
    let waiting = explain(&state, &too_big_now.id).expect("the job is queued");
    assert_eq!(waiting.code, Code::WaitingForCapacity);
    assert_eq!(
        Some(&waiting),
        schedule(&state).reasons.get(&too_big_now.id)
    );

    assert_eq!(explain(&state, &JobId::derive(b"never-submitted")), None);
}

#[test]
fn evaluate_reports_every_machine_in_id_order() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(4, 8));
    let small = machine("small", 2, 4);
    let state = fleet(
        vec![machine("zeta", 8, 32), small, machine("alpha", 8, 32)],
        vec![p],
        vec![],
    );
    let verdicts = evaluate(&state, &queued);
    assert_eq!(verdicts.len(), 3);
    let ids: Vec<_> = verdicts.iter().map(|(id, _)| id.clone()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    let ok = verdicts
        .iter()
        .filter(|(_, verdict)| verdict.is_ok())
        .count();
    assert_eq!(ok, 2);
    let blocked = verdicts
        .iter()
        .find_map(|(_, verdict)| verdict.clone().err())
        .expect("the small machine is blocked");
    assert_eq!(blocked.machine, "small");
    assert_eq!(blocked.code, Code::MachineTooSmall);
}
