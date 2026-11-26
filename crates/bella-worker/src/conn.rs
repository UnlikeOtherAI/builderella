//! The connection loop: connect with backoff, greet, reconcile, then move
//! messages until the connection ends, and start over.
//!
//! Three things run side by side on one connection and none waits for
//! another: reading and applying controller messages, the heartbeat timer,
//! and the writer. The writer always sends replies and heartbeats before
//! events and log chunks, and log chunks are bounded by the unacknowledged
//! window, so a flood of output delays neither commands nor heartbeats.

use crate::core::{Core, WorkerError};
use crate::{Connection, Connector, WorkerConfig};
use bella_core::defaults;
use bella_core::protocol::{ControllerMsg, WorkerMsg};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Run a blocking part of the daemon off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, WorkerError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| WorkerError::Internal(format!("a worker task failed: {error}")))
}

pub(crate) async fn run(
    mut config: WorkerConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), WorkerError> {
    let connector = config.connector.clone();
    // The copy inside `config` is not used again.
    config.connector = connector.clone();
    let core = blocking(move || Core::start(config)).await??;
    core.spawn_protection_loop()?;

    tokio::pin!(shutdown);
    tokio::select! {
        () = &mut shutdown => {}
        () = connection_loop(core.clone(), connector) => {}
    }
    let stopping = core.clone();
    blocking(move || stopping.shutdown()).await?;
    Ok(())
}

/// A small xorshift generator for backoff jitter; nothing here needs to be
/// unpredictable.
struct Jitter(u64);

impl Jitter {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Self(nanos | 1)
    }

    /// A duration in `[0, limit]`.
    fn up_to(&mut self, limit: Duration) -> Duration {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let millis = limit.as_millis().min(u128::from(u64::MAX)) as u64;
        Duration::from_millis(if millis == 0 { 0 } else { self.0 % (millis + 1) })
    }
}

/// The delay before connection attempt number `failures + 1`: the minimum
/// doubled per failure up to the maximum, plus a random part of up to half.
fn backoff(min: Duration, max: Duration, failures: u32, jitter: &mut Jitter) -> Duration {
    let doubled = min.saturating_mul(1u32.checked_shl(failures.min(16)).unwrap_or(u32::MAX));
    let base = doubled.min(max.max(min));
    base + jitter.up_to(base / 2)
}

async fn connection_loop(core: Arc<Core>, connector: Arc<dyn Connector>) {
    let mut jitter = Jitter::new();
    let mut failures = 0u32;
    loop {
        match connector.connect().await {
            Ok(connection) => {
                let served = serve(&core, connection).await;
                core.disconnected();
                match served {
                    Ok(reason) => {
                        tracing::info!(%reason, "control connection ended");
                        failures = 0;
                    }
                    Err(reason) => {
                        tracing::warn!(%reason, "control connection failed");
                        failures = failures.saturating_add(1);
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "could not connect to the controller");
                failures = failures.saturating_add(1);
            }
        }
        let delay = backoff(
            core.tuning.reconnect_min,
            core.tuning.reconnect_max,
            failures.saturating_sub(1),
            &mut jitter,
        );
        tokio::time::sleep(delay).await;
    }
}

/// Serve one connection. `Ok` when it was established and later ended,
/// `Err` when the handshake failed.
async fn serve(core: &Arc<Core>, connection: Connection) -> Result<String, String> {
    let Connection { tx, mut rx } = connection;

    let greeter = core.clone();
    let hello = blocking(move || greeter.hello())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("could not prepare the greeting: {error}"))?;
    tx.send(hello)
        .await
        .map_err(|_| "the connection closed before the greeting".to_string())?;

    let welcome = tokio::time::timeout(core.tuning.handshake_timeout, rx.recv())
        .await
        .map_err(|_| "the controller did not answer the greeting in time".to_string())?;
    let heartbeat_interval = match welcome {
        Some(ControllerMsg::Welcome {
            protocol_version,
            controller_epoch,
            fleet_id,
            policy,
            heartbeat_interval_ms,
            ..
        }) => {
            let greeted = core.clone();
            blocking(move || greeted.welcome(protocol_version, controller_epoch, &fleet_id, &policy))
                .await
                .map_err(|error| error.to_string())??;
            core.tuning.heartbeat_interval.unwrap_or_else(|| {
                if heartbeat_interval_ms == 0 {
                    defaults::HEARTBEAT_INTERVAL
                } else {
                    Duration::from_millis(heartbeat_interval_ms)
                }
            })
        }
        Some(ControllerMsg::Close { reason }) => {
            return Err(format!("the controller refused the connection: {reason}"));
        }
        Some(_) => return Err("the controller did not start with a welcome".to_string()),
        None => return Err("the connection closed during the greeting".to_string()),
    };

    let (control_tx, control_rx) = mpsc::unbounded_channel::<WorkerMsg>();
    let ended = tokio::select! {
        reason = read(core, &mut rx, &control_tx) => reason,
        reason = heartbeats(core, heartbeat_interval, &control_tx) => reason,
        reason = write(core, &tx, control_rx) => reason,
    };
    Ok(ended)
}

/// Apply controller messages in the order they arrive.
async fn read(
    core: &Arc<Core>,
    rx: &mut mpsc::Receiver<ControllerMsg>,
    control: &mpsc::UnboundedSender<WorkerMsg>,
) -> String {
    while let Some(message) = rx.recv().await {
        let handler = core.clone();
        let replies = match blocking(move || handler.on_message(message)).await {
            Ok(Ok(replies)) => replies,
            Ok(Err(reason)) => return reason,
            Err(error) => return error.to_string(),
        };
        for reply in replies {
            if control.send(reply).is_err() {
                return "the connection's writer ended".to_string();
            }
        }
    }
    "the controller closed the connection".to_string()
}

/// Send a heartbeat every interval, and at once whenever something a
/// scheduler cares about changed.
async fn heartbeats(
    core: &Arc<Core>,
    interval: Duration,
    control: &mpsc::UnboundedSender<WorkerMsg>,
) -> String {
    loop {
        let reporter = core.clone();
        match blocking(move || reporter.heartbeat()).await {
            Ok(Ok(heartbeat)) => {
                if control.send(heartbeat).is_err() {
                    return "the connection's writer ended".to_string();
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "could not build a heartbeat"),
            Err(error) => return error.to_string(),
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = core.heartbeat_wake.notified() => {}
        }
    }
}

/// Move messages onto the connection: replies and heartbeats first, then
/// journaled events, then log chunks within the unacknowledged window.
async fn write(
    core: &Arc<Core>,
    tx: &mpsc::Sender<WorkerMsg>,
    mut control: mpsc::UnboundedReceiver<WorkerMsg>,
) -> String {
    const CLOSED: &str = "the connection closed";
    loop {
        while let Ok(message) = control.try_recv() {
            if tx.send(message).await.is_err() {
                return CLOSED.to_string();
            }
        }
        let collector = core.clone();
        let batch = match blocking(move || collector.collect_outbound()).await {
            Ok(batch) => batch,
            Err(error) => return error.to_string(),
        };
        let more = !batch.is_empty();
        for message in batch {
            // Anything urgent that arrived meanwhile goes first.
            while let Ok(urgent) = control.try_recv() {
                if tx.send(urgent).await.is_err() {
                    return CLOSED.to_string();
                }
            }
            if tx.send(message).await.is_err() {
                return CLOSED.to_string();
            }
        }
        if more {
            continue;
        }
        tokio::select! {
            message = control.recv() => match message {
                Some(message) => {
                    if tx.send(message).await.is_err() {
                        return CLOSED.to_string();
                    }
                }
                None => return "the connection's reader ended".to_string(),
            },
            () = core.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_maximum_with_bounded_jitter() {
        let mut jitter = Jitter::new();
        let min = Duration::from_millis(100);
        let max = Duration::from_millis(1000);
        for (failures, base) in [(0u32, 100u64), (1, 200), (2, 400), (3, 800), (4, 1000), (40, 1000)] {
            for _ in 0..50 {
                let delay = backoff(min, max, failures, &mut jitter).as_millis() as u64;
                assert!(delay >= base && delay <= base + base / 2, "{failures}: {delay}");
            }
        }
    }
}
