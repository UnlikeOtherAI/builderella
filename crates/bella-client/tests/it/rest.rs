//! Every route: the request the server receives and the value the client
//! returns.

use crate::support::{
    Mock, Recorded, Reply, Server, assert_authenticated, assert_user_agent, fixtures,
};
use axum::http::Method;
use bella_client::{Client, ClientError, RetryPolicy};
use bella_core::api::{
    CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, EnrollResponse, ExplainResponse,
    ListJobsQuery, LogQuery, MissingObjectsRequest, MissingObjectsResponse, PlacementOption,
    PlanRequest, PlanResponse, ProjectPolicy, RegisterSnapshotRequest, Role, SnapshotView,
    SubmitJobRequest, UpdateMachineRequest, WaitQuery, WaitResponse,
};
use bella_core::job::{QueueReason, QueueReasonCode};
use bella_core::machine::MachineProfile;
use bella_core::platform::Platform;
use bella_core::snapshot::{Consistency, Entry, Manifest, RootManifest, SnapshotMeta};
use bella_core::{ApiError, BootId, Digest, ErrorCode, UnixMillis};

fn platform() -> Platform {
    serde_json::from_value(serde_json::json!({"os": "linux", "arch": "x86_64"})).unwrap()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = items
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    pairs.sort();
    pairs
}

/// The one request the mock saw since the last call.
fn take(mock: &Mock) -> Recorded {
    let request = mock.single();
    mock.clear();
    request
}

/// A read: authenticated, asking for JSON, no query and no body.
fn assert_plain(request: &Recorded) {
    assert_authenticated(request);
    assert_eq!(request.header("accept"), Some("application/json"));
    assert_eq!(request.query, None, "{}", request.path);
    assert!(request.body.is_empty(), "{}", request.path);
}

/// A request with a JSON body equal to `sent`.
fn assert_json_body<T>(request: &Recorded, sent: &T)
where
    T: serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(&request.json::<T>(), sent, "{}", request.path);
}

async fn every_route_round_trips(tls: bool) {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::start(tls, mock.router()).await;
    let client = server.client();

    let job = fx
        .jobs
        .iter()
        .find(|job| job.spec.is_some())
        .unwrap()
        .clone();
    let job_path = format!("/api/v1/jobs/{}", job.id);
    let spec = job.spec.clone().unwrap();
    let machine = fx.fleet.machines[0].clone();
    let environment = fx.environments[0].clone();
    let environment_path = format!("/api/v1/environments/{}", environment.id);

    // GET /healthz: no token.
    mock.on(Method::GET, "/api/v1/healthz", Reply::text(200, "ok"));
    client.health().await.unwrap();
    let request = take(&mock);
    assert_eq!(request.header("authorization"), None);
    assert_user_agent(&request);
    assert_eq!(request.query, None);

    // GET /whoami
    mock.on(Method::GET, "/api/v1/whoami", Reply::json(&fx.whoami));
    assert_eq!(client.whoami().await.unwrap(), fx.whoami);
    assert_plain(&take(&mock));

    // POST /tokens
    let create_token = CreateTokenRequest {
        name: "ci".into(),
        role: Role::Developer,
    };
    mock.on(Method::POST, "/api/v1/tokens", Reply::json(&fx.token));
    assert_eq!(client.create_token(&create_token).await.unwrap(), fx.token);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &create_token);

    // POST /enroll/pairing-codes
    let create_code = CreatePairingCodeRequest {
        profile: MachineProfile::DedicatedBuilder,
        ttl_secs: Some(600),
    };
    mock.on(
        Method::POST,
        "/api/v1/enroll/pairing-codes",
        Reply::json(&fx.pairing_code),
    );
    assert_eq!(
        client.create_pairing_code(&create_code).await.unwrap(),
        fx.pairing_code
    );
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &create_code);

    // POST /enroll: the pairing code is the credential, no token.
    let enroll = EnrollRequest {
        code: "K7Q2-M4XP".into(),
        name: "builder-9".into(),
        boot_id: BootId::derive(b"boot"),
        inventory: machine.inventory.clone(),
    };
    let enrolled = EnrollResponse {
        fleet_id: fx.whoami.fleet_id.clone(),
        machine_id: machine.id.clone(),
        machine_token: "bella_mch_secret".into(),
        policy: machine.policy.clone(),
    };
    mock.on(Method::POST, "/api/v1/enroll", Reply::json(&enrolled));
    assert_eq!(client.enroll(&enroll).await.unwrap(), enrolled);
    let request = take(&mock);
    assert_eq!(request.header("authorization"), None);
    assert_user_agent(&request);
    assert_json_body(&request, &enroll);

    // POST /objects/missing
    let wanted = MissingObjectsRequest {
        digests: vec![Digest::of_bytes(b"a"), Digest::of_bytes(b"b")],
    };
    let missing = MissingObjectsResponse {
        missing: vec![Digest::of_bytes(b"b")],
    };
    mock.on(
        Method::POST,
        "/api/v1/objects/missing",
        Reply::json(&missing),
    );
    assert_eq!(client.missing_objects(&wanted).await.unwrap(), missing);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &wanted);

    // POST /snapshots and GET /snapshots/{id}
    let manifest = Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries: vec![
            Entry::File {
                path: "src/main.rs".into(),
                digest: Digest::of_bytes(b"fn main() {}"),
                size: 12,
                executable: false,
            },
            Entry::Symlink {
                path: "latest".into(),
                target: "src".into(),
            },
            Entry::Dir {
                path: "empty".into(),
            },
        ],
    }]);
    let meta = SnapshotMeta {
        consistency: Consistency::BestEffort,
        captured_at: UnixMillis(1_795_000_000_000),
        roots: Vec::new(),
        windows_incompatible: None,
        case_sensitive_only: false,
        has_symlinks: true,
    };
    let snapshot_id = manifest.id();
    let register = RegisterSnapshotRequest {
        project: fx.projects[0].name.clone(),
        manifest: manifest.clone(),
        meta: meta.clone(),
    };
    let snapshot = SnapshotView {
        id: snapshot_id.clone(),
        project_id: fx.projects[0].id.clone(),
        manifest_digest: snapshot_id.digest().clone(),
        file_count: manifest.file_count(),
        total_bytes: manifest.total_bytes(),
        registered_at: UnixMillis(1_795_000_000_500),
        meta,
    };
    mock.on(Method::POST, "/api/v1/snapshots", Reply::json(&snapshot));
    assert_eq!(client.register_snapshot(&register).await.unwrap(), snapshot);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &register);

    let snapshot_path = format!("/api/v1/snapshots/{snapshot_id}");
    mock.on(Method::GET, &snapshot_path, Reply::json(&snapshot));
    assert_eq!(client.snapshot(&snapshot_id).await.unwrap(), snapshot);
    assert_plain(&take(&mock));

    // GET /projects and PUT /projects/{name}, with a name that needs
    // encoding.
    mock.on(Method::GET, "/api/v1/projects", Reply::json(&fx.projects));
    assert_eq!(client.projects().await.unwrap(), fx.projects);
    assert_plain(&take(&mock));

    let policy = ProjectPolicy {
        weight: 250,
        max_concurrent_attempts: Some(3),
        ..ProjectPolicy::default()
    };
    mock.on(
        Method::PUT,
        "/api/v1/projects/web%20app%2Fv2",
        Reply::json(&fx.projects[0]),
    );
    assert_eq!(
        client.put_project("web app/v2", &policy).await.unwrap(),
        fx.projects[0]
    );
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &policy);

    // POST /plan
    let plan = PlanRequest {
        project: job.project_name.clone(),
        spec: spec.clone(),
        origin_platform: platform(),
    };
    let planned = PlanResponse {
        feasible: true,
        placements: vec![PlacementOption {
            machine_id: machine.id.clone(),
            machine_name: machine.name.clone(),
            eligible_now: true,
            blocker: None,
        }],
        issues: vec!["one machine is draining".into()],
    };
    mock.on(Method::POST, "/api/v1/plan", Reply::json(&planned));
    assert_eq!(client.plan(&plan).await.unwrap(), planned);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &plan);

    // POST /jobs with the idempotency key.
    let submit = SubmitJobRequest {
        project: job.project_name.clone(),
        snapshot_id: job.snapshot_id.clone(),
        spec: spec.clone(),
        origin_platform: platform(),
        session: job.session.clone(),
    };
    mock.on(Method::POST, "/api/v1/jobs", Reply::json(&job));
    assert_eq!(client.submit_job(&submit, "key-123").await.unwrap(), job);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_eq!(request.header("idempotency-key"), Some("key-123"));
    assert_json_body(&request, &submit);

    // GET /jobs with and without a query.
    mock.on(Method::GET, "/api/v1/jobs", Reply::json(&fx.jobs));
    assert_eq!(
        client.jobs(&ListJobsQuery::default()).await.unwrap(),
        fx.jobs
    );
    assert_plain(&take(&mock));
    let session = job.session.as_ref().map(|session| session.id.clone());
    let query = ListJobsQuery {
        project: Some("web & api".into()),
        session: session.clone(),
        active: Some(true),
        limit: Some(50),
    };
    assert_eq!(client.jobs(&query).await.unwrap(), fx.jobs);
    let request = take(&mock);
    assert_authenticated(&request);
    let mut expected = vec![
        ("project", "web & api"),
        ("active", "true"),
        ("limit", "50"),
    ];
    let session_text = session.as_ref().map(ToString::to_string);
    if let Some(session) = &session_text {
        expected.push(("session", session));
    }
    assert_eq!(request.query_pairs(), pairs(&expected));

    // GET /jobs/{id}
    mock.on(Method::GET, &job_path, Reply::json(&job));
    assert_eq!(client.job(&job.id).await.unwrap(), job);
    assert_plain(&take(&mock));

    // GET /jobs/{id}/wait
    let waited = WaitResponse {
        job: job.clone(),
        timed_out: true,
    };
    mock.on(
        Method::GET,
        &format!("{job_path}/wait"),
        Reply::json(&waited),
    );
    let wait = WaitQuery {
        timeout_ms: Some(2_500),
        after_event: Some(job.last_event),
    };
    assert_eq!(client.wait_job(&job.id, &wait).await.unwrap(), waited);
    let request = take(&mock);
    assert_authenticated(&request);
    assert_eq!(
        request.query_pairs(),
        pairs(&[
            ("timeout_ms", "2500"),
            ("after_event", &job.last_event.to_string())
        ])
    );

    // POST /jobs/{id}/cancel: no body.
    mock.on(
        Method::POST,
        &format!("{job_path}/cancel"),
        Reply::json(&job),
    );
    assert_eq!(client.cancel_job(&job.id).await.unwrap(), job);
    assert_plain(&take(&mock));

    // GET /jobs/{id}/logs
    mock.on(
        Method::GET,
        &format!("{job_path}/logs"),
        Reply::json(&fx.log_pages[0]),
    );
    let logs = LogQuery {
        step: Some("build".into()),
        cursor: Some("b2Zmc2V0PTEw".into()),
        tail: Some(false),
        limit_bytes: Some(65_536),
    };
    assert_eq!(
        client.job_logs(&job.id, &logs).await.unwrap(),
        fx.log_pages[0]
    );
    let request = take(&mock);
    assert_authenticated(&request);
    assert_eq!(
        request.query_pairs(),
        pairs(&[
            ("step", "build"),
            ("cursor", "b2Zmc2V0PTEw"),
            ("tail", "false"),
            ("limit_bytes", "65536"),
        ])
    );
    assert_eq!(
        client
            .job_logs(&job.id, &LogQuery::default())
            .await
            .unwrap(),
        fx.log_pages[0]
    );
    assert_plain(&take(&mock));

    // GET /jobs/{id}/artifacts
    mock.on(
        Method::GET,
        &format!("{job_path}/artifacts"),
        Reply::json(&fx.artifacts),
    );
    assert_eq!(client.job_artifacts(&job.id).await.unwrap(), fx.artifacts);
    assert_plain(&take(&mock));

    // GET /jobs/{id}/explain
    let explained = ExplainResponse {
        job_id: job.id.clone(),
        phase: job.phase,
        reason: QueueReason::new(QueueReasonCode::WaitingForCapacity, "every builder is busy"),
        position: Some(3),
        suggestions: vec!["wait".into(), "release an environment".into()],
    };
    mock.on(
        Method::GET,
        &format!("{job_path}/explain"),
        Reply::json(&explained),
    );
    assert_eq!(client.explain_job(&job.id).await.unwrap(), explained);
    assert_plain(&take(&mock));

    // Environments.
    mock.on(
        Method::GET,
        "/api/v1/environments",
        Reply::json(&fx.environments),
    );
    assert_eq!(client.environments().await.unwrap(), fx.environments);
    assert_plain(&take(&mock));

    mock.on(Method::GET, &environment_path, Reply::json(&environment));
    assert_eq!(
        client.environment(&environment.id).await.unwrap(),
        environment
    );
    assert_plain(&take(&mock));

    mock.on(
        Method::POST,
        &format!("{environment_path}/renew"),
        Reply::json(&environment),
    );
    assert_eq!(
        client.renew_environment(&environment.id).await.unwrap(),
        environment
    );
    assert_plain(&take(&mock));

    mock.on(
        Method::POST,
        &format!("{environment_path}/finish"),
        Reply::json(&environment),
    );
    assert_eq!(
        client.finish_environment(&environment.id).await.unwrap(),
        environment
    );
    assert_plain(&take(&mock));

    // Fleet, machines, queue.
    mock.on(Method::GET, "/api/v1/fleet", Reply::json(&fx.fleet));
    assert_eq!(client.fleet().await.unwrap(), fx.fleet);
    assert_plain(&take(&mock));

    let machine_path = format!("/api/v1/machines/{}", machine.id);
    mock.on(Method::GET, &machine_path, Reply::json(&machine));
    assert_eq!(client.machine(&machine.id).await.unwrap(), machine);
    assert_plain(&take(&mock));

    let update = UpdateMachineRequest {
        name: Some("builder-one".into()),
        policy: Some(machine.policy.clone()),
        drain: Some(true),
        retire: None,
    };
    mock.on(Method::PATCH, &machine_path, Reply::json(&machine));
    assert_eq!(
        client.update_machine(&machine.id, &update).await.unwrap(),
        machine
    );
    let request = take(&mock);
    assert_authenticated(&request);
    assert_json_body(&request, &update);

    mock.on(Method::GET, "/api/v1/queue", Reply::json(&fx.queue));
    assert_eq!(client.queue().await.unwrap(), fx.queue);
    assert_plain(&take(&mock));
}

#[tokio::test]
async fn every_route_round_trips_over_plain_http() {
    every_route_round_trips(false).await;
}

#[tokio::test]
async fn every_route_round_trips_over_pinned_tls() {
    every_route_round_trips(true).await;
}

fn without_retries(server: &Server) -> Client {
    let mut options = crate::support::fast_options();
    options.retry = RetryPolicy::disabled();
    server.client_with(options)
}

#[tokio::test]
async fn every_error_code_arrives_as_the_controllers_api_error() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = without_retries(&server);
    let codes = [
        (ErrorCode::BadRequest, 400),
        (ErrorCode::Unauthorized, 401),
        (ErrorCode::Forbidden, 403),
        (ErrorCode::PolicyDenied, 403),
        (ErrorCode::NotFound, 404),
        (ErrorCode::Conflict, 409),
        (ErrorCode::IdempotencyMismatch, 409),
        (ErrorCode::InvalidState, 409),
        (ErrorCode::StaleGeneration, 409),
        (ErrorCode::TooLarge, 413),
        (ErrorCode::SnapshotIncomplete, 422),
        (ErrorCode::Internal, 500),
        (ErrorCode::Unavailable, 503),
    ];
    for (code, status) in codes {
        assert_eq!(code.http_status(), status);
        let sent = ApiError::new(code, format!("refused: {code:?}"))
            .with_details(serde_json::json!({"hint": status}));
        mock.on(
            Method::GET,
            "/api/v1/whoami",
            Reply {
                status,
                content_type: Some("application/json"),
                body: serde_json::to_vec(&sent).unwrap(),
            },
        );
        let error = client.whoami().await.unwrap_err();
        assert_eq!(error, ClientError::Api(sent.clone()), "{code:?}");
        assert_eq!(error.code(), Some(code));
        assert_eq!(error.api(), Some(&sent));
        assert!(!error.is_pin_mismatch());
    }
}

#[tokio::test]
async fn answers_that_are_not_the_api_are_told_apart_from_api_errors() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = without_retries(&server);

    // A status without an ApiError body.
    mock.on(
        Method::GET,
        "/api/v1/whoami",
        Reply::text(500, "upstream exploded"),
    );
    assert_eq!(
        client.whoami().await.unwrap_err(),
        ClientError::Http {
            status: 500,
            message: "upstream exploded".into()
        }
    );

    // No route at all: the mock's plain 404.
    let error = client.fleet().await.unwrap_err();
    assert!(matches!(error, ClientError::Http { status: 404, .. }));

    // Success with a body that is not the promised type.
    mock.on(Method::GET, "/api/v1/whoami", Reply::text(200, "not json"));
    assert!(matches!(
        client.whoami().await.unwrap_err(),
        ClientError::Decode { .. }
    ));
    mock.on(
        Method::GET,
        "/api/v1/whoami",
        Reply::json(&serde_json::json!({"name": "only a name"})),
    );
    assert!(matches!(
        client.whoami().await.unwrap_err(),
        ClientError::Decode { .. }
    ));

    // A redirect is not followed: the token must not travel elsewhere.
    mock.on(Method::GET, "/api/v1/whoami", Reply::status(302));
    assert!(matches!(
        client.whoami().await.unwrap_err(),
        ClientError::Http { status: 302, .. }
    ));
}

#[tokio::test]
async fn a_connection_that_cannot_be_made_is_a_transport_error() {
    // Bind and release a port so that nothing is listening on it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = bella_net::ControllerUrl::parse(&format!("http://{addr}"), false).unwrap();
    let mut options = crate::support::fast_options();
    options.retry = RetryPolicy::disabled();
    let client = Client::with_options(&url, None, Some("token".into()), options).unwrap();
    let error = client.whoami().await.unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
    assert!(error.api().is_none());
}

#[tokio::test]
async fn submit_job_refuses_a_missing_idempotency_key_without_sending() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = server.client();
    let job = &fx.jobs[0];
    let submit = SubmitJobRequest {
        project: job.project_name.clone(),
        snapshot_id: job.snapshot_id.clone(),
        spec: job.spec.clone().unwrap(),
        origin_platform: platform(),
        session: None,
    };
    for key in ["", "   ", "line\nbreak"] {
        assert!(matches!(
            client.submit_job(&submit, key).await.unwrap_err(),
            ClientError::Invalid { .. }
        ));
    }
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn a_client_without_a_token_sends_no_authorization_header() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = Client::new(&server.url, None, None).unwrap();
    assert!(!client.has_token());
    mock.on(Method::GET, "/api/v1/whoami", Reply::json(&fx.whoami));
    client.whoami().await.unwrap();
    assert_eq!(mock.single().header("authorization"), None);
}
