//! The real client against the real controller, over pinned TLS on
//! loopback. The crate's own tests use a mock server; this one checks that
//! both sides read the wire details in `docs/architecture.md` §6.16 the
//! same way.

use bella_client::{Client, ClientError, HttpObjectStore, WorkerSocket, new_idempotency_key};
use bella_controller::{Controller, ControllerConfig, ControllerHandle};
use bella_core::UnixMillis;
use bella_core::api::{
    CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, ListJobsQuery,
    RegisterSnapshotRequest, Role, SubmitJobRequest, WaitQuery,
};
use bella_core::ids::{BootId, Digest};
use bella_core::job::{CommandSpec, JobSpec, StepKind, StepSpec};
use bella_core::machine::{Hardware, Inventory, LocalControl, MachineProfile, OsInfo};
use bella_core::platform::{Arch, Os, Platform};
use bella_core::protocol::{ControllerCommand, ControllerMsg, PROTOCOL_VERSION, WorkerMsg};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::snapshot::{Consistency, Entry, Manifest, RootManifest, SnapshotMeta};
use bella_core::store::{ObjectStore, get_bytes, put_bytes};
use bella_net::{ControllerUrl, Fingerprint};
use std::time::Duration;

const GIB: u64 = 1 << 30;
const PATIENCE: Duration = Duration::from_secs(20);

struct Running {
    handle: ControllerHandle,
    url: ControllerUrl,
    fingerprint: Fingerprint,
    admin: String,
    _dir: tempfile::TempDir,
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let config = ControllerConfig::new(dir.path(), "127.0.0.1:0".parse().unwrap());
    let handle = Controller::start(config)
        .await
        .expect("the controller starts");
    let fingerprint = handle.fingerprint.expect("TLS is on by default");
    let url = ControllerUrl::parse(&format!("https://{}", handle.local_addr), false).unwrap();
    let admin = handle.bootstrap_admin_token.clone().unwrap();
    Running {
        handle,
        url,
        fingerprint,
        admin,
        _dir: dir,
    }
}

impl Running {
    fn client(&self, token: Option<&str>) -> Client {
        Client::new(
            &self.url,
            Some(&self.fingerprint),
            token.map(str::to_string),
        )
        .unwrap()
    }
}

fn platform() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

fn inventory() -> Inventory {
    Inventory {
        revision: 1,
        hardware: Hardware {
            logical_cpus: 8,
            physical_cpus: None,
            cpu_model: String::new(),
            memory_bytes: 32 * GIB,
            disks: Vec::new(),
            gpus: Vec::new(),
        },
        os: OsInfo {
            platform: platform(),
            os_version: "24.04".into(),
            kernel: String::new(),
            hostname: "builder".into(),
        },
        capabilities: Vec::new(),
        executors: Vec::new(),
        worker_version: "0.1.0".into(),
    }
}

fn spec() -> JobSpec {
    JobSpec {
        recipe: "build".into(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps: vec![StepSpec {
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
        }],
        envelope: ResourceEnvelope::fixed(ResourceVector::new(1000, GIB, 0)),
        compatibility: Default::default(),
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 1,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_and_controller_agree_on_the_wire() {
    let running = start().await;
    let admin = running.client(Some(&running.admin));

    admin.health().await.expect("healthz answers");
    assert_eq!(admin.whoami().await.unwrap().role, Role::Admin);

    // A wrong pin is refused as a pin mismatch, before the token is sent.
    let wrong = Fingerprint::of_certificate(b"some other certificate");
    let impostor = Client::new(&running.url, Some(&wrong), Some(running.admin.clone())).unwrap();
    match impostor.whoami().await {
        Err(ClientError::Tls { pin_mismatch, .. }) => assert!(pin_mismatch),
        other => panic!("expected a pin mismatch, got {other:?}"),
    }

    // Tokens and roles.
    let developer_token = admin
        .create_token(&CreateTokenRequest {
            name: "dev".into(),
            role: Role::Developer,
        })
        .await
        .unwrap()
        .token;
    let developer = running.client(Some(&developer_token));
    assert_eq!(developer.whoami().await.unwrap().role, Role::Developer);
    match developer
        .create_token(&CreateTokenRequest {
            name: "escalate".into(),
            role: Role::Admin,
        })
        .await
    {
        Err(ClientError::Api(error)) => assert_eq!(error.code.http_status(), 403),
        other => panic!("a developer must not mint tokens, got {other:?}"),
    }

    // Objects through the blocking store: an empty, a small and a
    // multi-megabyte object round-trip and are then not missing.
    let large: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let small = b"fn main() {}\n".to_vec();
    let store_client = developer.clone();
    let handle = tokio::runtime::Handle::current();
    // The store blocks its caller, so it runs on a plain thread: it refuses
    // runtime threads, `spawn_blocking` ones included.
    let uploader = std::thread::spawn(move || {
        let store = HttpObjectStore::new(store_client, handle);
        let mut digests = Vec::new();
        let mut sizes = Vec::new();
        for content in [Vec::new(), small, large] {
            let digest = put_bytes(&store, &content).expect("upload");
            assert!(store.contains(&digest).unwrap());
            assert_eq!(store.size(&digest).unwrap(), Some(content.len() as u64));
            assert_eq!(get_bytes(&store, &digest).unwrap(), content);
            digests.push(digest);
            sizes.push(content.len() as u64);
        }
        assert!(store.missing(&digests).unwrap().is_empty());
        let absent = Digest::of_bytes(b"never uploaded");
        assert_eq!(
            store.missing(std::slice::from_ref(&absent)).unwrap(),
            vec![absent]
        );
        (digests, sizes)
    });
    let (digests, sizes) = tokio::task::spawn_blocking(move || uploader.join().unwrap())
        .await
        .unwrap();

    // A snapshot of those objects.
    let entries = ["empty", "main.rs", "big.bin"]
        .iter()
        .zip(digests.iter().zip(&sizes))
        .map(|(path, (digest, size))| Entry::File {
            path: (*path).to_string(),
            digest: digest.clone(),
            size: *size,
            executable: false,
        })
        .collect();
    let manifest = Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries,
    }]);
    let snapshot = developer
        .register_snapshot(&RegisterSnapshotRequest {
            project: "wire".into(),
            manifest: manifest.clone(),
            meta: SnapshotMeta {
                consistency: Consistency::BestEffort,
                captured_at: UnixMillis::now(),
                roots: Vec::new(),
                windows_incompatible: None,
                case_sensitive_only: false,
                has_symlinks: false,
            },
        })
        .await
        .unwrap();
    assert_eq!(snapshot.id, manifest.id());
    assert_eq!(
        developer.snapshot(&snapshot.id).await.unwrap().id,
        snapshot.id
    );

    // Replay from the start. A stream without `after` would only see
    // events after it connects, which is on its first read.
    let mut events = developer.events(Some(0));

    let request = SubmitJobRequest {
        project: "wire".into(),
        snapshot_id: snapshot.id.clone(),
        spec: spec(),
        origin_platform: platform(),
        session: None,
    };
    let key = new_idempotency_key();
    let job = developer.submit_job(&request, &key).await.unwrap();
    let again = developer.submit_job(&request, &key).await.unwrap();
    assert_eq!(
        job.id, again.id,
        "the same idempotency key yields the same job"
    );
    let listed = developer.jobs(&ListJobsQuery::default()).await.unwrap();
    assert!(listed.iter().any(|j| j.id == job.id));
    developer.explain_job(&job.id).await.unwrap();
    developer.queue().await.unwrap();
    developer.fleet().await.unwrap();

    let seen = tokio::time::timeout(PATIENCE, async {
        while let Some(event) = events.next().await {
            let event = event.expect("event stream item");
            if event.job_id.as_ref() == Some(&job.id) {
                return event;
            }
        }
        panic!("the event stream ended");
    })
    .await
    .expect("an event about the job arrives");
    assert!(seen.seq > 0);

    // Machine enrollment and the worker socket handshake.
    let pairing = admin
        .create_pairing_code(&CreatePairingCodeRequest {
            profile: MachineProfile::DedicatedBuilder,
            ttl_secs: None,
        })
        .await
        .unwrap();
    let anonymous = running.client(None);
    let boot_id = BootId::derive(b"boot-1");
    let enrolled = anonymous
        .enroll(&EnrollRequest {
            code: pairing.code,
            name: "builder".into(),
            boot_id: boot_id.clone(),
            inventory: inventory(),
        })
        .await
        .unwrap();
    let (sink, mut stream) = WorkerSocket::connect(
        &running.url,
        Some(&running.fingerprint),
        &enrolled.machine_token,
    )
    .await
    .expect("the machine token opens the worker socket");
    sink.send(&WorkerMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        machine_id: enrolled.machine_id.clone(),
        boot_id,
        connection_generation: 1,
        known_epoch: 0,
        inventory: Box::new(inventory()),
        policy_revision: 1,
        local_control: LocalControl::default(),
        attempts: Vec::new(),
    })
    .await
    .unwrap();
    let first = tokio::time::timeout(PATIENCE, stream.next()).await.unwrap();
    assert!(
        matches!(first, Some(Ok(ControllerMsg::Welcome { .. }))),
        "Welcome comes first, got {first:?}"
    );
    let second = tokio::time::timeout(PATIENCE, stream.next()).await.unwrap();
    match second {
        Some(Ok(ControllerMsg::Command { envelope })) => {
            assert!(matches!(
                envelope.command,
                ControllerCommand::Reconcile { .. }
            ));
        }
        other => panic!("Reconcile comes second, got {other:?}"),
    }
    let machine = admin.machine(&enrolled.machine_id).await.unwrap();
    assert_eq!(machine.id, enrolled.machine_id);
    sink.close().await;

    // Cancelling the job ends it; a wait then returns the outcome.
    developer.cancel_job(&job.id).await.unwrap();
    let waited = developer
        .wait_job(
            &job.id,
            &WaitQuery {
                timeout_ms: Some(20_000),
                after_event: None,
            },
        )
        .await
        .unwrap();
    assert!(
        !waited.timed_out,
        "the cancelled job finished: {:?}",
        waited.job.phase
    );
    assert!(waited.job.phase.is_terminal());

    running.handle.shutdown().await;
}
