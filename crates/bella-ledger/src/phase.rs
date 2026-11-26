//! Which phase changes the ledger performs.
//!
//! `JobPhase::can_transition_to` defines the single steps. A worker may
//! skip an intermediate report (a result arrives without an explicit
//! "cleaning" notice), so the ledger also accepts a small, explicit set of
//! shortcuts. Each shortcut is a path of legal single steps; nothing here
//! allows a move that the state machine itself rules out.

use crate::{LedgerError, Result};
use bella_core::job::JobPhase;

/// May the ledger move a job or attempt from `from` to `to`?
pub(crate) fn legal(from: JobPhase, to: JobPhase) -> bool {
    use JobPhase::*;
    if from.can_transition_to(to) {
        return true;
    }
    matches!(
        (from, to),
        // Progress reports that skipped a step of the forward chain.
        (Offered, Executing)
            | (Offered, Collecting)
            | (Preparing, Collecting)
            // Results and releases that pass through `Cleaning` implicitly.
            | (
                Offered | Preparing | Executing | Collecting,
                Finished | Queued | Quarantined
            )
            | (Quarantined, Queued)
    )
}

/// Refuse a phase change that is not [`legal`].
pub(crate) fn check(entity: &str, from: JobPhase, to: JobPhase) -> Result<()> {
    if legal(from, to) {
        Ok(())
    } else {
        Err(LedgerError::IllegalTransition {
            entity: entity.to_string(),
            from,
            to,
        })
    }
}

/// Position on the forward chain of a running attempt.
fn rank(phase: JobPhase) -> Option<u8> {
    match phase {
        JobPhase::Offered => Some(0),
        JobPhase::Preparing => Some(1),
        JobPhase::Executing => Some(2),
        JobPhase::Collecting => Some(3),
        JobPhase::Cleaning => Some(4),
        _ => None,
    }
}

/// Should a progress report for `target` move an attempt that is in
/// `current`? Reports never move an attempt backwards, and never move a
/// quarantined or finished attempt at all.
pub(crate) fn advances(current: JobPhase, target: JobPhase) -> bool {
    match (rank(current), rank(target)) {
        (Some(from), Some(to)) => to > from,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JobPhase::*;

    const ALL: [JobPhase; 9] = [
        Capturing,
        Queued,
        Offered,
        Preparing,
        Executing,
        Collecting,
        Cleaning,
        Finished,
        Quarantined,
    ];

    /// Reachable through single steps without re-entering the queue on the
    /// way, i.e. within one attempt.
    fn reachable_within_attempt(from: JobPhase, to: JobPhase) -> bool {
        let mut seen = vec![from];
        let mut frontier = vec![from];
        while let Some(phase) = frontier.pop() {
            for next in ALL {
                if phase.can_transition_to(next) && !seen.contains(&next) {
                    seen.push(next);
                    // A path may end in the queue but not continue through it.
                    if next != Queued {
                        frontier.push(next);
                    }
                }
            }
        }
        from != to && seen.contains(&to)
    }

    #[test]
    fn every_shortcut_is_a_path_of_legal_steps() {
        for from in ALL {
            for to in ALL {
                if legal(from, to) {
                    assert!(
                        reachable_within_attempt(from, to),
                        "{from:?} -> {to:?} is not a path of legal steps"
                    );
                }
            }
        }
    }

    #[test]
    fn nothing_leaves_finished_or_runs_backwards() {
        for to in ALL {
            assert!(!legal(Finished, to), "Finished -> {to:?}");
        }
        assert!(!legal(Cleaning, Executing));
        assert!(!legal(Executing, Preparing));
        assert!(!legal(Queued, Executing));
        assert!(!legal(Queued, Cleaning));
        assert!(!legal(Collecting, Executing));
        assert!(check("job", Cleaning, Executing).is_err());
    }

    #[test]
    fn reports_only_move_forward() {
        assert!(advances(Offered, Preparing));
        assert!(advances(Offered, Cleaning));
        assert!(advances(Preparing, Executing));
        assert!(!advances(Executing, Preparing));
        assert!(!advances(Cleaning, Collecting));
        assert!(!advances(Cleaning, Cleaning));
        assert!(!advances(Quarantined, Cleaning));
        assert!(!advances(Quarantined, Executing));
        assert!(!advances(Finished, Cleaning));
        for from in ALL {
            for to in ALL {
                if advances(from, to) {
                    assert!(legal(from, to), "{from:?} -> {to:?}");
                }
            }
        }
    }
}
