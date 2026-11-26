//! Projects and registered snapshots.

use crate::db::{
    Ctx, all_rows, digest, exec, id, int, json, ms, opt_row, snapshot_id, text, time, to_json, uint,
};
use crate::events::emit;
use crate::identity::require_role;
use crate::{Ledger, LedgerError, NewSnapshot, Result};
use bella_core::api::{EntityKind, ProjectPolicy, ProjectView, Role, SnapshotView};
use bella_core::ids::{PrincipalId, ProjectId, SnapshotId};
use rusqlite::{Row, params};
use serde_json::json;

const PROJECT_COLUMNS: &str = "id, name, policy, created_at";

fn project_from(row: &Row<'_>) -> Result<ProjectView> {
    Ok(ProjectView {
        id: id(row, "id")?,
        name: text(row, "name")?,
        policy: json(row, "policy")?,
        created_at: time(row, "created_at")?,
    })
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 200 || name.trim() != name {
        return Err(LedgerError::Invalid(format!(
            "`{name}` is not a valid project name"
        )));
    }
    Ok(())
}

pub(crate) fn by_name(c: &Ctx<'_>, name: &str) -> Result<Option<ProjectView>> {
    opt_row(
        c.conn,
        &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE name = ?1"),
        params![name],
        project_from,
    )
}

pub(crate) fn get_or_create(c: &Ctx<'_>, name: &str) -> Result<ProjectView> {
    validate_name(name)?;
    if let Some(project) = by_name(c, name)? {
        return Ok(project);
    }
    let project = ProjectView {
        id: ProjectId::generate(),
        name: name.to_string(),
        policy: ProjectPolicy::default(),
        created_at: c.now,
    };
    exec(
        c.conn,
        "INSERT INTO projects (id, name, policy, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            project.id.as_str(),
            project.name,
            to_json(&project.policy)?,
            ms(c.now)
        ],
    )?;
    emit(
        c,
        EntityKind::Project,
        project.id.as_str(),
        None,
        "project.created",
        json!({ "name": name }),
    )?;
    Ok(project)
}

const SNAPSHOT_COLUMNS: &str =
    "id, project_id, manifest_digest, file_count, total_bytes, registered_at, meta";

fn snapshot_from(row: &Row<'_>) -> Result<SnapshotView> {
    Ok(SnapshotView {
        id: snapshot_id(row, "id")?,
        project_id: id(row, "project_id")?,
        manifest_digest: digest(row, "manifest_digest")?,
        file_count: uint(row, "file_count")?,
        total_bytes: uint(row, "total_bytes")?,
        registered_at: time(row, "registered_at")?,
        meta: json(row, "meta")?,
    })
}

pub(crate) fn snapshot(c: &Ctx<'_>, snapshot: &SnapshotId) -> Result<SnapshotView> {
    opt_row(
        c.conn,
        &format!("SELECT {SNAPSHOT_COLUMNS} FROM snapshots WHERE id = ?1"),
        params![snapshot.digest().as_str()],
        snapshot_from,
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("snapshot {snapshot}")))
}

impl Ledger {
    /// The project with this name, created with `ProjectPolicy::default()`
    /// if it does not exist.
    pub fn get_or_create_project(&self, name: &str) -> Result<ProjectView> {
        self.write(|c| get_or_create(c, name))
    }

    pub fn project_by_name(&self, name: &str) -> Result<ProjectView> {
        self.read(|c| {
            by_name(c, name)?.ok_or_else(|| LedgerError::NotFound(format!("project `{name}`")))
        })
    }

    pub fn project(&self, project: &ProjectId) -> Result<ProjectView> {
        self.read(|c| {
            opt_row(
                c.conn,
                &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?1"),
                params![project.as_str()],
                project_from,
            )?
            .ok_or_else(|| LedgerError::NotFound(format!("project {project}")))
        })
    }

    /// Every project, by name.
    pub fn projects(&self) -> Result<Vec<ProjectView>> {
        self.read(|c| {
            all_rows(
                c.conn,
                &format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY name"),
                params![],
                project_from,
            )
        })
    }

    /// Set a project's policy, creating the project if needed. `actor` must
    /// be an administrator; the check runs in the same transaction as the
    /// change.
    pub fn update_project_policy(
        &self,
        actor: &PrincipalId,
        name: &str,
        policy: &ProjectPolicy,
    ) -> Result<ProjectView> {
        self.write(|c| {
            let actor = require_role(c, actor, Role::Admin)?;
            if policy.weight == 0 {
                return Err(LedgerError::Invalid(
                    "a project's fair-share weight must be at least 1".into(),
                ));
            }
            let mut project = get_or_create(c, name)?;
            if project.policy == *policy {
                return Ok(project);
            }
            exec(
                c.conn,
                "UPDATE projects SET policy = ?1 WHERE id = ?2",
                params![to_json(policy)?, project.id.as_str()],
            )?;
            emit(
                c,
                EntityKind::Project,
                project.id.as_str(),
                None,
                "project.policy_changed",
                json!({ "by": actor.id, "policy": policy }),
            )?;
            project.policy = policy.clone();
            Ok(project)
        })
    }

    /// Record a snapshot whose objects are already stored. Registering a
    /// snapshot that is already known returns the existing record
    /// unchanged, whichever project registered it first: a snapshot id
    /// names content, not ownership.
    pub fn register_snapshot(&self, new: &NewSnapshot<'_>) -> Result<SnapshotView> {
        self.write(|c| {
            let existing = opt_row(
                c.conn,
                &format!("SELECT {SNAPSHOT_COLUMNS} FROM snapshots WHERE id = ?1"),
                params![new.id.digest().as_str()],
                snapshot_from,
            )?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
            let project = get_or_create(c, new.project)?;
            let view = SnapshotView {
                id: new.id.clone(),
                project_id: project.id,
                manifest_digest: new.manifest_digest.clone(),
                file_count: new.file_count,
                total_bytes: new.total_bytes,
                registered_at: c.now,
                meta: new.meta.clone(),
            };
            exec(
                c.conn,
                "INSERT INTO snapshots
                     (id, project_id, manifest_digest, file_count, total_bytes, meta, registered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    view.id.digest().as_str(),
                    view.project_id.as_str(),
                    view.manifest_digest.as_str(),
                    int(view.file_count)?,
                    int(view.total_bytes)?,
                    to_json(&view.meta)?,
                    ms(c.now)
                ],
            )?;
            emit(
                c,
                EntityKind::Snapshot,
                view.id.digest().as_str(),
                None,
                "snapshot.registered",
                json!({ "project_id": view.project_id, "file_count": view.file_count }),
            )?;
            Ok(view)
        })
    }

    pub fn snapshot(&self, id: &SnapshotId) -> Result<SnapshotView> {
        self.read(|c| snapshot(c, id))
    }
}
