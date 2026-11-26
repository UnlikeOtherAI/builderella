//! The blocking client, used the way a synchronous program uses it: from
//! threads that do not belong to a runtime.

use crate::support::{Mock, Reply, Server, TOKEN, fast_options, fixtures};
use axum::http::Method;
use bella_client::blocking::BlockingClient;
use bella_client::{ClientError, RetryPolicy};
use bella_core::api::{EventRecord, ListJobsQuery, SubmitJobRequest};
use bella_core::store::{ObjectStore, get_bytes, put_bytes};
use bella_core::{Digest, ErrorCode};
use tokio::runtime::Runtime;

/// The runtime the test server runs on. The code under test brings its
/// own.
fn server_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn blocking(server: &Server) -> BlockingClient {
    BlockingClient::with_options(
        &server.url,
        server.fingerprint.as_ref(),
        Some(TOKEN.to_owned()),
        fast_options(),
    )
    .unwrap()
}

fn calls_work(tls: bool) {
    let fx = fixtures();
    let runtime = server_runtime();
    let mock = Mock::new();
    let server = runtime.block_on(Server::start(tls, mock.router()));
    let client = blocking(&server);
    let job = &fx.jobs[0];

    mock.on(Method::GET, "/api/v1/healthz", Reply::text(200, "ok"));
    client.health().unwrap();

    mock.on(Method::GET, "/api/v1/whoami", Reply::json(&fx.whoami));
    assert_eq!(client.whoami().unwrap(), fx.whoami);

    mock.on(Method::GET, "/api/v1/jobs", Reply::json(&fx.jobs));
    mock.clear();
    let query = ListJobsQuery {
        active: Some(true),
        ..ListJobsQuery::default()
    };
    assert_eq!(client.jobs(&query).unwrap(), fx.jobs);
    assert_eq!(mock.single().query.as_deref(), Some("active=true"));

    // Retries and the idempotency key work as in the async client.
    mock.clear();
    mock.sequence(
        Method::POST,
        "/api/v1/jobs",
        vec![
            Reply::error(ErrorCode::Unavailable, "busy"),
            Reply::json(job),
        ],
    );
    let submit = SubmitJobRequest {
        project: job.project_name.clone(),
        snapshot_id: job.snapshot_id.clone(),
        spec: job.spec.clone().unwrap(),
        origin_platform: serde_json::from_value(serde_json::json!({
            "os": "windows", "arch": "x86_64"
        }))
        .unwrap(),
        session: None,
    };
    assert_eq!(&client.submit_job(&submit, "blocking-key").unwrap(), job);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.header("idempotency-key") == Some("blocking-key"))
    );

    // Errors are the same type.
    mock.on(
        Method::GET,
        &format!("/api/v1/jobs/{}", job.id),
        Reply::error(ErrorCode::NotFound, "job not found"),
    );
    assert_eq!(
        client.job(&job.id).unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );

    // Clones share the runtime; calls from several threads at once work.
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let client = client.clone();
            std::thread::spawn(move || client.whoami().unwrap())
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), fx.whoami);
    }
    drop(server);
}

#[test]
fn blocking_calls_work_over_plain_http() {
    calls_work(false);
}

#[test]
fn blocking_calls_work_over_pinned_tls() {
    calls_work(true);
}

#[test]
fn blocking_events_iterate_the_event_stream() {
    let runtime = server_runtime();
    let mock = Mock::new();
    let server = runtime.block_on(Server::http(mock.router()));

    let records: Vec<EventRecord> = fixtures().events.into_iter().take(3).collect();
    let mut body = String::from(": hello\n\n");
    for record in &records {
        body.push_str(&format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            record.seq,
            record.kind,
            serde_json::to_string(record).unwrap()
        ));
    }
    mock.on(
        Method::GET,
        "/api/v1/events",
        Reply {
            status: 200,
            content_type: Some("text/event-stream"),
            body: body.into_bytes(),
        },
    );

    let after = records[0].seq - 1;
    let mut events = blocking(&server).events(Some(after));
    let received: Vec<EventRecord> = (&mut events).take(3).map(Result::unwrap).collect();
    assert_eq!(received, records);
    assert_eq!(
        mock.requests()[0].query.as_deref(),
        Some(format!("after={after}").as_str())
    );
}

#[test]
fn the_blocking_object_store_outlives_its_client() {
    let runtime = server_runtime();
    let server = runtime.block_on(Server::http(crate::objects::object_router()));
    let client = blocking(&server);
    let store = client.object_store();
    // The store keeps the client's runtime alive by itself.
    drop(client);

    let content = b"stored through the blocking client".to_vec();
    let digest = put_bytes(&store, &content).unwrap();
    assert_eq!(digest, Digest::of_bytes(&content));
    assert!(store.contains(&digest).unwrap());
    assert_eq!(store.size(&digest).unwrap(), Some(content.len() as u64));
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin");
    assert_eq!(
        store.fetch_raw_to_file(&digest, &path).unwrap(),
        content.len() as u64
    );
    assert_eq!(std::fs::read(&path).unwrap(), content);
}

#[tokio::test]
async fn blocking_calls_from_async_code_are_refused_not_panics() {
    let mock = Mock::new();
    let server = Server::http(mock.router()).await;

    // Creating one inside a runtime is allowed.
    let client = blocking(&server);
    let error = client.whoami().unwrap_err();
    let ClientError::Invalid { message } = &error else {
        panic!("expected a refusal, got {error:?}");
    };
    assert!(message.contains("Tokio runtime"), "{message}");

    let mut events = client.events(None);
    assert!(matches!(
        events.next(),
        Some(Err(ClientError::Invalid { .. }))
    ));

    let inner = client.clone();
    let error = tokio::task::spawn_blocking(move || inner.fleet().unwrap_err())
        .await
        .unwrap();
    assert!(matches!(error, ClientError::Invalid { .. }));
    assert_eq!(mock.count(), 0, "nothing was sent");

    // The async client underneath is usable right here.
    mock.on(
        Method::GET,
        "/api/v1/whoami",
        Reply::json(&fixtures().whoami),
    );
    assert_eq!(client.client().whoami().await.unwrap(), fixtures().whoami);

    // Dropping the client, its events and a store inside async code must
    // not panic either.
    let store = client.object_store();
    drop(events);
    drop(client);
    drop(store);
}

#[test]
fn a_blocking_client_reports_an_unreachable_controller() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = bella_net::ControllerUrl::parse(&format!("http://{addr}"), false).unwrap();
    let mut options = fast_options();
    options.retry = RetryPolicy::disabled();
    let client = BlockingClient::with_options(&url, None, Some(TOKEN.to_owned()), options).unwrap();
    let error = client.whoami().unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
}
