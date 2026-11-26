use crate::common::{boot, fixture, inventory, vector};
use bella_core::ErrorCode;
use bella_core::api::{ProjectPolicy, Role};
use bella_core::machine::{MachinePolicy, MachineProfile};
use bella_ledger::{Authenticated, LedgerError};

#[test]
fn principals_authenticate_by_token_hash_until_revoked() {
    let f = fixture();
    let viewer = f
        .ledger
        .create_principal("dashboard", Role::Viewer, "hash-of-viewer-token")
        .unwrap();
    assert_eq!(f.ledger.principal(&viewer.id).unwrap(), viewer);
    assert_eq!(f.ledger.principals().unwrap().len(), 3);

    match f.ledger.authenticate("hash-of-viewer-token").unwrap() {
        Authenticated::Principal(found) => {
            assert_eq!(found.id, viewer.id);
            assert_eq!(found.role, Role::Viewer);
        }
        other => panic!("expected a principal, got {other:?}"),
    }
    assert!(matches!(
        f.ledger.authenticate("hash-nobody-has"),
        Err(LedgerError::Unauthorized(_))
    ));

    assert!(f.ledger.revoke_principal(&viewer.id).unwrap());
    assert!(!f.ledger.revoke_principal(&viewer.id).unwrap());
    let error = f.ledger.authenticate("hash-of-viewer-token").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unauthorized);
    assert!(f.ledger.principal(&viewer.id).unwrap().revoked_at.is_some());
}

#[test]
fn a_token_hash_identifies_one_bearer() {
    let f = fixture();
    assert!(matches!(
        f.ledger
            .create_principal("twin", Role::Developer, "hash-of-dev-token"),
        Err(LedgerError::Conflict(_))
    ));
    assert!(matches!(
        f.ledger.create_principal("empty", Role::Developer, ""),
        Err(LedgerError::Invalid(_))
    ));
    // A machine cannot reuse a principal's hash either.
    let error = f
        .ledger
        .enroll_machine(&bella_ledger::NewMachine {
            name: "builder",
            token_hash: "hash-of-admin-token",
            boot_id: &boot(1),
            inventory: &inventory("builder", 8, 16),
            profile: MachineProfile::DedicatedBuilder,
        })
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Conflict);
    assert!(f.ledger.machine_views().unwrap().is_empty());
}

#[test]
fn machines_authenticate_until_retired() {
    let f = fixture();
    let machine = f.enroll("builder", 8, 16);
    match f.ledger.authenticate("hash-of-builder-token").unwrap() {
        Authenticated::Machine { id, name } => {
            assert_eq!(id, machine);
            assert_eq!(name, "builder");
        }
        other => panic!("expected a machine, got {other:?}"),
    }
    assert!(f.ledger.retire_machine(&machine).unwrap());
    assert!(matches!(
        f.ledger.authenticate("hash-of-builder-token"),
        Err(LedgerError::Unauthorized(_))
    ));
}

#[test]
fn a_pairing_code_is_consumed_exactly_once() {
    let f = fixture();
    let expires = f.now().plus_secs(600);
    f.ledger
        .create_pairing_code(
            "hash-of-code",
            MachineProfile::ProtectedWorkstation,
            expires,
            Some(&f.admin),
        )
        .unwrap();
    assert!(matches!(
        f.ledger.create_pairing_code(
            "hash-of-code",
            MachineProfile::DedicatedBuilder,
            expires,
            None
        ),
        Err(LedgerError::Conflict(_))
    ));

    assert_eq!(
        f.ledger.consume_pairing_code("hash-of-code").unwrap(),
        MachineProfile::ProtectedWorkstation
    );
    let again = f.ledger.consume_pairing_code("hash-of-code").unwrap_err();
    assert!(matches!(&again, LedgerError::Unauthorized(m) if m.contains("already used")));
    let unknown = f
        .ledger
        .consume_pairing_code("hash-of-nothing")
        .unwrap_err();
    assert!(matches!(&unknown, LedgerError::Unauthorized(m) if m.contains("unknown")));
}

#[test]
fn an_expired_pairing_code_is_refused() {
    let f = fixture();
    f.ledger
        .create_pairing_code(
            "hash-of-code",
            MachineProfile::DedicatedBuilder,
            f.now().plus_secs(60),
            None,
        )
        .unwrap();
    f.advance_secs(60);
    let error = f.ledger.consume_pairing_code("hash-of-code").unwrap_err();
    assert!(matches!(&error, LedgerError::Unauthorized(m) if m.contains("expired")));
    // A code cannot be created already expired.
    assert!(matches!(
        f.ledger.create_pairing_code(
            "hash-of-late-code",
            MachineProfile::DedicatedBuilder,
            f.now(),
            None
        ),
        Err(LedgerError::Invalid(_))
    ));
}

#[test]
fn concurrent_consumers_of_one_pairing_code_get_one_success() {
    let f = fixture();
    f.ledger
        .create_pairing_code(
            "hash-of-code",
            MachineProfile::DedicatedBuilder,
            f.now().plus_secs(600),
            None,
        )
        .unwrap();
    let successes: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..12)
            .map(|_| scope.spawn(|| f.ledger.consume_pairing_code("hash-of-code").is_ok()))
            .collect();
        handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum()
    });
    assert_eq!(successes, 1);
}

#[test]
fn enrolling_with_a_pairing_code_is_one_transaction() {
    let f = fixture();
    f.ledger
        .create_pairing_code(
            "hash-of-code",
            MachineProfile::ProtectedWorkstation,
            f.now().plus_secs(600),
            None,
        )
        .unwrap();
    // The enrollment fails (the token hash is taken), so the code is not spent.
    let failed = f.ledger.enroll_with_pairing_code(
        "hash-of-code",
        "laptop",
        "hash-of-dev-token",
        &boot(1),
        &inventory("laptop", 8, 16),
    );
    assert!(matches!(failed, Err(LedgerError::Conflict(_))));
    assert!(f.ledger.machine_views().unwrap().is_empty());

    let enrolled = f
        .ledger
        .enroll_with_pairing_code(
            "hash-of-code",
            "laptop",
            "hash-of-laptop-token",
            &boot(1),
            &inventory("laptop", 8, 16),
        )
        .unwrap();
    assert_eq!(enrolled.fleet_id, *f.ledger.fleet_id());
    assert_eq!(
        enrolled.policy,
        MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation)
    );

    // The code is spent: a second machine cannot join with it.
    let second = f.ledger.enroll_with_pairing_code(
        "hash-of-code",
        "desktop",
        "hash-of-desktop-token",
        &boot(1),
        &inventory("desktop", 8, 16),
    );
    assert!(matches!(second, Err(LedgerError::Unauthorized(_))));
    assert_eq!(f.ledger.machine_views().unwrap().len(), 1);
}

/// A61: an ordinary agent cannot change machine or project policy. The
/// check is exposed for the API layer and repeated inside the mutators.
#[test]
fn a61_policy_changes_require_an_admin() {
    let f = fixture();
    let viewer = f
        .ledger
        .create_principal("dashboard", Role::Viewer, "hash-of-viewer-token")
        .unwrap()
        .id;
    let machine = f.machine("builder");

    // The check the API layer calls.
    assert_eq!(f.ledger.require_admin(&f.admin).unwrap().role, Role::Admin);
    for principal in [&f.dev, &viewer] {
        let error = f.ledger.require_admin(principal).unwrap_err();
        assert!(matches!(error, LedgerError::Forbidden(_)));
        assert_eq!(error.code(), ErrorCode::Forbidden);
    }
    assert!(f.ledger.require_role(&f.dev, Role::Developer).is_ok());
    assert!(f.ledger.require_role(&viewer, Role::Developer).is_err());

    // Lowering a workstation's reserve is refused and changes nothing.
    let mut generous = f.ledger.machine_policy(&machine).unwrap();
    generous.foreground_memory_percent = 0;
    generous.max_managed = Some(vector(64_000, 512));
    for principal in [&f.dev, &viewer] {
        let error = f
            .ledger
            .replace_machine_policy(principal, &machine, &generous)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Forbidden);
    }
    let stored = f.ledger.machine_policy(&machine).unwrap();
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.foreground_memory_percent, 10);

    // So is widening a project's policy.
    let project = f.ledger.get_or_create_project("app").unwrap();
    let mut wide = project.policy.clone();
    wide.weight = 10_000;
    wide.allow_urgent = true;
    for principal in [&f.dev, &viewer] {
        let error = f
            .ledger
            .update_project_policy(principal, "app", &wide)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Forbidden);
    }
    assert_eq!(
        f.ledger.project_by_name("app").unwrap().policy,
        ProjectPolicy::default()
    );

    // An administrator can do both.
    let replaced = f
        .ledger
        .replace_machine_policy(&f.admin, &machine, &generous)
        .unwrap();
    assert_eq!(replaced.revision, 2);
    let updated = f
        .ledger
        .update_project_policy(&f.admin, "app", &wide)
        .unwrap();
    assert_eq!(updated.policy.weight, 10_000);

    // A revoked administrator cannot.
    assert!(f.ledger.revoke_principal(&f.admin).unwrap());
    let error = f
        .ledger
        .update_project_policy(&f.admin, "app", &ProjectPolicy::default())
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unauthorized);
    assert_eq!(
        f.ledger.project_by_name("app").unwrap().policy.weight,
        10_000
    );
}
