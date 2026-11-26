//! The event stream: incremental parsing, keep-alives, reconnecting from
//! the last sequence.

use crate::support::{Server, TOKEN, fast_options, fixtures, within};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use bella_client::{ClientError, ClientOptions};
use bella_core::api::EventRecord;
use bella_core::{ApiError, ErrorCode};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

type Chunk = Result<Bytes, io::Error>;

/// What the server does with one connection to `/events`.
enum Script {
    /// Stream whatever is sent on the channel; closing it ends the body.
    Stream(mpsc::UnboundedReceiver<Chunk>),
    /// Answer with a status and a body instead of a stream.
    Refuse(StatusCode, &'static str, Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
struct Connect {
    query: Option<String>,
    accept: Option<String>,
    authorization: Option<String>,
}

#[derive(Default)]
struct EventsState {
    scripts: VecDeque<Script>,
    connects: Vec<Connect>,
}

#[derive(Clone, Default)]
struct Events {
    state: Arc<Mutex<EventsState>>,
}

impl Events {
    /// Queues a streamed connection and returns its feed.
    fn stream(&self) -> mpsc::UnboundedSender<Chunk> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.state
            .lock()
            .unwrap()
            .scripts
            .push_back(Script::Stream(rx));
        tx
    }

    fn refuse(&self, status: StatusCode, content_type: &'static str, body: Vec<u8>) {
        self.state
            .lock()
            .unwrap()
            .scripts
            .push_back(Script::Refuse(status, content_type, body));
    }

    fn connects(&self) -> Vec<Connect> {
        self.state.lock().unwrap().connects.clone()
    }

    fn queries(&self) -> Vec<Option<String>> {
        self.connects().into_iter().map(|c| c.query).collect()
    }

    async fn serve(&self) -> Server {
        self.serve_with(false).await
    }

    async fn serve_with(&self, tls: bool) -> Server {
        let router = Router::new()
            .route("/api/v1/events", get(events))
            .with_state(self.clone());
        Server::start(tls, router).await
    }
}

async fn events(State(events): State<Events>, uri: Uri, headers: HeaderMap) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let script = {
        let mut state = events.state.lock().unwrap();
        state.connects.push(Connect {
            query: uri.query().map(str::to_owned),
            accept: header("accept"),
            authorization: header("authorization"),
        });
        state.scripts.pop_front()
    };
    match script {
        Some(Script::Stream(rx)) => {
            let body = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-store")
                .body(Body::from_stream(body))
                .unwrap()
        }
        Some(Script::Refuse(status, content_type, body)) => Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap(),
        // Nothing scripted: hold the connection open and silent.
        None => {
            let (tx, rx) = mpsc::unbounded_channel::<Chunk>();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(120)).await;
                drop(tx);
            });
            let body = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body))
                .unwrap()
        }
    }
}

fn record(seq: u64) -> EventRecord {
    let mut record = fixtures().events[0].clone();
    record.seq = seq;
    record.kind = format!("job.step_{seq}");
    record.data = serde_json::json!({"n": seq, "text": "zażółć\nline"});
    record
}

fn frame(record: &EventRecord) -> String {
    format!(
        "id: {}\nevent: {}\ndata: {}\n\n",
        record.seq,
        record.kind,
        serde_json::to_string(record).unwrap()
    )
}

fn send(feed: &mpsc::UnboundedSender<Chunk>, text: &str) {
    feed.send(Ok(Bytes::copy_from_slice(text.as_bytes())))
        .unwrap();
}

#[tokio::test]
async fn events_split_across_chunks_and_keep_alives_are_handled() {
    let events = Events::default();
    let feed = events.stream();
    let server = events.serve().await;
    let mut stream = server.client().events(None);

    // One event delivered a few bytes at a time, with keep-alive comments
    // around it and a pause in the middle of a line.
    let first = frame(&record(1));
    send(&feed, ": keep-alive\n\n");
    let bytes = first.as_bytes();
    for piece in bytes.chunks(7) {
        feed.send(Ok(Bytes::copy_from_slice(piece))).unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(1));

    // Two events and a keep-alive in a single chunk.
    send(
        &feed,
        &format!("{}: keep-alive\n\n{}", frame(&record(2)), frame(&record(3))),
    );
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(2));
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(3));

    // CRLF line endings and a payload split over several data lines.
    let fourth = record(4);
    let json = serde_json::to_string_pretty(&fourth).unwrap();
    let mut text = String::from("id: 4\r\nevent: job.step_4\r\n");
    for line in json.lines() {
        text.push_str("data: ");
        text.push_str(line);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    send(&feed, &text);
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), fourth);

    let connects = events.connects();
    assert_eq!(connects.len(), 1);
    assert_eq!(connects[0].query, None, "no `after` was asked for");
    assert_eq!(connects[0].accept.as_deref(), Some("text/event-stream"));
    assert_eq!(connects[0].authorization, Some(format!("Bearer {TOKEN}")));
}

#[tokio::test]
async fn the_stream_reconnects_from_the_last_sequence_after_the_server_drops_it() {
    let events = Events::default();
    let first = events.stream();
    let second = events.stream();
    let server = events.serve().await;
    let mut stream = server.client().events(Some(40));

    send(&first, &frame(&record(41)));
    send(&first, &frame(&record(42)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 41);
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 42);

    // The server ends the response; nothing is yielded for that.
    drop(first);

    // The replay overlaps what was already delivered: 42 again, then 43.
    send(&second, &frame(&record(42)));
    send(&second, &frame(&record(43)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(43));

    assert_eq!(
        events.queries(),
        vec![Some("after=40".to_owned()), Some("after=42".to_owned())]
    );
}

#[tokio::test]
async fn the_stream_resumes_over_pinned_tls() {
    let events = Events::default();
    let first = events.stream();
    let second = events.stream();
    let server = events.serve_with(true).await;
    let mut stream = server.client().events(None);

    send(&first, &frame(&record(5)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(5));
    drop(first);
    send(&second, &frame(&record(6)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(6));
    assert_eq!(events.queries(), vec![None, Some("after=5".to_owned())]);
}

#[tokio::test]
async fn a_broken_connection_is_reported_and_then_resumed() {
    let events = Events::default();
    let first = events.stream();
    let second = events.stream();
    let server = events.serve().await;
    let mut stream = server.client().events(None);

    send(&first, &frame(&record(7)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 7);

    // Half an event, then the connection is torn down mid-body.
    send(&first, "id: 8\nevent: job.step_8\ndata: {\"seq\":");
    first
        .send(Err(io::Error::other("connection reset")))
        .unwrap();
    let error = within(stream.next()).await.unwrap().unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");

    // The half event is not delivered; the complete one arrives on the
    // new connection.
    send(&second, &frame(&record(8)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), record(8));
    assert_eq!(events.queries(), vec![None, Some("after=7".to_owned())]);
}

#[tokio::test]
async fn refused_connections_are_error_items_and_the_stream_keeps_trying() {
    let events = Events::default();
    let unavailable = ApiError::new(ErrorCode::Unavailable, "reconciling");
    let unauthorized = ApiError::new(ErrorCode::Unauthorized, "token revoked");
    events.refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "application/json",
        serde_json::to_vec(&unavailable).unwrap(),
    );
    events.refuse(
        StatusCode::UNAUTHORIZED,
        "application/json",
        serde_json::to_vec(&unauthorized).unwrap(),
    );
    events.refuse(StatusCode::OK, "application/json", b"{}".to_vec());
    let feed = events.stream();
    let server = events.serve().await;
    let mut stream = server.client().events(Some(3));

    assert_eq!(
        within(stream.next()).await.unwrap().unwrap_err(),
        ClientError::Api(unavailable)
    );
    assert_eq!(
        within(stream.next()).await.unwrap().unwrap_err(),
        ClientError::Api(unauthorized)
    );
    // A 200 that is not an event stream.
    let error = within(stream.next()).await.unwrap().unwrap_err();
    assert!(matches!(error, ClientError::Decode { .. }), "{error:?}");

    send(&feed, &frame(&record(4)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 4);
    // Every attempt asked for the same position: nothing was skipped.
    assert_eq!(events.queries(), vec![Some("after=3".to_owned()); 4]);
}

#[tokio::test]
async fn a_malformed_event_is_an_error_item_and_does_not_move_the_position() {
    let events = Events::default();
    let first = events.stream();
    let second = events.stream();
    let server = events.serve().await;
    let mut stream = server.client().events(None);

    send(&first, &frame(&record(1)));
    send(&first, "id: 2\nevent: job.broken\ndata: {not json}\n\n");
    send(&first, &frame(&record(3)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 1);
    let error = within(stream.next()).await.unwrap().unwrap_err();
    assert!(matches!(error, ClientError::Decode { .. }), "{error:?}");
    assert!(error.to_string().contains("job.broken"), "{error}");
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 3);

    drop(first);
    send(&second, &frame(&record(4)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 4);
    assert_eq!(events.queries(), vec![None, Some("after=3".to_owned())]);
}

#[tokio::test]
async fn a_silent_connection_times_out_and_is_replaced() {
    let events = Events::default();
    let silent = events.stream();
    let second = events.stream();
    let server = events.serve().await;
    let options = ClientOptions {
        stream_idle_timeout: Some(Duration::from_millis(150)),
        ..fast_options()
    };
    let mut stream = server.client_with(options).events(Some(10));

    // Nothing at all arrives on the first connection, not even a
    // keep-alive.
    assert_eq!(
        within(stream.next()).await.unwrap().unwrap_err(),
        ClientError::Timeout
    );
    send(&second, &frame(&record(11)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 11);
    assert_eq!(events.queries(), vec![Some("after=10".to_owned()); 2]);
    drop(silent);
}

#[tokio::test]
async fn keep_alives_hold_a_quiet_connection_open() {
    let events = Events::default();
    let feed = events.stream();
    let server = events.serve().await;
    let options = ClientOptions {
        stream_idle_timeout: Some(Duration::from_millis(300)),
        ..fast_options()
    };
    let mut stream = server.client_with(options).events(None);

    let keep_alive = feed.clone();
    let ticker = tokio::spawn(async move {
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(75)).await;
            if keep_alive
                .send(Ok(Bytes::from_static(b": ping\n\n")))
                .is_err()
            {
                return;
            }
        }
        let _ = keep_alive.send(Ok(Bytes::copy_from_slice(frame(&record(1)).as_bytes())));
    });
    // About 900 ms of nothing but comments, three idle timeouts long.
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 1);
    ticker.await.unwrap();
    assert_eq!(
        events.connects().len(),
        1,
        "the connection was never replaced"
    );
}

#[tokio::test]
async fn the_stream_does_not_end_while_the_server_is_away() {
    let events = Events::default();
    let first = events.stream();
    let server = events.serve().await;
    let client = server.client();
    let mut stream = client.events(None);
    send(&first, &frame(&record(1)));
    assert_eq!(within(stream.next()).await.unwrap().unwrap().seq, 1);

    // Take the whole server away.
    drop(first);
    drop(server);
    for _ in 0..3 {
        let item = within(stream.next()).await;
        let error = item.expect("the stream never ends").unwrap_err();
        assert!(error.is_transient(), "{error:?}");
    }
}
