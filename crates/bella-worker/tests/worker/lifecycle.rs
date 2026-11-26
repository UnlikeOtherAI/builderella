//! An attempt from offer to release: steps, services, environments,
//! outputs, timeouts, cancellation and cleanup.

use crate::support::*;
use bella_core::job::{LeaseSpec, Outcome, ReadinessProbe, StepKind};
use bella_core::protocol::{
    ArtifactKind, AttemptEvent, CommandResult, ControllerCommand, LocalAttemptState, LogStream,
    StepState,
};
use bella_core::resources::Enforcement;
use bella_core::store::ObjectStore;
use std::time::Duration;

fn position(events: &[&AttemptEvent], pick: impl Fn(&AttemptEvent) -> bool) -> usize {
    events
        .iter()
        .position(|event| pick(event))
        .unwrap_or_else(|| panic!("event missing from {events:#?}"))
}

#[test]
fn happy_path_streams_logs_publishes_artifacts_and_releases_with_proof() {
    let mut h = Harness::new();
    h.start_connected();
    let mut spec = spec(vec![
        helper("build", "lines|3|0"),
        helper("make", "write|result.bin|1000|0").needs(&["build"]),
        helper("report", "write|report.xml|10|0")
            .kind(StepKind::Test)
            .needs(&["make"]),
    ]);
    spec.outputs.artifacts = vec!["*.bin".to_string()];
    spec.outputs.test_reports = vec!["*.xml".to_string()];
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();

    let proof = h.ctl.wait_released(&id);
    assert!(proof.scope_empty && proof.workspace_removed, "{proof:?}");
    assert!(!proof.method.is_empty());
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));

    let events = h.ctl.events_of(&id);
    let preparing = position(&events, |e| matches!(e, AttemptEvent::Preparing));
    let started = position(&events, |e| matches!(e, AttemptEvent::Started { .. }));
    let collecting = position(&events, |e| matches!(e, AttemptEvent::Collecting));
    let published = position(&events, |e| {
        matches!(e, AttemptEvent::ArtifactPublished { .. })
    });
    let exited = position(&events, |e| matches!(e, AttemptEvent::Exited { .. }));
    let released = position(&events, |e| matches!(e, AttemptEvent::Released { .. }));
    assert!(preparing < started && started < collecting && collecting < published);
    assert!(published < exited && exited < released, "{events:#?}");
    assert_eq!(released, events.len() - 1, "nothing follows the release");

    let artifacts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AttemptEvent::ArtifactPublished {
                path,
                kind,
                digest,
                size,
            } => Some((path.clone(), *kind, digest.clone(), *size)),
            _ => None,
        })
        .collect();
    assert_eq!(artifacts.len(), 2, "{artifacts:?}");
    for (path, kind, digest, size) in &artifacts {
        let expected = if path == "result.bin" {
            (ArtifactKind::Artifact, 1000)
        } else {
            assert_eq!(path, "report.xml");
            (ArtifactKind::TestReport, 10)
        };
        assert_eq!((*kind, *size), expected);
        assert!(h.remote.contains(digest).unwrap(), "{path} is in the store");
    }

    for step in ["build", "make", "report"] {
        assert_eq!(
            h.ctl.step_states(&id, step),
            [StepState::Running, StepState::Succeeded],
            "{step}"
        );
    }
    h.ctl.wait("all log bytes", |ctl| {
        let out = ctl.log(&id, "build", LogStream::Stdout);
        let err = ctl.log(&id, "build", LogStream::Stderr);
        (out.contains("line 0002") && err.contains("last words")).then_some(())
    });
    let system = h.ctl.log(&id, bella_worker::SYSTEM_STEP, LogStream::System);
    assert!(system.contains("materialized 1 file(s)"), "{system}");

    let record = h.journal().attempt(&id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::Released);
    h.assert_no_workspaces();
    h.stop().unwrap();
}

#[test]
fn a19_a_service_and_a_test_step_are_alive_at_the_same_time() {
    let mut h = Harness::new();
    h.start_connected();
    let port = free_port();
    let offer = h.launch(spec(vec![
        helper("db", &format!("listen|{port}"))
            .kind(StepKind::Service)
            .ready(ReadinessProbe::Tcp { port }, 30)
            .port(port),
        helper("tests", &format!("connect|{port}"))
            .kind(StepKind::Test)
            .needs(&["db"]),
    ]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(
        h.ctl.outcome(&id),
        Some(Outcome::Succeeded),
        "{}",
        h.ctl.dump()
    );
    assert_eq!(
        h.ctl.step_states(&id, "db"),
        [StepState::Running, StepState::Ready, StepState::Stopped]
    );
    assert_eq!(
        h.ctl.step_states(&id, "tests"),
        [StepState::Running, StepState::Succeeded]
    );
    assert!(
        h.ctl
            .log(&id, "tests", LogStream::Stdout)
            .contains("connected")
    );
    assert!(!port_open(port), "the service ended with the attempt");
    h.assert_no_workspaces();
}

#[test]
fn an_environment_is_held_until_it_is_finished() {
    let mut h = Harness::new();
    h.start_connected();
    let port = free_port();
    let mut spec = spec(vec![
        helper("web", &format!("listen|{port}"))
            .kind(StepKind::Service)
            .ready(
                ReadinessProbe::Http {
                    port,
                    path: "/health".to_string(),
                    status: Some(200),
                },
                30,
            )
            .port(port),
        helper("seed", &format!("connect|{port}")).needs(&["web"]),
    ]);
    spec.environment = Some(LeaseSpec::default());
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();

    let endpoints = h
        .ctl
        .wait_event(&id, "the environment", |event| match event {
            AttemptEvent::EnvironmentReady { endpoints } => Some(endpoints.clone()),
            _ => None,
        });
    assert_eq!(endpoints.len(), 1);
    assert_eq!(
        (endpoints[0].step.as_str(), endpoints[0].port),
        ("web", port)
    );
    assert_eq!(endpoints[0].host, "127.0.0.1");
    // Held: the service stays up and nothing is released.
    h.ctl.pump(Duration::from_millis(500));
    assert!(port_open(port));
    assert_eq!(h.ctl.outcome(&id), None);
    let record = h.journal().attempt(&id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::EnvironmentHeld);

    let finish = h.ctl.command(ControllerCommand::FinishEnvironment {
        attempt: offer.attempt.clone(),
        reason: "done with it".to_string(),
    });
    assert_eq!(h.ctl.wait_ack(&finish), CommandResult::Applied);
    h.ctl.wait_released(&id);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));
    assert!(!port_open(port));
    // A late repeat finds nothing left to finish.
    let again = h.ctl.command(ControllerCommand::FinishEnvironment {
        attempt: offer.attempt.clone(),
        reason: "again".to_string(),
    });
    assert_eq!(h.ctl.wait_ack(&again), CommandResult::Applied);
    h.assert_no_workspaces();
}

#[test]
fn a_step_over_its_timeout_is_timed_out_and_cleanup_still_runs() {
    let mut h = Harness::new();
    h.start_connected();
    let marker = h.outside("cleanup-ran");
    let offer = h.launch(spec(vec![
        helper("slow", "sleep|60").timeout(1),
        helper("after", "lines|1|0").needs(&["slow"]),
        helper("tidy", &format!("append|{marker}|tidy")).kind(StepKind::Cleanup),
    ]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(
        h.ctl.outcome(&id),
        Some(Outcome::TimedOut {
            step: Some("slow".to_string())
        })
    );
    assert_eq!(
        h.ctl.step_states(&id, "slow"),
        [StepState::Running, StepState::TimedOut]
    );
    assert_eq!(h.ctl.step_states(&id, "after"), [StepState::Skipped]);
    assert_eq!(
        h.ctl.step_states(&id, "tidy"),
        [StepState::Running, StepState::Succeeded]
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "tidy\n");
    let note = h.ctl.log(&id, "slow", LogStream::System);
    assert!(note.contains("timeout of 1 s"), "{note}");
}

#[test]
fn a_failed_dependency_skips_its_dependants_and_keeps_the_last_lines() {
    let mut h = Harness::new();
    h.start_connected();
    let marker = h.outside("cleanup-ran");
    let offer = h.launch(spec(vec![
        helper("compile", "lines|200|3"),
        helper("link", "lines|1|0").needs(&["compile"]),
        helper("package", "lines|1|0").needs(&["link"]),
        helper("lint", "lines|1|0"),
        helper("tidy", &format!("append|{marker}|tidy")).kind(StepKind::Cleanup),
    ]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(
        h.ctl.outcome(&id),
        Some(Outcome::Failed {
            step: Some("compile".to_string()),
            exit_code: Some(3)
        })
    );
    assert_eq!(h.ctl.step_states(&id, "link"), [StepState::Skipped]);
    assert_eq!(h.ctl.step_states(&id, "package"), [StepState::Skipped]);
    assert_eq!(
        h.ctl.step_states(&id, "lint"),
        [StepState::Running, StepState::Succeeded]
    );
    assert!(std::fs::read_to_string(&marker).unwrap().contains("tidy"));
    h.ctl.wait("the failing step's last lines", |ctl| {
        let out = ctl.log(&id, "compile", LogStream::Stdout);
        let err = ctl.log(&id, "compile", LogStream::Stderr);
        (out.contains("line 0199") && err.contains("last words")).then_some(())
    });
}

/// The marker the environment test plants in its own process.
const LEAK_MARKER: &str = "BELLA_TEST_LEAK_MARKER";

#[test]
fn a_step_sees_only_the_constructed_environment() {
    if std::env::var_os(LEAK_MARKER).is_none() {
        // Run this test again in a child process that carries the marker,
        // so the marker is really part of the worker's own environment.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "lifecycle::a_step_sees_only_the_constructed_environment",
                "--exact",
                "--test-threads=1",
            ])
            .env(LEAK_MARKER, "leaked-value")
            .status()
            .unwrap();
        assert!(status.success(), "the test failed in its child process");
        return;
    }
    assert_eq!(std::env::var(LEAK_MARKER).unwrap(), "leaked-value");

    let mut h = Harness::new();
    h.start_connected();
    let dump = h.outside("env.txt");
    let mut spec = spec(vec![
        helper("show", &format!("env|{dump}"))
            .var("FROM_STEP", "step")
            .var("SHARED", "step-wins"),
    ]);
    spec.env.insert("FROM_JOB".to_string(), "job".to_string());
    spec.env.insert("SHARED".to_string(), "job".to_string());
    spec.env.insert("BELLA_JOBS".to_string(), "9".to_string());
    spec.parallelism = Some(5);
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));

    let text = std::fs::read_to_string(&dump).unwrap();
    let env: std::collections::BTreeMap<String, String> = text
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect();
    let get = |name: &str| -> Option<&str> {
        env.iter()
            .find(|(key, _)| {
                if cfg!(windows) {
                    key.eq_ignore_ascii_case(name)
                } else {
                    key.as_str() == name
                }
            })
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(
        get(LEAK_MARKER),
        None,
        "the worker's environment leaked: {text}"
    );
    assert!(!text.contains(TOKEN), "the machine token reached a step");
    assert!(
        !text.contains(CONTROLLER_URL),
        "the controller URL reached a step"
    );
    assert!(!text.contains("leaked-value"));
    assert_eq!(get("BELLA_ATTEMPT_ID"), Some(id.as_str()));
    assert_eq!(get("BELLA_JOB_ID"), Some(offer.job_id.as_str()));
    assert_eq!(
        get("BELLA_SNAPSHOT_ID"),
        Some(offer.snapshot_id.digest().as_str())
    );
    assert_eq!(get("BELLA_CPU_MILLI"), Some("2000"));
    assert_eq!(get("BELLA_MEMORY_BYTES"), Some("1073741824"));
    assert_eq!(get("CI"), Some("true"));
    // The recipe's own variables come after the worker's.
    assert_eq!(get("BELLA_JOBS"), Some("9"));
    assert_eq!(get("FROM_JOB"), Some("job"));
    assert_eq!(get("FROM_STEP"), Some("step"));
    assert_eq!(get("SHARED"), Some("step-wins"));
    let workspace = get("BELLA_WORKSPACE").expect("BELLA_WORKSPACE");
    assert!(
        std::path::Path::new(workspace).starts_with(h.work_root()),
        "{workspace}"
    );
    let tmp = if cfg!(windows) {
        get("TEMP")
    } else {
        get("TMPDIR")
    };
    let tmp = tmp.expect("a temporary directory");
    assert!(
        std::path::Path::new(tmp).starts_with(h.work_root()),
        "{tmp}"
    );
    assert!(get("PATH").is_some());

    let allowed: &[&str] = if cfg!(windows) {
        &[
            "PATH",
            "SystemRoot",
            "SystemDrive",
            "ComSpec",
            "PATHEXT",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "TEMP",
            "TMP",
            "CI",
        ]
    } else {
        &["PATH", "HOME", "USER", "LANG", "TMPDIR", "CI"]
    };
    for name in env.keys() {
        let known = name.starts_with("BELLA_")
            || name.starts_with("FROM_")
            || name == "SHARED"
            || name.to_ascii_lowercase().starts_with("programfiles")
            || allowed.iter().any(|allowed| {
                if cfg!(windows) {
                    allowed.eq_ignore_ascii_case(name)
                } else {
                    allowed == name
                }
            })
            // Set by the containment adapter itself, see bella-contain.
            || name == "BELLA_SCOPE"
            // Added by the operating system to every process it starts.
            || (cfg!(windows) && name.starts_with('='))
            || (cfg!(target_os = "macos") && name == "__CF_USER_TEXT_ENCODING");
        assert!(known, "unexpected variable {name} in a step: {text}");
    }
}

#[test]
fn cancel_converges_on_cleanup_and_repeats_change_nothing() {
    let mut h = Harness::new();
    h.start_connected();
    let marker = h.outside("cleanup-ran");
    let offer = h.launch(spec(vec![
        helper("wait", "sleep|60"),
        helper("tidy", &format!("append|{marker}|tidy")).kind(StepKind::Cleanup),
    ]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "wait", StepState::Running);
    assert_eq!(h.ctl.cancel(&offer.attempt, 1), CommandResult::Applied);
    assert_eq!(h.ctl.cancel(&offer.attempt, 1), CommandResult::Applied);
    h.ctl.wait_released(&id);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Cancelled));
    assert_eq!(
        h.ctl.step_states(&id, "wait"),
        [StepState::Running, StepState::Stopped]
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "tidy\n");
    // Cancelling a released attempt is a no-op too.
    assert_eq!(h.ctl.cancel(&offer.attempt, 1), CommandResult::Applied);
    h.ctl.pump(Duration::from_millis(300));
    let releases = h
        .ctl
        .events_of(&id)
        .iter()
        .filter(|event| matches!(event, AttemptEvent::Released { .. }))
        .count();
    assert_eq!(releases, 1);
    h.assert_no_workspaces();
}

#[test]
fn a24_a_daemon_left_by_a_step_keeps_the_attempt_until_it_is_killed() {
    let mut h = Harness::new();
    h.start_connected();
    let beat = h.outside("beat");
    let offer = h.launch(spec(vec![helper("spawn", &format!("daemon|{beat}"))]));
    let id = offer.attempt.attempt_id.clone();
    wait_until("the daemon to beat", || {
        std::fs::metadata(&beat).is_ok_and(|meta| meta.len() > 0)
    });
    let proof = h.ctl.wait_released(&id);
    assert!(proof.scope_empty, "{proof:?}");
    // Released only once the daemon was gone.
    assert!(
        !still_growing(&beat, Duration::from_millis(400)),
        "the daemon outlived the release"
    );
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));
    h.assert_no_workspaces();
}

#[test]
fn a66_a_hidden_background_process_is_never_released_while_known_alive() {
    let mut h = Harness::new();
    let containment = bella_contain::native().unwrap();
    let tree = containment.enforcement().process_tree;
    h.containment = Some(containment);
    h.start_connected();
    let beat = h.outside("beat");
    let offer = h.launch(spec(vec![helper("spawn", &format!("hide|{beat}"))]));
    let id = offer.attempt.attempt_id.clone();
    wait_until("the hidden process to beat", || {
        std::fs::metadata(&beat).is_ok_and(|meta| meta.len() > 0)
    });
    let ended = h.ctl.wait("a release or a quarantine", |ctl| {
        ctl.events_of(&id)
            .into_iter()
            .find_map(|event| match event {
                AttemptEvent::Released { proof } => Some(Ok(proof.clone())),
                AttemptEvent::Quarantined { reason } => Some(Err(reason.clone())),
                _ => None,
            })
    });
    let pid = pid_after(&h.ctl.log(&id, "spawn", LogStream::Stdout), "hidden");
    let alive = still_growing(&beat, Duration::from_millis(400));
    if tree == Enforcement::Hard {
        // The whole tree is in the scope: it dies with it.
        let proof = ended.expect("a hard adapter releases the attempt");
        assert!(proof.scope_empty);
        assert!(!alive, "the hidden process survived a hard scope");
    } else {
        // The process-group adapter can lose a process that left the
        // session and dropped its tag. It then says so (advisory), and a
        // release never comes while it still sees a member.
        assert_ne!(tree, Enforcement::Hard);
        if alive && let Some(pid) = pid {
            kill_pid(pid);
        }
        match ended {
            Ok(proof) => assert!(proof.scope_empty),
            Err(reason) => assert!(
                reason.contains("alive") || reason.contains("tell"),
                "{reason}"
            ),
        }
    }
    if let Some(pid) = pid {
        kill_pid(pid);
    }
}

#[test]
fn a47_a_workspace_over_its_disk_reservation_is_resource_exhausted() {
    let mut h = Harness::new();
    h.start_connected();
    let mut spec = spec(vec![helper("fill", "write|big.bin|3000000|60")]);
    spec.envelope.peak.disk_bytes = 1_000_000;
    spec.envelope.maximum = spec.envelope.peak.clone();
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::ResourceExhausted { detail }) => {
            assert!(
                detail.contains("scratch disk reservation of 1000000 bytes"),
                "{detail}"
            );
        }
        other => panic!(
            "expected ResourceExhausted, got {other:?}\n{}",
            h.ctl.dump()
        ),
    }
    // The journal kept its reserve file for recovery records.
    let reserve = bella_journal::ReserveFile::beside(
        &h.data_dir().join(bella_worker::JOURNAL_FILE),
        bella_journal::RESERVE_BYTES,
    );
    assert!(reserve.is_present());
    h.assert_no_workspaces();
}

#[test]
fn a_second_daemon_on_the_same_data_directory_is_refused() {
    let mut h = Harness::new();
    h.start_connected();
    let config = h.config();
    let second =
        h.rt.block_on(bella_worker::run(config, std::future::pending()));
    match second {
        Err(bella_worker::WorkerError::AlreadyRunning { .. }) => {}
        other => panic!("a second daemon must be refused, got {other:?}"),
    }
    // The first one is unaffected.
    h.ctl.pump(Duration::from_millis(200));
    let offer = h.launch(spec(vec![helper("ok", "lines|1|0")]));
    h.ctl.wait_released(&offer.attempt.attempt_id);
    h.stop().unwrap();
    // Once it stopped, the directory can be used again.
    h.start_connected();
}

#[test]
fn self_test_runs_a_managed_command_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let report = bella_worker::self_test(dir.path(), None);
    assert!(report.command.ok, "{}", report.command.detail);
    assert!(!report.mechanism.is_empty());
    assert!(report.hardware.logical_cpus > 0);
    // The scratch directory is gone again.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
