//! Log spooling, delivery across reconnects, truncation and floods.

use crate::support::*;
use bella_core::job::Outcome;
use bella_core::protocol::{CommandResult, ControllerMsg, LogStream, StepState};
use std::time::{Duration, Instant};

#[test]
fn a_reconnect_mid_run_resends_events_and_logs_without_duplication() {
    let mut h = Harness::new();
    h.start_connected();
    h.ctl.auto_ack = false;
    let offer = h.launch(spec(vec![helper("tick", "tick|60|20")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait("some output", |ctl| {
        ctl.log(&id, "tick", LogStream::Stdout)
            .contains("line 0010")
            .then_some(())
    });
    // Part of what arrived is acknowledged, the rest is not.
    h.ctl.ack_everything();
    h.ctl.wait("more output", |ctl| {
        ctl.log(&id, "tick", LogStream::Stdout)
            .contains("line 0020")
            .then_some(())
    });
    let acked_seq = h.ctl.acked_seq.get(&id).copied().unwrap_or(0);
    assert!(acked_seq > 0);

    h.ctl.drop_connection();
    // The worker reconnects on its own; the report tells where it is.
    let hello = h.ctl.accept_hello();
    let report = hello
        .attempts
        .iter()
        .find(|report| report.attempt.attempt_id == id)
        .expect("the running attempt is reported");
    assert!(report.last_event_seq >= acked_seq);
    h.ctl.welcome();
    h.ctl
        .reconcile(vec![bella_core::protocol::ReconcileVerdict {
            attempt: offer.attempt.clone(),
            action: bella_core::protocol::ReconcileAction::Continue,
        }]);
    h.ctl.auto_ack = true;
    h.ctl.ack_everything();
    h.ctl.wait_released(&id);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));

    // The controller saw every line exactly once, in order, with no gap
    // in offsets or sequences (the scripted controller checks those).
    let out = h.ctl.log(&id, "tick", LogStream::Stdout);
    let lines: Vec<&str> = out
        .lines()
        .filter(|line| line.starts_with("line "))
        .collect();
    let expected: Vec<String> = (0..60).map(|i| format!("line {i:04}")).collect();
    assert_eq!(lines, expected);
    assert!(
        h.ctl.resent_events > 0 || h.ctl.resent_log_bytes > 0,
        "what was not acknowledged was sent again"
    );
}

#[test]
fn the_log_allowance_truncates_with_a_marker_and_the_step_still_finishes() {
    let mut h = Harness::new();
    h.start_connected();
    let mut spec = spec(vec![
        helper("flood", "flood|3000000"),
        helper("after", "lines|1|0").needs(&["flood"]),
    ]);
    spec.outputs.max_log_bytes = 10_000;
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(
        h.ctl.outcome(&id),
        Some(Outcome::Succeeded),
        "{}",
        h.ctl.dump()
    );
    assert_eq!(
        h.ctl.step_states(&id, "flood"),
        [StepState::Running, StepState::Succeeded]
    );
    let captured: usize = [LogStream::Stdout, LogStream::Stderr]
        .into_iter()
        .flat_map(|stream| ["flood", "after"].map(|step| (step, stream)))
        .map(|(step, stream)| h.ctl.log(&id, step, stream).len())
        .sum();
    assert_eq!(captured, 10_000, "exactly the allowance was captured");
    assert!(
        !h.ctl
            .log(&id, "flood", LogStream::Stdout)
            .contains("flood-done")
    );
    let system = h.ctl.log(&id, "flood", LogStream::System);
    assert!(system.contains("log truncated at 10000 bytes"), "{system}");
    assert!(
        h.ctl
            .truncated
            .contains(&(id.clone(), "flood".to_string(), LogStream::Stdout)),
        "the last chunk carries the truncation mark"
    );
    let offsets = h
        .journal()
        .log_offsets(&id, "flood", LogStream::Stdout)
        .unwrap();
    assert!(offsets.truncated);
}

#[test]
fn a68_a_log_flood_does_not_delay_commands_or_heartbeats() {
    let mut h = Harness::new();
    h.start_connected();
    let offer = h.launch(spec(vec![helper("flood", "flood|20000000")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait("the flood to start", |ctl| {
        (ctl.log(&id, "flood", LogStream::Stdout).len() > 100_000).then_some(())
    });
    let mut worst = Duration::ZERO;
    for nonce in 0..5u64 {
        let sent = Instant::now();
        h.ctl.send(ControllerMsg::Ping { nonce });
        h.ctl
            .wait("a pong", |ctl| ctl.pongs.contains(&nonce).then_some(()));
        worst = worst.max(sent.elapsed());
        let beats = h.ctl.heartbeats.len();
        let waited = Instant::now();
        h.ctl.wait("a heartbeat", |ctl| {
            (ctl.heartbeats.len() > beats).then_some(())
        });
        worst = worst.max(
            waited
                .elapsed()
                .saturating_sub(h.tuning.heartbeat_interval.unwrap()),
        );
    }
    assert!(
        worst < Duration::from_secs(2),
        "a reply took {worst:?} during a flood"
    );
    assert_eq!(h.ctl.cancel(&offer.attempt, 0), CommandResult::Applied);
    h.ctl.wait_released(&id);
}
