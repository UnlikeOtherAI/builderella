//! What a starting daemon does with the attempts its journal holds.

use crate::support::*;
use bella_contain::{Limits, ProcessSpec};
use bella_core::BootId;
use bella_core::job::Outcome;
use bella_core::protocol::{
    CommandResult, LocalAttemptState, Offer, OfferDecision, ScopeIdentity, StepState,
};
use bella_journal::Journal;
use std::time::{Duration, Instant};

/// Journal `offer` as started and running in `boot`, the way a worker that
/// then vanished would have left it, with `workspace` on record.
fn journal_running(
    h: &Harness,
    boot: &BootId,
    offer: &Offer,
    workspace: &std::path::Path,
) -> Journal {
    let journal = Journal::open(
        h.data_dir().join(bella_worker::JOURNAL_FILE),
        h.clock.clone(),
    )
    .unwrap();
    journal.recover(boot).unwrap();
    journal.hold_offer(offer).unwrap();
    journal
        .authorize_start(&offer.attempt, h.authority())
        .unwrap();
    let id = &offer.attempt.attempt_id;
    journal.set_state(id, LocalAttemptState::Preparing).unwrap();
    journal
        .set_workspace(id, &workspace.to_string_lossy())
        .unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src").join("left.txt"), b"left behind").unwrap();
    journal
}

#[test]
fn a16_an_attempt_from_another_boot_is_released_without_signalling_anything() {
    let mut h = Harness::new();
    let offer = h.offer(spec(vec![helper("work", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    let earlier = BootId::derive(b"an earlier boot");
    let workspace = h.work_root().join("lost");
    let journal = journal_running(&h, &earlier, &offer, &workspace);
    // A process that happens to look like the attempt's: the journal names
    // it, but after a reboot it cannot be the attempt's own.
    let mut stranger = spawn_helper("sleep|60");
    let containment = bella_contain::native().unwrap();
    journal
        .set_scope(
            &id,
            &ScopeIdentity {
                kind: containment.kind(),
                name: "lost-scope".to_string(),
                detail: format!("{{\"pids\":[{}]}}", stranger.id()),
            },
        )
        .unwrap();
    journal.set_state(&id, LocalAttemptState::Running).unwrap();
    drop(journal);

    h.boot_id = Some(BootId::derive(b"this boot"));
    h.start();
    let hello = h.ctl.connect();
    assert!(
        hello
            .attempts
            .iter()
            .any(|report| report.attempt.attempt_id == id)
    );
    let proof = h.ctl.wait_released(&id);
    assert!(proof.method.contains("rebooted"), "{}", proof.method);
    assert!(proof.workspace_removed);
    match h.ctl.outcome(&id) {
        Some(Outcome::InfraInterrupted { detail }) => {
            assert!(detail.contains("restarted"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!workspace.exists(), "the workspace was removed");
    assert!(
        stranger.try_wait().unwrap().is_none(),
        "a process named by a journal from another boot was signalled"
    );
    let _ = stranger.kill();
    let _ = stranger.wait();
}

#[test]
fn an_attempt_of_this_boot_is_readopted_stopped_and_reported_interrupted() {
    let mut h = Harness::new();
    let offer = h.offer(spec(vec![helper("work", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    let boot = BootId::derive(b"the same boot");
    let workspace = h.work_root().join("kept");
    let journal = journal_running(&h, &boot, &offer, &workspace);

    // A scope with a live process, as the previous daemon left it.
    let containment = bella_contain::native().unwrap();
    let mut scope = containment
        .create("readopt-test", &Limits::default())
        .unwrap();
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars()
        .filter(|(name, _)| {
            ["PATH", "SystemRoot", "HOME", "TEMP", "TMP"]
                .iter()
                .any(|wanted| wanted.eq_ignore_ascii_case(name))
        })
        .collect();
    env.insert(HELPER_ENV.to_string(), "sleep|60".to_string());
    let mut argv = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(["helper_entry", "--exact", "--nocapture", "--test-threads=1"].map(String::from));
    let mut spawned = scope
        .spawn(&ProcessSpec {
            argv,
            cwd: workspace.join("src"),
            env,
        })
        .unwrap();
    journal.set_scope(&id, &scope.identity()).unwrap();
    journal.set_state(&id, LocalAttemptState::Running).unwrap();
    drop(journal);
    // The test keeps its handle only so that nothing ends with it.
    let started = Instant::now();

    h.boot_id = Some(boot);
    h.start_connected();
    let proof = h.ctl.wait_released(&id);
    assert!(proof.scope_empty && proof.workspace_removed, "{proof:?}");
    match h.ctl.outcome(&id) {
        Some(Outcome::InfraInterrupted { detail }) => {
            assert!(detail.contains("re-attached"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
    wait_until("the re-adopted process to end", || {
        spawned.child.try_wait().unwrap().is_some()
    });
    assert!(
        started.elapsed() < Duration::from_secs(55),
        "it was stopped, not waited for"
    );
    assert!(!workspace.exists());
    drop(scope);
}

#[test]
fn a_held_offer_survives_a_restart_and_can_still_start() {
    let mut h = Harness::new();
    h.start_connected();
    let offer = h.offer(spec(vec![helper("work", "lines|1|0")]));
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
    h.stop().unwrap();

    let hello = h.start_connected();
    let report = hello
        .attempts
        .iter()
        .find(|report| report.attempt == offer.attempt)
        .expect("the hold is reported");
    assert_eq!(report.state, LocalAttemptState::OfferHeld);
    assert_eq!(
        h.ctl.start(&offer.attempt, h.authority()),
        CommandResult::Applied
    );
    h.ctl.wait_released(&offer.attempt.attempt_id);
    assert_eq!(
        h.ctl.outcome(&offer.attempt.attempt_id),
        Some(Outcome::Succeeded)
    );
}

#[test]
fn shutdown_stops_running_work_and_its_events_wait_for_the_next_connection() {
    let mut h = Harness::new();
    h.start_connected();
    let offer = h.launch(spec(vec![helper("work", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "work", StepState::Running);
    h.ctl.auto_ack = false;
    let started = Instant::now();
    h.stop().unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    let record = h.journal().attempt(&id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::Released);
    h.assert_no_workspaces();

    h.ctl.auto_ack = true;
    h.start_connected();
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::InfraInterrupted { detail }) => {
            assert!(detail.contains("shut down"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_expired_offer_is_dropped_at_startup() {
    let mut h = Harness::with(Options {
        manual_clock: true,
        ..Options::default()
    });
    h.start_connected();
    let offer = h.offer(spec(vec![helper("work", "lines|1|0")]));
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
    h.stop().unwrap();
    h.manual.as_ref().unwrap().advance(Duration::from_secs(60));
    let hello = h.start_connected();
    assert!(
        hello
            .attempts
            .iter()
            .all(|report| report.attempt != offer.attempt
                || report.state == LocalAttemptState::Released),
        "{:?}",
        hello.attempts
    );
    let record = h
        .journal()
        .attempt(&offer.attempt.attempt_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, LocalAttemptState::Released);
    assert!(h.journal().held_resources().unwrap().memory_bytes == 0);
}
