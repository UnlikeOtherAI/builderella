//! The transaction handle every journal operation runs on, and the
//! identity and counter operations.

use crate::codec::{from_db, from_json, to_db, to_json};
use crate::error::{JournalError, Result};
use bella_core::machine::{LocalControl, MachinePolicy};
use bella_core::{BootId, FleetId, MachineId, UnixMillis};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

/// Who this worker is and which controller it belongs to.
///
/// The machine token is deliberately not part of this: the worker keeps it
/// in a separate private file, so a copy of the journal authenticates
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    pub fleet_id: FleetId,
    pub machine_id: MachineId,
    pub controller_url: String,
    /// SHA-256 fingerprint of the fleet certificate pinned at pairing.
    pub fingerprint: String,
    /// The machine policy. Written at enrollment and replaced by
    /// [`JournalTx::set_policy`]; a read always returns the current one.
    pub policy: MachinePolicy,
}

/// The part of an enrollment that never changes.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct EnrolledIdentity {
    fleet_id: FleetId,
    machine_id: MachineId,
    controller_url: String,
    fingerprint: String,
}

/// One open journal transaction.
///
/// Every operation of the journal is a method here.
/// [`Journal::with_transaction`](crate::Journal::with_transaction) hands one
/// to a closure so that several operations (a state change, the event that
/// reports it, the result of the command that caused it) commit together or
/// not at all.
///
/// An operation that returns a refusal (anything for which
/// [`JournalError::reject_code`] is `Some`) has changed nothing, so the
/// closure may handle it and carry on, for example to record the rejection
/// as the command's result. A storage failure is different: once one has
/// happened the transaction can no longer commit, whatever the closure
/// returns.
pub struct JournalTx<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) now: UnixMillis,
    /// The boot `recover` established, carried back to the journal when the
    /// transaction commits.
    pub(crate) boot: RefCell<Option<BootId>>,
    /// The first storage failure seen in this transaction.
    pub(crate) failure: RefCell<Option<String>>,
}

impl JournalTx<'_> {
    /// The journal clock's reading when this transaction began. Every
    /// timestamp the transaction writes is this instant.
    pub fn now(&self) -> UnixMillis {
        self.now
    }

    /// Converts a SQLite result, remembering a failure so the transaction
    /// cannot commit half of an operation.
    pub(crate) fn db<T>(&self, result: rusqlite::Result<T>) -> Result<T> {
        result.map_err(|error| {
            let mut failure = self.failure.borrow_mut();
            if failure.is_none() {
                *failure = Some(error.to_string());
            }
            JournalError::sqlite(error)
        })
    }

    pub(crate) fn exec<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        let changed = self
            .conn
            .prepare_cached(sql)
            .and_then(|mut statement| statement.execute(params));
        self.db(changed)
    }

    pub(crate) fn one<T, P, F>(&self, sql: &str, params: P, read: F) -> Result<Option<T>>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    {
        let found = self
            .conn
            .prepare_cached(sql)
            .and_then(|mut statement| statement.query_row(params, read).optional());
        self.db(found)
    }

    pub(crate) fn all<T, P, F>(&self, sql: &str, params: P, read: F) -> Result<Vec<T>>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
    {
        let rows = self.conn.prepare_cached(sql).and_then(|mut statement| {
            let mapped = statement.query_map(params, read)?;
            mapped.collect::<rusqlite::Result<Vec<T>>>()
        });
        self.db(rows)
    }

    /// The boot established by `recover`, required before attempts change.
    pub(crate) fn require_boot(&self) -> Result<BootId> {
        self.boot.borrow().clone().ok_or(JournalError::NotRecovered)
    }

    fn identity_text(&self, sql: &str) -> Result<Option<String>> {
        Ok(self
            .one(sql, params![], |row| row.get::<_, Option<String>>(0))?
            .flatten())
    }

    fn identity_counter(&self, sql: &str, what: &'static str) -> Result<u64> {
        let value = self
            .one(sql, params![], |row| row.get::<_, i64>(0))?
            .ok_or_else(|| JournalError::corrupt("identity row", "it is missing"))?;
        from_db(value, what)
    }

    /// The enrollment, with the current policy, or `None` before
    /// [`set_enrollment`](Self::set_enrollment).
    pub fn enrollment(&self) -> Result<Option<Enrollment>> {
        let Some(identity) = self.enrolled_identity()? else {
            return Ok(None);
        };
        let policy = self
            .policy()?
            .ok_or_else(|| JournalError::corrupt("enrollment", "it has no policy"))?;
        Ok(Some(Enrollment {
            fleet_id: identity.fleet_id,
            machine_id: identity.machine_id,
            controller_url: identity.controller_url,
            fingerprint: identity.fingerprint,
            policy,
        }))
    }

    fn enrolled_identity(&self) -> Result<Option<EnrolledIdentity>> {
        self.identity_text("SELECT enrollment FROM identity WHERE id = 1")?
            .map(|text| from_json(&text, "enrollment"))
            .transpose()
    }

    /// Records who this worker is. It can be set once.
    ///
    /// Returns `true` when it was recorded and `false` when the same
    /// identity was already there (the stored policy is then left alone).
    /// A different identity fails with [`JournalError::AlreadyEnrolled`]:
    /// a journal never changes fleets or machines.
    pub fn set_enrollment(&self, enrollment: &Enrollment) -> Result<bool> {
        let identity = EnrolledIdentity {
            fleet_id: enrollment.fleet_id.clone(),
            machine_id: enrollment.machine_id.clone(),
            controller_url: enrollment.controller_url.clone(),
            fingerprint: enrollment.fingerprint.clone(),
        };
        match self.enrolled_identity()? {
            Some(existing) if existing == identity => Ok(false),
            Some(_) => Err(JournalError::AlreadyEnrolled),
            None => {
                let identity = to_json(&identity, "enrollment")?;
                let policy = to_json(&enrollment.policy, "machine policy")?;
                self.exec(
                    "UPDATE identity SET enrollment = ?1, policy = ?2 WHERE id = 1",
                    params![identity, policy],
                )?;
                Ok(true)
            }
        }
    }

    /// The current machine policy, or `None` before enrollment. Its
    /// `revision` is the policy revision the worker reports.
    pub fn policy(&self) -> Result<Option<MachinePolicy>> {
        self.identity_text("SELECT policy FROM identity WHERE id = 1")?
            .map(|text| from_json(&text, "machine policy"))
            .transpose()
    }

    /// Replaces the machine policy with a newer revision.
    ///
    /// Returns `true` when the stored policy changed and `false` when the
    /// identical policy was already recorded. The revision never goes
    /// backwards, and one revision never names two different policies: both
    /// fail with [`JournalError::StalePolicyRevision`].
    pub fn set_policy(&self, policy: &MachinePolicy) -> Result<bool> {
        let current = self.policy()?.ok_or(JournalError::NotEnrolled)?;
        if *policy == current {
            return Ok(false);
        }
        if policy.revision <= current.revision {
            return Err(JournalError::StalePolicyRevision {
                known: current.revision,
                offered: policy.revision,
            });
        }
        let policy = to_json(policy, "machine policy")?;
        self.exec(
            "UPDATE identity SET policy = ?1 WHERE id = 1",
            params![policy],
        )?;
        Ok(true)
    }

    /// The limit the machine owner put on running without controller
    /// contact, when a policy is recorded.
    pub(crate) fn max_disconnected_secs(&self) -> Result<Option<u64>> {
        Ok(self.policy()?.map(|policy| policy.max_disconnected_secs))
    }

    /// The generation of the most recent connection, 0 before the first.
    pub fn connection_generation(&self) -> Result<u64> {
        self.identity_counter(
            "SELECT connection_generation FROM identity WHERE id = 1",
            "connection generation",
        )
    }

    /// Takes the next connection generation. The increment is durable
    /// before the value is returned, so no two connections of this
    /// installation ever share a generation, across restarts included.
    pub fn next_connection_generation(&self) -> Result<u64> {
        let next = self
            .connection_generation()?
            .checked_add(1)
            .ok_or(JournalError::OutOfRange("connection generation"))?;
        self.exec(
            "UPDATE identity SET connection_generation = ?1 WHERE id = 1",
            params![to_db(next, "connection generation")?],
        )?;
        Ok(next)
    }

    /// The newest controller epoch this worker has seen, 0 before the first.
    pub fn known_epoch(&self) -> Result<u64> {
        self.identity_counter(
            "SELECT known_epoch FROM identity WHERE id = 1",
            "controller epoch",
        )
    }

    /// Checks a command's controller epoch against the newest one seen and
    /// remembers it when it is newer.
    ///
    /// The known epoch never goes backwards: an older epoch fails with
    /// [`JournalError::StaleEpoch`] and leaves the journal unchanged.
    pub fn observe_epoch(&self, epoch: u64) -> Result<()> {
        let known = self.known_epoch()?;
        if epoch < known {
            return Err(JournalError::StaleEpoch {
                known,
                offered: epoch,
            });
        }
        if epoch > known {
            self.exec(
                "UPDATE identity SET known_epoch = ?1 WHERE id = 1",
                params![to_db(epoch, "controller epoch")?],
            )?;
        }
        Ok(())
    }

    /// The machine owner's pause, drain and boost settings.
    pub fn local_control(&self) -> Result<LocalControl> {
        Ok(self
            .identity_text("SELECT local_control FROM identity WHERE id = 1")?
            .map(|text| from_json(&text, "local control"))
            .transpose()?
            .unwrap_or_default())
    }

    /// Stores the machine owner's pause, drain and boost settings. They
    /// survive restarts and need no controller.
    pub fn set_local_control(&self, control: LocalControl) -> Result<()> {
        let control = to_json(&control, "local control")?;
        self.exec(
            "UPDATE identity SET local_control = ?1 WHERE id = 1",
            params![control],
        )?;
        Ok(())
    }
}
