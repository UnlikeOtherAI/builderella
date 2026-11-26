//! Shared fixtures: an in-memory ledger on a manual clock, plus builders
//! for the values most tests need.

#![allow(dead_code)]

use bella_core::api::{Role, SubmitJobRequest};
use bella_core::ids::{AttemptId, BootId, Digest, JobId, MachineId, PrincipalId, SnapshotId};
use bella_core::job::{CommandSpec, JobSpec, Outcome, StepKind, StepSpec};
use bella_core::machine::{Hardware, Inventory, LocalControl, MachineProfile, OsInfo, Telemetry};
use bella_core::platform::{Arch, Os, Platform};
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, Heartbeat, OfferDecision, ReleaseProof, UsageSummary,
};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::snapshot::{Consistency, SnapshotMeta};
use bella_core::{ManualClock, UnixMillis};
use bella_ledger::{EventDisposition, Ledger, LedgerError, NewMachine, NewSnapshot, OfferRecord};
use std::sync::Arc;
use std::time::Duration;

pub const GIB: u64 = 1 << 30;
pub const START: UnixMillis = UnixMillis(1_700_000_000_000);

pub struct Fixture {
    pub ledger: Arc<Ledger>,
    pub clock: ManualClock,
    pub admin: PrincipalId,
    pub dev: PrincipalId,
}

pub fn fixture() -> Fixture {
    let clock = ManualClock::at(START);
    let ledger = Ledger::open_in_memory(Arc::new(clock.clone())).unwrap();
    with_principals(ledger, clock)
}

pub fn with_principals(ledger: Ledger, clock: ManualClock) -> Fixture {
    let admin = ledger
        .create_principal("admin", Role::Admin, "hash-of-admin-token")
        .unwrap()
        .id;
    let dev = ledger
        .create_principal("dev", Role::Developer, "hash-of-dev-token")
        .unwrap()
        .id;
    Fixture {
        ledger: Arc::new(ledger),
        clock,
        admin,
        dev,
    }
}

pub fn platform() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

pub fn inventory(name: &str, cpus: u32, memory_gib: u64) -> Inventory {
    Inventory {
        revision: 1,
        hardware: Hardware {
            logical_cpus: cpus,
            physical_cpus: None,
            cpu_model: String::new(),
            memory_bytes: memory_gib * GIB,
            disks: Vec::new(),
            gpus: Vec::new(),
        },
        os: OsInfo {
            platform: platform(),
            os_version: "24.04".into(),
            kernel: String::new(),
            hostname: name.into(),
        },
        capabilities: Vec::new(),
        executors: Vec::new(),
        worker_version: "0.1.0".into(),
    }
}

pub fn boot(n: u32) -> BootId {
    BootId::derive(format!("boot-{n}").as_bytes())
}

pub fn vector(cpu_milli: u64, memory_gib: u64) -> ResourceVector {
    ResourceVector::new(cpu_milli, memory_gib * GIB, 0)
}

/// Capacity most tests pass for a 16-CPU, 64 GiB builder: well inside its
/// policy ceiling.
pub fn capacity() -> ResourceVector {
    ResourceVector::new(8000, 16 * GIB, 100 * GIB)
}

pub fn spec() -> JobSpec {
    JobSpec {
        recipe: "build".into(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps: vec![
            StepSpec {
                name: "compile".into(),
                kind: StepKind::Task,
                command: CommandSpec::Argv {
                    argv: vec!["make".into()],
                },
                cwd: None,
                env: Default::default(),
                depends_on: Vec::new(),
                readiness: None,
                timeout_secs: None,
                resources: None,
                ports: Vec::new(),
            },
            StepSpec {
                name: "test".into(),
                kind: StepKind::Test,
                command: CommandSpec::Argv {
                    argv: vec!["make".into(), "test".into()],
                },
                cwd: None,
                env: Default::default(),
                depends_on: vec!["compile".into()],
                readiness: None,
                timeout_secs: None,
                resources: None,
                ports: Vec::new(),
            },
        ],
        envelope: ResourceEnvelope::fixed(vector(1000, 1)),
        compatibility: Default::default(),
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 2,
        timeout_secs: 600,
        cleanup_timeout_secs: 60,
        cloud: Default::default(),
        source_lan_only: false,
        outputs: Default::default(),
        environment: None,
        env: Default::default(),
        disconnected_continuation_secs: 120,
        parallelism: None,
    }
}

pub fn proof() -> ReleaseProof {
    ReleaseProof {
        scope_empty: true,
        workspace_removed: true,
        method: "cgroup.procs empty".into(),
    }
}

pub fn usage() -> UsageSummary {
    UsageSummary {
        cpu_time_ms: 1200,
        memory_current_bytes: 0,
        memory_peak_bytes: 512 << 20,
        processes: 0,
        disk_bytes: 4096,
    }
}

pub fn exited(outcome: Outcome) -> AttemptEvent {
    AttemptEvent::Exited {
        outcome,
        usage: usage(),
    }
}

pub fn lost() -> Outcome {
    Outcome::InfraInterrupted {
        detail: "worker lost".into(),
    }
}

pub fn released() -> AttemptEvent {
    AttemptEvent::Released { proof: proof() }
}

pub fn unknown_attempt() -> AttemptRef {
    AttemptRef {
        attempt_id: AttemptId::generate(),
        generation: 1,
    }
}

pub fn heartbeat(machine: &MachineId, boot_id: &BootId, connection: u64, seq: u64) -> Heartbeat {
    Heartbeat {
        machine_id: machine.clone(),
        boot_id: boot_id.clone(),
        connection_generation: connection,
        seq,
        sent_at: START,
        inventory_revision: 1,
        policy_revision: 1,
        last_applied_command: None,
        telemetry: Telemetry::default(),
        local_control: LocalControl::default(),
        effective_budget: capacity(),
        admitting: true,
        flags: Vec::new(),
        attempts: Vec::new(),
    }
}

/// Standard base64 with padding, for building log chunks.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

impl Fixture {
    pub fn now(&self) -> UnixMillis {
        self.ledger.now()
    }

    pub fn advance_secs(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    /// When an offer created now expires.
    pub fn expiry(&self) -> UnixMillis {
        self.now().plus_secs(15)
    }

    /// Enroll a dedicated builder without connecting it.
    pub fn enroll(&self, name: &str, cpus: u32, memory_gib: u64) -> MachineId {
        self.ledger
            .enroll_machine(&NewMachine {
                name,
                token_hash: &format!("hash-of-{name}-token"),
                boot_id: &boot(1),
                inventory: &inventory(name, cpus, memory_gib),
                profile: MachineProfile::DedicatedBuilder,
            })
            .unwrap()
            .machine_id
    }

    /// Enroll a 16-CPU, 64 GiB builder, connect it and reconcile it, so it
    /// can be offered work.
    pub fn machine(&self, name: &str) -> MachineId {
        let machine = self.enroll(name, 16, 64);
        self.ledger.machine_connected(&machine, 1).unwrap();
        self.ledger.reconcile(&machine, &boot(1), &[]).unwrap();
        machine
    }

    pub fn snapshot(&self, project: &str, seed: &str) -> SnapshotId {
        let id = SnapshotId(Digest::of_bytes(seed.as_bytes()));
        let manifest_digest = Digest::of_bytes(format!("manifest of {seed}").as_bytes());
        self.ledger
            .register_snapshot(&NewSnapshot {
                id: &id,
                project,
                manifest_digest: &manifest_digest,
                file_count: 3,
                total_bytes: 4096,
                meta: &SnapshotMeta {
                    consistency: Consistency::BestEffort,
                    captured_at: self.now(),
                    roots: Vec::new(),
                    windows_incompatible: None,
                    case_sensitive_only: false,
                    has_symlinks: false,
                },
            })
            .unwrap();
        id
    }

    pub fn request(&self, project: &str, spec: JobSpec) -> SubmitJobRequest {
        SubmitJobRequest {
            project: project.into(),
            snapshot_id: self.snapshot(project, project),
            spec,
            origin_platform: platform(),
            session: None,
        }
    }

    pub fn submit_spec(&self, project: &str, spec: JobSpec) -> JobId {
        let request = self.request(project, spec);
        self.ledger
            .submit_job(&self.dev, None, &request)
            .unwrap()
            .id
    }

    pub fn submit(&self, project: &str) -> JobId {
        self.submit_spec(project, spec())
    }

    pub fn try_offer(
        &self,
        job: &JobId,
        machine: &MachineId,
        reservation: &ResourceVector,
    ) -> Result<OfferRecord, LedgerError> {
        self.ledger
            .create_offer(job, machine, reservation, &capacity(), None, self.expiry())
    }

    pub fn offer(&self, job: &JobId, machine: &MachineId) -> AttemptRef {
        self.try_offer(job, machine, &vector(1000, 1))
            .unwrap()
            .attempt
    }

    pub fn accept(&self, attempt: &AttemptRef) {
        self.ledger
            .record_offer_reply(attempt, &OfferDecision::Accepted)
            .unwrap();
    }

    /// Offer, accept and commit the start: the attempt may now be running.
    pub fn start(&self, job: &JobId, machine: &MachineId) -> AttemptRef {
        let attempt = self.offer(job, machine);
        self.accept(&attempt);
        self.ledger
            .commit_start(&attempt, self.ledger.epoch())
            .unwrap();
        attempt
    }

    pub fn event(
        &self,
        attempt: &AttemptRef,
        seq: u64,
        event: AttemptEvent,
    ) -> Result<EventDisposition, LedgerError> {
        self.ledger.record_attempt_event(&AttemptEventMsg {
            attempt: attempt.clone(),
            seq,
            at: self.now(),
            event,
        })
    }

    /// Apply events with consecutive sequence numbers starting at `first`.
    pub fn events(&self, attempt: &AttemptRef, first: u64, events: Vec<AttemptEvent>) {
        for (offset, event) in events.into_iter().enumerate() {
            let disposition = self.event(attempt, first + offset as u64, event).unwrap();
            assert_eq!(disposition, EventDisposition::Applied);
        }
    }

    /// Run a started attempt to a verified release with `outcome`.
    pub fn finish(&self, attempt: &AttemptRef, outcome: Outcome) {
        self.events(
            attempt,
            1,
            vec![AttemptEvent::Preparing, exited(outcome), released()],
        );
    }

    pub fn held(&self, machine: &MachineId) -> ResourceVector {
        self.ledger.machine_view(machine).unwrap().reserved
    }

    /// Kinds of the events recorded for a job, oldest first.
    pub fn job_event_kinds(&self, job: &JobId) -> Vec<String> {
        self.ledger
            .job_events_after(job, 0, 10_000)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }
}
