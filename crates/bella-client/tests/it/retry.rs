//! Which requests are repeated, how often, and how long a long poll may
//! take.

use crate::support::{Mock, Reply, Server, TOKEN, fast_options, fixtures, within};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::Method;
use axum::routing::get;
use bella_client::{Client, ClientError, ClientOptions, MAX_WAIT, RetryPolicy};
use bella_core::api::{
    CreateTokenRequest, MissingObjectsRequest, MissingObjectsResponse, Role, SubmitJobRequest,
    WaitQuery, WaitResponse,
};
use bella_core::{Digest, ErrorCode};
use bella_net::ControllerUrl;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn unavailable() -> Reply {
    Reply::error(ErrorCode::Unavailable, "reconciling")
}

fn submit_request() -> SubmitJobRequest {
    let fx = fixtures();
    let job = &fx.jobs[0];
    SubmitJobRequest {
        project: job.project_name.clone(),
        snapshot_id: job.snapshot_id.clone(),
        spec: job.spec.clone().unwrap(),
        origin_platform: serde_json::from_value(serde_json::json!({
            "os": "macos", "arch": "aarch64"
        }))
        .unwrap(),
        session: job.session.clone(),
    }
}

#[tokio::test]
async fn a_read_is_repeated_after_transient_answers() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = server.client();

    // ErrorCode::Unavailable, then a 502 and a 504 from something in
    // front of the controller, then the answer.
    mock.sequence(
        Method::GET,
        "/api/v1/whoami",
        vec![
            unavailable(),
            Reply::text(502, "bad gateway"),
            Reply::status(504),
            Reply::json(&fx.whoami),
        ],
    );
    assert_eq!(client.whoami().await.unwrap(), fx.whoami);
    assert_eq!(mock.count(), 4);
}

#[tokio::test]
async fn attempts_are_bounded_and_the_last_error_is_returned() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    mock.on(Method::GET, "/api/v1/fleet", unavailable());

    let client = server.client();
    let error = client.fleet().await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert_eq!(mock.count(), 4, "the default test policy allows four tries");

    mock.clear();
    let mut options = fast_options();
    options.retry.max_attempts = 2;
    let error = server.client_with(options).fleet().await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert_eq!(mock.count(), 2);
}

#[tokio::test]
async fn total_retry_time_is_bounded() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    mock.on(Method::GET, "/api/v1/fleet", unavailable());

    let mut options = fast_options();
    options.retry = RetryPolicy {
        max_attempts: 1_000,
        initial_backoff: Duration::from_millis(40),
        max_backoff: Duration::from_millis(40),
        max_elapsed: Duration::from_millis(300),
    };
    let started = Instant::now();
    let error = server.client_with(options).fleet().await.unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    // Waits are 20 to 40 ms each, so at most about fifteen tries fit.
    assert!(mock.count() >= 2, "{} tries", mock.count());
    assert!(mock.count() < 40, "{} tries", mock.count());
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[tokio::test]
async fn retries_can_be_disabled() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    mock.on(Method::GET, "/api/v1/fleet", unavailable());
    let mut options = fast_options();
    options.retry = RetryPolicy::disabled();
    let error = server.client_with(options).fleet().await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn submit_job_repeats_with_the_same_idempotency_key_and_body() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    mock.sequence(
        Method::POST,
        "/api/v1/jobs",
        vec![unavailable(), Reply::status(503), Reply::json(&fx.jobs[0])],
    );
    let submit = submit_request();
    let key = bella_client::new_idempotency_key();
    let job = server.client().submit_job(&submit, &key).await.unwrap();
    assert_eq!(job, fx.jobs[0]);

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.header("idempotency-key"), Some(key.as_str()));
        assert_eq!(request.json::<SubmitJobRequest>(), submit);
    }
}

#[tokio::test]
async fn client_errors_are_never_retried() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = server.client();
    let submit = submit_request();

    for (code, status) in [
        (ErrorCode::BadRequest, 400),
        (ErrorCode::Unauthorized, 401),
        (ErrorCode::NotFound, 404),
        (ErrorCode::IdempotencyMismatch, 409),
        (ErrorCode::SnapshotIncomplete, 422),
        (ErrorCode::Internal, 500),
    ] {
        mock.clear();
        mock.on(Method::POST, "/api/v1/jobs", Reply::error(code, "no"));
        mock.on(Method::GET, "/api/v1/fleet", Reply::error(code, "no"));

        let error = client.submit_job(&submit, "key").await.unwrap_err();
        assert_eq!(error.code(), Some(code));
        assert_eq!(mock.count(), 1, "POST /jobs after {status}");

        mock.clear();
        let error = client.fleet().await.unwrap_err();
        assert_eq!(error.code(), Some(code));
        assert_eq!(mock.count(), 1, "GET /fleet after {status}");
    }

    // The same without an ApiError body.
    mock.clear();
    mock.on(Method::GET, "/api/v1/fleet", Reply::text(400, "bad"));
    assert!(matches!(
        client.fleet().await.unwrap_err(),
        ClientError::Http { status: 400, .. }
    ));
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn requests_that_are_not_safe_to_repeat_are_sent_once() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let client = server.client();
    let job = &fx.jobs[0];
    let machine = &fx.fleet.machines[0];

    mock.on(Method::POST, "/api/v1/tokens", unavailable());
    let error = client
        .create_token(&CreateTokenRequest {
            name: "ci".into(),
            role: Role::Viewer,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert_eq!(mock.count(), 1, "POST /tokens");

    mock.clear();
    mock.on(
        Method::PATCH,
        &format!("/api/v1/machines/{}", machine.id),
        unavailable(),
    );
    client
        .update_machine(&machine.id, &Default::default())
        .await
        .unwrap_err();
    assert_eq!(mock.count(), 1, "PATCH /machines");

    mock.clear();
    mock.on(Method::PUT, "/api/v1/projects/web", unavailable());
    client
        .put_project("web", &Default::default())
        .await
        .unwrap_err();
    assert_eq!(mock.count(), 1, "PUT /projects");

    // The idempotent POSTs are repeated.
    let environment = &fx.environments[0];
    let cancel = format!("/api/v1/jobs/{}/cancel", job.id);
    let renew = format!("/api/v1/environments/{}/renew", environment.id);
    let finish = format!("/api/v1/environments/{}/finish", environment.id);
    mock.clear();
    mock.sequence(Method::POST, &cancel, vec![unavailable(), Reply::json(job)]);
    mock.sequence(
        Method::POST,
        &renew,
        vec![unavailable(), Reply::json(environment)],
    );
    mock.sequence(
        Method::POST,
        &finish,
        vec![unavailable(), Reply::json(environment)],
    );
    assert_eq!(&client.cancel_job(&job.id).await.unwrap(), job);
    assert_eq!(
        &client.renew_environment(&environment.id).await.unwrap(),
        environment
    );
    assert_eq!(
        &client.finish_environment(&environment.id).await.unwrap(),
        environment
    );
    assert_eq!(mock.count(), 6);

    // So is the object lookup.
    mock.clear();
    let answer = MissingObjectsResponse { missing: vec![] };
    mock.sequence(
        Method::POST,
        "/api/v1/objects/missing",
        vec![unavailable(), Reply::json(&answer)],
    );
    let request = MissingObjectsRequest {
        digests: vec![Digest::of_bytes(b"x")],
    };
    assert_eq!(client.missing_objects(&request).await.unwrap(), answer);
    assert_eq!(mock.count(), 2);
}

/// Serves raw HTTP: the first `broken` connections are closed without an
/// answer, later ones get `body` as a JSON 200.
async fn flaky_server(broken: usize, body: Vec<u8>) -> (ControllerUrl, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            tokio::spawn(async move {
                // Read the request head (and whatever body came with it).
                let mut seen = Vec::new();
                let mut buffer = [0u8; 4096];
                while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => seen.extend_from_slice(&buffer[..read]),
                    }
                }
                if index < broken {
                    // Dropping the stream closes the connection unanswered.
                    return;
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let url = ControllerUrl::parse(&format!("http://{addr}"), false).unwrap();
    (url, connections)
}

#[tokio::test]
async fn a_read_is_repeated_after_the_connection_breaks() {
    let fx = fixtures();
    let (url, connections) = flaky_server(2, serde_json::to_vec(&fx.whoami).unwrap()).await;
    let client = Client::with_options(&url, None, Some(TOKEN.into()), fast_options()).unwrap();
    assert_eq!(within(client.whoami()).await.unwrap(), fx.whoami);
    assert_eq!(connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_broken_connection_does_not_repeat_an_unsafe_request() {
    let fx = fixtures();
    let (url, connections) = flaky_server(2, serde_json::to_vec(&fx.token).unwrap()).await;
    let client = Client::with_options(&url, None, Some(TOKEN.into()), fast_options()).unwrap();
    let error = within(client.create_token(&CreateTokenRequest {
        name: "ci".into(),
        role: Role::Viewer,
    }))
    .await
    .unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[derive(Clone)]
struct SlowWait {
    hold: Duration,
    answer: Arc<WaitResponse>,
    queries: Arc<std::sync::Mutex<Vec<WaitQuery>>>,
}

async fn slow_wait(
    State(state): State<SlowWait>,
    Query(query): Query<WaitQuery>,
) -> axum::Json<WaitResponse> {
    state.queries.lock().unwrap().push(query);
    tokio::time::sleep(state.hold).await;
    axum::Json((*state.answer).clone())
}

async fn slow_fleet(State(state): State<SlowWait>) -> &'static str {
    tokio::time::sleep(state.hold).await;
    "{}"
}

#[tokio::test]
async fn the_long_poll_outlasts_the_request_timeout_and_is_clamped() {
    let fx = fixtures();
    let job = fx.jobs[0].clone();
    let state = SlowWait {
        hold: Duration::from_millis(900),
        answer: Arc::new(WaitResponse {
            job: job.clone(),
            timed_out: true,
        }),
        queries: Arc::default(),
    };
    let router = Router::new()
        .route(&format!("/api/v1/jobs/{}/wait", job.id), get(slow_wait))
        .route("/api/v1/fleet", get(slow_fleet))
        .with_state(state.clone());
    let server = Server::http(router).await;

    // A request timeout far shorter than the server holds the poll.
    let options = ClientOptions {
        request_timeout: Duration::from_millis(300),
        retry: RetryPolicy::disabled(),
        ..fast_options()
    };
    let client = server.client_with(options);

    // An ordinary request to an equally slow route does time out.
    assert_eq!(client.fleet().await.unwrap_err(), ClientError::Timeout);

    // The long poll gets its wait on top of the request timeout.
    let wait = WaitQuery {
        timeout_ms: Some(2_000),
        after_event: Some(5),
    };
    let answer = within(client.wait_job(&job.id, &wait)).await.unwrap();
    assert!(answer.timed_out);
    assert_eq!(answer.job, job);

    // More than the controller's limit is clamped to it.
    let too_long = WaitQuery {
        timeout_ms: Some(10 * 60 * 1000),
        after_event: None,
    };
    within(client.wait_job(&job.id, &too_long)).await.unwrap();

    // No timeout asked for: none is sent, and the controller's own limit
    // is what the HTTP timeout allows for.
    within(client.wait_job(&job.id, &WaitQuery::default()))
        .await
        .unwrap();

    let queries = state.queries.lock().unwrap().clone();
    assert_eq!(
        queries,
        vec![
            wait,
            WaitQuery {
                timeout_ms: Some(MAX_WAIT.as_millis() as u64),
                after_event: None
            },
            WaitQuery::default(),
        ]
    );
    assert_eq!(MAX_WAIT, Duration::from_secs(60));
}

#[tokio::test]
async fn a_long_poll_that_exceeds_wait_plus_margin_times_out() {
    let fx = fixtures();
    let job = fx.jobs[0].clone();
    let state = SlowWait {
        hold: Duration::from_secs(30),
        answer: Arc::new(WaitResponse {
            job: job.clone(),
            timed_out: true,
        }),
        queries: Arc::default(),
    };
    let router = Router::new()
        .route(&format!("/api/v1/jobs/{}/wait", job.id), get(slow_wait))
        .with_state(state);
    let server = Server::http(router).await;
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        retry: RetryPolicy::disabled(),
        ..fast_options()
    };
    let wait = WaitQuery {
        timeout_ms: Some(100),
        after_event: None,
    };
    let started = Instant::now();
    let error = within(server.client_with(options).wait_job(&job.id, &wait))
        .await
        .unwrap_err();
    assert_eq!(error, ClientError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_repeated_long_poll_only_asks_for_the_time_that_is_left() {
    let fx = fixtures();
    let job = fx.jobs[0].clone();
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;
    let path = format!("/api/v1/jobs/{}/wait", job.id);
    let answer = WaitResponse {
        job: job.clone(),
        timed_out: false,
    };
    mock.sequence(
        Method::GET,
        &path,
        vec![unavailable(), unavailable(), Reply::json(&answer)],
    );
    let wait = WaitQuery {
        timeout_ms: Some(30_000),
        after_event: Some(9),
    };
    assert_eq!(
        server.client().wait_job(&job.id, &wait).await.unwrap(),
        answer
    );

    let timeouts: Vec<u64> = mock
        .requests()
        .iter()
        .map(|request| {
            let pairs = request.query_pairs();
            assert!(pairs.contains(&("after_event".into(), "9".into())));
            pairs
                .iter()
                .find(|(name, _)| name == "timeout_ms")
                .map(|(_, value)| value.parse().unwrap())
                .unwrap()
        })
        .collect();
    assert_eq!(timeouts.len(), 3);
    assert_eq!(timeouts[0], 30_000);
    assert!(
        timeouts[1] <= 30_000 && timeouts[1] > 20_000,
        "{timeouts:?}"
    );
    assert!(timeouts[2] <= timeouts[1], "{timeouts:?}");
}
