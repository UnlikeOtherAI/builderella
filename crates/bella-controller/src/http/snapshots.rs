//! Snapshot registration and lookup.

use super::{ApiJson, ApiPath, parse_id, validate_project_name};
use crate::auth::{Developer, Reader};
use crate::error::{ApiResult, Failure};
use crate::state::{App, Shared, lock};
use crate::sweeps::missing_objects;
use axum::Json;
use axum::extract::State;
use bella_core::api::{RegisterSnapshotRequest, SnapshotView};
use bella_core::snapshot::{
    Entry, Manifest, SnapshotMeta, find_collisions, windows_incompatibility,
};
use bella_core::store::{ObjectStore, StoreError, get_bytes};
use bella_core::{Digest, ErrorCode, SnapshotId};
use bella_ledger::NewSnapshot;
use tracing::info;

/// Missing digests listed in a `snapshot_incomplete` error.
const MAX_LISTED_MISSING: usize = 20;

fn incomplete(message: String, missing: &[Digest]) -> Failure {
    let listed: Vec<&Digest> = missing.iter().take(MAX_LISTED_MISSING).collect();
    Failure::new(ErrorCode::SnapshotIncomplete, message).with_details(serde_json::json!({
        "missing": listed,
        "missing_count": missing.len(),
    }))
}

/// Strengthen the client's description of the tree with what the manifest
/// itself shows. A client can under-report by mistake; the scheduler relies
/// on these facts to keep a snapshot away from machines that cannot hold it.
fn harden(meta: &mut SnapshotMeta, manifest: &Manifest) {
    let mut paths = Vec::new();
    for root in &manifest.roots {
        for entry in &root.entries {
            if matches!(entry, Entry::Symlink { .. }) {
                meta.has_symlinks = true;
            }
            paths.push(if root.mount == "." {
                entry.path().to_string()
            } else {
                format!("{}/{}", root.mount, entry.path())
            });
        }
    }
    if meta.windows_incompatible.is_none() {
        meta.windows_incompatible = paths.iter().find_map(|path| windows_incompatibility(path));
    }
    if !meta.case_sensitive_only {
        meta.case_sensitive_only = !find_collisions(paths.iter().map(String::as_str)).is_empty();
    }
}

/// `POST /snapshots`: validate the manifest, require every object it lists,
/// store its canonical bytes as an object and register it. The digest of
/// those bytes is the snapshot id.
pub(crate) async fn register(
    State(app): State<App>,
    Developer(principal): Developer,
    ApiJson(request): ApiJson<RegisterSnapshotRequest>,
) -> ApiResult<Json<SnapshotView>> {
    validate_project_name(&request.project)?;
    // Held so that a garbage collection cannot remove an object between the
    // check below and the registration.
    let _no_collection = app.gc_gate.read().await;
    let view = app
        .blocking(move |app| {
            let RegisterSnapshotRequest {
                project,
                manifest,
                mut meta,
            } = request;
            manifest
                .validate()
                .map_err(|error| Failure::bad_request(format!("invalid manifest: {error}")))?;
            let missing = missing_objects(app.store.as_ref(), manifest.digests())?;
            if !missing.is_empty() {
                return Err(incomplete(
                    format!(
                        "{} of the snapshot's objects have not been uploaded",
                        missing.len()
                    ),
                    &missing,
                ));
            }
            harden(&mut meta, &manifest);
            let bytes = manifest.canonical_bytes();
            let id = SnapshotId(Digest::of_bytes(&bytes));
            app.store.put(&mut bytes.as_slice(), Some(id.digest()))?;
            let view = app.ledger.register_snapshot(&NewSnapshot {
                id: &id,
                project: &project,
                manifest_digest: id.digest(),
                file_count: manifest.file_count(),
                total_bytes: manifest.total_bytes(),
                meta: &meta,
            })?;
            lock(&app.complete_snapshots).insert(id);
            Ok(view)
        })
        .await?;
    app.events.touch();
    info!(
        snapshot_id = %view.id,
        project_id = %view.project_id,
        files = view.file_count,
        bytes = view.total_bytes,
        registered_by = %principal.id,
        "snapshot registered"
    );
    Ok(Json(view))
}

/// `GET /snapshots/{id}`.
pub(crate) async fn show(
    State(app): State<App>,
    _: Reader,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<SnapshotView>> {
    let id = SnapshotId(parse_id::<Digest>(&id)?);
    Ok(Json(app.db(move |ledger| ledger.snapshot(&id)).await?))
}

/// Require that a registered snapshot's manifest and every object it lists
/// are still stored. Objects only disappear through garbage collection, so
/// a snapshot found complete is not checked again until the next one.
/// Call with the collection gate held.
pub(crate) fn ensure_complete(app: &Shared, id: &SnapshotId) -> ApiResult<()> {
    if lock(&app.complete_snapshots).contains(id) {
        return Ok(());
    }
    let snapshot = app.ledger.snapshot(id)?;
    let gone = |missing: &[Digest]| {
        incomplete(
            format!(
                "snapshot {id} is registered but {} of its objects are no longer stored; upload \
                 and register it again",
                missing.len()
            ),
            missing,
        )
    };
    let bytes = match get_bytes(app.store.as_ref(), &snapshot.manifest_digest) {
        Ok(bytes) => bytes,
        Err(StoreError::NotFound(digest)) => return Err(gone(&[digest])),
        Err(error) => return Err(error.into()),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| Failure::internal(format!("stored manifest {id} is unreadable: {error}")))?;
    let missing = missing_objects(app.store.as_ref(), manifest.digests())?;
    if !missing.is_empty() {
        return Err(gone(&missing));
    }
    lock(&app.complete_snapshots).insert(id.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::UnixMillis;
    use bella_core::snapshot::{Consistency, RootManifest};

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            consistency: Consistency::BestEffort,
            captured_at: UnixMillis(0),
            roots: Vec::new(),
            windows_incompatible: None,
            case_sensitive_only: false,
            has_symlinks: false,
        }
    }

    fn file(path: &str) -> Entry {
        Entry::File {
            path: path.into(),
            digest: Digest::of_bytes(path.as_bytes()),
            size: 1,
            executable: false,
        }
    }

    #[test]
    fn under_reported_facts_are_corrected_from_the_manifest() {
        let manifest = Manifest::new(vec![RootManifest {
            mount: ".".into(),
            entries: vec![
                file("README"),
                file("readme"),
                file("aux.txt"),
                Entry::Symlink {
                    path: "link".into(),
                    target: "README".into(),
                },
            ],
        }]);
        let mut described = meta();
        harden(&mut described, &manifest);
        assert!(described.has_symlinks);
        assert!(described.case_sensitive_only);
        assert!(described.windows_incompatible.is_some());
    }

    #[test]
    fn a_plain_tree_stays_portable() {
        let manifest = Manifest::new(vec![RootManifest {
            mount: ".".into(),
            entries: vec![file("src/main.rs"), file("Cargo.toml")],
        }]);
        let mut described = meta();
        harden(&mut described, &manifest);
        assert_eq!(described, meta());
    }

    #[test]
    fn at_most_twenty_missing_digests_are_listed() {
        let missing: Vec<Digest> = (0..50u32)
            .map(|n| Digest::of_bytes(&n.to_be_bytes()))
            .collect();
        let failure = incomplete("incomplete".into(), &missing);
        assert_eq!(failure.code(), ErrorCode::SnapshotIncomplete);
        let details = failure.details();
        assert_eq!(details["missing"].as_array().unwrap().len(), 20);
        assert_eq!(details["missing_count"], 50);
    }
}
