//! The worker's control connection: one outbound WebSocket to the
//! controller carrying JSON text frames.

use crate::client::{USER_AGENT, bearer, error_from_response};
use crate::error::ClientError;
use crate::tls;
use bella_core::protocol::{ControllerMsg, WORKER_WS_PATH, WorkerMsg};
use bella_net::{ControllerUrl, Fingerprint};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, Stream, StreamExt};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{
    AUTHORIZATION, USER_AGENT as USER_AGENT_HEADER,
};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Bytes, Error as WsError, Message};

/// Messages accepted from callers before `send` starts to wait.
const OUTBOUND_QUEUE: usize = 64;
/// Time allowed for the closing frame when the connection is given up.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

type Item = Result<ControllerMsg, ClientError>;

/// Limits and liveness settings of a worker connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerSocketOptions {
    /// Time allowed for connecting: TCP, TLS and the WebSocket upgrade
    /// together. Default 10 s.
    pub connect_timeout: Duration,
    /// How often a WebSocket ping is sent. Default
    /// `bella_core::defaults::HEARTBEAT_INTERVAL` (5 s).
    pub ping_interval: Duration,
    /// A ping that is not answered within this time means the connection
    /// is dead. Default `bella_core::defaults::TELEMETRY_STALE_AFTER`
    /// (15 s).
    pub pong_timeout: Duration,
    /// Largest frame, and largest whole message, accepted from the
    /// controller or sent to it. Default 8 MiB, the controller's own limit.
    pub max_message_bytes: usize,
    /// Messages from the controller buffered for the reader before the
    /// connection stops reading. Default 256.
    pub inbound_buffer: usize,
}

impl Default for WorkerSocketOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            ping_interval: bella_core::defaults::HEARTBEAT_INTERVAL,
            pong_timeout: bella_core::defaults::TELEMETRY_STALE_AFTER,
            max_message_bytes: 8 * 1024 * 1024,
            inbound_buffer: 256,
        }
    }
}

/// Connects a worker to the controller's WebSocket.
///
/// The connection is driven by a background task until it ends or the
/// reading half is dropped, so WebSocket pings from the controller are
/// answered and this side's own pings are sent without the caller polling
/// anything. A reader that leaves [`WorkerSocketOptions::inbound_buffer`]
/// messages unread stops the connection from being read, and with it the
/// answering of pings, until it catches up.
pub struct WorkerSocket;

impl WorkerSocket {
    /// Opens the worker connection with the default
    /// [`WorkerSocketOptions`].
    ///
    /// The transport follows the same rule as [`Client`](crate::Client):
    /// `https` becomes `wss` with the pinned fleet certificate and requires
    /// a fingerprint; `http` becomes plain `ws`. The request goes to
    /// `bella_core::protocol::WORKER_WS_PATH` and carries the machine token
    /// as `authorization: Bearer <token>`.
    ///
    /// Errors: [`ClientError::Tls`] (with `pin_mismatch` when the
    /// certificate is not the pinned one, in which case the token was not
    /// sent), [`ClientError::Api`] or [`ClientError::Http`] when the
    /// controller refuses the upgrade, [`ClientError::Timeout`],
    /// [`ClientError::Transport`].
    ///
    /// Must be called from within a Tokio runtime.
    pub async fn connect(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        machine_token: &str,
    ) -> Result<(WorkerSink, WorkerStream), ClientError> {
        Self::connect_with(
            url,
            fingerprint,
            machine_token,
            WorkerSocketOptions::default(),
        )
        .await
    }

    /// [`WorkerSocket::connect`] with explicit limits and liveness
    /// settings.
    pub async fn connect_with(
        url: &ControllerUrl,
        fingerprint: Option<&Fingerprint>,
        machine_token: &str,
        options: WorkerSocketOptions,
    ) -> Result<(WorkerSink, WorkerStream), ClientError> {
        let tls = tls::pinned_config(url, fingerprint)?;
        let authorization = bearer(machine_token)?;
        let socket = tokio::time::timeout(
            options.connect_timeout,
            establish(url, tls, authorization, &options),
        )
        .await
        .map_err(|_| ClientError::Timeout)??;

        let (out_tx, out_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (in_tx, in_rx) = mpsc::channel(options.inbound_buffer.max(1));
        let max_message_bytes = options.max_message_bytes;
        tokio::spawn(Driver::new(socket, out_rx, in_tx, options).run());
        Ok((
            WorkerSink {
                tx: out_tx,
                max_message_bytes,
            },
            WorkerStream { rx: in_rx },
        ))
    }
}

async fn establish(
    url: &ControllerUrl,
    tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    authorization: HeaderValue,
    options: &WorkerSocketOptions,
) -> Result<WebSocketStream<Connection>, ClientError> {
    let tcp = TcpStream::connect((url.host().to_string(), url.port()))
        .await
        .map_err(|e| ClientError::from_io(&e))?;
    // Control messages are small and must not wait for more data.
    let _ = tcp.set_nodelay(true);
    let connection = match tls {
        None => Connection::Plain(tcp),
        Some(config) => {
            let stream = tokio_rustls::TlsConnector::from(config)
                .connect(url.server_name(), tcp)
                .await
                .map_err(|e| ClientError::from_io(&e))?;
            Connection::Tls(Box::new(stream))
        }
    };

    let scheme = if url.is_tls() { "wss" } else { "ws" };
    let mut request = format!("{scheme}://{}{WORKER_WS_PATH}", url.authority())
        .into_client_request()
        .map_err(|e| ClientError::invalid(format!("unusable worker socket address: {e}")))?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request
        .headers_mut()
        .insert(USER_AGENT_HEADER, HeaderValue::from_static(USER_AGENT));

    let config = WebSocketConfig::default()
        .max_message_size(Some(options.max_message_bytes))
        .max_frame_size(Some(options.max_message_bytes));
    match tokio_tungstenite::client_async_with_config(request, connection, Some(config)).await {
        Ok((socket, _response)) => Ok(socket),
        Err(WsError::Http(response)) => Err(error_from_response(
            response.status().as_u16(),
            response.body().as_deref().unwrap_or_default(),
        )),
        Err(error) => Err(socket_error(error, options.max_message_bytes)),
    }
}

/// The sending half of a worker connection. Cheap to clone.
#[derive(Clone, Debug)]
pub struct WorkerSink {
    tx: mpsc::Sender<Outgoing>,
    max_message_bytes: usize,
}

impl WorkerSink {
    /// Sends one message as a JSON text frame and returns once it has been
    /// written to the connection.
    ///
    /// That is not an acknowledgement: the controller confirms events and
    /// logs with its own messages. An error means the connection is gone
    /// ([`ClientError::Transport`]) or the message is larger than the
    /// frame limit ([`ClientError::Invalid`], connection unaffected).
    pub async fn send(&self, message: &WorkerMsg) -> Result<(), ClientError> {
        let text = serde_json::to_string(message)
            .map_err(|e| ClientError::invalid(format!("the message cannot be encoded: {e}")))?;
        if text.len() > self.max_message_bytes {
            return Err(ClientError::invalid(format!(
                "the message is {} bytes, more than the {}-byte frame limit",
                text.len(),
                self.max_message_bytes
            )));
        }
        let (done, written) = oneshot::channel();
        self.tx
            .send(Outgoing {
                message: Message::text(text),
                done: Some(done),
            })
            .await
            .map_err(|_| closed())?;
        written.await.map_err(|_| closed())?
    }

    /// Starts an orderly close. Messages already accepted are written
    /// first and later sends fail; the reading half ends once the controller
    /// has closed too, or after the pong timeout if it does not.
    pub async fn close(&self) {
        let _ = self
            .tx
            .send(Outgoing {
                message: Message::Close(None),
                done: None,
            })
            .await;
    }

    /// Whether the connection has ended.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// The receiving half of a worker connection.
///
/// Dropping it closes the connection.
#[derive(Debug)]
pub struct WorkerStream {
    rx: mpsc::Receiver<Item>,
}

impl WorkerStream {
    /// The next message from the controller.
    ///
    /// * `Some(Ok(message))`: a decoded message.
    /// * `Some(Err(ClientError::Decode { .. }))`: a frame that is not a
    ///   valid `ControllerMsg` (bad JSON, a binary frame). The connection
    ///   stays usable, except after a frame over the size limit, which
    ///   ends it.
    /// * `Some(Err(_))` of another kind: the connection failed or stopped
    ///   answering pings. `None` follows.
    /// * `None`: the connection is closed.
    pub async fn next(&mut self) -> Option<Item> {
        self.rx.recv().await
    }
}

impl Stream for WorkerStream {
    type Item = Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn closed() -> ClientError {
    ClientError::transport("the worker connection is closed")
}

/// One frame to write, and who to tell when it has been written.
#[derive(Debug)]
struct Outgoing {
    message: Message,
    done: Option<oneshot::Sender<Result<(), ClientError>>>,
}

/// Why the driver stopped.
enum Exit {
    /// The peer closed the connection.
    Closed,
    /// The reading half was dropped.
    ReaderGone,
    /// The connection failed.
    Failed(ClientError),
}

/// Owns the socket and moves frames in both directions.
///
/// Reads and writes make progress independently, so a peer that is slow to
/// read does not stop this side from reading (and answering pings), and
/// the other way round.
struct Driver {
    sink: SplitSink<WebSocketStream<Connection>, Message>,
    stream: SplitStream<WebSocketStream<Connection>>,
    out_rx: mpsc::Receiver<Outgoing>,
    in_tx: mpsc::Sender<Item>,
    options: WorkerSocketOptions,
    /// Frames accepted but not yet handed to the socket.
    queue: VecDeque<Outgoing>,
    /// Callers whose frame is in the socket's buffer, waiting for a flush.
    unflushed: Vec<oneshot::Sender<Result<(), ClientError>>>,
    needs_flush: bool,
    /// A decoded item the reader has no room for yet. While it is set the
    /// socket is not read.
    held: Option<Item>,
    /// When the ping that is still unanswered was queued.
    awaiting_pong: Option<Instant>,
    ping_nonce: u64,
    senders_open: bool,
    /// Since when a closing handshake has been under way. No more pings are
    /// sent and no more messages accepted.
    closing: Option<Instant>,
}

impl Driver {
    fn new(
        socket: WebSocketStream<Connection>,
        out_rx: mpsc::Receiver<Outgoing>,
        in_tx: mpsc::Sender<Item>,
        options: WorkerSocketOptions,
    ) -> Self {
        let (sink, stream) = socket.split();
        Self {
            sink,
            stream,
            out_rx,
            in_tx,
            options,
            queue: VecDeque::new(),
            unflushed: Vec::new(),
            needs_flush: false,
            held: None,
            awaiting_pong: None,
            ping_nonce: 0,
            senders_open: true,
            closing: None,
        }
    }

    async fn run(mut self) {
        let exit = self.drive().await;
        let failure = match &exit {
            Exit::Failed(error) => error.clone(),
            Exit::Closed | Exit::ReaderGone => closed(),
        };
        for outgoing in self.queue.drain(..) {
            if let Some(done) = outgoing.done {
                let _ = done.send(Err(failure.clone()));
            }
        }
        for done in self.unflushed.drain(..) {
            let _ = done.send(Err(failure.clone()));
        }
        // Later sends fail at once instead of queueing behind nothing.
        self.out_rx.close();

        match exit {
            Exit::Closed => {}
            Exit::ReaderGone => {
                let _ =
                    tokio::time::timeout(CLOSE_GRACE, self.sink.send(Message::Close(None))).await;
            }
            Exit::Failed(error) => {
                tracing::debug!(%error, "worker connection ended");
                if let Some(item) = self.held.take() {
                    let _ = self.in_tx.send(item).await;
                }
                let _ = self.in_tx.send(Err(error)).await;
            }
        }
    }

    async fn drive(&mut self) -> Exit {
        let period = self.options.ping_interval.max(Duration::from_millis(1));
        let mut ping = tokio::time::interval_at(Instant::now() + period, period);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // A ping can only be judged while this side is reading.
            let pong_deadline = match (self.awaiting_pong, &self.held) {
                (Some(sent), None) => Some(sent + self.options.pong_timeout),
                _ => None,
            };
            let reading = self.held.is_none();
            let writing = !self.queue.is_empty() || self.needs_flush;
            let accepting = self.senders_open && self.queue.len() < OUTBOUND_QUEUE;
            let close_deadline = match (self.closing, &self.held) {
                (Some(since), None) => Some(since + self.options.pong_timeout),
                _ => None,
            };

            // Set when a write finds the connection closing. Acted on after
            // the `select!`, which keeps `self.in_tx` borrowed.
            let mut peer_closing = false;

            tokio::select! {
                permit = self.in_tx.reserve(), if !reading => match permit {
                    Ok(permit) => {
                        if let Some(item) = self.held.take() {
                            permit.send(item);
                        }
                        // Time spent not reading does not count against
                        // the peer.
                        if self.awaiting_pong.is_some() {
                            self.awaiting_pong = Some(Instant::now());
                        }
                    }
                    Err(_) => return Exit::ReaderGone,
                },
                () = self.in_tx.closed(), if reading => return Exit::ReaderGone,
                frame = self.stream.next(), if reading => match frame {
                    None => return Exit::Closed,
                    // A function over the fields it changes: the permit of
                    // the first branch keeps `self.in_tx` borrowed for the
                    // whole `select!`.
                    Some(Ok(message)) => {
                        if message.is_close() {
                            self.closing.get_or_insert_with(Instant::now);
                            self.awaiting_pong = None;
                            self.senders_open = false;
                        }
                        received(
                            message,
                            &mut self.held,
                            &mut self.awaiting_pong,
                            &mut self.needs_flush,
                        );
                    }
                    Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                        return Exit::Closed;
                    }
                    Some(Err(error)) => {
                        return Exit::Failed(socket_error(error, self.options.max_message_bytes));
                    }
                },
                outgoing = self.out_rx.recv(), if accepting => match outgoing {
                    Some(outgoing) => {
                        if outgoing.message.is_close() {
                            self.closing.get_or_insert_with(Instant::now);
                            self.awaiting_pong = None;
                            self.senders_open = false;
                            self.out_rx.close();
                        }
                        self.queue.push_back(outgoing);
                    }
                    None => self.senders_open = false,
                },
                written = poll_fn(|cx| {
                    poll_write(
                        &mut self.sink,
                        &mut self.queue,
                        &mut self.unflushed,
                        &mut self.needs_flush,
                        cx,
                    )
                }), if writing => {
                    match written {
                        Ok(()) => {}
                        // The controller started to close while this side
                        // still had something to write. What it sent before
                        // that is still to be read.
                        Err(
                            WsError::ConnectionClosed
                            | WsError::Protocol(ProtocolError::SendAfterClosing),
                        ) => peer_closing = true,
                        Err(error) => {
                            return Exit::Failed(socket_error(
                                error,
                                self.options.max_message_bytes,
                            ));
                        }
                    }
                },
                _ = ping.tick() => {
                    if self.awaiting_pong.is_none() && self.closing.is_none() {
                        self.ping_nonce = self.ping_nonce.wrapping_add(1);
                        let payload = Bytes::copy_from_slice(&self.ping_nonce.to_be_bytes());
                        // Ahead of queued messages: liveness is not held
                        // up by a backlog of logs.
                        self.queue.push_front(Outgoing {
                            message: Message::Ping(payload),
                            done: None,
                        });
                        self.awaiting_pong = Some(Instant::now());
                    }
                },
                () = wait_until(close_deadline), if close_deadline.is_some() => {
                    return Exit::Closed;
                },
                () = wait_until(pong_deadline), if pong_deadline.is_some() => {
                    return Exit::Failed(ClientError::transport(format!(
                        "the controller did not answer a ping within {:?}",
                        self.options.pong_timeout
                    )));
                },
            }
            if peer_closing {
                self.stop_writing();
            }
        }
    }

    /// Fails every message not yet written, and accepts no more.
    fn stop_writing(&mut self) {
        self.closing.get_or_insert_with(Instant::now);
        self.awaiting_pong = None;
        self.senders_open = false;
        self.needs_flush = false;
        self.out_rx.close();
        // Dropping a waiter tells its sender that the connection is closed.
        self.queue.clear();
        self.unflushed.clear();
    }
}

/// Acts on one frame from the controller.
fn received(
    message: Message,
    held: &mut Option<Item>,
    awaiting_pong: &mut Option<Instant>,
    needs_flush: &mut bool,
) {
    match message {
        Message::Text(text) => {
            *held = Some(
                serde_json::from_str::<ControllerMsg>(text.as_str()).map_err(|e| {
                    ClientError::decode(format!("a frame is not a controller message: {e}"))
                }),
            );
        }
        Message::Binary(_) => {
            *held = Some(Err(ClientError::decode(
                "a binary frame arrived where a JSON text frame was expected",
            )));
        }
        Message::Pong(_) => *awaiting_pong = None,
        // The WebSocket layer has queued the answer (a pong, or the closing
        // frame); make sure it goes out even when nothing else is being
        // written.
        Message::Ping(_) | Message::Close(_) => *needs_flush = true,
        Message::Frame(_) => {}
    }
}

/// Hands queued frames to the socket and flushes it.
///
/// Resolves when everything queued has been written and flushed, or with
/// the error that stopped it. It can be dropped and called again at any
/// point: a frame leaves the queue only when the socket has taken it.
fn poll_write(
    sink: &mut SplitSink<WebSocketStream<Connection>, Message>,
    queue: &mut VecDeque<Outgoing>,
    unflushed: &mut Vec<oneshot::Sender<Result<(), ClientError>>>,
    needs_flush: &mut bool,
    cx: &mut Context<'_>,
) -> Poll<Result<(), WsError>> {
    while !queue.is_empty() {
        match sink.poll_ready_unpin(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }
        let Some(outgoing) = queue.pop_front() else {
            break;
        };
        sink.start_send_unpin(outgoing.message)?;
        *needs_flush = true;
        if let Some(done) = outgoing.done {
            unflushed.push(done);
        }
    }
    if *needs_flush {
        match sink.poll_flush_unpin(cx) {
            Poll::Ready(Ok(())) => {
                *needs_flush = false;
                for done in unflushed.drain(..) {
                    let _ = done.send(Ok(()));
                }
            }
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }
    }
    Poll::Ready(Ok(()))
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn socket_error(error: WsError, max_message_bytes: usize) -> ClientError {
    match error {
        WsError::Io(error) => ClientError::from_io(&error),
        WsError::Capacity(error) => ClientError::decode(format!(
            "a frame from the controller breaks the {max_message_bytes}-byte limit: {error}"
        )),
        WsError::Utf8(error) => {
            ClientError::decode(format!("a text frame is not valid UTF-8: {error}"))
        }
        other => ClientError::transport(format!("worker connection failed: {other}")),
    }
}

/// The byte stream under the WebSocket.
enum Connection {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Connection::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Connection::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Connection::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Connection::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}
