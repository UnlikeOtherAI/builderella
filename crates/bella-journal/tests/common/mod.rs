//! Fixtures shared by the journal's integration tests.
#![allow(dead_code)]

use bella_core::job::{CommandSpec, JobSpec, StepKind, StepSpec};
use bella_core::machine::{MachinePolicy, MachineProfile};
use bella_core::protocol::{
    AttemptRef, ExecutionAuthority, LocalAttemptState, Offer, ReleaseProof, ScopeIdentity,
    ScopeKind,
};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::{
    AttemptId, BootId, Digest, FleetId, JobId, MachineId, ManualClock, ProjectId, SnapshotId,
    UnixMillis,
};
use bella_journal::{Enrollment, Journal};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub const T0: UnixMillis = UnixMillis(1_700_000_000_000);
pub const GIB: u64 = 1024 * 1024 * 1024;

pub fn clock() -> ManualClock {
    ManualClock::at(T0)
}

pub fn boot(n: u32) -> BootId {
    BootId::derive(format!("boot-{n}").as_bytes())
}

/// An in-memory journal that has recovered in boot 1.
pub fn memory(clock: &ManualClock) -> Journal {
    let journal = Journal::open_in_memory(Arc::new(clock.clone())).unwrap();
    journal.recover(&boot(1)).unwrap();
    journal
}

/// Opens the journal file without recovering, as a starting process does.
pub fn open(path: &Path, clock: &ManualClock) -> Journal {
    Journal::open(path, Arc::new(clock.clone())).unwrap()
}

pub fn job(n: u32) -> JobId {
    JobId::derive(format!("job-{n}").as_bytes())
}

pub fn attempt_ref(job: u32, generation: u32) -> AttemptRef {
    AttemptRef {
        attempt_id: AttemptId::derive(format!("job-{job}-generation-{generation}").as_bytes()),
        generation,
    }
}

pub fn reservation() -> ResourceVector {
    ResourceVector::new(2000, 4 * GIB, 10 * GIB)
}

fn spec(reservation: &ResourceVector) -> JobSpec {
    let step = |name: &str, kind: StepKind| StepSpec {
        name: name.to_owned(),
        kind,
        command: CommandSpec::Argv {
            argv: vec!["true".to_owned()],
        },
        cwd: None,
        env: BTreeMap::new(),
        depends_on: Vec::new(),
        readiness: None,
        timeout_secs: None,
        resources: None,
        ports: Vec::new(),
    };
    JobSpec {
        recipe: "check".to_owned(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps: vec![
            step("build", StepKind::Task),
            step("test", StepKind::Test),
            step("teardown", StepKind::Cleanup),
        ],
        envelope: ResourceEnvelope::fixed(reservation.clone()),
        compatibility: Default::default(),
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 3,
        timeout_secs: 3600,
        cleanup_timeout_secs: 120,
        cloud: Default::default(),
        source_lan_only: false,
        outputs: Default::default(),
        environment: None,
        env: BTreeMap::new(),
        disconnected_continuation_secs: 120,
        parallelism: None,
    }
}

/// An offer for `job` at `generation` that expires at `expires_at`.
pub fn offer_with(
    job_number: u32,
    generation: u32,
    reservation: ResourceVector,
    expires_at: UnixMillis,
) -> Offer {
    Offer {
        attempt: attempt_ref(job_number, generation),
        job_id: job(job_number),
        project_id: ProjectId::derive(b"project"),
        snapshot_id: SnapshotId(Digest::of_bytes(b"snapshot")),
        manifest_digest: Digest::of_bytes(b"manifest"),
        spec: spec(&reservation),
        reservation,
        policy_revision: 1,
        expires_at,
    }
}

/// Generation 1 of `job`, expiring 15 s after [`T0`].
pub fn offer(job_number: u32) -> Offer {
    offer_with(job_number, 1, reservation(), T0.plus_secs(15))
}

/// A 60 s window, a one hour hard deadline and two minutes of permitted
/// disconnected running, all counted from `from`.
pub fn authority(from: UnixMillis) -> ExecutionAuthority {
    ExecutionAuthority {
        valid_until: from.plus_secs(60),
        hard_deadline: from.plus_secs(3600),
        disconnected_continuation_secs: 120,
    }
}

pub fn scope(name: &str) -> ScopeIdentity {
    ScopeIdentity {
        kind: ScopeKind::UnixProcessGroup,
        name: name.to_owned(),
        detail: "pgid=4242 started=1700000000".to_owned(),
    }
}

pub fn proof() -> ReleaseProof {
    ReleaseProof {
        scope_empty: true,
        workspace_removed: true,
        method: "process group has no members".to_owned(),
    }
}

/// Holds the offer and journals its start permit.
pub fn start(journal: &Journal, offer: &Offer, authority: ExecutionAuthority) -> AttemptRef {
    journal.hold_offer(offer).unwrap();
    journal.authorize_start(&offer.attempt, authority).unwrap();
    offer.attempt.clone()
}

/// Takes the offer all the way to `Running`, in the order a worker would.
pub fn run(journal: &Journal, offer: &Offer, authority: ExecutionAuthority) -> AttemptRef {
    let attempt = start(journal, offer, authority);
    let id = &attempt.attempt_id;
    journal.set_workspace(id, "work/attempt").unwrap();
    journal.set_state(id, LocalAttemptState::Preparing).unwrap();
    journal.set_scope(id, &scope("bella-attempt")).unwrap();
    journal.set_state(id, LocalAttemptState::Running).unwrap();
    attempt
}

pub fn state(journal: &Journal, attempt: &AttemptRef) -> LocalAttemptState {
    journal
        .attempt(&attempt.attempt_id)
        .unwrap()
        .expect("the attempt is journaled")
        .state
}

pub fn enrollment() -> Enrollment {
    Enrollment {
        fleet_id: FleetId::derive(b"fleet"),
        machine_id: MachineId::derive(b"machine"),
        controller_url: "https://controller.example:7420".to_owned(),
        fingerprint: Digest::of_bytes(b"certificate").to_string(),
        policy: MachinePolicy::defaults_for(MachineProfile::DedicatedBuilder),
    }
}
