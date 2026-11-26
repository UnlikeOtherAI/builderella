//! One worker's control connection, from `Hello` to teardown.
//!
//! Messages from a worker are handled strictly in the order they arrive,
//! one at a time, and each is acknowledged only after the ledger has
//! committed it. Handling one message never waits on another worker.

use crate::auth::MachineAuth;
use crate::error::Failure;
use crate::hub::{CommandKind, Conn, Outbox};
use crate::server::PeerAddr;
use crate::state::{App, lock};
use axum::Extension;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use bella_core::ids::{AttemptId, BootId, CommandId, MachineId};
use bella_core::machine::{AvailabilityFlag, Inventory};
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, AttemptReport, CommandResult, ControllerCommand,
    ControllerMsg, Heartbeat, LocalAttemptState, LogChunk, OfferDecision, PROTOCOL_VERSION,
    ReconcileAction, RejectCode, WorkerMsg,
};
use bella_core::store::ObjectStore;
use bella_ledger::{EventDisposition, LedgerError, LogAppendStatus, OfferReplyOutcome};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{Instrument, debug, info, warn};

/// WebSocket close code for a policy violation (RFC 6455 §7.4.1).
const CLOSE_POLICY: u16 = 1008;
/// WebSocket close code for a normal closure.
const CLOSE_NORMAL: u16 = 1000;
/// Longest close reason a close frame can carry is 123 bytes.
const MAX_CLOSE_REASON: usize = 120;
/// Attempt ownership answers cached per connection.
const MAX_OWNERSHIP_CACHE: usize = 8192;

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

/// `GET /api/v1/worker/ws`. The machine token is checked before the upgrade.
pub(crate) async fn upgrade(
    State(app): State<App>,
    machine: MachineAuth,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => {
            return Failure::bad_request(format!(
                "this route is a WebSocket endpoint: {rejection}"
            ))
            .into_response();
        }
    };
    let limit = app.tuning.ws_max_message_bytes;
    ws.max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| {
            let span = tracing::info_span!("worker", machine_id = %machine.id);
            session(app, socket, machine.id, peer).instrument(span)
        })
}

fn encode(message: &ControllerMsg) -> Option<Message> {
    match serde_json::to_string(message) {
        Ok(text) => Some(Message::Text(text.into())),
        Err(error) => {
            warn!(%error, "a controller message could not be serialized");
            None
        }
    }
}

fn close_frame(code: u16, reason: &str) -> Message {
    let mut end = reason.len().min(MAX_CLOSE_REASON);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Message::Close(Some(CloseFrame {
        code,
        reason: reason[..end].into(),
    }))
}

/// Tell the worker why the connection ends, then close it.
async fn say_goodbye(sink: &mut Sink, reason: &str, code: u16, limit: Duration) {
    let farewell = async {
        if let Some(message) = encode(&ControllerMsg::Close {
            reason: reason.to_string(),
        }) {
            sink.send(message).await?;
        }
        sink.send(close_frame(code, reason)).await?;
        sink.flush().await
    };
    // The worker may already be gone; there is nobody left to tell.
    let _ = tokio::time::timeout(limit, farewell).await;
}

struct Hello {
    boot_id: BootId,
    connection_generation: u64,
    known_epoch: u64,
    inventory: Inventory,
    attempts: Vec<AttemptReport>,
}

/// Read the first frame, which must be a `Hello` this controller can serve.
async fn read_hello(stream: &mut Stream, machine: &MachineId) -> Result<Hello, String> {
    loop {
        let frame = match stream.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(error)) => return Err(format!("the connection failed: {error}")),
            None => return Err("the connection closed before Hello".into()),
        };
        let text = match frame {
            Message::Text(text) => text,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err("the connection closed before Hello".into()),
            Message::Binary(_) => return Err("binary frames are not part of the protocol".into()),
        };
        return match serde_json::from_str::<WorkerMsg>(text.as_str()) {
            Ok(WorkerMsg::Hello {
                protocol_version,
                machine_id,
                boot_id,
                connection_generation,
                known_epoch,
                inventory,
                attempts,
                ..
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    Err(format!(
                        "unsupported protocol version {protocol_version}; this controller speaks \
                         version {PROTOCOL_VERSION}"
                    ))
                } else if machine_id != *machine {
                    Err("the Hello names a different machine than the token belongs to".into())
                } else {
                    Ok(Hello {
                        boot_id,
                        connection_generation,
                        known_epoch,
                        inventory: *inventory,
                        attempts,
                    })
                }
            }
            Ok(_) => Err("the first message must be Hello".into()),
            Err(error) => Err(format!("the first message is not a valid Hello: {error}")),
        };
    }
}

async fn session(app: App, socket: WebSocket, machine: MachineId, peer: SocketAddr) {
    let (mut sink, mut stream) = socket.split();
    let tuning = &app.tuning;

    let hello = tokio::time::timeout(tuning.hello_timeout, read_hello(&mut stream, &machine)).await;
    let hello = match hello {
        Ok(Ok(hello)) => hello,
        Ok(Err(reason)) => {
            info!(%reason, "worker connection refused");
            say_goodbye(&mut sink, &reason, CLOSE_POLICY, tuning.ws_send_timeout).await;
            return;
        }
        Err(_) => {
            let reason = "no Hello arrived in time";
            info!(reason, "worker connection refused");
            say_goodbye(&mut sink, reason, CLOSE_POLICY, tuning.ws_send_timeout).await;
            return;
        }
    };

    let (conn, outbox) = Conn::new(
        app.hub.next_id(),
        machine.clone(),
        hello.connection_generation,
        peer,
        tuning,
    );
    // One connection per machine: the older one is told why it ends, and
    // its closure is recorded before this one is.
    if let Some(old) = app.hub.register(conn.clone()) {
        old.close("replaced by a newer connection from the same machine");
        if tokio::time::timeout(tuning.ws_send_timeout, old.finished())
            .await
            .is_err()
        {
            warn!("the replaced connection did not finish closing in time");
        }
    }

    {
        let reader = async {
            match handshake(&app, &conn, hello).await {
                Ok(session) => read_loop(session, &mut stream).await,
                Err(reason) => {
                    info!(%reason, "worker handshake failed");
                    conn.close(reason);
                }
            }
        };
        let writer = write_loop(&app, &conn, &mut sink, outbox);
        tokio::pin!(reader, writer);
        tokio::select! {
            () = &mut writer => {}
            () = &mut reader => {
                conn.close("the connection ended");
                // Let the writer deliver the reason before the socket goes.
                let _ = tokio::time::timeout(tuning.ws_send_timeout * 2, &mut writer).await;
            }
        }
    }

    // Record the closure only if no newer connection has taken over.
    if app.hub.remove(&conn) {
        let generation = conn.connection_generation;
        let id = machine.clone();
        match app
            .db(move |ledger| ledger.machine_disconnected(&id, generation))
            .await
        {
            Ok(_) => {}
            Err(error) => warn!(%error, "could not record the disconnect"),
        }
        app.changed();
    }
    info!(reason = %conn.close_reason(), "worker disconnected");
    conn.mark_finished();
}

/// Record the connection, reconcile, and queue `Welcome` and `Reconcile`.
/// Only after this is the machine eligible for offers.
async fn handshake(app: &App, conn: &Arc<Conn>, hello: Hello) -> Result<Session, String> {
    let Hello {
        boot_id,
        connection_generation,
        known_epoch,
        inventory,
        attempts,
    } = hello;
    let machine = conn.machine.clone();
    if known_epoch > app.epoch {
        // A newer controller has run against this worker. The ledger refuses
        // writes from a superseded controller as well; say so plainly.
        return Err(format!(
            "the worker has seen controller epoch {known_epoch}, newer than this controller's {}",
            app.epoch
        ));
    }

    let id = machine.clone();
    app.db(move |ledger| ledger.machine_connected(&id, connection_generation))
        .await
        .map_err(|error| error.to_string())?;
    debug!(connection_generation, "connection recorded");

    let id = machine.clone();
    let revision = app
        .db(move |ledger| ledger.update_inventory(&id, &inventory))
        .await
        .map_err(|error| error.to_string())?;
    debug!(inventory_revision = revision, "inventory recorded");

    let id = machine.clone();
    let boot = boot_id.clone();
    let reports = attempts.clone();
    let (verdicts, policy) = app
        .db(move |ledger| {
            let verdicts = ledger.reconcile(&id, &boot, &reports)?;
            // The machine is in contact again.
            ledger.set_machine_condition(&id, AvailabilityFlag::Stale, false)?;
            ledger.set_machine_condition(&id, AvailabilityFlag::Unreachable, false)?;
            Ok((verdicts, ledger.machine_policy(&id)?))
        })
        .await
        .map_err(|error| error.to_string())?;
    lock(&app.out_of_contact).remove(&machine);
    for verdict in &verdicts {
        info!(
            attempt_id = %verdict.attempt.attempt_id,
            generation = verdict.attempt.generation,
            action = ?verdict.action,
            "reconciled attempt"
        );
    }
    info!(
        boot_id = %boot_id,
        connection_generation,
        reported = attempts.len(),
        "worker reconciled"
    );

    let undeliverable = |error| format!("the connection closed during the handshake: {error}");
    conn.send_control(ControllerMsg::Welcome {
        protocol_version: PROTOCOL_VERSION,
        controller_epoch: app.epoch,
        fleet_id: app.fleet_id.clone(),
        policy,
        server_time: app.now(),
        heartbeat_interval_ms: u64::try_from(app.tuning.heartbeat_interval.as_millis())
            .unwrap_or(u64::MAX),
    })
    .map_err(undeliverable)?;
    conn.send_command(
        ControllerCommand::Reconcile {
            verdicts: verdicts.clone(),
        },
        app.epoch,
        app.now(),
    )
    .map_err(undeliverable)?;
    conn.mark_ready();

    let session = Session {
        app: app.clone(),
        conn: conn.clone(),
        owned: HashMap::new(),
        last_heartbeat_seq: None,
        frames_since_heartbeat: 0,
    };
    // A start permit that was committed but never arrived: the worker still
    // only holds the offer. The permit stands, so the start is sent again.
    for (report, verdict) in attempts.iter().zip(&verdicts) {
        if verdict.action == ReconcileAction::Continue
            && report.state == LocalAttemptState::OfferHeld
        {
            session.send_start(&report.attempt, true).await;
        }
    }
    app.changed();
    Ok(session)
}

/// Drain the connection's queues into the socket, commands first, and ping
/// the worker. Ends when the connection is asked to close or the socket
/// stops taking frames.
async fn write_loop(app: &App, conn: &Arc<Conn>, sink: &mut Sink, mut outbox: Outbox) {
    let tuning = &app.tuning;
    let mut ping = tokio::time::interval(tuning.ws_ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let frame = tokio::select! {
            biased;
            () = conn.closing() => break,
            Some(message) = outbox.control.recv() => encode(&message),
            Some(message) = outbox.acks.recv() => encode(&message),
            _ = ping.tick() => Some(Message::Ping(Default::default())),
        };
        let Some(frame) = frame else { continue };
        match tokio::time::timeout(tuning.ws_send_timeout, sink.send(frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                conn.close(format!("the socket failed: {error}"));
                return;
            }
            Err(_) => {
                conn.close("the worker did not take a frame in time");
                return;
            }
        }
    }
    let reason = conn.close_reason();
    say_goodbye(sink, &reason, CLOSE_NORMAL, tuning.ws_send_timeout).await;
}

/// Read frames until the socket ends, goes idle or a message makes the
/// controller close the connection.
async fn read_loop(mut session: Session, stream: &mut Stream) {
    let idle = session.app.tuning.ws_idle_timeout;
    loop {
        let frame = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(error))) => {
                session.conn.close(format!("the socket failed: {error}"));
                return;
            }
            Ok(None) => {
                session.conn.close("the worker closed the connection");
                return;
            }
            Err(_) => {
                session
                    .conn
                    .close("nothing was received from the worker for too long");
                return;
            }
        };
        let text = match frame {
            Message::Text(text) => text,
            // Pings are answered by the WebSocket layer; both count as contact.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                session.conn.close("the worker closed the connection");
                return;
            }
            Message::Binary(_) => {
                session
                    .conn
                    .close("binary frames are not part of the protocol");
                return;
            }
        };
        let message = match serde_json::from_str::<WorkerMsg>(text.as_str()) {
            Ok(message) => message,
            Err(error) => {
                session
                    .conn
                    .close(format!("a message could not be understood: {error}"));
                return;
            }
        };
        if let Flow::Close(reason) = session.handle(message).await {
            session.conn.close(reason);
            return;
        }
        if session.conn.is_closing() {
            return;
        }
    }
}

enum Flow {
    Continue,
    Close(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// The attempt was created for this machine.
    Mine,
    /// The attempt belongs to another machine.
    Foreign,
    /// The ledger has no such attempt.
    Unknown,
}

struct Session {
    app: App,
    conn: Arc<Conn>,
    owned: HashMap<AttemptId, Ownership>,
    last_heartbeat_seq: Option<u64>,
    /// Messages received since the last heartbeat, counting the next one.
    frames_since_heartbeat: u64,
}

impl Session {
    fn machine(&self) -> &MachineId {
        &self.conn.machine
    }

    fn command(&self, command: ControllerCommand) {
        if let Err(error) = self
            .conn
            .send_command(command, self.app.epoch, self.app.now())
        {
            debug!(%error, "command not queued");
        }
    }

    /// Whose attempt this is. A worker may only speak for attempts that
    /// were placed on its own machine.
    async fn ownership(&mut self, attempt: &AttemptId) -> Ownership {
        if let Some(known) = self.owned.get(attempt) {
            return *known;
        }
        let id = attempt.clone();
        let ownership = match self.app.db(move |ledger| ledger.attempt(&id)).await {
            Ok(record) if record.machine_id == *self.machine() => Ownership::Mine,
            Ok(_) => Ownership::Foreign,
            Err(LedgerError::NotFound(_)) => Ownership::Unknown,
            Err(error) => {
                warn!(%error, attempt_id = %attempt, "could not look up an attempt");
                // Not cached: the next message asks again.
                return Ownership::Unknown;
            }
        };
        if self.owned.len() >= MAX_OWNERSHIP_CACHE {
            self.owned.clear();
        }
        self.owned.insert(attempt.clone(), ownership);
        ownership
    }

    async fn handle(&mut self, message: WorkerMsg) -> Flow {
        self.frames_since_heartbeat += 1;
        match message {
            WorkerMsg::Hello { .. } => Flow::Close("Hello was already received".into()),
            WorkerMsg::Heartbeat { heartbeat } => self.heartbeat(*heartbeat).await,
            WorkerMsg::OfferReply {
                attempt, decision, ..
            } => {
                self.offer_reply(attempt, decision).await;
                Flow::Continue
            }
            WorkerMsg::CommandAck { command_id, result } => {
                self.command_ack(command_id, result).await
            }
            WorkerMsg::AttemptEvent { event } => {
                self.attempt_event(event).await;
                Flow::Continue
            }
            WorkerMsg::Log { chunk } => {
                self.log(chunk).await;
                Flow::Continue
            }
            WorkerMsg::InventoryChanged { inventory } => {
                let machine = self.machine().clone();
                match self
                    .app
                    .db(move |ledger| ledger.update_inventory(&machine, &inventory))
                    .await
                {
                    Ok(revision) => {
                        debug!(inventory_revision = revision, "inventory updated");
                        self.app.changed();
                    }
                    Err(error) => warn!(%error, "could not record the inventory"),
                }
                Flow::Continue
            }
            WorkerMsg::Pong { .. } => Flow::Continue,
        }
    }

    async fn heartbeat(&mut self, heartbeat: Heartbeat) -> Flow {
        if heartbeat.machine_id != *self.machine() {
            return Flow::Close("a heartbeat names a different machine".into());
        }
        if heartbeat.connection_generation != self.conn.connection_generation {
            return Flow::Close("a heartbeat names a different connection".into());
        }
        // The sequence must advance by one per heartbeat or by one per
        // message; anything else means messages were lost or replayed.
        let in_sequence = match self.last_heartbeat_seq {
            None => true,
            Some(previous) => {
                heartbeat.seq > previous && {
                    let step = heartbeat.seq - previous;
                    step == 1 || step == self.frames_since_heartbeat
                }
            }
        };
        self.last_heartbeat_seq = Some(heartbeat.seq);
        self.frames_since_heartbeat = 0;

        let machine = self.machine().clone();
        let was_out_of_contact = lock(&self.app.out_of_contact).contains(&machine);
        let recorded = self
            .app
            .db(move |ledger| {
                let outcome = ledger.record_heartbeat(&heartbeat)?;
                if was_out_of_contact {
                    ledger.set_machine_condition(&machine, AvailabilityFlag::Stale, false)?;
                    ledger.set_machine_condition(&machine, AvailabilityFlag::Unreachable, false)?;
                }
                let policy = if outcome.policy_stale {
                    Some(ledger.machine_policy(&machine)?)
                } else {
                    None
                };
                Ok((outcome, policy))
            })
            .await;
        match recorded {
            Ok((outcome, policy)) => {
                if was_out_of_contact {
                    lock(&self.app.out_of_contact).remove(self.machine());
                    info!("machine is in contact again");
                }
                if outcome.needs_reconcile || !in_sequence {
                    // Nothing is assumed about the worker's attempts: the
                    // machine stops being offered work and reconciles on
                    // its next Hello.
                    return Flow::Close(
                        "the heartbeat sequence is broken; reconnect to reconcile".into(),
                    );
                }
                if let Some(policy) = policy {
                    debug!(revision = policy.revision, "worker policy is out of date");
                    self.command(ControllerCommand::UpdatePolicy { policy });
                }
                self.app.changed();
                Flow::Continue
            }
            Err(error @ (LedgerError::StaleConnection { .. } | LedgerError::Unauthorized(_))) => {
                Flow::Close(error.to_string())
            }
            Err(error) => {
                warn!(%error, "could not record a heartbeat");
                Flow::Continue
            }
        }
    }

    /// Commit (or look up) the start permit and send `Start`. `resend` is
    /// set when the permit is expected to exist already.
    async fn send_start(&self, attempt: &AttemptRef, resend: bool) {
        let epoch = self.app.epoch;
        let reference = attempt.clone();
        let committed = self
            .app
            .db(move |ledger| {
                let permit = ledger.commit_start(&reference, epoch)?;
                let authority = if permit.already_committed {
                    // The stored window may have lapsed while the start was lost.
                    ledger
                        .renew_authority(&reference, epoch)
                        .unwrap_or(permit.authority)
                } else {
                    permit.authority
                };
                Ok((permit, authority))
            })
            .await;
        match committed {
            Ok((permit, authority)) => {
                info!(
                    job_id = %permit.job_id,
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    machine_id = %permit.machine_id,
                    resend,
                    "start committed; sending Start"
                );
                self.command(ControllerCommand::Start {
                    attempt: attempt.clone(),
                    authority,
                });
                self.app.events.touch();
            }
            Err(error) => {
                info!(
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    %error,
                    "start not committed; withdrawing the offer"
                );
                self.withdraw(attempt, &error.to_string()).await;
            }
        }
    }

    /// Release an offer that will not start and tell the worker to drop its
    /// hold. An attempt that already holds a start permit is left alone.
    async fn withdraw(&self, attempt: &AttemptRef, reason: &str) {
        let reference = attempt.clone();
        let text = reason.to_string();
        let revoked = self
            .app
            .db(move |ledger| ledger.revoke_offer(&reference, &text))
            .await;
        // A start permit exists: the attempt may run and stays as it is.
        if let Err(LedgerError::InvalidState(_)) = revoked {
            return;
        }
        self.command(ControllerCommand::Revoke {
            attempt: attempt.clone(),
            reason: reason.to_string(),
        });
        self.app.changed();
    }

    async fn offer_reply(&mut self, attempt: AttemptRef, decision: OfferDecision) {
        let accepted = matches!(decision, OfferDecision::Accepted);
        match self.ownership(&attempt.attempt_id).await {
            Ownership::Mine => {}
            Ownership::Foreign => {
                warn!(attempt_id = %attempt.attempt_id, "offer reply for another machine's attempt dropped");
                return;
            }
            Ownership::Unknown => {
                if accepted {
                    self.command(ControllerCommand::Revoke {
                        attempt,
                        reason: "the controller does not know this offer".into(),
                    });
                }
                return;
            }
        }
        let reference = attempt.clone();
        let recorded = self
            .app
            .db(move |ledger| {
                let outcome = ledger.record_offer_reply(&reference, &decision)?;
                Ok((outcome, ledger.attempt(&reference.attempt_id)?.job_id))
            })
            .await;
        match recorded {
            Ok((OfferReplyOutcome::Accepted, job_id)) => {
                info!(
                    %job_id,
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    "offer accepted"
                );
                self.send_start(&attempt, false).await;
            }
            Ok((OfferReplyOutcome::Requeued, job_id)) => {
                info!(
                    %job_id,
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    "offer rejected; job queued again"
                );
                self.app.note_rejection(&job_id, self.machine());
                self.app.changed();
            }
            Ok((OfferReplyOutcome::AlreadyReleased, _)) => {}
            Err(error) => {
                // Expired, revoked, cancelled or superseded: a late answer
                // starts nothing.
                info!(
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    %error,
                    "offer reply not applied"
                );
                if accepted {
                    self.command(ControllerCommand::Revoke {
                        attempt,
                        reason: error.to_string(),
                    });
                }
            }
        }
    }

    async fn command_ack(&mut self, command_id: CommandId, result: CommandResult) -> Flow {
        let Some(sent) = self.conn.take_sent(&command_id) else {
            return Flow::Continue;
        };
        let attempt = sent.attempt;
        match result {
            CommandResult::Applied | CommandResult::Duplicate => {
                if sent.kind == CommandKind::Start {
                    let reference = attempt.clone();
                    match self
                        .app
                        .db(move |ledger| ledger.record_start_delivered(&reference))
                        .await
                    {
                        Ok(_) => {
                            info!(
                                attempt_id = %attempt.attempt_id,
                                generation = attempt.generation,
                                "start delivered"
                            );
                            self.app.events.touch();
                        }
                        Err(error) => warn!(%error, "could not record a delivered start"),
                    }
                }
                Flow::Continue
            }
            CommandResult::Rejected { code, message } => {
                info!(
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    kind = ?sent.kind,
                    ?code,
                    %message,
                    "worker rejected a command"
                );
                // The worker refuses to run an attempt the ledger counts as
                // started, or does not know one the ledger holds. Which of
                // the two is right is for a reconcile to decide, once.
                let disagrees = match sent.kind {
                    CommandKind::Start => true,
                    CommandKind::RenewAuthority
                    | CommandKind::Cancel
                    | CommandKind::FinishEnvironment => {
                        matches!(code, RejectCode::UnknownAttempt | RejectCode::OfferNotHeld)
                    }
                    CommandKind::Offer | CommandKind::Revoke => false,
                };
                if disagrees && lock(&self.app.forced_reconciles).insert(attempt.attempt_id.clone())
                {
                    return Flow::Close(format!(
                        "the worker and the controller disagree about attempt {}; reconnect to \
                         reconcile",
                        attempt.attempt_id
                    ));
                }
                Flow::Continue
            }
        }
    }

    fn ack_events(&self, attempt: &AttemptRef, through_seq: u64) {
        if let Err(error) = self.conn.send_control(ControllerMsg::EventsAcked {
            attempt_id: attempt.attempt_id.clone(),
            through_seq,
        }) {
            debug!(%error, "event acknowledgement not queued");
        }
    }

    async fn attempt_event(&mut self, event: AttemptEventMsg) {
        let attempt = event.attempt.clone();
        let seq = event.seq;
        match self.ownership(&attempt.attempt_id).await {
            Ownership::Mine => {}
            Ownership::Foreign => {
                warn!(attempt_id = %attempt.attempt_id, "event for another machine's attempt dropped");
                return;
            }
            Ownership::Unknown => {
                // Never applied. Acknowledged so the worker stops resending
                // events nobody will take.
                warn!(attempt_id = %attempt.attempt_id, seq, "event for an unknown attempt dropped");
                self.ack_events(&attempt, seq);
                return;
            }
        }
        let kind = event_name(&event.event);
        let released = matches!(event.event, AttemptEvent::Released { .. });
        let published = match &event.event {
            AttemptEvent::ArtifactPublished { digest, .. } => Some(digest.clone()),
            _ => None,
        };
        let app = self.app.clone();
        let applied = self
            .app
            .db(move |ledger| {
                let job_id = ledger.attempt(&event.attempt.attempt_id)?.job_id;
                let missing_object = match &published {
                    Some(digest) => !app.store.contains(digest).unwrap_or(false),
                    None => false,
                };
                Ok((job_id, missing_object, ledger.record_attempt_event(&event)))
            })
            .await;
        let (job_id, missing_object, disposition) = match applied {
            Ok(applied) => applied,
            Err(error) => {
                warn!(%error, attempt_id = %attempt.attempt_id, seq, "event not applied");
                return;
            }
        };
        match disposition {
            Ok(disposition) => {
                // Durable (or already durable, or closed): only now is it
                // acknowledged.
                self.ack_events(&attempt, seq);
                if disposition == EventDisposition::Applied {
                    info!(
                        %job_id,
                        attempt_id = %attempt.attempt_id,
                        generation = attempt.generation,
                        seq,
                        event = kind,
                        "attempt event applied"
                    );
                    if missing_object {
                        warn!(
                            %job_id,
                            attempt_id = %attempt.attempt_id,
                            "an artifact was announced before its object was stored; it is not \
                             listed until the object exists"
                        );
                    }
                    if released {
                        lock(&self.app.pending_cancels).remove(&attempt.attempt_id);
                        lock(&self.app.finish_sent).remove(&attempt.attempt_id);
                        lock(&self.app.forced_reconciles).remove(&attempt.attempt_id);
                    }
                    self.app.changed();
                }
            }
            Err(LedgerError::StaleGeneration { current, .. }) => {
                // A15/A60: an attempt that is no longer the job's current
                // generation can never set its result.
                warn!(
                    %job_id,
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    current_generation = current,
                    seq,
                    event = kind,
                    "event from a stale generation dropped"
                );
                self.ack_events(&attempt, seq);
            }
            Err(LedgerError::SequenceGap { expected, .. }) => {
                debug!(attempt_id = %attempt.attempt_id, expected, got = seq, "event sequence gap");
                self.ack_events(&attempt, expected.saturating_sub(1));
            }
            Err(error) => {
                warn!(
                    %job_id,
                    attempt_id = %attempt.attempt_id,
                    generation = attempt.generation,
                    seq,
                    event = kind,
                    %error,
                    "event refused"
                );
            }
        }
    }

    async fn ack_log(&self, chunk_ref: &LogRef, through_offset: u64) {
        let ack = ControllerMsg::LogsAcked {
            attempt_id: chunk_ref.attempt.attempt_id.clone(),
            step: chunk_ref.step.clone(),
            stream: chunk_ref.stream,
            through_offset,
        };
        // Waits while this worker's acknowledgement queue is full.
        let _ = self.conn.send_ack(ack).await;
    }

    async fn log(&mut self, chunk: LogChunk) {
        let reference = LogRef {
            attempt: chunk.attempt.clone(),
            step: chunk.step.clone(),
            stream: chunk.stream,
        };
        let end = chunk.offset.saturating_add(decoded_len(&chunk.data_b64));
        match self.ownership(&chunk.attempt.attempt_id).await {
            Ownership::Mine => {}
            Ownership::Foreign => {
                warn!(attempt_id = %chunk.attempt.attempt_id, "log for another machine's attempt dropped");
                return;
            }
            Ownership::Unknown => {
                self.ack_log(&reference, end).await;
                return;
            }
        }
        let appended = self.app.db(move |ledger| ledger.append_log(&chunk)).await;
        match appended {
            Ok(append) => {
                if append.status == LogAppendStatus::Truncated {
                    self.app.events.touch();
                }
                self.ack_log(&reference, append.through_offset).await;
            }
            Err(LedgerError::LogGap { expected, .. }) => {
                self.ack_log(&reference, expected).await;
            }
            Err(LedgerError::StaleGeneration { .. }) => {
                // Output of an attempt that no longer counts is not kept.
                self.ack_log(&reference, end).await;
            }
            Err(error) => {
                warn!(
                    attempt_id = %reference.attempt.attempt_id,
                    step = %reference.step,
                    %error,
                    "log chunk refused"
                );
            }
        }
    }
}

struct LogRef {
    attempt: AttemptRef,
    step: String,
    stream: bella_core::protocol::LogStream,
}

/// Number of bytes a padded base64 string decodes to.
fn decoded_len(b64: &str) -> u64 {
    let padding = b64.bytes().rev().take(2).filter(|b| *b == b'=').count();
    ((b64.len() / 4) * 3).saturating_sub(padding) as u64
}

fn event_name(event: &AttemptEvent) -> &'static str {
    match event {
        AttemptEvent::Preparing => "preparing",
        AttemptEvent::Started { .. } => "started",
        AttemptEvent::StepChanged { .. } => "step_changed",
        AttemptEvent::EnvironmentReady { .. } => "environment_ready",
        AttemptEvent::Collecting => "collecting",
        AttemptEvent::ArtifactPublished { .. } => "artifact_published",
        AttemptEvent::Exited { .. } => "exited",
        AttemptEvent::Cleanup { .. } => "cleanup",
        AttemptEvent::Released { .. } => "released",
        AttemptEvent::Quarantined { .. } => "quarantined",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_lengths() {
        assert_eq!(decoded_len(""), 0);
        assert_eq!(decoded_len("YQ=="), 1);
        assert_eq!(decoded_len("YWI="), 2);
        assert_eq!(decoded_len("YWJj"), 3);
        assert_eq!(decoded_len("YWJjZA=="), 4);
    }

    #[test]
    fn close_reasons_are_cut_on_a_character_boundary() {
        let long = "é".repeat(100);
        match close_frame(CLOSE_NORMAL, &long) {
            Message::Close(Some(frame)) => {
                assert!(frame.reason.as_str().len() <= MAX_CLOSE_REASON);
                assert!(frame.reason.as_str().chars().all(|c| c == 'é'));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
