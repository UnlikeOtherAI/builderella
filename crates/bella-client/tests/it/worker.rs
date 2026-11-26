//! The worker socket against a WebSocket endpoint with scripted behaviour.

use crate::support::{Server, TOKEN, fixtures, within};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bella_client::{ClientError, WorkerSink, WorkerSocket, WorkerSocketOptions, WorkerStream};
use bella_core::protocol::{
    AttemptRef, CommandEnvelope, ControllerCommand, ControllerMsg, LogChunk, LogStream,
    PROTOCOL_VERSION, WORKER_WS_PATH, WorkerMsg,
};
use bella_core::{ApiError, AttemptId, BootId, CommandId, ErrorCode, UnixMillis};
use bella_net::Fingerprint;
use futures_util::future::BoxFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

type Behaviour = Arc<dyn Fn(WebSocket) -> BoxFuture<'static, ()> + Send + Sync>;

/// One upgrade request as the server saw it.
#[derive(Clone, Debug)]
struct Upgrade {
    path: String,
    authorization: Option<String>,
    user_agent: Option<String>,
}

#[derive(Clone)]
struct Hub {
    behaviour: Behaviour,
    upgrades: Arc<Mutex<Vec<Upgrade>>>,
}

async fn upgrade(
    State(hub): State<Hub>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let seen = Upgrade {
        path: uri.path().to_owned(),
        authorization: header("authorization"),
        user_agent: header("user-agent"),
    };
    let authorized = seen.authorization.as_deref() == Some(format!("Bearer {TOKEN}").as_str());
    hub.upgrades.lock().unwrap().push(seen);
    if !authorized {
        let error = ApiError::new(ErrorCode::Unauthorized, "unknown machine token");
        return (StatusCode::UNAUTHORIZED, axum::Json(error)).into_response();
    }
    let behaviour = hub.behaviour.clone();
    upgrade.on_upgrade(move |socket| behaviour(socket))
}

struct Harness {
    server: Server,
    upgrades: Arc<Mutex<Vec<Upgrade>>>,
}

impl Harness {
    async fn start<F, Fut>(tls: bool, behaviour: F) -> Harness
    where
        F: Fn(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let upgrades = Arc::new(Mutex::new(Vec::new()));
        let hub = Hub {
            behaviour: Arc::new(move |socket| -> BoxFuture<'static, ()> {
                Box::pin(behaviour(socket))
            }),
            upgrades: upgrades.clone(),
        };
        let router = Router::new()
            .route(WORKER_WS_PATH, get(upgrade))
            .with_state(hub);
        Harness {
            server: Server::start(tls, router).await,
            upgrades,
        }
    }

    async fn connect(&self) -> (WorkerSink, WorkerStream) {
        self.connect_with(WorkerSocketOptions::default()).await
    }

    async fn connect_with(&self, options: WorkerSocketOptions) -> (WorkerSink, WorkerStream) {
        within(WorkerSocket::connect_with(
            &self.server.url,
            self.server.fingerprint.as_ref(),
            TOKEN,
            options,
        ))
        .await
        .unwrap()
    }
}

fn hello() -> WorkerMsg {
    let machine = fixtures().fleet.machines[0].clone();
    WorkerMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        machine_id: machine.id,
        boot_id: BootId::derive(b"this boot"),
        connection_generation: 3,
        known_epoch: 4,
        inventory: Box::new(machine.inventory),
        policy_revision: machine.policy.revision,
        local_control: machine.local_control,
        attempts: Vec::new(),
    }
}

fn welcome() -> ControllerMsg {
    let fx = fixtures();
    ControllerMsg::Welcome {
        protocol_version: PROTOCOL_VERSION,
        controller_epoch: 5,
        fleet_id: fx.fleet.fleet_id,
        policy: fx.fleet.machines[0].policy.clone(),
        server_time: UnixMillis(1_795_000_000_000),
        heartbeat_interval_ms: 5_000,
    }
}

fn reconcile() -> ControllerMsg {
    ControllerMsg::Command {
        envelope: CommandEnvelope {
            command_id: CommandId::derive(b"reconcile"),
            controller_epoch: 5,
            sent_at: UnixMillis(1_795_000_000_100),
            command: ControllerCommand::Reconcile {
                verdicts: Vec::new(),
            },
        },
    }
}

fn text(message: &impl serde::Serialize) -> Message {
    Message::Text(serde_json::to_string(message).unwrap().into())
}

/// Keeps the server side reading until the peer goes away.
async fn drain(mut socket: WebSocket) {
    while let Some(Ok(_)) = socket.recv().await {}
}

async fn hello_and_welcome(tls: bool) {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<WorkerMsg>();
    let harness = Harness::start(tls, move |mut socket| {
        let seen = seen_tx.clone();
        async move {
            // The first frame from the worker is Hello, as JSON text.
            let Some(Ok(Message::Text(first))) = socket.recv().await else {
                panic!("expected a text frame first");
            };
            seen.send(serde_json::from_str(&first).unwrap()).unwrap();
            socket.send(text(&welcome())).await.unwrap();
            socket.send(text(&reconcile())).await.unwrap();
            drain(socket).await;
        }
    })
    .await;

    let (sink, mut stream) = harness.connect().await;
    within(sink.send(&hello())).await.unwrap();
    assert_eq!(within(seen_rx.recv()).await.unwrap(), hello());
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), welcome());
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), reconcile());

    let upgrades = harness.upgrades.lock().unwrap().clone();
    assert_eq!(upgrades.len(), 1);
    assert_eq!(upgrades[0].path, "/api/v1/worker/ws");
    assert_eq!(
        upgrades[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(
        upgrades[0].user_agent.as_deref(),
        Some(format!("bella/{}", env!("CARGO_PKG_VERSION")).as_str())
    );
}

#[tokio::test]
async fn hello_is_answered_with_welcome_over_plain_ws() {
    hello_and_welcome(false).await;
}

#[tokio::test]
async fn hello_is_answered_with_welcome_over_pinned_tls() {
    hello_and_welcome(true).await;
}

#[tokio::test]
async fn a_refused_upgrade_is_the_controllers_api_error() {
    let harness = Harness::start(false, drain).await;
    let error = within(WorkerSocket::connect(
        &harness.server.url,
        None,
        "bella_mch_not_the_token",
    ))
    .await
    .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unauthorized), "{error:?}");
    // The token was presented on the upgrade request.
    assert_eq!(
        harness.upgrades.lock().unwrap()[0].authorization.as_deref(),
        Some("Bearer bella_mch_not_the_token")
    );
}

#[tokio::test]
async fn a_changed_certificate_is_a_pin_mismatch_before_the_upgrade() {
    let harness = Harness::start(true, drain).await;
    let wrong = Fingerprint::of_certificate(b"another certificate");
    let error = within(WorkerSocket::connect(
        &harness.server.url,
        Some(&wrong),
        TOKEN,
    ))
    .await
    .unwrap_err();
    assert!(error.is_pin_mismatch(), "{error:?}");
    assert!(
        harness.upgrades.lock().unwrap().is_empty(),
        "no upgrade request, so no token, reached the server"
    );
}

#[tokio::test]
async fn nothing_listening_is_a_transport_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = bella_net::ControllerUrl::parse(&format!("http://{addr}"), false).unwrap();
    let error = within(WorkerSocket::connect(&url, None, TOKEN))
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
}

#[tokio::test]
async fn pings_from_the_controller_are_answered_without_the_caller_reading() {
    let (answered_tx, mut answered_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let harness = Harness::start(false, move |mut socket| {
        let answered = answered_tx.clone();
        async move {
            socket
                .send(Message::Ping(b"are you there".as_slice().into()))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.recv().await {
                if let Message::Pong(payload) = message {
                    let _ = answered.send(payload.to_vec());
                }
            }
        }
    })
    .await;

    // Neither half is polled: the connection answers by itself.
    let (_sink, _stream) = harness.connect().await;
    assert_eq!(
        within(answered_rx.recv()).await.unwrap(),
        b"are you there".to_vec()
    );
}

#[tokio::test]
async fn the_worker_pings_on_its_interval_and_stays_connected_while_answered() {
    let (pings_tx, mut pings_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let harness = Harness::start(false, move |mut socket| {
        let pings = pings_tx.clone();
        async move {
            // Reading is what makes the server side answer with pongs.
            while let Some(Ok(message)) = socket.recv().await {
                if let Message::Ping(payload) = message {
                    let _ = pings.send(payload.to_vec());
                }
            }
        }
    })
    .await;

    let options = WorkerSocketOptions {
        ping_interval: Duration::from_millis(40),
        pong_timeout: Duration::from_millis(400),
        ..WorkerSocketOptions::default()
    };
    let (sink, mut stream) = harness.connect_with(options).await;
    let mut payloads = Vec::new();
    for _ in 0..5 {
        payloads.push(within(pings_rx.recv()).await.unwrap());
    }
    // Each ping is distinguishable from the one before.
    payloads.dedup();
    assert_eq!(payloads.len(), 5);

    // Several pong timeouts have passed and the connection is still up.
    assert!(!sink.is_closed());
    let quiet = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    assert!(quiet.is_err(), "no item was expected: {quiet:?}");
}

#[tokio::test]
async fn a_missing_pong_ends_the_connection() {
    let harness = Harness::start(false, |socket| async move {
        // Hold the connection without ever reading: no pong is sent.
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(socket);
    })
    .await;

    let options = WorkerSocketOptions {
        ping_interval: Duration::from_millis(30),
        pong_timeout: Duration::from_millis(200),
        ..WorkerSocketOptions::default()
    };
    let (sink, mut stream) = harness.connect_with(options).await;
    let error = within(stream.next()).await.unwrap().unwrap_err();
    assert!(matches!(&error, ClientError::Transport { .. }), "{error:?}");
    assert!(error.to_string().contains("ping"), "{error}");
    assert!(within(stream.next()).await.is_none());

    let error = within(sink.send(&WorkerMsg::Pong { nonce: 1 }))
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
    assert!(sink.is_closed());
}

#[tokio::test]
async fn malformed_frames_are_error_items_and_the_connection_goes_on() {
    let harness = Harness::start(false, |mut socket| async move {
        socket.send(Message::Text("not json".into())).await.unwrap();
        socket
            .send(Message::Text(r#"{"type":"no_such_message"}"#.into()))
            .await
            .unwrap();
        socket
            .send(Message::Binary(vec![0u8, 159, 146, 150].into()))
            .await
            .unwrap();
        socket.send(text(&welcome())).await.unwrap();
        drain(socket).await;
    })
    .await;

    let (_sink, mut stream) = harness.connect().await;
    for _ in 0..3 {
        let error = within(stream.next()).await.unwrap().unwrap_err();
        assert!(matches!(error, ClientError::Decode { .. }), "{error:?}");
    }
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), welcome());
}

#[tokio::test]
async fn an_oversize_frame_is_an_error_item_and_ends_the_connection() {
    let harness = Harness::start(false, |mut socket| async move {
        socket.send(text(&welcome())).await.unwrap();
        let huge = format!(r#"{{"type":"close","reason":"{}"}}"#, "x".repeat(64 * 1024));
        let _ = socket.send(Message::Text(huge.into())).await;
        drain(socket).await;
    })
    .await;

    let options = WorkerSocketOptions {
        max_message_bytes: 8 * 1024,
        ..WorkerSocketOptions::default()
    };
    let (sink, mut stream) = harness.connect_with(options).await;
    // A frame within the limit still arrives.
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), welcome());
    let error = within(stream.next()).await.unwrap().unwrap_err();
    assert!(matches!(&error, ClientError::Decode { .. }), "{error:?}");
    assert!(error.to_string().contains("8192"), "{error}");
    assert!(within(stream.next()).await.is_none());
    assert!(
        within(sink.send(&WorkerMsg::Pong { nonce: 1 }))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_outgoing_message_over_the_limit_is_refused_and_nothing_breaks() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<WorkerMsg>();
    let harness = Harness::start(false, move |mut socket| {
        let seen = seen_tx.clone();
        async move {
            while let Some(Ok(message)) = socket.recv().await {
                if let Message::Text(body) = message {
                    seen.send(serde_json::from_str(&body).unwrap()).unwrap();
                }
            }
        }
    })
    .await;

    let options = WorkerSocketOptions {
        max_message_bytes: 4 * 1024,
        ..WorkerSocketOptions::default()
    };
    let (sink, _stream) = harness.connect_with(options).await;
    let too_big = WorkerMsg::Log {
        chunk: LogChunk {
            attempt: AttemptRef {
                attempt_id: AttemptId::derive(b"attempt"),
                generation: 1,
            },
            step: "build".into(),
            stream: LogStream::Stdout,
            offset: 0,
            data_b64: "QUJD".repeat(2000),
            truncated: false,
        },
    };
    let error = within(sink.send(&too_big)).await.unwrap_err();
    assert!(matches!(error, ClientError::Invalid { .. }), "{error:?}");

    within(sink.send(&WorkerMsg::Pong { nonce: 9 }))
        .await
        .unwrap();
    assert_eq!(
        within(seen_rx.recv()).await.unwrap(),
        WorkerMsg::Pong { nonce: 9 }
    );
}

#[tokio::test]
async fn messages_keep_their_order_in_both_directions_under_backpressure() {
    const COUNT: u64 = 300;
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<u64>();
    let harness = Harness::start(false, move |mut socket| {
        let seen = seen_tx.clone();
        async move {
            for nonce in 0..COUNT {
                socket
                    .send(text(&ControllerMsg::Ping { nonce }))
                    .await
                    .unwrap();
            }
            while let Some(Ok(message)) = socket.recv().await {
                if let Message::Text(body) = message
                    && let Ok(WorkerMsg::Pong { nonce }) = serde_json::from_str(&body)
                {
                    let _ = seen.send(nonce);
                }
            }
        }
    })
    .await;

    // A reader buffer far smaller than the burst.
    let options = WorkerSocketOptions {
        inbound_buffer: 2,
        ..WorkerSocketOptions::default()
    };
    let (sink, mut stream) = harness.connect_with(options).await;

    let writer = tokio::spawn(async move {
        for nonce in 0..COUNT {
            sink.send(&WorkerMsg::Pong { nonce }).await.unwrap();
        }
        sink
    });
    for expected in 0..COUNT {
        // A slow reader now and then.
        if expected % 50 == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            within(stream.next()).await.unwrap().unwrap(),
            ControllerMsg::Ping { nonce: expected }
        );
    }
    let _sink = within(writer).await.unwrap();
    for expected in 0..COUNT {
        assert_eq!(within(seen_rx.recv()).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn a_connection_the_controller_closes_ends_the_stream() {
    let harness = Harness::start(false, |mut socket| async move {
        socket
            .send(text(&ControllerMsg::Close {
                reason: "restarting".into(),
            }))
            .await
            .unwrap();
        // Dropping the socket closes the connection.
    })
    .await;

    let (sink, mut stream) = harness.connect().await;
    assert_eq!(
        within(stream.next()).await.unwrap().unwrap(),
        ControllerMsg::Close {
            reason: "restarting".into()
        }
    );
    // The end of the connection is either a clean close or a reset,
    // depending on how the server tears it down; then the stream is over.
    let mut items = 0;
    while let Some(item) = within(stream.next()).await {
        assert!(item.is_err(), "{item:?}");
        items += 1;
        assert!(items <= 1);
    }
    assert!(
        within(sink.send(&WorkerMsg::Pong { nonce: 1 }))
            .await
            .is_err()
    );
    assert!(sink.is_closed());
}

#[tokio::test]
async fn closing_and_dropping_end_the_connection_on_the_server_side() {
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<&'static str>();
    let harness = Harness::start(false, move |mut socket| {
        let ended = ended_tx.clone();
        async move {
            loop {
                match socket.recv().await {
                    Some(Ok(Message::Close(_))) => {
                        let _ = ended.send("close frame");
                        return;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        let _ = ended.send("connection ended");
                        return;
                    }
                }
            }
        }
    })
    .await;

    // An orderly close through the sink.
    let (sink, mut stream) = harness.connect().await;
    sink.close().await;
    assert_eq!(within(ended_rx.recv()).await.unwrap(), "close frame");
    // The server drops the socket once it has seen the close frame, which
    // may reach this side as a clean close or as a reset; either way the
    // stream ends without delivering a message.
    while let Some(item) = within(stream.next()).await {
        assert!(item.is_err(), "{item:?}");
    }

    // Dropping the reading half closes the connection too.
    let (sink, stream) = harness.connect().await;
    drop(stream);
    assert!(within(ended_rx.recv()).await.is_some());
    // The sink notices on its next use at the latest.
    let mut failed = false;
    for _ in 0..50 {
        if sink.send(&WorkerMsg::Pong { nonce: 1 }).await.is_err() {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(failed, "sending on a closed connection kept succeeding");
}

#[tokio::test]
async fn closing_a_connection_the_controller_does_not_answer_ends_cleanly() {
    let harness = Harness::start(false, |socket| async move {
        // Never read, so the close frame is not answered.
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(socket);
    })
    .await;
    let options = WorkerSocketOptions {
        ping_interval: Duration::from_millis(30),
        pong_timeout: Duration::from_millis(200),
        ..WorkerSocketOptions::default()
    };
    let (sink, mut stream) = harness.connect_with(options).await;
    within(sink.close()).await;
    // No pings follow the close frame, so there is no error to report.
    assert!(within(stream.next()).await.is_none());
    let error = within(sink.send(&WorkerMsg::Pong { nonce: 1 }))
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
}

#[tokio::test]
async fn the_default_frame_limit_matches_the_controllers() {
    assert_eq!(
        WorkerSocketOptions::default().max_message_bytes,
        8 * 1024 * 1024
    );
}
