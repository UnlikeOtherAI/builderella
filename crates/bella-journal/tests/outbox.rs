//! The event outbox, seen commands and log offsets.

mod common;

use bella_core::job::CleanupState;
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, CommandResult, LocalAttemptState, LogStream, RejectCode,
    StepState,
};
use bella_core::{Clock, CommandId};
use bella_journal::{JournalError, LogOffsets, StartDecision};
use common::*;
use std::time::Duration;

fn step_changed(step: &str, state: StepState) -> AttemptEvent {
    AttemptEvent::StepChanged {
        step: step.to_owned(),
        state,
        exit_code: None,
    }
}

fn seqs(events: &[AttemptEventMsg]) -> Vec<u64> {
    events.iter().map(|event| event.seq).collect()
}

#[test]
fn sequences_start_at_one_and_have_no_gaps_per_attempt() {
    let clock = clock();
    let journal = memory(&clock);
    let first = run(&journal, &offer(1), authority(T0));
    let second = run(&journal, &offer(2), authority(T0));

    let mut sent = Vec::new();
    sent.push(
        journal
            .append_event(&first, AttemptEvent::Preparing)
            .unwrap(),
    );
    clock.advance(Duration::from_millis(250));
    sent.push(
        journal
            .append_event(
                &first,
                AttemptEvent::Started {
                    scope: scope("bella-attempt"),
                },
            )
            .unwrap(),
    );
    // The other attempt counts on its own.
    let other = journal
        .append_event(&second, AttemptEvent::Preparing)
        .unwrap();
    assert_eq!(other.seq, 1);
    sent.push(
        journal
            .append_event(&first, step_changed("build", StepState::Running))
            .unwrap(),
    );

    assert_eq!(seqs(&sent), vec![1, 2, 3]);
    assert_eq!(sent[0].attempt, first);
    assert_eq!(sent[0].at, T0);
    assert_eq!(sent[1].at, T0.plus(Duration::from_millis(250)));
    assert_eq!(journal.unacked_events(&first.attempt_id).unwrap(), sent);
    assert_eq!(
        journal.unacked_events(&second.attempt_id).unwrap(),
        vec![other]
    );
    let record = journal.attempt(&first.attempt_id).unwrap().unwrap();
    assert_eq!((record.last_event_seq, record.acked_event_seq), (3, 0));
}

#[test]
fn a_rolled_back_transaction_consumes_no_sequence_and_changes_no_state() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    journal
        .append_event(&attempt, AttemptEvent::Preparing)
        .unwrap();

    // The event and the state change it reports are one unit. The second
    // operation is refused, so neither may remain.
    let failed = journal.with_transaction(|tx| {
        tx.append_event(&attempt, AttemptEvent::Collecting)?;
        tx.set_state(&attempt.attempt_id, LocalAttemptState::Collecting)?;
        tx.set_state(&attempt.attempt_id, LocalAttemptState::Running)?;
        Ok(())
    });
    assert!(matches!(failed, Err(JournalError::InvalidState { .. })));
    assert_eq!(state(&journal, &attempt), LocalAttemptState::Running);
    assert_eq!(
        seqs(&journal.unacked_events(&attempt.attempt_id).unwrap()),
        vec![1]
    );

    // Done properly, both commit, and the sequence continues without a hole.
    let event = journal
        .with_transaction(|tx| {
            tx.set_state(&attempt.attempt_id, LocalAttemptState::Collecting)?;
            tx.append_event(&attempt, AttemptEvent::Collecting)
        })
        .unwrap();
    assert_eq!(event.seq, 2);
    assert_eq!(state(&journal, &attempt), LocalAttemptState::Collecting);
    assert_eq!(
        seqs(&journal.unacked_events(&attempt.attempt_id).unwrap()),
        vec![1, 2]
    );
}

#[test]
fn a_refusal_inside_a_transaction_can_be_recorded_as_the_command_result() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    journal.revoke(&offer.attempt).unwrap();
    let command = CommandId::derive(b"start-1");

    // The Start is refused; the refusal is what the journal remembers for
    // this command, in the same transaction.
    let result = journal
        .with_transaction(|tx| {
            let result = match tx.authorize_start(&offer.attempt, authority(T0)) {
                Ok(_) => CommandResult::Applied,
                Err(refusal) => refusal.command_result().ok_or(refusal)?,
            };
            tx.record_command(&command, &result)?;
            Ok(result)
        })
        .unwrap();
    assert!(matches!(
        result,
        CommandResult::Rejected {
            code: RejectCode::OfferNotHeld,
            ..
        }
    ));
    assert_eq!(journal.command_result(&command).unwrap(), Some(result));
}

#[test]
fn acknowledgement_drops_events_never_regresses_and_cannot_run_ahead() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    let id = &attempt.attempt_id;
    for state in [StepState::Running, StepState::Succeeded] {
        journal
            .append_event(&attempt, step_changed("build", state))
            .unwrap();
    }
    journal
        .append_event(&attempt, AttemptEvent::Collecting)
        .unwrap();

    // Acknowledging what was never produced is an error and changes nothing.
    let ahead = journal.ack_events(id, 4).unwrap_err();
    assert!(matches!(
        ahead,
        JournalError::AckBeyondProduced {
            produced: 3,
            acknowledged: 4
        }
    ));
    assert_eq!(seqs(&journal.unacked_events(id).unwrap()), vec![1, 2, 3]);

    journal.ack_events(id, 2).unwrap();
    assert_eq!(seqs(&journal.unacked_events(id).unwrap()), vec![3]);
    // A duplicate or older acknowledgement does not bring anything back.
    journal.ack_events(id, 1).unwrap();
    journal.ack_events(id, 2).unwrap();
    assert_eq!(seqs(&journal.unacked_events(id).unwrap()), vec![3]);
    assert_eq!(journal.attempt(id).unwrap().unwrap().acked_event_seq, 2);

    // New events carry on from the last sequence, not from what is left.
    let next = journal
        .append_event(
            &attempt,
            AttemptEvent::Cleanup {
                state: CleanupState::Requested,
                detail: String::new(),
            },
        )
        .unwrap();
    assert_eq!(next.seq, 4);
    journal.ack_events(id, 4).unwrap();
    assert!(journal.unacked_events(id).unwrap().is_empty());
    assert_eq!(journal.reports().unwrap()[0].last_event_seq, 4);

    assert!(matches!(
        journal.ack_events(&attempt_ref(404, 1).attempt_id, 1),
        Err(JournalError::NotFound(_))
    ));
}

#[test]
fn events_survive_a_restart_and_are_resent_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    let attempt = offer.attempt.clone();
    let mut produced = Vec::new();
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
        for event in [
            AttemptEvent::Preparing,
            AttemptEvent::Started {
                scope: scope("bella-attempt"),
            },
            step_changed("build", StepState::Running),
            step_changed("build", StepState::Succeeded),
            step_changed("test", StepState::Running),
        ] {
            clock.advance(Duration::from_millis(10));
            produced.push(journal.append_event(&attempt, event).unwrap());
        }
        // The controller confirmed the first two before the worker died.
        journal.ack_events(&attempt.attempt_id, 2).unwrap();
    }

    let journal = open(&path, &clock);
    journal.recover(&boot(1)).unwrap();
    let report = &journal.reports().unwrap()[0];
    assert_eq!(report.last_event_seq, 5);
    let resend = journal.unacked_events(&attempt.attempt_id).unwrap();
    assert_eq!(resend, produced[2..].to_vec());
    assert_eq!(seqs(&resend), vec![3, 4, 5]);

    // The sequence continues where it stopped.
    let next = journal
        .append_event(&attempt, step_changed("test", StepState::Failed))
        .unwrap();
    assert_eq!(next.seq, 6);
}

#[test]
fn a_released_attempt_is_reported_until_its_events_are_acknowledged() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    journal
        .set_state(&attempt.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal.release(&attempt, proof()).unwrap();
    // A revoked offer produced nothing, so there is nothing to wait for.
    let revoked = offer(2);
    journal.hold_offer(&revoked).unwrap();
    journal.revoke(&revoked.attempt).unwrap();

    let reports = journal.reports().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].attempt, attempt);
    assert_eq!(reports[0].state, LocalAttemptState::Released);
    assert_eq!(reports[0].last_event_seq, 1);
    assert!(
        reports[0].reserved.is_zero(),
        "a released attempt reserves nothing"
    );

    journal.ack_events(&attempt.attempt_id, 1).unwrap();
    assert!(journal.reports().unwrap().is_empty());
}

#[test]
fn a_command_result_is_recorded_once_and_pruned_by_age() {
    let clock = clock();
    let journal = memory(&clock);
    let old = CommandId::derive(b"command-old");
    let new = CommandId::derive(b"command-new");
    assert_eq!(journal.command_result(&old).unwrap(), None);

    assert!(
        journal
            .record_command(&old, &CommandResult::Applied)
            .unwrap()
    );
    let rejected = CommandResult::Rejected {
        code: RejectCode::StaleEpoch,
        message: "epoch 3 is older than 4".to_owned(),
    };
    // The retransmission must get the first answer, not a second opinion.
    assert!(!journal.record_command(&old, &rejected).unwrap());
    assert_eq!(
        journal.command_result(&old).unwrap(),
        Some(CommandResult::Applied)
    );

    clock.advance(Duration::from_secs(600));
    assert!(journal.record_command(&new, &rejected).unwrap());
    assert_eq!(journal.command_result(&new).unwrap(), Some(rejected));

    // Keep ten minutes: the old one is exactly at the edge and stays.
    let keep = Duration::from_secs(600);
    assert_eq!(journal.prune(clock.now(), keep).unwrap(), 0);
    clock.advance(Duration::from_millis(1));
    assert_eq!(journal.prune(clock.now(), keep).unwrap(), 1);
    assert_eq!(journal.command_result(&old).unwrap(), None);
    assert!(journal.command_result(&new).unwrap().is_some());
}

#[test]
fn a_duplicate_start_command_is_answered_from_the_journal() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let command = CommandId::derive(b"start");

    let apply = || {
        journal.with_transaction(|tx| {
            if let Some(earlier) = tx.command_result(&command)? {
                return Ok((earlier, None));
            }
            let decision = tx.authorize_start(&offer.attempt, authority(T0))?;
            tx.record_command(&command, &CommandResult::Applied)?;
            Ok((CommandResult::Applied, Some(decision)))
        })
    };
    assert_eq!(
        apply().unwrap(),
        (CommandResult::Applied, Some(StartDecision::Fresh))
    );
    assert_eq!(apply().unwrap(), (CommandResult::Applied, None));
}

#[test]
fn log_offsets_only_move_forwards_and_acks_cannot_pass_what_was_produced() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    let id = &attempt.attempt_id;
    let out = LogStream::Stdout;

    assert_eq!(
        journal.log_offsets(id, "build", out).unwrap(),
        LogOffsets::default()
    );
    assert!(journal.log_cursors(id).unwrap().is_empty());

    assert_eq!(
        journal
            .record_log_produced(id, "build", out, 4096)
            .unwrap()
            .produced,
        4096
    );
    // A lower offset, as after a reordered write, changes nothing.
    assert_eq!(
        journal
            .record_log_produced(id, "build", out, 1024)
            .unwrap()
            .produced,
        4096
    );
    journal
        .record_log_produced(id, "build", LogStream::Stderr, 10)
        .unwrap();

    let ahead = journal.ack_log(id, "build", out, 4097).unwrap_err();
    assert!(matches!(
        ahead,
        JournalError::AckBeyondProduced {
            produced: 4096,
            acknowledged: 4097
        }
    ));
    assert_eq!(journal.ack_log(id, "build", out, 2048).unwrap().acked, 2048);
    assert_eq!(
        journal.ack_log(id, "build", out, 100).unwrap().acked,
        2048,
        "an older acknowledgement does not move the mark back"
    );

    // Streams are independent of each other.
    assert_eq!(
        journal.log_offsets(id, "build", LogStream::Stderr).unwrap(),
        LogOffsets {
            produced: 10,
            acked: 0,
            truncated: false
        }
    );
    assert_eq!(
        journal.log_offsets(id, "test", out).unwrap(),
        LogOffsets::default()
    );

    // Truncation closes the stream at its current length, for good.
    let closed = journal.mark_log_truncated(id, "build", out).unwrap();
    assert_eq!(
        closed,
        LogOffsets {
            produced: 4096,
            acked: 2048,
            truncated: true
        }
    );
    assert!(matches!(
        journal.record_log_produced(id, "build", out, 5000),
        Err(JournalError::LogTruncated { produced: 4096, .. })
    ));
    assert_eq!(journal.ack_log(id, "build", out, 4096).unwrap().acked, 4096);

    let cursors = journal.log_cursors(id).unwrap();
    let listed: Vec<_> = cursors
        .iter()
        .map(|cursor| (cursor.step.as_str(), cursor.stream, cursor.offsets.produced))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("build", LogStream::Stderr, 10),
            ("build", LogStream::Stdout, 4096)
        ]
    );

    assert!(matches!(
        journal.record_log_produced(&attempt_ref(404, 1).attempt_id, "build", out, 1),
        Err(JournalError::NotFound(_))
    ));
}

#[test]
fn log_offsets_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    let id = offer.attempt.attempt_id.clone();
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
        journal
            .record_log_produced(&id, "build", LogStream::System, 900)
            .unwrap();
        journal
            .ack_log(&id, "build", LogStream::System, 300)
            .unwrap();
        journal
            .mark_log_truncated(&id, "build", LogStream::System)
            .unwrap();
    }
    let journal = open(&path, &clock);
    assert_eq!(
        journal
            .log_offsets(&id, "build", LogStream::System)
            .unwrap(),
        LogOffsets {
            produced: 900,
            acked: 300,
            truncated: true
        }
    );
}
