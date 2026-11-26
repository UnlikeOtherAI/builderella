//! The worker hub: one live control connection per machine and the queues
//! that feed it.
//!
//! Every connection has two bounded outgoing queues. Commands, `Welcome`
//! and event acknowledgements go through the control queue, which the
//! writer always drains first; log acknowledgements go through their own
//! queue. A command is therefore never stuck behind log acknowledgements.
//!
//! Nothing that sends a command ever waits for a worker: a full control
//! queue closes that worker's connection instead. Only the task reading a
//! worker's own socket waits on that worker's log acknowledgement queue, so
//! a worker that floods logs or stops reading slows down its own log stream
//! and nobody else.

use crate::config::Tuning;
use crate::state::lock;
use bella_core::UnixMillis;
use bella_core::ids::{CommandId, MachineId};
use bella_core::protocol::{AttemptRef, CommandEnvelope, ControllerCommand, ControllerMsg};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Sent commands remembered per connection for matching acknowledgements.
const MAX_REMEMBERED_COMMANDS: usize = 4096;

/// The command could not be queued for the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Undeliverable {
    #[error("the machine has no ready connection")]
    NotConnected,
    #[error("the machine's connection is closing")]
    Closing,
    #[error("the machine is not reading its commands")]
    QueueFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandKind {
    Offer,
    Start,
    Revoke,
    RenewAuthority,
    Cancel,
    FinishEnvironment,
}

/// A command that was queued for a worker and may be acknowledged.
#[derive(Clone, Debug)]
pub(crate) struct SentCommand {
    pub kind: CommandKind,
    pub attempt: AttemptRef,
}

fn describe(command: &ControllerCommand) -> Option<SentCommand> {
    let (kind, attempt) = match command {
        ControllerCommand::Offer { offer } => (CommandKind::Offer, &offer.attempt),
        ControllerCommand::Start { attempt, .. } => (CommandKind::Start, attempt),
        ControllerCommand::Revoke { attempt, .. } => (CommandKind::Revoke, attempt),
        ControllerCommand::RenewAuthority { attempt, .. } => (CommandKind::RenewAuthority, attempt),
        ControllerCommand::Cancel { attempt, .. } => (CommandKind::Cancel, attempt),
        ControllerCommand::FinishEnvironment { attempt, .. } => {
            (CommandKind::FinishEnvironment, attempt)
        }
        ControllerCommand::UpdatePolicy { .. }
        | ControllerCommand::SetDrain { .. }
        | ControllerCommand::Reconcile { .. } => return None,
    };
    Some(SentCommand {
        kind,
        attempt: attempt.clone(),
    })
}

#[derive(Default)]
struct Remembered {
    order: VecDeque<CommandId>,
    by_id: HashMap<CommandId, SentCommand>,
}

/// The receiving ends of a connection's queues, owned by its writer.
pub(crate) struct Outbox {
    pub control: mpsc::Receiver<ControllerMsg>,
    pub acks: mpsc::Receiver<ControllerMsg>,
}

/// One worker's control connection.
pub(crate) struct Conn {
    /// Unique within this process, to tell a connection from its successor.
    pub id: u64,
    pub machine: MachineId,
    pub connection_generation: u64,
    pub peer: SocketAddr,
    control: mpsc::Sender<ControllerMsg>,
    acks: mpsc::Sender<ControllerMsg>,
    closing: CancellationToken,
    close_reason: Mutex<Option<String>>,
    /// `Welcome` and `Reconcile` are queued; commands may follow.
    ready: AtomicBool,
    remembered: Mutex<Remembered>,
    /// Cancelled once the connection is torn down and recorded as closed.
    finished: CancellationToken,
}

impl Conn {
    pub(crate) fn new(
        id: u64,
        machine: MachineId,
        connection_generation: u64,
        peer: SocketAddr,
        tuning: &Tuning,
    ) -> (Arc<Self>, Outbox) {
        let (control, control_rx) = mpsc::channel(tuning.ws_control_queue.max(8));
        let (acks, acks_rx) = mpsc::channel(tuning.ws_ack_queue.max(1));
        let conn = Arc::new(Self {
            id,
            machine,
            connection_generation,
            peer,
            control,
            acks,
            closing: CancellationToken::new(),
            close_reason: Mutex::new(None),
            ready: AtomicBool::new(false),
            remembered: Mutex::new(Remembered::default()),
            finished: CancellationToken::new(),
        });
        (
            conn,
            Outbox {
                control: control_rx,
                acks: acks_rx,
            },
        )
    }

    /// Ask the connection to close. The first reason given is the one the
    /// worker is told.
    pub(crate) fn close(&self, reason: impl Into<String>) {
        lock(&self.close_reason).get_or_insert_with(|| reason.into());
        self.closing.cancel();
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.is_cancelled()
    }

    pub(crate) async fn closing(&self) {
        self.closing.cancelled().await;
    }

    pub(crate) fn close_reason(&self) -> String {
        lock(&self.close_reason)
            .clone()
            .unwrap_or_else(|| "the connection is closing".to_string())
    }

    pub(crate) fn mark_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.is_closing()
    }

    pub(crate) fn mark_finished(&self) {
        self.finished.cancel();
    }

    pub(crate) async fn finished(&self) {
        self.finished.cancelled().await;
    }

    /// Queue a control-plane message. Never waits: a full queue means the
    /// worker stopped reading, and its connection is closed.
    pub(crate) fn send_control(&self, message: ControllerMsg) -> Result<(), Undeliverable> {
        if self.is_closing() {
            return Err(Undeliverable::Closing);
        }
        match self.control.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.close("the worker stopped reading its commands");
                Err(Undeliverable::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Undeliverable::Closing),
        }
    }

    /// Wrap `command` in an envelope and queue it. The command is
    /// remembered so its acknowledgement can be matched.
    pub(crate) fn send_command(
        &self,
        command: ControllerCommand,
        epoch: u64,
        now: UnixMillis,
    ) -> Result<CommandId, Undeliverable> {
        let command_id = CommandId::generate();
        let sent = describe(&command);
        self.send_control(ControllerMsg::Command {
            envelope: CommandEnvelope {
                command_id: command_id.clone(),
                controller_epoch: epoch,
                sent_at: now,
                command,
            },
        })?;
        if let Some(sent) = sent {
            let mut remembered = lock(&self.remembered);
            if remembered.order.len() >= MAX_REMEMBERED_COMMANDS
                && let Some(oldest) = remembered.order.pop_front()
            {
                remembered.by_id.remove(&oldest);
            }
            remembered.order.push_back(command_id.clone());
            remembered.by_id.insert(command_id.clone(), sent);
        }
        Ok(command_id)
    }

    /// The command an acknowledgement refers to, if it was sent on this
    /// connection and not acknowledged before.
    pub(crate) fn take_sent(&self, command_id: &CommandId) -> Option<SentCommand> {
        lock(&self.remembered).by_id.remove(command_id)
    }

    /// Queue a log acknowledgement, waiting while the queue is full. Only
    /// the task that reads this worker's socket calls this, so the wait
    /// throttles this worker's own log stream.
    pub(crate) async fn send_ack(&self, message: ControllerMsg) -> Result<(), Undeliverable> {
        tokio::select! {
            biased;
            () = self.closing.cancelled() => Err(Undeliverable::Closing),
            sent = self.acks.send(message) => sent.map_err(|_| Undeliverable::Closing),
        }
    }

    /// The peer is on a private, link-local or loopback network.
    pub(crate) fn peer_is_local(&self) -> bool {
        is_local_address(self.peer.ip())
    }
}

/// Whether `addr` belongs to a network that does not leave the site:
/// loopback, RFC 1918, link-local, or an IPv6 unique local address.
pub(crate) fn is_local_address(addr: IpAddr) -> bool {
    match addr.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Registry of live worker connections, at most one per machine.
pub(crate) struct Hub {
    conns: Mutex<HashMap<MachineId, Arc<Conn>>>,
    next_id: AtomicU64,
}

impl Hub {
    pub(crate) fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Make `conn` the machine's connection and return the one it replaces.
    pub(crate) fn register(&self, conn: Arc<Conn>) -> Option<Arc<Conn>> {
        lock(&self.conns).insert(conn.machine.clone(), conn)
    }

    /// Remove `conn` if it is still the machine's connection. Returns
    /// `false` when a newer connection has taken its place.
    pub(crate) fn remove(&self, conn: &Conn) -> bool {
        let mut conns = lock(&self.conns);
        if conns
            .get(&conn.machine)
            .is_some_and(|current| current.id == conn.id)
        {
            conns.remove(&conn.machine);
            true
        } else {
            false
        }
    }

    pub(crate) fn get(&self, machine: &MachineId) -> Option<Arc<Conn>> {
        lock(&self.conns).get(machine).cloned()
    }

    /// The machine's connection, once it is past `Welcome` and `Reconcile`.
    pub(crate) fn ready(&self, machine: &MachineId) -> Option<Arc<Conn>> {
        self.get(machine).filter(|conn| conn.is_ready())
    }

    /// Queue a command for a machine whose connection is ready.
    pub(crate) fn send(
        &self,
        machine: &MachineId,
        command: ControllerCommand,
        epoch: u64,
        now: UnixMillis,
    ) -> Result<CommandId, Undeliverable> {
        self.ready(machine)
            .ok_or(Undeliverable::NotConnected)?
            .send_command(command, epoch, now)
    }

    pub(crate) fn close(&self, machine: &MachineId, reason: &str) {
        if let Some(conn) = self.get(machine) {
            conn.close(reason);
        }
    }

    /// Ask every connection to close and return them.
    pub(crate) fn close_all(&self, reason: &str) -> Vec<Arc<Conn>> {
        let conns: Vec<Arc<Conn>> = lock(&self.conns).values().cloned().collect();
        for conn in &conns {
            conn.close(reason);
        }
        conns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::ids::AttemptId;
    use bella_core::protocol::LogStream;

    fn tuning(control: usize, acks: usize) -> Tuning {
        Tuning {
            ws_control_queue: control,
            ws_ack_queue: acks,
            ..Tuning::default()
        }
    }

    fn conn(tuning: &Tuning) -> (Arc<Conn>, Outbox) {
        Conn::new(
            1,
            MachineId::generate(),
            1,
            "127.0.0.1:9".parse().unwrap(),
            tuning,
        )
    }

    fn attempt() -> AttemptRef {
        AttemptRef {
            attempt_id: AttemptId::generate(),
            generation: 1,
        }
    }

    fn log_ack() -> ControllerMsg {
        ControllerMsg::LogsAcked {
            attempt_id: AttemptId::generate(),
            step: "build".into(),
            stream: LogStream::Stdout,
            through_offset: 1,
        }
    }

    /// A68 at the hub: a worker whose log acknowledgements are backed up
    /// still receives commands, and filling its queues stalls nobody else.
    #[tokio::test]
    async fn a68_commands_are_not_queued_behind_log_acknowledgements() {
        let tuning = tuning(8, 2);
        let (conn, mut outbox) = conn(&tuning);
        conn.send_ack(log_ack()).await.unwrap();
        conn.send_ack(log_ack()).await.unwrap();

        // The acknowledgement queue is full: the reader of this worker's
        // socket would wait here.
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            conn.send_ack(log_ack()),
        )
        .await;
        assert!(
            blocked.is_err(),
            "a full ack queue must make its sender wait"
        );

        // A command still goes out at once, through the other queue.
        let cancel = ControllerCommand::Cancel {
            attempt: attempt(),
            reason: "cancelled".into(),
            grace_secs: 10,
        };
        let id = conn.send_command(cancel, 3, UnixMillis(5)).unwrap();
        match outbox.control.try_recv().unwrap() {
            ControllerMsg::Command { envelope } => {
                assert_eq!(envelope.command_id, id);
                assert_eq!(envelope.controller_epoch, 3);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(conn.take_sent(&id).unwrap().kind, CommandKind::Cancel);
        assert!(conn.take_sent(&id).is_none(), "an ack matches once");
        assert!(outbox.acks.try_recv().is_ok());
    }

    #[tokio::test]
    async fn a68_a_worker_that_stops_reading_is_disconnected_not_waited_for() {
        let tuning = tuning(8, 2);
        let (conn, _outbox) = conn(&tuning);
        let renew = || ControllerCommand::Revoke {
            attempt: attempt(),
            reason: "expired".into(),
        };
        for _ in 0..8 {
            conn.send_command(renew(), 1, UnixMillis(1)).unwrap();
        }
        assert_eq!(
            conn.send_command(renew(), 1, UnixMillis(1)),
            Err(Undeliverable::QueueFull)
        );
        assert!(conn.is_closing());
        assert_eq!(
            conn.send_command(renew(), 1, UnixMillis(1)),
            Err(Undeliverable::Closing)
        );
        // A sender waiting on the ack queue is released by the close.
        assert_eq!(conn.send_ack(log_ack()).await, Err(Undeliverable::Closing));
    }

    #[test]
    fn a_newer_connection_replaces_the_older_one() {
        let hub = Hub::new();
        let tuning = Tuning::default();
        let machine = MachineId::generate();
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (first, _a) = Conn::new(hub.next_id(), machine.clone(), 1, peer, &tuning);
        let (second, _b) = Conn::new(hub.next_id(), machine.clone(), 2, peer, &tuning);
        assert!(hub.register(first.clone()).is_none());
        let replaced = hub.register(second.clone()).unwrap();
        assert_eq!(replaced.id, first.id);
        // The replaced connection's teardown must not unregister its successor.
        assert!(!hub.remove(&first));
        assert_eq!(hub.get(&machine).unwrap().id, second.id);
        // Not ready until Welcome and Reconcile are queued.
        assert_eq!(
            hub.send(
                &machine,
                ControllerCommand::SetDrain { draining: true },
                1,
                UnixMillis(1)
            ),
            Err(Undeliverable::NotConnected)
        );
        second.mark_ready();
        assert!(
            hub.send(
                &machine,
                ControllerCommand::SetDrain { draining: true },
                1,
                UnixMillis(1)
            )
            .is_ok()
        );
        assert!(hub.remove(&second));
        assert!(hub.get(&machine).is_none());
    }

    #[test]
    fn the_first_close_reason_wins() {
        let (conn, _outbox) = conn(&Tuning::default());
        conn.close("replaced");
        conn.close("shutting down");
        assert_eq!(conn.close_reason(), "replaced");
    }

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.9",
            "169.254.0.5",
            "::1",
            "fd12::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_local_address(local.parse().unwrap()), "{local}");
        }
        for remote in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_local_address(remote.parse().unwrap()), "{remote}");
        }
    }
}
