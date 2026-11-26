use crate::common::{exited, fixture, lost, proof, released, spec};
use bella_core::ids::Digest;
use bella_core::job::Outcome;
use bella_core::protocol::{ArtifactKind, AttemptEvent};
use bella_ledger::{LedgerError, LiveDigests};
use std::collections::BTreeSet;

fn published(path: &str, kind: ArtifactKind, content: &[u8]) -> AttemptEvent {
    AttemptEvent::ArtifactPublished {
        path: path.into(),
        kind,
        digest: Digest::of_bytes(content),
        size: content.len() as u64,
    }
}

#[test]
fn artifacts_are_recorded_with_their_provenance() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(
        &attempt,
        1,
        vec![
            AttemptEvent::Preparing,
            published("target/app", ArtifactKind::Artifact, b"binary"),
            published("reports/junit.xml", ArtifactKind::TestReport, b"<xml/>"),
        ],
    );
    let listed = f
        .ledger
        .record_artifact(
            &attempt,
            "target/app.dSYM",
            ArtifactKind::Artifact,
            &Digest::of_bytes(b"symbols"),
            7,
        )
        .unwrap();
    assert_eq!(listed.len(), 3);

    let artifacts = f.ledger.job_artifacts(&job).unwrap();
    assert_eq!(artifacts, listed);
    let summary: Vec<_> = artifacts
        .iter()
        .map(|a| (a.kind, a.path.as_str(), a.size))
        .collect();
    assert_eq!(
        summary,
        [
            (ArtifactKind::Artifact, "target/app", 6),
            (ArtifactKind::Artifact, "target/app.dSYM", 7),
            (ArtifactKind::TestReport, "reports/junit.xml", 6)
        ]
    );
    let snapshot = f.ledger.job(&job).unwrap().snapshot_id;
    for artifact in &artifacts {
        assert_eq!(artifact.job_id, job);
        assert_eq!(artifact.attempt_id, attempt.attempt_id);
        assert_eq!(
            artifact.snapshot_id, snapshot,
            "results stay tied to their source"
        );
    }
    assert_eq!(artifacts[0].digest, Digest::of_bytes(b"binary"));

    // Publishing a path again replaces it and keeps its id.
    f.events(
        &attempt,
        4,
        vec![published("target/app", ArtifactKind::Artifact, b"rebuilt!")],
    );
    let again = f.ledger.job_artifacts(&job).unwrap();
    assert_eq!(again.len(), 3);
    assert_eq!(again[0].id, artifacts[0].id);
    assert_eq!(again[0].digest, Digest::of_bytes(b"rebuilt!"));
    assert_eq!(again[0].size, 8);

    // Results are closed once the attempt is released.
    f.events(&attempt, 5, vec![exited(Outcome::Succeeded), released()]);
    assert!(matches!(
        f.ledger.record_artifact(
            &attempt,
            "late",
            ArtifactKind::Artifact,
            &Digest::of_bytes(b"late"),
            4
        ),
        Err(LedgerError::InvalidState(_))
    ));
    assert_eq!(f.ledger.job_artifacts(&job).unwrap().len(), 3);
    assert_eq!(
        f.ledger
            .attempt_artifacts(&attempt.attempt_id)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn a_job_shows_the_artifacts_of_its_current_attempt() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let first = f.start(&job, &machine);
    f.events(
        &first,
        1,
        vec![
            published("partial.log", ArtifactKind::Artifact, b"partial"),
            exited(lost()),
        ],
    );
    f.ledger.record_release(&first, &proof()).unwrap();
    // While the retry has produced nothing, the job has no results.
    let second = f.start(&job, &machine);
    assert!(f.ledger.job_artifacts(&job).unwrap().is_empty());
    f.events(
        &second,
        1,
        vec![published("target/app", ArtifactKind::Artifact, b"binary")],
    );
    f.events(&second, 2, vec![exited(Outcome::Succeeded), released()]);

    let current = f.ledger.job_artifacts(&job).unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].path, "target/app");
    assert_eq!(current[0].attempt_id, second.attempt_id);
    let earlier = f.ledger.attempt_artifacts(&first.attempt_id).unwrap();
    assert_eq!(earlier.len(), 1);
    assert_eq!(earlier[0].path, "partial.log");
}

#[test]
fn live_digests_cover_unfinished_snapshots_and_retained_artifacts() {
    let f = fixture();
    let machine = f.machine("builder");
    assert_eq!(f.ledger.live_digests().unwrap(), LiveDigests::default());

    let manifest = |project: &str| Digest::of_bytes(format!("manifest of {project}").as_bytes());
    // A registered snapshot that no job uses is not live.
    f.snapshot("unused", "unused");
    assert!(
        f.ledger
            .live_digests()
            .unwrap()
            .snapshot_manifests
            .is_empty()
    );

    let app = f.submit("app");
    let lib = f.submit("lib");
    let second_app = f.submit_spec("app", spec());
    assert_eq!(
        f.ledger.live_digests().unwrap().snapshot_manifests,
        BTreeSet::from([manifest("app"), manifest("lib")])
    );

    // `lib` finishes: its source is no longer needed, its artifact is.
    let attempt = f.start(&lib, &machine);
    f.events(
        &attempt,
        1,
        vec![published("lib.a", ArtifactKind::Artifact, b"archive")],
    );
    assert!(
        f.ledger
            .live_digests()
            .unwrap()
            .artifacts
            .contains(&Digest::of_bytes(b"archive"))
    );
    f.events(&attempt, 2, vec![exited(Outcome::Succeeded), released()]);
    let live = f.ledger.live_digests().unwrap();
    assert_eq!(live.snapshot_manifests, BTreeSet::from([manifest("app")]));
    assert_eq!(
        live.artifacts,
        BTreeSet::from([Digest::of_bytes(b"archive")])
    );

    // A snapshot stays live until every job that uses it has finished.
    f.ledger.request_cancel(&app).unwrap();
    assert_eq!(
        f.ledger.live_digests().unwrap().snapshot_manifests,
        BTreeSet::from([manifest("app")])
    );
    f.ledger.request_cancel(&second_app).unwrap();
    assert!(
        f.ledger
            .live_digests()
            .unwrap()
            .snapshot_manifests
            .is_empty()
    );
}
