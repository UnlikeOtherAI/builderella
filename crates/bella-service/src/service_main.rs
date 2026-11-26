//! The process side of a Windows service.

use crate::{
    ENV_SERVICE_WORKING_DIR, STOP_TIMEOUT, ServiceError, ShutdownSignal, shutdown_channel,
};
use std::ffi::OsString;
use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: started from a console.
const NOT_A_SERVICE: i32 = 1063;
/// Service-specific exit codes reported to the Service Control Manager.
const EXIT_FAILED: u32 = 1;
const EXIT_PANICKED: u32 = 2;
/// How often a stop in progress is reported, and how long each report covers.
const STOP_REPORT_EVERY: Duration = Duration::from_secs(2);
const STOP_WAIT_HINT: Duration = Duration::from_secs(15);

type Body = Box<dyn FnOnce(ShutdownSignal) -> Result<(), String> + Send>;

/// The dispatcher calls a plain function, so its input and output pass through
/// these. A process hosts one service.
static PENDING: Mutex<Option<(String, Body)>> = Mutex::new(None);
static OUTCOME: Mutex<Option<Result<(), ServiceError>>> = Mutex::new(None);

enum Event {
    Stop,
    Done,
}

/// Run `body` as the Windows service `name`, and return when the service has
/// stopped. Call it once, early, from `main`.
///
/// It connects to the Service Control Manager, changes to the directory in
/// `BELLA_SERVICE_WORKING_DIR`, reports `Running` and calls `body` with a
/// signal that resolves on a Stop or system Shutdown request. While `body`
/// winds down it reports `StopPending`. A `body` still running
/// [`STOP_TIMEOUT`](crate::STOP_TIMEOUT) after the request ends the process,
/// as the other platforms do. At system shutdown Windows allows far less time;
/// recovery at the next start covers what is left.
///
/// A `body` that returns an error or panics without having been asked to stop
/// is reported as a failure, which makes the recovery actions restart the
/// service. After a requested stop the service is reported as stopped cleanly,
/// and the error is only returned.
///
/// A service has no standard output: log to files under
/// [`service_log_dir`](crate::service_log_dir).
///
/// Fails with [`ServiceError::NotStartedByServiceManager`] when the process was
/// started any other way; `body` is not called then.
pub fn run_as_service<E: Display>(
    name: &str,
    body: impl FnOnce(ShutdownSignal) -> Result<(), E> + Send + 'static,
) -> Result<(), ServiceError> {
    let body: Body = Box::new(move |signal| body(signal).map_err(|error| error.to_string()));
    {
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.is_some() {
            return Err(ServiceError::Body("a service is already starting".into()));
        }
        *pending = Some((name.to_owned(), body));
    }
    if let Err(error) = service_dispatcher::start(name, ffi_service_main) {
        PENDING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        return Err(match error {
            windows_service::Error::Winapi(ref io) if io.raw_os_error() == Some(NOT_A_SERVICE) => {
                ServiceError::NotStartedByServiceManager
            }
            other => failed("connect to the service manager", other),
        });
    }
    OUTCOME
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| {
            Err(ServiceError::Body(
                "the service manager returned without running the service".into(),
            ))
        })
}

/// The arguments are the ones given to `StartService`, which are not used.
extern "system" fn ffi_service_main(_count: u32, _arguments: *mut *mut u16) {
    let outcome = serve();
    *OUTCOME.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
}

fn serve() -> Result<(), ServiceError> {
    let (name, body) = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or_else(|| ServiceError::Body("there is no service body to run".into()))?;

    let (trigger, signal) = shutdown_channel();
    let (events, received) = mpsc::channel();
    let stop_events = events.clone();
    let handle = service_control_handler::register(&name, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            trigger.request();
            // The receiver is gone only when the service is already ending.
            let _ = stop_events.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(|error| failed("register the control handler", error))?;

    if let Some(dir) = std::env::var_os(ENV_SERVICE_WORKING_DIR).filter(|d| !d.is_empty())
        && let Err(error) = change_dir(dir)
    {
        report_stopped(handle, ServiceExitCode::ServiceSpecific(EXIT_FAILED))?;
        return Err(error);
    }
    report(
        handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::NO_ERROR,
        0,
    )?;

    let monitor = thread::spawn(move || watch_stop(handle, &received));
    let result = catch_unwind(AssertUnwindSafe(|| body(signal.clone())));
    let _ = events.send(Event::Done);
    // Joined before `Stopped` is reported, so no later `StopPending` follows it.
    let _ = monitor.join();

    let (failure, code) = match result {
        Ok(Ok(())) => (None, 0),
        Ok(Err(error)) => (Some(error), EXIT_FAILED),
        Err(_) => (Some("the service body panicked".to_owned()), EXIT_PANICKED),
    };
    let exit = if code == 0 || signal.is_requested() {
        ServiceExitCode::NO_ERROR
    } else {
        ServiceExitCode::ServiceSpecific(code)
    };
    report_stopped(handle, exit)?;
    failure.map_or(Ok(()), |error| Err(ServiceError::Body(error)))
}

fn change_dir(dir: OsString) -> Result<(), ServiceError> {
    std::env::set_current_dir(&dir).map_err(|source| ServiceError::Io {
        action: "change to the working directory",
        path: dir.into(),
        source,
    })
}

/// Keep the service manager informed while the body winds down, and enforce the
/// stop timeout.
fn watch_stop(handle: ServiceStatusHandle, events: &Receiver<Event>) {
    if !matches!(events.recv(), Ok(Event::Stop)) {
        return;
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    let mut checkpoint = 0;
    loop {
        checkpoint += 1;
        // A failed progress report must not end the cleanup it reports on.
        let _ = report(
            handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::NO_ERROR,
            checkpoint,
        );
        match events.recv_timeout(STOP_REPORT_EVERY) {
            Ok(Event::Done) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Event::Stop) | Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            // Reported as a clean stop so that recovery does not restart a
            // service somebody asked to stop.
            let _ = report_stopped(handle, ServiceExitCode::NO_ERROR);
            std::process::exit(1);
        }
    }
}

fn report_stopped(handle: ServiceStatusHandle, exit: ServiceExitCode) -> Result<(), ServiceError> {
    report(
        handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit,
        0,
    )
}

fn report(
    handle: ServiceStatusHandle,
    state: ServiceState,
    accepted: ServiceControlAccept,
    exit: ServiceExitCode,
    checkpoint: u32,
) -> Result<(), ServiceError> {
    handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: exit,
            checkpoint,
            wait_hint: if checkpoint == 0 {
                Duration::ZERO
            } else {
                STOP_WAIT_HINT
            },
            process_id: None,
        })
        .map_err(|error| failed("report the service state", error))
}

fn failed(action: &str, error: windows_service::Error) -> ServiceError {
    ServiceError::Manager {
        action: action.to_owned(),
        reason: describe(&error),
    }
}

/// `windows-service` keeps the OS error in the source chain only.
pub(crate) fn describe(error: &windows_service::Error) -> String {
    match error {
        windows_service::Error::Winapi(io) => io.to_string(),
        other => other.to_string(),
    }
}
