use crate::common::fixture;
use bella_core::ErrorCode;
use bella_core::api::ProjectPolicy;
use bella_core::ids::{Digest, ProjectId, SnapshotId};
use bella_core::job::CloudMode;
use bella_core::snapshot::{Consistency, SnapshotMeta};
use bella_ledger::{LedgerError, NewSnapshot};

#[test]
fn a_project_is_created_once_with_the_default_policy() {
    let f = fixture();
    let created = f.ledger.get_or_create_project("app").unwrap();
    assert_eq!(created.name, "app");
    assert_eq!(created.policy, ProjectPolicy::default());
    assert_eq!(created.created_at, f.now());
    f.advance_secs(60);
    assert_eq!(f.ledger.get_or_create_project("app").unwrap(), created);
    assert_eq!(f.ledger.project(&created.id).unwrap(), created);
    assert_eq!(f.ledger.project_by_name("app").unwrap(), created);

    f.ledger.get_or_create_project("Zeta").unwrap();
    f.ledger.get_or_create_project("alpha").unwrap();
    let names: Vec<_> = f
        .ledger
        .projects()
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["Zeta", "alpha", "app"]);

    for invalid in ["", " padded ", &"x".repeat(201)] {
        assert!(matches!(
            f.ledger.get_or_create_project(invalid),
            Err(LedgerError::Invalid(_))
        ));
    }
    assert_eq!(
        f.ledger.project_by_name("missing").unwrap_err().code(),
        ErrorCode::NotFound
    );
    assert_eq!(
        f.ledger.project(&ProjectId::generate()).unwrap_err().code(),
        ErrorCode::NotFound
    );
}

#[test]
fn an_administrator_sets_project_policy() {
    let f = fixture();
    let policy = ProjectPolicy {
        weight: 250,
        max_concurrent_attempts: Some(3),
        max_environments: Some(1),
        cloud: CloudMode::Manual,
        source_lan_only: true,
        allow_urgent: true,
    };
    // Setting the policy of an unknown project creates it.
    let created = f
        .ledger
        .update_project_policy(&f.admin, "app", &policy)
        .unwrap();
    assert_eq!(created.policy, policy);
    assert_eq!(f.ledger.project_by_name("app").unwrap(), created);

    // The same policy again is not a change.
    let events = f.ledger.last_event_seq().unwrap();
    f.ledger
        .update_project_policy(&f.admin, "app", &policy)
        .unwrap();
    assert_eq!(f.ledger.last_event_seq().unwrap(), events);

    let weightless = ProjectPolicy {
        weight: 0,
        ..policy.clone()
    };
    assert!(matches!(
        f.ledger.update_project_policy(&f.admin, "app", &weightless),
        Err(LedgerError::Invalid(_))
    ));
    assert_eq!(f.ledger.project_by_name("app").unwrap().policy, policy);
}

#[test]
fn a_snapshot_is_registered_once() {
    let f = fixture();
    let id = SnapshotId(Digest::of_bytes(b"tree"));
    let manifest_digest = Digest::of_bytes(b"manifest object");
    let meta = SnapshotMeta {
        consistency: Consistency::Coordinated,
        captured_at: f.now(),
        roots: Vec::new(),
        windows_incompatible: Some("path `aux.c` is reserved".into()),
        case_sensitive_only: true,
        has_symlinks: true,
    };
    let new = |project: &'static str| NewSnapshot {
        id: &id,
        project,
        manifest_digest: &manifest_digest,
        file_count: 12,
        total_bytes: 34_567,
        meta: &meta,
    };
    assert_eq!(
        f.ledger.snapshot(&id).unwrap_err().code(),
        ErrorCode::NotFound
    );

    let view = f.ledger.register_snapshot(&new("app")).unwrap();
    assert_eq!(view.id, id);
    assert_eq!(view.project_id, f.ledger.project_by_name("app").unwrap().id);
    assert_eq!(view.manifest_digest, manifest_digest);
    assert_eq!((view.file_count, view.total_bytes), (12, 34_567));
    assert_eq!(view.meta, meta);
    assert_eq!(view.registered_at, f.now());
    assert_eq!(f.ledger.snapshot(&id).unwrap(), view);

    // Identical content registered again, even for another project, is the
    // same snapshot: nothing new is written.
    f.advance_secs(60);
    let events = f.ledger.last_event_seq().unwrap();
    assert_eq!(f.ledger.register_snapshot(&new("app")).unwrap(), view);
    assert_eq!(f.ledger.register_snapshot(&new("other")).unwrap(), view);
    assert_eq!(f.ledger.last_event_seq().unwrap(), events);
    assert!(f.ledger.project_by_name("other").is_err());
}
