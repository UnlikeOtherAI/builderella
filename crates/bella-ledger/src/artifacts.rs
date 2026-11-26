//! Artifacts and the digests the object store must keep.

use crate::db::{
    Ctx, all_rows, digest, enum_text, enumeration, exec, id, int, ms, snapshot_id, text, uint,
};
use crate::jobs::{self, JobRow};
use crate::{AttemptRecord, Ledger, LedgerError, LiveDigests, Result, attempts};
use bella_core::api::ArtifactView;
use bella_core::ids::{ArtifactId, AttemptId, Digest, JobId};
use bella_core::protocol::{ArtifactKind, AttemptRef};
use rusqlite::{Row, params};

const ARTIFACT_COLUMNS: &str = "id, job_id, attempt_id, snapshot_id, path, kind, digest, size";

fn artifact_from(row: &Row<'_>) -> Result<ArtifactView> {
    Ok(ArtifactView {
        id: id(row, "id")?,
        job_id: id(row, "job_id")?,
        attempt_id: id(row, "attempt_id")?,
        snapshot_id: snapshot_id(row, "snapshot_id")?,
        path: text(row, "path")?,
        kind: enumeration(row, "kind")?,
        digest: digest(row, "digest")?,
        size: uint(row, "size")?,
    })
}

fn for_attempt(c: &Ctx<'_>, attempt: &AttemptId) -> Result<Vec<ArtifactView>> {
    all_rows(
        c.conn,
        &format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE attempt_id = ?1 ORDER BY kind, path"
        ),
        params![attempt.as_str()],
        artifact_from,
    )
}

/// Record an output of an attempt. Publishing the same path and kind again
/// replaces the digest and size and keeps the artifact id.
pub(crate) fn record(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    job: &JobRow,
    path: &str,
    kind: ArtifactKind,
    digest: &Digest,
    size: u64,
) -> Result<()> {
    if path.is_empty() {
        return Err(LedgerError::Invalid("an artifact needs a path".into()));
    }
    exec(
        c.conn,
        "INSERT INTO artifacts
             (id, job_id, attempt_id, snapshot_id, path, kind, digest, size, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (attempt_id, kind, path)
         DO UPDATE SET digest = excluded.digest, size = excluded.size",
        params![
            ArtifactId::generate().as_str(),
            job.id.as_str(),
            attempt.id.as_str(),
            job.snapshot_id.digest().as_str(),
            path,
            enum_text(&kind)?,
            digest.as_str(),
            int(size)?,
            ms(c.now)
        ],
    )?;
    Ok(())
}

impl Ledger {
    /// Record an artifact outside the attempt event stream. The attempt
    /// must be the job's current generation and still hold its
    /// reservation. The object must already be in the object store.
    pub fn record_artifact(
        &self,
        attempt: &AttemptRef,
        path: &str,
        kind: ArtifactKind,
        digest: &Digest,
        size: u64,
    ) -> Result<Vec<ArtifactView>> {
        self.write(|c| {
            let found = attempts::load(c, &attempt.attempt_id)?;
            let job = jobs::load_row(c, &found.job_id)?;
            attempts::fence(&found, &job, attempt)?;
            if !found.held {
                return Err(LedgerError::InvalidState(format!(
                    "attempt {} is released; its results are closed",
                    found.id
                )));
            }
            record(c, &found, &job, path, kind, digest, size)?;
            for_attempt(c, &found.id)
        })
    }

    /// The artifacts of a job's current attempt, by kind and path. Outputs
    /// of earlier attempts that were lost and retried are not part of the
    /// job's result; read them with [`Ledger::attempt_artifacts`].
    pub fn job_artifacts(&self, job: &JobId) -> Result<Vec<ArtifactView>> {
        self.read(|c| {
            let job = jobs::load_row(c, job)?;
            all_rows(
                c.conn,
                &format!(
                    "SELECT {ARTIFACT_COLUMNS} FROM artifacts
                     WHERE attempt_id = (SELECT id FROM attempts
                                         WHERE job_id = ?1 AND generation = ?2)
                     ORDER BY kind, path"
                ),
                params![job.id.as_str(), job.generation],
                artifact_from,
            )
        })
    }

    pub fn attempt_artifacts(&self, attempt: &AttemptId) -> Result<Vec<ArtifactView>> {
        self.read(|c| {
            attempts::load(c, attempt)?;
            for_attempt(c, attempt)
        })
    }

    /// The roots the object store must keep: the manifest of every
    /// snapshot used by a job that is not finished or still holds a
    /// reservation, and every recorded artifact. See [`LiveDigests`] for
    /// how manifests extend to the objects they list.
    pub fn live_digests(&self) -> Result<LiveDigests> {
        self.read(|c| {
            let snapshot_manifests = all_rows(
                c.conn,
                "SELECT DISTINCT s.manifest_digest FROM snapshots s
                 JOIN jobs j ON j.snapshot_id = s.id
                 WHERE j.phase != 'finished'
                    OR EXISTS (SELECT 1 FROM attempts a WHERE a.job_id = j.id AND a.held = 1)",
                params![],
                |row| digest(row, 0),
            )?;
            let artifacts = all_rows(
                c.conn,
                "SELECT DISTINCT digest FROM artifacts",
                params![],
                |row| digest(row, 0),
            )?;
            Ok(LiveDigests {
                snapshot_manifests: snapshot_manifests.into_iter().collect(),
                artifacts: artifacts.into_iter().collect(),
            })
        })
    }
}
