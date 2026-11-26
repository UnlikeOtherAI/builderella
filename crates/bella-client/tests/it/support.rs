//! A controller stand-in on `127.0.0.1:0`, over plain HTTP or over TLS
//! with a freshly generated fleet identity.

#![allow(dead_code)]

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bella_client::{Client, ClientOptions, RetryPolicy};
use bella_core::api::{
    ArtifactView, EnvironmentView, EventRecord, FleetOverview, JobView, LogPage,
    PairingCodeResponse, ProjectView, QueueView, TokenResponse, WhoAmI,
};
use bella_core::{ApiError, ErrorCode};
use bella_net::{ControllerUrl, Fingerprint, ServerIdentity};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// The bearer token the test clients send.
pub const TOKEN: &str = "bella_dev_0123456789abcdefghijklmnopqrstuv";

/// Upper bound for anything a test waits for, so a bug fails the test
/// instead of hanging it.
pub const STEP: Duration = Duration::from_secs(20);

/// Awaits `future`, failing the test if it takes longer than [`STEP`].
pub async fn within<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(STEP, future)
        .await
        .expect("the step did not finish in time")
}

/// Sample API values generated from the real `bella_core::api` types by
/// `cargo run -p bella-core --example api_fixtures`.
#[derive(Deserialize)]
pub struct Fixtures {
    pub whoami: WhoAmI,
    pub fleet: FleetOverview,
    pub queue: QueueView,
    pub jobs: Vec<JobView>,
    pub log_pages: Vec<LogPage>,
    pub artifacts: Vec<ArtifactView>,
    pub environments: Vec<EnvironmentView>,
    pub projects: Vec<ProjectView>,
    pub events: Vec<EventRecord>,
    pub pairing_code: PairingCodeResponse,
    pub token: TokenResponse,
}

pub fn fixtures() -> Fixtures {
    serde_json::from_str(include_str!(
        "../../../../dashboard/src/mocks/fixtures.json"
    ))
    .expect("the dashboard fixtures match bella_core::api")
}

/// A running test server. Stops when dropped.
pub struct Server {
    pub url: ControllerUrl,
    pub fingerprint: Option<Fingerprint>,
    pub addr: SocketAddr,
    task: JoinHandle<()>,
    _identity: Option<TempDir>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Client settings that keep failing tests fast: short backoff, short
/// timeouts, the default number of tries.
pub fn fast_options() -> ClientOptions {
    ClientOptions {
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        retry: RetryPolicy {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            max_elapsed: Duration::from_secs(10),
        },
        stream_idle_timeout: Some(Duration::from_secs(10)),
        reconnect_initial_backoff: Duration::from_millis(10),
        reconnect_max_backoff: Duration::from_millis(40),
    }
}

impl Server {
    /// Serves `router` over plain HTTP on loopback.
    pub async fn http(router: Router) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server {
            url: ControllerUrl::parse(&format!("http://{addr}"), false).unwrap(),
            fingerprint: None,
            addr,
            task,
            _identity: None,
        }
    }

    /// Serves `router` over TLS with a new fleet identity.
    pub async fn https(router: Router) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let identity =
            ServerIdentity::load_or_generate(dir.path(), &["fleet.builderella.test".to_owned()])
                .unwrap();
        let config = bella_net::server_config(&identity).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let listener = TlsListener {
            tcp,
            acceptor: TlsAcceptor::from(config),
        };
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server {
            url: ControllerUrl::parse(&format!("https://{addr}"), false).unwrap(),
            fingerprint: Some(identity.fingerprint()),
            addr,
            task,
            _identity: Some(dir),
        }
    }

    pub async fn start(tls: bool, router: Router) -> Server {
        if tls {
            Self::https(router).await
        } else {
            Self::http(router).await
        }
    }

    /// A client for this server with [`fast_options`] and [`TOKEN`].
    pub fn client(&self) -> Client {
        self.client_with(fast_options())
    }

    pub fn client_with(&self, options: ClientOptions) -> Client {
        Client::with_options(
            &self.url,
            self.fingerprint.as_ref(),
            Some(TOKEN.to_owned()),
            options,
        )
        .unwrap()
    }
}

/// Accepts TCP connections and completes the TLS handshake. A connection
/// whose handshake fails (a client that rejects the certificate) is dropped
/// and the next one is accepted.
struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else {
                continue;
            };
            let handshake = tokio::time::timeout(STEP, self.acceptor.accept(tcp)).await;
            if let Ok(Ok(stream)) = handshake {
                return (stream, addr);
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// One request as the server received it.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).expect("the request body is the expected JSON")
    }

    /// The query parameters, decoded and sorted.
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .query
            .as_deref()
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(name), percent_decode(value))
            })
            .collect();
        pairs.sort();
        pairs
    }
}

/// Decodes `application/x-www-form-urlencoded` text.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                index += 2;
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8(out).unwrap()
}

/// What the mock answers.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json<T: serde::Serialize>(value: &T) -> Reply {
        Reply {
            status: 200,
            content_type: Some("application/json"),
            body: serde_json::to_vec(value).unwrap(),
        }
    }

    pub fn error(code: ErrorCode, message: &str) -> Reply {
        let error = ApiError::new(code, message);
        Reply {
            status: code.http_status(),
            content_type: Some("application/json"),
            body: serde_json::to_vec(&error).unwrap(),
        }
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            content_type: None,
            body: Vec::new(),
        }
    }

    pub fn text(status: u16, body: &str) -> Reply {
        Reply {
            status,
            content_type: Some("text/plain"),
            body: body.as_bytes().to_vec(),
        }
    }
}

#[derive(Default)]
struct MockState {
    replies: HashMap<(Method, String), VecDeque<Reply>>,
    requests: Vec<Recorded>,
}

/// Records every request and answers from a table keyed by method and
/// full path. A request nothing was registered for gets a plain-text 404,
/// which no client call accepts as an answer.
#[derive(Clone, Default)]
pub struct Mock {
    state: Arc<Mutex<MockState>>,
}

impl Mock {
    pub fn new() -> Mock {
        Mock::default()
    }

    /// Answers every `method path` request with `reply`.
    pub fn on(&self, method: Method, path: &str, reply: Reply) -> &Mock {
        self.sequence(method, path, vec![reply])
    }

    /// Answers `method path` with each reply in turn; the last one repeats.
    pub fn sequence(&self, method: Method, path: &str, replies: Vec<Reply>) -> &Mock {
        assert!(!replies.is_empty());
        self.state
            .lock()
            .unwrap()
            .replies
            .insert((method, path.to_owned()), replies.into());
        self
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The only request received so far.
    pub fn single(&self) -> Recorded {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected one request: {requests:#?}");
        requests.into_iter().next().unwrap()
    }

    pub fn count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    pub fn clear(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    pub fn router(&self) -> Router {
        Router::new()
            .fallback(record_and_reply)
            .layer(DefaultBodyLimit::disable())
            .with_state(self.clone())
    }
}

async fn record_and_reply(State(mock): State<Mock>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let key = (parts.method.clone(), parts.uri.path().to_owned());
    let reply = {
        let mut state = mock.state.lock().unwrap();
        state.requests.push(Recorded {
            method: parts.method,
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers,
            body,
        });
        match state.replies.get_mut(&key) {
            Some(replies) if replies.len() > 1 => replies.pop_front(),
            Some(replies) => replies.front().cloned(),
            None => None,
        }
    };
    let reply = reply.unwrap_or_else(|| Reply::text(404, "no such route in the mock"));
    let mut response = Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
    if let Some(content_type) = reply.content_type {
        response = response.header("content-type", content_type);
    }
    response.body(Body::from(reply.body)).unwrap()
}

/// Asserts the headers every authenticated request carries.
pub fn assert_authenticated(request: &Recorded) {
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str()),
        "{} {}",
        request.method,
        request.path
    );
    assert_user_agent(request);
}

pub fn assert_user_agent(request: &Recorded) {
    let agent = request.header("user-agent").unwrap_or_default();
    assert_eq!(agent, format!("bella/{}", env!("CARGO_PKG_VERSION")));
}
