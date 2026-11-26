//! The stop request as a value that can be awaited or blocked on.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Default)]
struct State {
    requested: bool,
    wakers: Vec<Waker>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Requests the stop that the paired [`ShutdownSignal`] reports.
#[derive(Clone)]
pub struct ShutdownTrigger(Arc<Shared>);

impl ShutdownTrigger {
    /// Request the stop. Later calls do nothing.
    pub fn request(&self) {
        let wakers = {
            let mut state = self.0.lock();
            state.requested = true;
            std::mem::take(&mut state.wakers)
        };
        self.0.changed.notify_all();
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// Resolves once a stop has been requested, and stays resolved. It works with
/// any async runtime and from plain threads. Dropping every trigger does not
/// resolve it.
#[derive(Clone)]
pub struct ShutdownSignal(Arc<Shared>);

impl ShutdownSignal {
    /// Whether a stop has been requested.
    pub fn is_requested(&self) -> bool {
        self.0.lock().requested
    }

    /// Block the thread until a stop is requested.
    pub fn wait(&self) {
        let mut state = self.0.lock();
        while !state.requested {
            state = self
                .0
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Block for at most `timeout`; `true` if a stop was requested.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(self.0.lock(), timeout, |state| !state.requested)
            .unwrap_or_else(PoisonError::into_inner);
        state.requested
    }
}

impl Future for ShutdownSignal {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut state = self.0.lock();
        if state.requested {
            return Poll::Ready(());
        }
        if !state.wakers.iter().any(|w| w.will_wake(context.waker())) {
            state.wakers.push(context.waker().clone());
        }
        Poll::Pending
    }
}

/// A connected trigger and signal, for running in a console where the caller
/// turns Ctrl-C or `SIGTERM` into the request.
pub fn shutdown_channel() -> (ShutdownTrigger, ShutdownSignal) {
    let shared = Arc::new(Shared::default());
    (ShutdownTrigger(shared.clone()), ShutdownSignal(shared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    use std::thread;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_pending_future_is_woken_once_and_resolves() {
        let (trigger, mut signal) = shutdown_channel();
        let count = Arc::new(Count::default());
        let waker = Waker::from(count.clone());
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut signal).poll(&mut context).is_pending());
        assert!(Pin::new(&mut signal).poll(&mut context).is_pending());
        trigger.request();
        trigger.request();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(Pin::new(&mut signal).poll(&mut context).is_ready());
    }

    #[test]
    fn blocking_waiters_are_released() {
        let (trigger, signal) = shutdown_channel();
        assert!(!signal.wait_timeout(Duration::from_millis(1)));
        let waiter = {
            let signal = signal.clone();
            thread::spawn(move || signal.wait())
        };
        trigger.request();
        waiter.join().unwrap();
        assert!(signal.is_requested());
        assert!(signal.wait_timeout(Duration::ZERO));
    }

    #[test]
    fn dropping_the_trigger_is_not_a_stop_request() {
        let (trigger, signal) = shutdown_channel();
        drop(trigger);
        assert!(!signal.wait_timeout(Duration::from_millis(1)));
    }
}
