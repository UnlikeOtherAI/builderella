//! Attempts: the held offer, the start permit, the scope, and every local
//! state change in between.

use crate::codec::{
    from_db, from_json, from_text, opt_from_json, time_from_db, time_to_db, to_json, to_text,
};
use crate::error::{JournalError, Result};
use crate::tx::JournalTx;
use bella_core::job::Outcome;
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, AttemptReport, ExecutionAuthority,
    LocalAttemptState, Offer, ReleaseProof, ScopeIdentity, StepState, UsageSummary,
};
use bella_core::resources::ResourceVector;
use bella_core::{AttemptId, BootId, JobId, UnixMillis};
use rusqlite::{Row, params};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// What [`JournalTx::hold_offer`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HoldOutcome {
    /// A new attempt was journaled in `OfferHeld`.
    Held,
    /// The same attempt and generation were already journaled and still
    /// hold their resources. Nothing changed; the earlier offer stands.
    Duplicate,
}

/// What [`JournalTx::authorize_start`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartDecision {
    /// The permit was journaled now. This is the only answer after which
    /// the worker may launch the attempt.
    Fresh,
    /// The attempt was authorised before. The worker must not launch it
    /// again; the earlier permit and whatever followed from it stand.
    AlreadyAuthorized,
}

/// What [`JournalTx::revoke`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RevokeOutcome {
    /// The held offer was dropped and its resources are free.
    Revoked,
    /// The offer had already been released without ever starting.
    AlreadyReleased,
}

/// A recorded request to stop an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    pub requested_at: UnixMillis,
    pub reason: String,
}

/// The last recorded state of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRecord {
    pub state: StepState,
    pub exit_code: Option<i32>,
    pub updated_at: UnixMillis,
}

/// Everything the journal knows about one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRecord {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    /// The boot during which the offer was accepted.
    pub boot_id: BootId,
    /// The offer exactly as it was accepted.
    pub offer: Offer,
    /// What the attempt holds on this machine while
    /// `state.holds_resources()`.
    pub reservation: ResourceVector,
    pub state: LocalAttemptState,
    /// The start permit, once one was journaled.
    pub authority: Option<ExecutionAuthority>,
    /// The scope that owns the attempt's processes during the current boot.
    pub scope: Option<ScopeIdentity>,
    /// A scope recorded during an earlier boot. Its processes cannot have
    /// survived and its identifiers may since belong to something else, so
    /// it is kept for diagnosis only: never adopt it and never signal it.
    pub stale_scope: Option<ScopeIdentity>,
    /// The workspace directory, which must be removed before release.
    pub workspace: Option<String>,
    pub steps: BTreeMap<String, StepRecord>,
    pub outcome: Option<Outcome>,
    /// Resource use reported together with the outcome.
    pub usage: Option<UsageSummary>,
    /// Set once a stop was requested, by the controller or locally.
    pub cancel: Option<CancelRequest>,
    /// Highest event sequence produced for this attempt.
    pub last_event_seq: u64,
    /// Highest event sequence the controller has acknowledged.
    pub acked_event_seq: u64,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub authorized_at: Option<UnixMillis>,
    pub released_at: Option<UnixMillis>,
}

/// May [`JournalTx::set_state`] move an attempt from `from` to `to`?
///
/// | from | to |
/// | --- | --- |
/// | `StartAuthorized` | `Preparing`, `Cleaning` |
/// | `Preparing` | `Running`, `Cleaning` |
/// | `Running` | `EnvironmentHeld`, `Collecting`, `Cleaning` |
/// | `EnvironmentHeld` | `Collecting`, `Cleaning` |
/// | `Collecting` | `Cleaning` |
/// | `Cleaning` | `Released`, `Quarantined` |
/// | `Quarantined` | `Cleaning` |
/// | `LostAcrossBoot` | `Cleaning` |
///
/// Everything that was authorised to start can go to `Cleaning`, and
/// `Cleaning` is the only way to `Released` for it: resources are never
/// released without a cleanup pass. Three changes are not in the table
/// because only their own operation makes them: an offer becomes
/// `OfferHeld` through [`hold_offer`](JournalTx::hold_offer),
/// `StartAuthorized` through [`authorize_start`](JournalTx::authorize_start)
/// and `Released` (if it never started) through
/// [`revoke`](JournalTx::revoke) or expiry, and only
/// [`recover`](JournalTx::recover) makes an attempt `LostAcrossBoot`.
/// `Released` is final.
pub fn can_transition(from: LocalAttemptState, to: LocalAttemptState) -> bool {
    use LocalAttemptState::*;
    matches!(
        (from, to),
        (StartAuthorized, Preparing | Cleaning)
            | (Preparing, Running | Cleaning)
            | (Running, EnvironmentHeld | Collecting | Cleaning)
            | (EnvironmentHeld, Collecting | Cleaning)
            | (Collecting, Cleaning)
            | (Cleaning, Released | Quarantined)
            | (Quarantined, Cleaning)
            | (LostAcrossBoot, Cleaning)
    )
}

/// The instant at which the worker must stop an attempt on its own, with
/// no word from the controller: the hard deadline, or `valid_until` plus
/// the permitted disconnected time, whichever comes first.
///
/// `max_disconnected_secs` is the machine owner's limit from the policy; it
/// caps whatever the authority allows.
pub fn local_stop_deadline(
    authority: &ExecutionAuthority,
    max_disconnected_secs: Option<u64>,
) -> UnixMillis {
    lapse_at(authority, max_disconnected_secs).min(authority.hard_deadline)
}

fn lapse_at(authority: &ExecutionAuthority, max_disconnected_secs: Option<u64>) -> UnixMillis {
    let continuation = max_disconnected_secs.map_or(
        authority.disconnected_continuation_secs,
        |limit| limit.min(authority.disconnected_continuation_secs),
    );
    authority.valid_until.plus_secs(continuation)
}

/// Why an authority no longer permits the attempt to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExpiryReason {
    /// The absolute deadline of the attempt passed.
    HardDeadline,
    /// `valid_until` plus the permitted disconnected time passed without a
    /// renewal.
    AuthorityLapsed,
}

pub(crate) fn authority_expiry(
    authority: &ExecutionAuthority,
    max_disconnected_secs: Option<u64>,
    now: UnixMillis,
) -> Option<ExpiryReason> {
    if now >= authority.hard_deadline {
        Some(ExpiryReason::HardDeadline)
    } else if now >= lapse_at(authority, max_disconnected_secs) {
        Some(ExpiryReason::AuthorityLapsed)
    } else {
        None
    }
}

/// States in which the attempt runs, or is about to run, under its
/// execution authority.
pub(crate) fn runs_under_authority(state: LocalAttemptState) -> bool {
    use LocalAttemptState::*;
    matches!(
        state,
        StartAuthorized | Preparing | Running | EnvironmentHeld | Collecting
    )
}

/// The columns most operations decide on, without the offer itself.
pub(crate) struct Head {
    pub generation: u32,
    pub state: LocalAttemptState,
    /// Offer expiry, in storage form.
    pub expires_at: i64,
    pub authority: Option<ExecutionAuthority>,
    pub scope: Option<ScopeIdentity>,
    pub workspace: Option<String>,
    pub has_outcome: bool,
    pub cancel_requested: bool,
    /// Was a start permit ever journaled?
    pub authorized: bool,
    pub last_event_seq: u64,
    pub acked_event_seq: u64,
}

struct RawHead {
    generation: i64,
    state: String,
    expires_at: i64,
    authority: Option<String>,
    scope: Option<String>,
    workspace: Option<String>,
    has_outcome: bool,
    cancel_requested: bool,
    authorized: bool,
    last_event_seq: i64,
    acked_event_seq: i64,
}

const HEAD_SQL: &str = "SELECT generation, state, expires_at, authority, scope, workspace, \
     outcome IS NOT NULL, cancel_requested_at IS NOT NULL, authorized_at IS NOT NULL, \
     last_event_seq, acked_event_seq \
     FROM attempts WHERE attempt_id = ?1";

const RECORD_COLUMNS: &str = "attempt_id, generation, job_id, boot_id, offer, reservation, state, \
     authority, scope, stale_scope, workspace, outcome, usage, cancel_requested_at, \
     cancel_reason, last_event_seq, acked_event_seq, created_at, updated_at, authorized_at, \
     released_at";

struct RawRecord {
    attempt_id: String,
    generation: i64,
    job_id: String,
    boot_id: String,
    offer: String,
    reservation: String,
    state: String,
    authority: Option<String>,
    scope: Option<String>,
    stale_scope: Option<String>,
    workspace: Option<String>,
    outcome: Option<String>,
    usage: Option<String>,
    cancel_requested_at: Option<i64>,
    cancel_reason: Option<String>,
    last_event_seq: i64,
    acked_event_seq: i64,
    created_at: i64,
    updated_at: i64,
    authorized_at: Option<i64>,
    released_at: Option<i64>,
}

fn read_record(row: &Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok(RawRecord {
        attempt_id: row.get(0)?,
        generation: row.get(1)?,
        job_id: row.get(2)?,
        boot_id: row.get(3)?,
        offer: row.get(4)?,
        reservation: row.get(5)?,
        state: row.get(6)?,
        authority: row.get(7)?,
        scope: row.get(8)?,
        stale_scope: row.get(9)?,
        workspace: row.get(10)?,
        outcome: row.get(11)?,
        usage: row.get(12)?,
        cancel_requested_at: row.get(13)?,
        cancel_reason: row.get(14)?,
        last_event_seq: row.get(15)?,
        acked_event_seq: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        authorized_at: row.get(19)?,
        released_at: row.get(20)?,
    })
}

struct RawReport {
    attempt_id: String,
    generation: i64,
    job_id: String,
    state: String,
    last_event_seq: i64,
    acked_event_seq: i64,
    reservation: String,
    usage: Option<String>,
}

pub(crate) fn generation_from_db(value: i64) -> Result<u32> {
    u32::try_from(value).map_err(|_| JournalError::corrupt("generation", "it is out of range"))
}

pub(crate) fn attempt_id_from_db(text: &str) -> Result<AttemptId> {
    AttemptId::parse(text).map_err(|error| JournalError::corrupt("attempt id", error))
}

impl JournalTx<'_> {
    pub(crate) fn head(&self, attempt: &AttemptId) -> Result<Option<Head>> {
        let raw = self.one(HEAD_SQL, params![attempt.as_str()], |row| {
            Ok(RawHead {
                generation: row.get(0)?,
                state: row.get(1)?,
                expires_at: row.get(2)?,
                authority: row.get(3)?,
                scope: row.get(4)?,
                workspace: row.get(5)?,
                has_outcome: row.get(6)?,
                cancel_requested: row.get(7)?,
                authorized: row.get(8)?,
                last_event_seq: row.get(9)?,
                acked_event_seq: row.get(10)?,
            })
        })?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        Ok(Some(Head {
            generation: generation_from_db(raw.generation)?,
            state: from_text(&raw.state, "attempt state")?,
            expires_at: raw.expires_at,
            authority: opt_from_json(raw.authority, "execution authority")?,
            scope: opt_from_json(raw.scope, "scope identity")?,
            workspace: raw.workspace,
            has_outcome: raw.has_outcome,
            cancel_requested: raw.cancel_requested,
            authorized: raw.authorized,
            last_event_seq: from_db(raw.last_event_seq, "event sequence")?,
            acked_event_seq: from_db(raw.acked_event_seq, "event sequence")?,
        }))
    }

    /// The attempt's head, or `NotFound`.
    pub(crate) fn require(&self, attempt: &AttemptId) -> Result<Head> {
        self.head(attempt)?
            .ok_or_else(|| JournalError::NotFound(attempt.clone()))
    }

    /// The attempt's head after checking the generation the caller names
    /// against the one the journal holds.
    pub(crate) fn fenced(&self, attempt: &AttemptRef) -> Result<Head> {
        let head = self.require(&attempt.attempt_id)?;
        match attempt.generation.cmp(&head.generation) {
            Ordering::Equal => Ok(head),
            Ordering::Less => Err(JournalError::StaleGeneration {
                held: head.generation,
                offered: attempt.generation,
            }),
            Ordering::Greater => Err(JournalError::GenerationConflict {
                held: head.generation,
                offered: attempt.generation,
            }),
        }
    }

    /// Refuses when the attempt must not make progress: a stop was
    /// requested, or its authority has run out on the journal clock.
    fn require_live_authority(&self, attempt: &AttemptId, head: &Head) -> Result<()> {
        if head.cancel_requested {
            return Err(JournalError::CancelRequested {
                attempt: attempt.clone(),
            });
        }
        let limit = self.max_disconnected_secs()?;
        let live = head
            .authority
            .is_some_and(|authority| authority_expiry(&authority, limit, self.now).is_none());
        if live {
            Ok(())
        } else {
            Err(JournalError::AuthorityExpired {
                attempt: attempt.clone(),
            })
        }
    }

    pub(crate) fn write_state(
        &self,
        attempt: &AttemptId,
        state: LocalAttemptState,
        at: UnixMillis,
    ) -> Result<()> {
        let name = to_text(&state, "attempt state")?;
        let at = time_to_db(at);
        let sql = if state == LocalAttemptState::Released {
            "UPDATE attempts SET state = ?2, updated_at = ?3, released_at = ?3 \
             WHERE attempt_id = ?1"
        } else {
            "UPDATE attempts SET state = ?2, updated_at = ?3 WHERE attempt_id = ?1"
        };
        self.exec(sql, params![attempt.as_str(), name, at])?;
        Ok(())
    }

    pub(crate) fn write_cancel(
        &self,
        attempt: &AttemptId,
        reason: &str,
        at: UnixMillis,
    ) -> Result<bool> {
        let changed = self.exec(
            "UPDATE attempts SET cancel_requested_at = ?2, cancel_reason = ?3, updated_at = ?2 \
             WHERE attempt_id = ?1 AND cancel_requested_at IS NULL",
            params![attempt.as_str(), time_to_db(at), reason],
        )?;
        Ok(changed == 1)
    }

    /// Journals an accepted offer as a new attempt in `OfferHeld`.
    ///
    /// From this commit on the attempt's reservation counts in
    /// [`held_resources`](Self::held_resources). Nothing may be executed
    /// for it before [`authorize_start`](Self::authorize_start).
    ///
    /// * The same attempt and generation again: `Duplicate`, as long as it
    ///   still holds resources. The stored offer is not replaced.
    /// * The same attempt after it was released: `OfferNotHeld`. A repeated
    ///   offer never revives a hold that is gone.
    /// * A generation lower than one the journal holds for the attempt or
    ///   for another attempt of the same job: `StaleGeneration`.
    /// * A generation that is already taken by a different attempt of the
    ///   job, or a newer one under the same attempt id: `GenerationConflict`.
    ///
    /// A newer generation of a job is accepted while an older attempt of it
    /// is still journaled; resolving the older one is the controller's
    /// call (revoke, abort or expiry). Attempts removed by garbage
    /// collection no longer fence their job's generations.
    pub fn hold_offer(&self, offer: &Offer) -> Result<HoldOutcome> {
        let boot = self.require_boot()?;
        let id = &offer.attempt.attempt_id;
        let offered = offer.attempt.generation;
        if let Some(head) = self.head(id)? {
            return match offered.cmp(&head.generation) {
                Ordering::Less => Err(JournalError::StaleGeneration {
                    held: head.generation,
                    offered,
                }),
                Ordering::Greater => Err(JournalError::GenerationConflict {
                    held: head.generation,
                    offered,
                }),
                Ordering::Equal if head.state == LocalAttemptState::Released => {
                    Err(JournalError::OfferNotHeld {
                        attempt: id.clone(),
                        reason: "the attempt was already released",
                    })
                }
                Ordering::Equal => Ok(HoldOutcome::Duplicate),
            };
        }
        let newest = self
            .one(
                "SELECT MAX(generation) FROM attempts WHERE job_id = ?1",
                params![offer.job_id.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .flatten();
        if let Some(newest) = newest {
            let held = generation_from_db(newest)?;
            match offered.cmp(&held) {
                Ordering::Less => return Err(JournalError::StaleGeneration { held, offered }),
                Ordering::Equal => return Err(JournalError::GenerationConflict { held, offered }),
                Ordering::Greater => {}
            }
        }
        let offer_json = to_json(offer, "offer")?;
        let reservation = to_json(&offer.reservation, "reservation")?;
        let state = to_text(&LocalAttemptState::OfferHeld, "attempt state")?;
        self.exec(
            "INSERT INTO attempts (attempt_id, generation, job_id, boot_id, active_boot_id, \
             offer, reservation, expires_at, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                id.as_str(),
                i64::from(offered),
                offer.job_id.as_str(),
                boot.as_str(),
                offer_json,
                reservation,
                time_to_db(offer.expires_at),
                state,
                time_to_db(self.now),
            ],
        )?;
        Ok(HoldOutcome::Held)
    }

    /// Journals the start permit for a held offer.
    ///
    /// `Fresh` is returned exactly once per attempt, when the attempt goes
    /// from `OfferHeld` to `StartAuthorized`. Every later call for the same
    /// attempt returns `AlreadyAuthorized`, whatever state the attempt has
    /// reached since and whatever authority the retransmission carries, so
    /// a repeated `Start` can never launch a second copy.
    ///
    /// Refused, with nothing changed:
    ///
    /// * `OfferNotHeld` when the offer's `expires_at` has passed on the
    ///   journal clock, or the offer was revoked or expired before any
    ///   start.
    /// * `AuthorityExpired` when the authority in the command is already
    ///   past its local stop deadline, as it is when the command sat in a
    ///   queue through a sleep.
    pub fn authorize_start(
        &self,
        attempt: &AttemptRef,
        authority: ExecutionAuthority,
    ) -> Result<StartDecision> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        let id = &attempt.attempt_id;
        match head.state {
            LocalAttemptState::OfferHeld => {
                if time_to_db(self.now) >= head.expires_at {
                    return Err(JournalError::OfferNotHeld {
                        attempt: id.clone(),
                        reason: "the offer expired",
                    });
                }
                let limit = self.max_disconnected_secs()?;
                if authority_expiry(&authority, limit, self.now).is_some() {
                    return Err(JournalError::AuthorityExpired {
                        attempt: id.clone(),
                    });
                }
                let state = to_text(&LocalAttemptState::StartAuthorized, "attempt state")?;
                self.exec(
                    "UPDATE attempts SET state = ?2, authority = ?3, authorized_at = ?4, \
                     updated_at = ?4 WHERE attempt_id = ?1",
                    params![
                        id.as_str(),
                        state,
                        to_json(&authority, "execution authority")?,
                        time_to_db(self.now),
                    ],
                )?;
                Ok(StartDecision::Fresh)
            }
            LocalAttemptState::Released if !head.authorized => Err(JournalError::OfferNotHeld {
                attempt: id.clone(),
                reason: "the offer was released before it was started",
            }),
            _ => Ok(StartDecision::AlreadyAuthorized),
        }
    }

    /// Withdraws an offer that has no start permit.
    ///
    /// From `OfferHeld` the attempt goes straight to `Released`: nothing
    /// ran, so there is nothing to clean, and its resources are free. A
    /// repeated revoke of such an attempt returns `AlreadyReleased`. Once a
    /// start permit is journaled the attempt may be running, and revoke
    /// fails with `InvalidState`; stopping it takes a cancel and a cleanup.
    pub fn revoke(&self, attempt: &AttemptRef) -> Result<RevokeOutcome> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        match head.state {
            LocalAttemptState::OfferHeld => {
                self.write_state(&attempt.attempt_id, LocalAttemptState::Released, self.now)?;
                Ok(RevokeOutcome::Revoked)
            }
            LocalAttemptState::Released if !head.authorized => Ok(RevokeOutcome::AlreadyReleased),
            from => Err(JournalError::InvalidState {
                from,
                to: LocalAttemptState::Released,
            }),
        }
    }

    /// Extends the attempt's authority.
    ///
    /// Only `valid_until` changes, and only forwards: a renewal that
    /// arrives late or out of order never shortens the window. The hard
    /// deadline and the permitted disconnected time stay as the start
    /// permit set them. Returns the authority now in force.
    ///
    /// A renewal for an attempt that is already cleaning up, quarantined,
    /// lost or released after running changes nothing and returns the
    /// stored authority. It fails with `AuthorityExpired` once the hard
    /// deadline has passed, `NotAllowed` before any start permit, and
    /// `OfferNotHeld` for an offer released without starting.
    pub fn renew_authority(
        &self,
        attempt: &AttemptRef,
        authority: ExecutionAuthority,
    ) -> Result<ExecutionAuthority> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        let id = &attempt.attempt_id;
        let Some(current) = head.authority else {
            return Err(match head.state {
                LocalAttemptState::Released => JournalError::OfferNotHeld {
                    attempt: id.clone(),
                    reason: "the offer was released before it was started",
                },
                state => JournalError::NotAllowed {
                    operation: "renew an authority",
                    state,
                },
            });
        };
        if !runs_under_authority(head.state) {
            return Ok(current);
        }
        if self.now >= current.hard_deadline {
            return Err(JournalError::AuthorityExpired {
                attempt: id.clone(),
            });
        }
        if authority.valid_until <= current.valid_until {
            return Ok(current);
        }
        let renewed = ExecutionAuthority {
            valid_until: authority.valid_until,
            ..current
        };
        self.exec(
            "UPDATE attempts SET authority = ?2, updated_at = ?3 WHERE attempt_id = ?1",
            params![
                id.as_str(),
                to_json(&renewed, "execution authority")?,
                time_to_db(self.now),
            ],
        )?;
        Ok(renewed)
    }

    /// Records that the attempt must stop. Returns `true` when this call
    /// set the flag and `false` when it was already set or the attempt is
    /// already released; the first reason is kept.
    ///
    /// The flag is a request, not a result: the attempt keeps its state and
    /// its resources until cleanup is observed. While it is set the attempt
    /// can no longer move to `Preparing`, `Running` or `EnvironmentHeld`,
    /// and no step can start outside cleanup.
    pub fn request_cancel(&self, attempt: &AttemptRef, reason: &str) -> Result<bool> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        if head.state == LocalAttemptState::Released || head.cancel_requested {
            return Ok(false);
        }
        self.write_cancel(&attempt.attempt_id, reason, self.now)
    }

    /// Records the containment scope that will own the attempt's processes.
    ///
    /// Ordering contract: this must commit **before the first process of
    /// the scope is spawned**. A scope that exists but is not in the
    /// journal cannot be found again after a crash, and its processes would
    /// outlive the attempt unowned. The journal enforces the part it can
    /// see: an attempt cannot become `Running`, and no step can be recorded
    /// as running, until a scope is recorded.
    ///
    /// The kind and name are fixed once recorded (`ScopeMismatch`
    /// otherwise); `detail` may be replaced as the adapter learns more,
    /// for example further process-group ids. Not allowed before a start
    /// permit, after release, or while the attempt is `LostAcrossBoot`.
    pub fn set_scope(&self, attempt: &AttemptId, scope: &ScopeIdentity) -> Result<()> {
        self.require_boot()?;
        let head = self.require(attempt)?;
        if matches!(
            head.state,
            LocalAttemptState::OfferHeld
                | LocalAttemptState::Released
                | LocalAttemptState::LostAcrossBoot
        ) {
            return Err(JournalError::NotAllowed {
                operation: "record a scope",
                state: head.state,
            });
        }
        if let Some(existing) = &head.scope {
            if existing.kind != scope.kind || existing.name != scope.name {
                return Err(JournalError::ScopeMismatch {
                    attempt: attempt.clone(),
                });
            }
            if existing.detail == scope.detail {
                return Ok(());
            }
        }
        self.exec(
            "UPDATE attempts SET scope = ?2, updated_at = ?3 WHERE attempt_id = ?1",
            params![
                attempt.as_str(),
                to_json(scope, "scope identity")?,
                time_to_db(self.now),
            ],
        )?;
        Ok(())
    }

    /// Records the workspace directory of the attempt.
    ///
    /// Like the scope, it must commit before the directory is created, so
    /// that a crash never leaves a workspace nobody knows to remove. It is
    /// fixed once recorded (`WorkspaceMismatch` otherwise) and can only be
    /// set between the start permit and the first process.
    pub fn set_workspace(&self, attempt: &AttemptId, workspace: &str) -> Result<()> {
        self.require_boot()?;
        let head = self.require(attempt)?;
        match &head.workspace {
            Some(existing) if existing == workspace => return Ok(()),
            Some(_) => {
                return Err(JournalError::WorkspaceMismatch {
                    attempt: attempt.clone(),
                });
            }
            None => {}
        }
        if !matches!(
            head.state,
            LocalAttemptState::StartAuthorized | LocalAttemptState::Preparing
        ) {
            return Err(JournalError::NotAllowed {
                operation: "record a workspace",
                state: head.state,
            });
        }
        self.exec(
            "UPDATE attempts SET workspace = ?2, updated_at = ?3 WHERE attempt_id = ?1",
            params![attempt.as_str(), workspace, time_to_db(self.now)],
        )?;
        Ok(())
    }

    /// Moves the attempt along the table documented on [`can_transition`].
    ///
    /// Setting the state the attempt is already in succeeds and changes
    /// nothing. On top of the table:
    ///
    /// * `Preparing`, `Running` and `EnvironmentHeld` need a live authority
    ///   on the journal clock and no stop request (`AuthorityExpired`,
    ///   `CancelRequested`). After a sleep that outlasted the authority the
    ///   only way on is `Cleaning`.
    /// * `Running` needs a recorded scope (`ScopeNotRecorded`).
    ///
    /// `Released` drops the attempt from
    /// [`held_resources`](Self::held_resources). Use
    /// [`release`](Self::release) to make that step together with its proof.
    pub fn set_state(&self, attempt: &AttemptId, state: LocalAttemptState) -> Result<()> {
        self.require_boot()?;
        let head = self.require(attempt)?;
        let from = head.state;
        if from == state {
            return Ok(());
        }
        if !can_transition(from, state) {
            return Err(JournalError::InvalidState { from, to: state });
        }
        if matches!(
            state,
            LocalAttemptState::Preparing
                | LocalAttemptState::Running
                | LocalAttemptState::EnvironmentHeld
        ) {
            self.require_live_authority(attempt, &head)?;
        }
        if state == LocalAttemptState::Running && head.scope.is_none() {
            return Err(JournalError::ScopeNotRecorded {
                attempt: attempt.clone(),
            });
        }
        self.write_state(attempt, state, self.now)
    }

    /// Records the state of one step.
    ///
    /// Recording a step as `Running` is recording that a process is about
    /// to exist, so it is the point where the journal holds the line:
    ///
    /// * a scope must be recorded (`ScopeNotRecorded`);
    /// * while the attempt is `Running` or `EnvironmentHeld`, the authority
    ///   must be live and no stop requested (`AuthorityExpired`,
    ///   `CancelRequested`), so nothing new runs under a stale authority;
    /// * while it is `Cleaning`, cleanup steps always may run;
    /// * in any other state it is `NotAllowed`.
    ///
    /// Repeating the state a step is already in changes nothing. Other step
    /// states can be recorded in every attempt state except `OfferHeld` and
    /// `Released`.
    pub fn set_step_state(
        &self,
        attempt: &AttemptId,
        step: &str,
        state: StepState,
        exit_code: Option<i32>,
    ) -> Result<()> {
        self.require_boot()?;
        let head = self.require(attempt)?;
        if matches!(
            head.state,
            LocalAttemptState::OfferHeld | LocalAttemptState::Released
        ) {
            return Err(JournalError::NotAllowed {
                operation: "record a step",
                state: head.state,
            });
        }
        let name = to_text(&state, "step state")?;
        let current = self.one(
            "SELECT state, exit_code FROM attempt_steps WHERE attempt_id = ?1 AND step = ?2",
            params![attempt.as_str(), step],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i32>>(1)?)),
        )?;
        if current
            .as_ref()
            .is_some_and(|(stored, code)| *stored == name && *code == exit_code)
        {
            return Ok(());
        }
        let already_running = current.is_some_and(|(stored, _)| stored == name);
        if state == StepState::Running && !already_running {
            match head.state {
                LocalAttemptState::Running | LocalAttemptState::EnvironmentHeld => {
                    self.require_live_authority(attempt, &head)?;
                }
                LocalAttemptState::Cleaning => {}
                other => {
                    return Err(JournalError::NotAllowed {
                        operation: "start a step",
                        state: other,
                    });
                }
            }
            if head.scope.is_none() {
                return Err(JournalError::ScopeNotRecorded {
                    attempt: attempt.clone(),
                });
            }
        }
        self.exec(
            "INSERT INTO attempt_steps (attempt_id, step, state, exit_code, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (attempt_id, step) DO UPDATE SET state = excluded.state, \
             exit_code = excluded.exit_code, updated_at = excluded.updated_at",
            params![attempt.as_str(), step, name, exit_code, time_to_db(self.now)],
        )?;
        Ok(())
    }

    /// Records the attempt's result and the resource use that goes with it.
    ///
    /// The first outcome stands. Returns `true` when this call recorded it
    /// and `false` when an outcome was already there, which is then left
    /// untouched. Recording an outcome releases nothing: the attempt holds
    /// its resources until it is `Released`. Not allowed for an offer that
    /// never started or after release.
    pub fn record_outcome(
        &self,
        attempt: &AttemptId,
        outcome: &Outcome,
        usage: &UsageSummary,
    ) -> Result<bool> {
        self.require_boot()?;
        let head = self.require(attempt)?;
        if head.has_outcome {
            return Ok(false);
        }
        if matches!(
            head.state,
            LocalAttemptState::OfferHeld | LocalAttemptState::Released
        ) {
            return Err(JournalError::NotAllowed {
                operation: "record an outcome",
                state: head.state,
            });
        }
        self.exec(
            "UPDATE attempts SET outcome = ?2, usage = ?3, updated_at = ?4 WHERE attempt_id = ?1",
            params![
                attempt.as_str(),
                to_json(outcome, "outcome")?,
                to_json(usage, "usage summary")?,
                time_to_db(self.now),
            ],
        )?;
        Ok(true)
    }

    /// Releases a cleaned-up attempt and queues the `Released` event that
    /// tells the controller, in one step.
    ///
    /// Only from `Cleaning`, and only with a proof that says the scope is
    /// empty and the workspace is gone (`IncompleteReleaseProof`
    /// otherwise). An attempt whose cleanup cannot be verified goes to
    /// `Quarantined` instead and keeps its resources.
    pub fn release(&self, attempt: &AttemptRef, proof: ReleaseProof) -> Result<AttemptEventMsg> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        if head.state != LocalAttemptState::Cleaning {
            return Err(JournalError::InvalidState {
                from: head.state,
                to: LocalAttemptState::Released,
            });
        }
        if !(proof.scope_empty && proof.workspace_removed) {
            return Err(JournalError::IncompleteReleaseProof {
                attempt: attempt.attempt_id.clone(),
            });
        }
        self.write_state(&attempt.attempt_id, LocalAttemptState::Released, self.now)?;
        self.append_event(attempt, AttemptEvent::Released { proof })
    }

    fn records(&self, filter: &str, id: Option<&AttemptId>) -> Result<Vec<AttemptRecord>> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM attempts {filter} ORDER BY created_at, attempt_id"
        );
        let raws = match id {
            Some(id) => self.all(&sql, params![id.as_str()], read_record)?,
            None => self.all(&sql, params![], read_record)?,
        };
        raws.into_iter().map(|raw| self.decode(raw)).collect()
    }

    fn decode(&self, raw: RawRecord) -> Result<AttemptRecord> {
        let attempt_id = attempt_id_from_db(&raw.attempt_id)?;
        let steps = self
            .all(
                "SELECT step, state, exit_code, updated_at FROM attempt_steps \
                 WHERE attempt_id = ?1 ORDER BY step",
                params![attempt_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i32>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .into_iter()
            .map(|(step, state, exit_code, updated_at)| {
                Ok((
                    step,
                    StepRecord {
                        state: from_text(&state, "step state")?,
                        exit_code,
                        updated_at: time_from_db(updated_at, "step timestamp")?,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let cancel = match (raw.cancel_requested_at, raw.cancel_reason) {
            (Some(at), reason) => Some(CancelRequest {
                requested_at: time_from_db(at, "cancel timestamp")?,
                reason: reason.unwrap_or_default(),
            }),
            (None, _) => None,
        };
        Ok(AttemptRecord {
            attempt: AttemptRef {
                attempt_id,
                generation: generation_from_db(raw.generation)?,
            },
            job_id: JobId::parse(&raw.job_id)
                .map_err(|error| JournalError::corrupt("job id", error))?,
            boot_id: BootId::parse(&raw.boot_id)
                .map_err(|error| JournalError::corrupt("boot id", error))?,
            offer: from_json(&raw.offer, "offer")?,
            reservation: from_json(&raw.reservation, "reservation")?,
            state: from_text(&raw.state, "attempt state")?,
            authority: opt_from_json(raw.authority, "execution authority")?,
            scope: opt_from_json(raw.scope, "scope identity")?,
            stale_scope: opt_from_json(raw.stale_scope, "scope identity")?,
            workspace: raw.workspace,
            steps,
            outcome: opt_from_json(raw.outcome, "outcome")?,
            usage: opt_from_json(raw.usage, "usage summary")?,
            cancel,
            last_event_seq: from_db(raw.last_event_seq, "event sequence")?,
            acked_event_seq: from_db(raw.acked_event_seq, "event sequence")?,
            created_at: time_from_db(raw.created_at, "attempt timestamp")?,
            updated_at: time_from_db(raw.updated_at, "attempt timestamp")?,
            authorized_at: raw
                .authorized_at
                .map(|at| time_from_db(at, "attempt timestamp"))
                .transpose()?,
            released_at: raw
                .released_at
                .map(|at| time_from_db(at, "attempt timestamp"))
                .transpose()?,
        })
    }

    /// One attempt, or `None` when the journal has no row for it.
    pub fn attempt(&self, attempt: &AttemptId) -> Result<Option<AttemptRecord>> {
        Ok(self
            .records("WHERE attempt_id = ?1", Some(attempt))?
            .into_iter()
            .next())
    }

    /// Every attempt still in the journal, released ones included, oldest
    /// first.
    pub fn attempts(&self) -> Result<Vec<AttemptRecord>> {
        self.records("", None)
    }

    /// Every attempt that still holds resources, oldest first.
    pub fn active_attempts(&self) -> Result<Vec<AttemptRecord>> {
        let mut records = self.attempts()?;
        records.retain(|record| record.state.holds_resources());
        Ok(records)
    }

    /// What this machine has promised to attempts: the sum of the
    /// reservation of every attempt whose state holds resources.
    ///
    /// An offer that is held but not started counts in full, as does a
    /// quarantined or lost attempt. A released attempt never counts. Low
    /// measured use never reduces the figure.
    pub fn held_resources(&self) -> Result<ResourceVector> {
        let rows = self.all(
            "SELECT state, reservation FROM attempts",
            params![],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut held = ResourceVector::ZERO;
        for (state, reservation) in rows {
            let state: LocalAttemptState = from_text(&state, "attempt state")?;
            if state.holds_resources() {
                let reservation: ResourceVector = from_json(&reservation, "reservation")?;
                held = held.saturating_add(&reservation);
            }
        }
        Ok(held)
    }

    /// The attempt list for `Hello`: every attempt that is not released,
    /// plus released ones whose events the controller has not acknowledged
    /// in full. A released attempt reports nothing reserved.
    pub fn reports(&self) -> Result<Vec<AttemptReport>> {
        let rows = self.all(
            "SELECT attempt_id, generation, job_id, state, last_event_seq, acked_event_seq, \
             reservation, usage FROM attempts ORDER BY created_at, attempt_id",
            params![],
            |row| {
                Ok(RawReport {
                    attempt_id: row.get(0)?,
                    generation: row.get(1)?,
                    job_id: row.get(2)?,
                    state: row.get(3)?,
                    last_event_seq: row.get(4)?,
                    acked_event_seq: row.get(5)?,
                    reservation: row.get(6)?,
                    usage: row.get(7)?,
                })
            },
        )?;
        let mut reports = Vec::new();
        for raw in rows {
            let state: LocalAttemptState = from_text(&raw.state, "attempt state")?;
            let last_event_seq = from_db(raw.last_event_seq, "event sequence")?;
            let acked_event_seq = from_db(raw.acked_event_seq, "event sequence")?;
            if !state.holds_resources() && acked_event_seq >= last_event_seq {
                continue;
            }
            let reserved = if state.holds_resources() {
                from_json(&raw.reservation, "reservation")?
            } else {
                ResourceVector::ZERO
            };
            reports.push(AttemptReport {
                attempt: AttemptRef {
                    attempt_id: attempt_id_from_db(&raw.attempt_id)?,
                    generation: generation_from_db(raw.generation)?,
                },
                job_id: JobId::parse(&raw.job_id)
                    .map_err(|error| JournalError::corrupt("job id", error))?,
                state,
                last_event_seq,
                reserved,
                usage: opt_from_json(raw.usage, "usage summary")?,
            });
        }
        Ok(reports)
    }
}
