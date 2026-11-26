//! The HTTP surface: authentication and roles, objects, snapshots, the
//! event stream, enrollment limits, TLS and the dashboard.

use crate::common::*;
use bella_controller::{Controller, ControllerConfig, ControllerError, EnrollLimits, Tls};
use bella_core::ErrorCode;
use bella_core::api::{
    CreatePairingCodeRequest, EnrollRequest, EnrollResponse, MissingObjectsRequest,
    MissingObjectsResponse, PairingCodeResponse, ProjectPolicy, RegisterSnapshotRequest, Role,
    SnapshotView, UpdateMachineRequest, WhoAmI,
};
use bella_core::ids::Digest;
use bella_core::machine::MachineProfile;
use bella_core::snapshot::{Entry, Manifest, RootManifest};
use bella_net::{Enrollment, Fingerprint};
use futures_util::StreamExt;
use std::io::Cursor;
use std::time::Duration;

/// A61: a developer cannot change machine or project policy, mint tokens
/// or create pairing codes; a viewer cannot submit; a machine token works
/// only for the worker socket, objects and snapshots.
#[tokio::test]
async fn a61_roles_are_enforced_on_every_route() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let viewer = harness.api(&harness.token(Role::Viewer).await);
    let enrolled = harness.enroll("builder", 8, 16).await;
    let machine = harness.api(&enrolled.token);
    let view = harness.machine(&enrolled.machine_id).await;

    let forbidden = |result: Result<serde_json::Value, Refusal>, what: &str| {
        let refusal = result.expect_err(what);
        assert_eq!(refusal.status, 403, "{what}");
        assert_eq!(refusal.error.code, ErrorCode::Forbidden, "{what}");
    };

    let mut policy = view.policy.clone();
    policy.foreground_cpu_percent = 0;
    let patch = UpdateMachineRequest {
        policy: Some(policy),
        ..UpdateMachineRequest::default()
    };
    let machine_path = format!("/machines/{}", enrolled.machine_id);
    forbidden(
        developer.try_patch(&machine_path, &patch).await,
        "developer patches a machine policy",
    );
    forbidden(
        developer
            .try_patch(
                &machine_path,
                &UpdateMachineRequest {
                    drain: Some(true),
                    ..UpdateMachineRequest::default()
                },
            )
            .await,
        "developer drains a machine",
    );
    forbidden(
        developer
            .try_put("/projects/app", &ProjectPolicy::default())
            .await,
        "developer changes a project policy",
    );
    forbidden(
        developer
            .try_post(
                "/tokens",
                &bella_core::api::CreateTokenRequest {
                    name: "escalation".into(),
                    role: Role::Admin,
                },
            )
            .await,
        "developer mints a token",
    );
    forbidden(
        developer
            .try_post(
                "/enroll/pairing-codes",
                &CreatePairingCodeRequest {
                    profile: MachineProfile::DedicatedBuilder,
                    ttl_secs: None,
                },
            )
            .await,
        "developer creates a pairing code",
    );

    // Nothing changed.
    let after = harness.machine(&enrolled.machine_id).await;
    assert_eq!(after.policy, view.policy);
    assert!(after.flags.is_empty());

    // A viewer reads but does not submit or upload.
    let snapshot = harness.snapshot(&developer, "app", &[("a", &b"a"[..])]).await;
    let _: Vec<serde_json::Value> = viewer.get("/jobs").await;
    forbidden(
        viewer
            .try_post("/jobs", &request("app", snapshot.clone(), spec()))
            .await,
        "viewer submits",
    );
    let refusal = viewer
        .put_bytes(&Digest::of_bytes(b"v"), "application/octet-stream", b"v".to_vec())
        .await
        .expect_err("viewer uploads");
    assert_eq!(refusal.status, 403);

    // A machine token is refused on user routes...
    for path in ["/fleet", "/jobs", "/whoami", "/queue", "/projects"] {
        forbidden(machine.try_get(path).await, &format!("machine GET {path}"));
    }
    forbidden(
        machine
            .try_post("/jobs", &request("app", snapshot.clone(), spec()))
            .await,
        "machine submits",
    );
    // ...and accepted for what a worker needs.
    let _: SnapshotView = machine.get(&format!("/snapshots/{snapshot}")).await;
    let response = machine
        .raw(
            reqwest::Method::GET,
            &format!("/objects/{}", Digest::of_bytes(b"a")),
        )
        .await;
    assert_eq!(response.status(), 200);
    machine.put_object(&Digest::of_bytes(b"artifact"), b"artifact").await;

    // A principal cannot open the worker socket.
    let opened = open_socket(harness.addr(), None, &harness.admin).await;
    assert_eq!(opened.err(), Some(403));

    // Missing and malformed tokens are 401 in the API's error shape.
    let refusal = harness
        .anonymous()
        .try_get::<WhoAmI>("/whoami")
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 401);
    assert_eq!(refusal.error.code, ErrorCode::Unauthorized);
    assert_eq!(refusal.headers["www-authenticate"], "Bearer");
    let refusal = harness
        .api("bella_dev_not-a-real-token")
        .try_get::<WhoAmI>("/whoami")
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 401);
    assert_eq!(open_socket(harness.addr(), None, "nonsense").await.err(), Some(401));

    // The administrator may do all of it.
    let changed: bella_core::api::MachineView = harness
        .admin_api()
        .try_patch(&machine_path, &patch)
        .await
        .expect("an administrator patches the policy");
    assert_eq!(changed.policy.foreground_cpu_percent, 0);
    assert!(changed.policy.revision > view.policy.revision);
}

#[tokio::test]
async fn whoami_version_and_health() {
    let harness = Harness::start().await;
    let me: WhoAmI = harness.admin_api().get("/whoami").await;
    assert_eq!(me.role, Role::Admin);
    let version: serde_json::Value = harness.admin_api().get("/version").await;
    assert_eq!(
        version["protocol_version"],
        bella_core::protocol::PROTOCOL_VERSION
    );
    for url in [
        format!("http://{}/healthz", harness.addr()),
        harness.anonymous().url("/healthz"),
    ] {
        let response = http_client(None).get(&url).send().await.unwrap();
        assert_eq!(response.status(), 200, "{url}");
    }
    let unknown = harness.admin_api().try_get::<serde_json::Value>("/nope").await;
    assert_eq!(unknown.unwrap_err().error.code, ErrorCode::NotFound);
}

/// Encode `content` the way the store keeps it.
fn stored_encoding(content: &[u8]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    bella_store::format::encode(&mut &content[..], &mut out, u64::MAX).unwrap();
    out.into_inner()
}

#[tokio::test]
async fn objects_in_both_encodings_and_digest_mismatch_refused() {
    let harness = Harness::start().await;
    let api = harness.developer().await;

    // Raw bytes in.
    let raw = b"raw object content\n".repeat(1000);
    let raw_digest = Digest::of_bytes(&raw);
    assert_eq!(
        api.put_bytes(&raw_digest, "application/octet-stream", raw.clone())
            .await
            .unwrap(),
        204
    );
    // The store's encoding in.
    let stored = b"stored object content".repeat(500);
    let stored_digest = Digest::of_bytes(&stored);
    assert_eq!(
        api.put_bytes(
            &stored_digest,
            "application/vnd.builderella.object",
            stored_encoding(&stored)
        )
        .await
        .unwrap(),
        204
    );
    // An object already stored is accepted again.
    assert_eq!(
        api.put_bytes(&raw_digest, "application/octet-stream", raw.clone())
            .await
            .unwrap(),
        204
    );

    // HEAD: 200 with the length, or 404.
    let head = api
        .raw(reqwest::Method::HEAD, &format!("/objects/{raw_digest}"))
        .await;
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["x-bella-object-length"],
        raw.len().to_string().as_str()
    );
    let absent = Digest::of_bytes(b"never uploaded");
    let head = api
        .raw(reqwest::Method::HEAD, &format!("/objects/{absent}"))
        .await;
    assert_eq!(head.status(), 404);

    // GET in the stored encoding, verified by decoding it.
    for (digest, content) in [(&raw_digest, &raw), (&stored_digest, &stored)] {
        let response = api
            .raw(reqwest::Method::GET, &format!("/objects/{digest}"))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "application/vnd.builderella.object"
        );
        let body = response.bytes().await.unwrap();
        let (mut reader, length) = bella_store::format::decode(&body[..], digest).unwrap();
        assert_eq!(length, content.len() as u64);
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut decoded).unwrap();
        assert_eq!(&decoded, content);

        let response = api
            .raw(reqwest::Method::GET, &format!("/objects/{digest}?raw=1"))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/octet-stream");
        assert_eq!(&response.bytes().await.unwrap()[..], &content[..]);
    }
    let missing = api
        .raw(reqwest::Method::GET, &format!("/objects/{absent}"))
        .await;
    assert_eq!(missing.status(), 404);

    // Content that does not match its digest is refused with 409, in both
    // encodings, and nothing is published.
    let wrong = Digest::of_bytes(b"something else");
    let refusal = api
        .put_bytes(&wrong, "application/octet-stream", b"not that".to_vec())
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 409);
    assert_eq!(refusal.error.code, ErrorCode::Conflict);
    let refusal = api
        .put_bytes(
            &wrong,
            "application/vnd.builderella.object",
            stored_encoding(b"not that either"),
        )
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 409);
    let head = api
        .raw(reqwest::Method::HEAD, &format!("/objects/{wrong}"))
        .await;
    assert_eq!(head.status(), 404);

    // A body that is not the store's encoding is a bad request.
    let refusal = api
        .put_bytes(&wrong, "application/vnd.builderella.object", b"garbage".to_vec())
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 400);

    // Which of these still need uploading.
    let answer: MissingObjectsResponse = api
        .post(
            "/objects/missing",
            &MissingObjectsRequest {
                digests: vec![raw_digest.clone(), absent.clone(), stored_digest.clone()],
            },
        )
        .await;
    assert_eq!(answer.missing, vec![absent]);
}

#[tokio::test]
async fn a_snapshot_with_a_missing_object_is_refused() {
    let harness = Harness::start().await;
    let api = harness.developer().await;
    let present = b"present".to_vec();
    api.put_object(&Digest::of_bytes(&present), &present).await;
    let absent = Digest::of_bytes(b"absent");
    let manifest = Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries: vec![
            Entry::File {
                path: "present".into(),
                digest: Digest::of_bytes(&present),
                size: present.len() as u64,
                executable: false,
            },
            Entry::File {
                path: "absent".into(),
                digest: absent.clone(),
                size: 6,
                executable: false,
            },
        ],
    }]);
    let register = RegisterSnapshotRequest {
        project: "app".into(),
        manifest: manifest.clone(),
        meta: meta(),
    };
    let refusal = api
        .try_post::<SnapshotView>("/snapshots", &register)
        .await
        .unwrap_err();
    assert_eq!(refusal.error.code, ErrorCode::SnapshotIncomplete);
    assert_eq!(refusal.status, 422);
    assert_eq!(
        refusal.error.details["missing"][0],
        serde_json::to_value(&absent).unwrap()
    );
    assert_eq!(refusal.error.details["missing_count"], 1);
    let unknown = api
        .try_get::<SnapshotView>(&format!("/snapshots/{}", manifest.id()))
        .await;
    assert_eq!(unknown.unwrap_err().status, 404);

    // Once the object is there the same snapshot registers, and its id is
    // the digest of the manifest object, which is itself stored.
    api.put_object(&absent, b"absent").await;
    let view: SnapshotView = api.post("/snapshots", &register).await;
    assert_eq!(view.id, manifest.id());
    assert_eq!(&view.manifest_digest, view.id.digest());
    let head = api
        .raw(
            reqwest::Method::HEAD,
            &format!("/objects/{}", view.manifest_digest),
        )
        .await;
    assert_eq!(head.status(), 200);

    // A manifest that does not validate is a bad request.
    let escaping = Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries: vec![Entry::Symlink {
            path: "out".into(),
            target: "../../etc".into(),
        }],
    }]);
    let refusal = api
        .try_post::<SnapshotView>(
            "/snapshots",
            &RegisterSnapshotRequest {
                project: "app".into(),
                manifest: escaping,
                meta: meta(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(refusal.error.code, ErrorCode::BadRequest);
}

// ------------------------------------------------------------------ events

/// One parsed server-sent event, or a comment line.
#[derive(Debug)]
enum Sse {
    Event { id: u64, kind: String, data: bella_core::api::EventRecord },
    Comment,
}

struct SseReader {
    body: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
}

impl SseReader {
    async fn open(api: &Api, query: &str, last_event_id: Option<u64>) -> Self {
        let mut builder = api.builder(reqwest::Method::GET, &format!("/events{query}"));
        if let Some(id) = last_event_id {
            builder = builder.header("last-event-id", id.to_string());
        }
        let response = builder.send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        Self {
            body: Box::pin(response.bytes_stream()),
            buffer: String::new(),
        }
    }

    /// The next frame, or `None` if nothing arrives within `limit`.
    async fn next_within(&mut self, limit: Duration) -> Option<Sse> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                return Some(parse_sse(block.trim_end()));
            }
            let chunk = tokio::time::timeout_at(deadline, self.body.next()).await.ok()??;
            self.buffer
                .push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
    }

    /// The next event, skipping comments; panics after [`PATIENCE`].
    async fn next_event(&mut self) -> (u64, String, bella_core::api::EventRecord) {
        loop {
            match self.next_within(PATIENCE).await.expect("an event arrives") {
                Sse::Event { id, kind, data } => return (id, kind, data),
                Sse::Comment => {}
            }
        }
    }

    /// Every event that arrives before the stream is quiet for `quiet`.
    async fn drain(&mut self, quiet: Duration) -> Vec<u64> {
        let mut seen = Vec::new();
        while let Some(frame) = self.next_within(quiet).await {
            if let Sse::Event { id, .. } = frame {
                seen.push(id);
            }
        }
        seen
    }
}

fn parse_sse(block: &str) -> Sse {
    if block.starts_with(':') {
        return Sse::Comment;
    }
    let mut lines = block.lines();
    let id = lines.next().unwrap().strip_prefix("id: ").unwrap().parse().unwrap();
    let kind = lines.next().unwrap().strip_prefix("event: ").unwrap().to_string();
    let data = serde_json::from_str(lines.next().unwrap().strip_prefix("data: ").unwrap()).unwrap();
    assert!(lines.next().is_none(), "one data line per event: {block}");
    Sse::Event { id, kind, data }
}

#[tokio::test]
async fn events_replay_after_a_sequence_and_otherwise_only_new_ones() {
    let harness = Harness::start_with(|config| {
        config.tuning.sse_keepalive = Duration::from_millis(200);
    })
    .await;
    let api = harness.admin_api();

    // Replay from the beginning: gap-free sequences from 1, in order, with
    // the event kind in both the field and the record.
    let mut replay = SseReader::open(&api, "?after=0", None).await;
    let (first, kind, record) = replay.next_event().await;
    assert_eq!(first, 1);
    assert_eq!(kind, record.kind);
    assert_eq!(record.seq, 1);
    let mut history = vec![first];
    history.extend(replay.drain(Duration::from_millis(500)).await);
    for pair in history.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "sequences are consecutive");
    }
    let newest = *history.last().unwrap();
    drop(replay);

    // Live only: nothing recorded before the stream opened is sent, and a
    // keep-alive comment comes while it is idle.
    let mut live = SseReader::open(&api, "", None).await;
    let mut comment = false;
    while let Some(frame) = live.next_within(Duration::from_millis(600)).await {
        match frame {
            Sse::Comment => comment = true,
            Sse::Event { id, .. } => panic!("an old event {id} was sent to a live stream"),
        }
    }
    assert!(comment, "an idle stream gets keep-alive comments");
    harness.token(Role::Viewer).await;
    let (id, kind, record) = live.next_event().await;
    assert!(id > newest);
    assert_eq!(kind, "principal.created");
    assert_eq!(record.seq, id);

    // Resuming after a sequence, by query or by header, starts right after it.
    let mut resumed = SseReader::open(&api, &format!("?after={newest}"), None).await;
    assert_eq!(resumed.next_event().await.0, newest + 1);
    let mut resumed = SseReader::open(&api, "", Some(newest - 1)).await;
    assert_eq!(resumed.next_event().await.0, newest);
    // A reconnect that repeats an older `after` with a newer header resumes
    // from the header.
    let mut resumed = SseReader::open(&api, "?after=1", Some(newest)).await;
    assert_eq!(resumed.next_event().await.0, newest + 1);
}

// -------------------------------------------------------------- enrollment

#[tokio::test]
async fn enrollment_is_rate_limited_before_the_code_is_checked() {
    let harness = Harness::start_with(|config| {
        config.tuning.enroll_limits = EnrollLimits {
            per_source_burst: 2,
            per_source_refill: Duration::from_secs(60),
            global_burst: 100,
            global_refill: Duration::from_secs(1),
        };
    })
    .await;
    let pairing: PairingCodeResponse = harness
        .admin_api()
        .post(
            "/enroll/pairing-codes",
            &CreatePairingCodeRequest {
                profile: MachineProfile::DedicatedBuilder,
                ttl_secs: None,
            },
        )
        .await;
    let attempt = |code: String| EnrollRequest {
        code,
        name: "guesser".into(),
        boot_id: boot(1),
        inventory: inventory("guesser", 4, 8),
    };
    let wrong = bella_net::PairingCode::generate().unwrap().to_string();
    for _ in 0..2 {
        let refusal = harness
            .anonymous()
            .try_post::<EnrollResponse>("/enroll", &attempt(wrong.clone()))
            .await
            .unwrap_err();
        assert_eq!(refusal.error.code, ErrorCode::Unauthorized);
    }
    // Over the limit even the right code is refused, without being used up.
    let refusal = harness
        .anonymous()
        .try_post::<EnrollResponse>("/enroll", &attempt(pairing.code.clone()))
        .await
        .unwrap_err();
    assert_eq!(refusal.status, 503);
    assert_eq!(refusal.error.code, ErrorCode::Unavailable);
    let retry_after: u64 = refusal.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry_after));

    harness.advance(Duration::from_secs(60));
    let enrolled: EnrollResponse = harness
        .anonymous()
        .post("/enroll", &attempt(pairing.code.clone()))
        .await;
    assert!(enrolled.machine_token.starts_with(bella_net::TOKEN_PREFIX));
    // A pairing code is single use.
    harness.advance(Duration::from_secs(60));
    let refusal = harness
        .anonymous()
        .try_post::<EnrollResponse>("/enroll", &attempt(pairing.code))
        .await
        .unwrap_err();
    assert_eq!(refusal.error.code, ErrorCode::Unauthorized);
}

// --------------------------------------------------------------------- TLS

#[tokio::test]
async fn tls_with_a_pinned_client_and_a_wrong_pin_refused() {
    let harness = Harness::start_with(|config| {
        config.tls = Tls::SelfSigned {
            subject_alt_names: Vec::new(),
        };
    })
    .await;
    let fingerprint = harness.fingerprint().expect("TLS is on");
    assert!(harness.handle().public_url().starts_with("https://"));

    // The pinned client gets through.
    let me: WhoAmI = harness.admin_api().get("/whoami").await;
    assert_eq!(me.role, Role::Admin);

    // A client pinning another certificate is refused at the handshake.
    let wrong = http_client(Some(Fingerprint::of_certificate(b"another certificate")));
    let result = wrong
        .get(format!("https://{}/api/v1/healthz", harness.addr()))
        .send()
        .await;
    assert!(result.is_err(), "a wrong pin must fail the handshake");
    // Plain HTTP on the TLS port gets no answer.
    let plain = http_client(None)
        .get(format!("http://{}/api/v1/healthz", harness.addr()))
        .send()
        .await;
    assert!(plain.is_err() || !plain.unwrap().status().is_success());

    // The enrollment string carries the URL and the pin.
    let pairing: PairingCodeResponse = harness
        .admin_api()
        .post(
            "/enroll/pairing-codes",
            &CreatePairingCodeRequest {
                profile: MachineProfile::DedicatedBuilder,
                ttl_secs: Some(60),
            },
        )
        .await;
    let enrollment = Enrollment::parse(&pairing.enrollment, false).unwrap();
    assert_eq!(enrollment.fingerprint(), Some(fingerprint));
    assert_eq!(enrollment.url().port(), harness.addr().port());
    assert_eq!(enrollment.code().to_string(), pairing.code);

    // A worker over TLS completes the handshake.
    let enrolled = harness.enroll("tls-builder", 4, 8).await;
    let (mut worker, verdicts) =
        FakeWorker::connect(&harness, &enrolled, boot(1), 1, Vec::new()).await;
    assert!(verdicts.is_empty());
    worker.heartbeat().await;
}

#[tokio::test]
async fn plain_http_is_refused_off_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ControllerConfig::new(dir.path(), "0.0.0.0:0".parse().unwrap());
    config.tls = Tls::Disabled;
    match Controller::start(config.clone()).await {
        Err(ControllerError::InsecureListen(_)) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Nothing was created for the refused start.
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn the_admin_token_is_created_once_and_kept_private() {
    let mut harness = Harness::start().await;
    let file = harness.dir.path().join(bella_controller::ADMIN_TOKEN_FILE);
    let stored = std::fs::read_to_string(&file).unwrap();
    assert_eq!(stored.trim(), harness.admin);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the token file is private");
    }
    let epoch = harness.handle().epoch();
    harness.restart().await;
    assert!(harness.handle().bootstrap_admin_token.is_none());
    assert_eq!(harness.handle().epoch(), epoch + 1);
    let me: WhoAmI = harness.admin_api().get("/whoami").await;
    assert_eq!(me.role, Role::Admin);
    assert!(harness.dir.path().join("ledger.sqlite").is_file());
    assert!(harness.dir.path().join("objects").is_dir());
}

// --------------------------------------------------------------- dashboard

#[tokio::test]
async fn the_dashboard_is_served_with_its_security_headers() {
    let harness = Harness::start().await;
    let base = format!("http://{}", harness.addr());
    let client = http_client(None);
    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let headers = page.headers().clone();
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["cache-control"], "no-cache");
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-frame-options"], "DENY");
    let body = page.text().await.unwrap();
    assert!(body.contains("<html"), "{body}");

    for path in ["/nope", "/assets/../ledger.sqlite", "/%2e%2e/ledger.sqlite", "/assets/missing.js"] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
}
