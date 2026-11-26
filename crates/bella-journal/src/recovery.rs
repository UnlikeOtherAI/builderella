//! What to do with journaled attempts after a restart, a sleep or a long
//! silence, and when to forget finished ones.

use crate::attempt::{
    AttemptRecord, ExpiryReason, attempt_id_from_db, authority_expiry, generation_from_db,
    runs_under_authority,
};
use crate::codec::{from_text, opt_from_json, time_to_db, to_text};
use crate::error::{JournalError, Result};
use crate::tx::JournalTx;
use bella_core::protocol::{AttemptRef, ExecutionAuthority, LocalAttemptState};
use bella_core::{AttemptId, BootId, UnixMillis};
use rusqlite::params;
use std::time::Duration;

/// What the worker should do with an attempt found in the journal at
/// startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryAction {
    /// Accepted during this boot, with a scope on record: re-adopt the
    /// scope through the containment adapter, then carry on from the
    /// attempt's state (or stop it and clean up).
    Readopt,
    /// Authorised during this boot, but no scope was ever journaled. The
    /// scope is journaled before the first spawn, so no process of this
    /// attempt exists. Any recorded workspace may be half written.
    ResumeUnspawned,
    /// An offer held during this boot that has not expired. The hold
    /// stands; the controller may still start or revoke it.
    KeepOffer,
    /// An offer that never started and can no longer start: it expired, or
    /// it was accepted before a reboot. `recover` already released it.
    DropOffer,
    /// The attempt was started before a reboot. Its processes cannot have
    /// survived, so nothing is adopted or signalled; `recover` moved it to
    /// `LostAcrossBoot`. It still holds its resources until its workspace
    /// is removed and it is released through `Cleaning`.
    CleanUpAfterReboot,
}

/// One attempt found by [`JournalTx::recover`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredAttempt {
    /// The attempt as `recover` left it.
    pub record: AttemptRecord,
    /// The state it was found in.
    pub found_state: LocalAttemptState,
    pub action: RecoveryAction,
}

/// An attempt whose authority no longer covers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredAttempt {
    pub attempt: AttemptRef,
    pub state: LocalAttemptState,
    pub reason: ExpiryReason,
    pub authority: ExecutionAuthority,
}

/// What [`JournalTx::expire`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpireReport {
    /// Attempts the worker must stop on its own. Each now carries a stop
    /// request in the journal.
    pub stop: Vec<ExpiredAttempt>,
    /// Offers that passed `expires_at` without a start. They are now
    /// `Released` and hold nothing.
    pub released_offers: Vec<AttemptRef>,
}

struct Survivor {
    attempt_id: String,
    boot_id: String,
    active_boot_id: String,
    state: String,
    expires_at: i64,
    has_scope: bool,
}

struct Governed {
    attempt_id: String,
    generation: i64,
    state: String,
    expires_at: i64,
    authority: Option<String>,
}

impl JournalTx<'_> {
    /// Sorts every attempt that is not released into what the worker has
    /// to do about it, given the boot the worker is running in now.
    ///
    /// This must be the first thing done with the attempts after opening
    /// the journal: until it has run in this process, every operation that
    /// changes an attempt fails with `NotRecovered`. That is what keeps a
    /// scope or process identity from another boot from ever being acted
    /// on. Follow it with [`expire`](Self::expire), because the journal may
    /// have been closed across a sleep.
    ///
    /// * Unstarted offer from this boot, not expired: kept (`KeepOffer`).
    /// * Unstarted offer that expired or predates a reboot: released
    ///   (`DropOffer`).
    /// * Started attempt last handled in this boot: `Readopt` when a scope
    ///   is on record, `ResumeUnspawned` when none is.
    /// * Started attempt from another boot: becomes `LostAcrossBoot`
    ///   (`CleanUpAfterReboot`). Its scope moves to
    ///   [`AttemptRecord::stale_scope`] so that nothing can adopt it; the
    ///   workspace stays on record because it still has to be removed.
    ///
    /// Returns the attempts oldest first. Running it again in the same
    /// boot gives the same answers.
    pub fn recover(&self, current_boot: &BootId) -> Result<Vec<RecoveredAttempt>> {
        let survivors = self.all(
            "SELECT attempt_id, boot_id, active_boot_id, state, expires_at, scope IS NOT NULL \
             FROM attempts ORDER BY created_at, attempt_id",
            params![],
            |row| {
                Ok(Survivor {
                    attempt_id: row.get(0)?,
                    boot_id: row.get(1)?,
                    active_boot_id: row.get(2)?,
                    state: row.get(3)?,
                    expires_at: row.get(4)?,
                    has_scope: row.get(5)?,
                })
            },
        )?;
        let now = time_to_db(self.now);
        let lost = to_text(&LocalAttemptState::LostAcrossBoot, "attempt state")?;
        let mut recovered = Vec::new();
        for survivor in survivors {
            let found_state: LocalAttemptState = from_text(&survivor.state, "attempt state")?;
            if found_state == LocalAttemptState::Released {
                continue;
            }
            let id = attempt_id_from_db(&survivor.attempt_id)?;
            let action = if found_state == LocalAttemptState::OfferHeld {
                if survivor.boot_id == current_boot.as_str() && now < survivor.expires_at {
                    RecoveryAction::KeepOffer
                } else {
                    self.write_state(&id, LocalAttemptState::Released, self.now)?;
                    RecoveryAction::DropOffer
                }
            } else if survivor.active_boot_id != current_boot.as_str() {
                self.exec(
                    "UPDATE attempts SET state = ?2, active_boot_id = ?3, \
                     stale_scope = COALESCE(scope, stale_scope), scope = NULL, updated_at = ?4 \
                     WHERE attempt_id = ?1",
                    params![id.as_str(), lost, current_boot.as_str(), now],
                )?;
                RecoveryAction::CleanUpAfterReboot
            } else if found_state == LocalAttemptState::LostAcrossBoot {
                RecoveryAction::CleanUpAfterReboot
            } else if survivor.has_scope {
                RecoveryAction::Readopt
            } else {
                RecoveryAction::ResumeUnspawned
            };
            let record = self
                .attempt(&id)?
                .ok_or_else(|| JournalError::NotFound(id.clone()))?;
            recovered.push(RecoveredAttempt {
                record,
                found_state,
                action,
            });
        }
        *self.boot.borrow_mut() = Some(current_boot.clone());
        Ok(recovered)
    }

    /// Re-evaluates every time limit against `now`, the wall clock.
    ///
    /// Call it on a timer and immediately after waking from sleep. A
    /// sleeping machine renews nothing, so an authority can be long dead by
    /// the time the worker runs again.
    ///
    /// * An attempt running under an authority whose hard deadline has
    ///   passed, or whose `valid_until` plus the permitted disconnected
    ///   time has passed, is returned in `stop`, and a stop request is
    ///   journaled for it. The permitted disconnected time is capped by
    ///   the policy's `max_disconnected_secs`. From then on the attempt
    ///   can only move towards cleanup, even if the clock is later set
    ///   back or a renewal arrives. While `now` is past the limit it is
    ///   returned again on every call until it reaches `Cleaning`, and the
    ///   stop request stays on its record, so a crash between the call and
    ///   the stop loses nothing.
    /// * An offer past its `expires_at` that was never started is released
    ///   and returned in `released_offers`.
    ///
    /// Nothing here stops a process or frees a started attempt's
    /// resources; the worker does the stopping and reports the release.
    pub fn expire(&self, now: UnixMillis) -> Result<ExpireReport> {
        self.require_boot()?;
        let limit = self.max_disconnected_secs()?;
        let rows = self.all(
            "SELECT attempt_id, generation, state, expires_at, authority FROM attempts \
             ORDER BY created_at, attempt_id",
            params![],
            |row| {
                Ok(Governed {
                    attempt_id: row.get(0)?,
                    generation: row.get(1)?,
                    state: row.get(2)?,
                    expires_at: row.get(3)?,
                    authority: row.get(4)?,
                })
            },
        )?;
        let mut report = ExpireReport::default();
        for row in rows {
            let state: LocalAttemptState = from_text(&row.state, "attempt state")?;
            let unstarted = state == LocalAttemptState::OfferHeld;
            if !unstarted && !runs_under_authority(state) {
                continue;
            }
            let attempt = AttemptRef {
                attempt_id: attempt_id_from_db(&row.attempt_id)?,
                generation: generation_from_db(row.generation)?,
            };
            if unstarted {
                if time_to_db(now) >= row.expires_at {
                    self.write_state(&attempt.attempt_id, LocalAttemptState::Released, now)?;
                    report.released_offers.push(attempt);
                }
                continue;
            }
            let authority: Option<ExecutionAuthority> =
                opt_from_json(row.authority, "execution authority")?;
            let Some(authority) = authority else {
                return Err(JournalError::corrupt(
                    "attempt",
                    "it was started without an authority",
                ));
            };
            if let Some(reason) = authority_expiry(&authority, limit, now) {
                let why = match reason {
                    ExpiryReason::HardDeadline => "the attempt's hard deadline passed",
                    ExpiryReason::AuthorityLapsed => {
                        "the execution authority lapsed without controller contact"
                    }
                };
                self.write_cancel(&attempt.attempt_id, why, now)?;
                report.stop.push(ExpiredAttempt {
                    attempt,
                    state,
                    reason,
                    authority,
                });
            }
        }
        Ok(report)
    }

    fn delete_attempt(&self, attempt: &AttemptId) -> Result<()> {
        let id = attempt.as_str();
        self.exec("DELETE FROM events WHERE attempt_id = ?1", params![id])?;
        self.exec(
            "DELETE FROM attempt_steps WHERE attempt_id = ?1",
            params![id],
        )?;
        self.exec("DELETE FROM log_offsets WHERE attempt_id = ?1", params![id])?;
        self.exec("DELETE FROM attempts WHERE attempt_id = ?1", params![id])?;
        Ok(())
    }

    /// Removes attempts that are finished in every respect: released more
    /// than `keep` before `now`, every event acknowledged, and every log
    /// stream acknowledged up to what was produced.
    ///
    /// Returns the removed attempt ids so the worker can delete their spool
    /// files. An attempt the controller still owes an acknowledgement for
    /// is kept, however old.
    pub fn garbage_collect(&self, now: UnixMillis, keep: Duration) -> Result<Vec<AttemptId>> {
        let released = to_text(&LocalAttemptState::Released, "attempt state")?;
        let ids = self.all(
            "SELECT attempt_id FROM attempts AS a \
             WHERE a.state = ?1 AND a.acked_event_seq >= a.last_event_seq \
             AND a.released_at IS NOT NULL AND a.released_at <= ?2 \
             AND NOT EXISTS (SELECT 1 FROM log_offsets AS l \
             WHERE l.attempt_id = a.attempt_id AND l.acked < l.produced) \
             ORDER BY a.created_at, a.attempt_id",
            params![released, time_to_db(now.minus(keep))],
            |row| row.get::<_, String>(0),
        )?;
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            let id = attempt_id_from_db(&id)?;
            self.delete_attempt(&id)?;
            removed.push(id);
        }
        Ok(removed)
    }

    /// Removes a released attempt together with events and log offsets the
    /// controller never acknowledged.
    ///
    /// For attempts the controller has disowned (reconciliation answered
    /// that it does not know them), whose acknowledgements will therefore
    /// never come. Only a `Released` attempt can be forgotten, so this can
    /// never free resources that are still held.
    pub fn forget_attempt(&self, attempt: &AttemptId) -> Result<()> {
        let head = self.require(attempt)?;
        if head.state != LocalAttemptState::Released {
            return Err(JournalError::NotAllowed {
                operation: "forget an attempt",
                state: head.state,
            });
        }
        self.delete_attempt(attempt)
    }
}
