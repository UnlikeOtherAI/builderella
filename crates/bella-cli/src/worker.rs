//! `bella worker …`: enrollment, the daemon, the machine owner's local
//! controls and the commissioning self-test.

use crate::cli::{BoostArgs, DataDirArg, EnrollArgs, SelfTestArgs, WorkerRunArgs};
use crate::ctx::{Ctx, runtime, shutdown_signal};
use crate::error::{CliError, CliResult, Exit, wire_name};
use crate::output::utc;
use bella_client::{Client, ClientError, HttpObjectStore, WorkerSocket};
use bella_core::api::EnrollRequest;
use bella_core::machine::LocalControl;
use bella_core::protocol::{ControllerMsg, WorkerMsg};
use bella_core::store::{ObjectStore, StoreError};
use bella_core::{Digest, SystemClock, UnixMillis};
use bella_net::{ControllerUrl, Enrollment, Fingerprint};
use bella_recipe::units::parse_duration_secs;
use bella_worker::{
    ConnectError, ConnectFuture, Connection, Connector, Hooks, Tuning, WorkerConfig,
};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Version reported in the worker's inventory.
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Messages buffered in each direction between the worker and its socket.
const CHANNEL_DEPTH: usize = 256;

fn absolute(flag: &str, path: &Path) -> CliResult<PathBuf> {
    std::path::absolute(path)
        .map_err(|error| CliError::local(format!("--{flag} {}: {error}", path.display())))
}

/// The work root when none is given. Short on Windows, where the path limit
/// applies to everything materialized below it.
pub fn default_work_root(data_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\bw")
    } else {
        data_dir.join("work")
    }
}

pub fn enroll(ctx: &Ctx, args: &EnrollArgs) -> CliResult<Exit> {
    let data_dir = absolute("data-dir", &args.data_dir)?;
    let enrollment = Enrollment::parse(&args.enrollment, args.allow_insecure)
        .map_err(|error| CliError::local(error.to_string()))?;
    if let Some(existing) = bella_worker::load_enrollment(&data_dir)
        .map_err(|error| CliError::local(error.to_string()))?
    {
        return Err(CliError::local(format!(
            "{} is already enrolled as machine {} in fleet {}; use another data directory",
            data_dir.display(),
            existing.machine_id,
            existing.fleet_id
        )));
    }
    std::fs::create_dir_all(&data_dir).map_err(|error| {
        CliError::local(format!("cannot create {}: {error}", data_dir.display()))
    })?;

    ctx.out
        .note("probing this machine and testing containment (this can take a few seconds)");
    let containment = bella_contain::native()
        .map_err(|error| CliError::local(format!("containment: {error}")))?;
    let os = bella_probe::os_info();
    let executor = bella_worker::executor_info(&data_dir, containment.as_ref(), &os);
    let inventory = bella_probe::inventory(WORKER_VERSION, vec![executor], None);
    let name = args.name.clone().unwrap_or_else(bella_probe::hostname);
    let fingerprint = enrollment.fingerprint();
    let client = Client::new(enrollment.url(), fingerprint.as_ref(), None)?;
    let request = EnrollRequest {
        code: enrollment.code().to_string(),
        name: name.clone(),
        boot_id: bella_probe::boot_id(),
        inventory,
    };
    let response = runtime()?.block_on(client.enroll(&request))?;
    bella_worker::store_enrollment(
        &data_dir,
        &bella_journal::Enrollment {
            fleet_id: response.fleet_id.clone(),
            machine_id: response.machine_id.clone(),
            controller_url: enrollment.url().to_string(),
            fingerprint: fingerprint.map(|fp| fp.to_string()).unwrap_or_default(),
            policy: response.policy.clone(),
        },
        &response.machine_token,
    )
    .map_err(|error| CliError::local(format!("cannot store the enrollment: {error}")))?;

    let document = json!({
        "machine_id": response.machine_id,
        "fleet_id": response.fleet_id,
        "name": name,
        "controller": enrollment.url().to_string(),
        "profile": response.policy.profile,
        "data_dir": data_dir,
    });
    ctx.out.emit(&document, || {
        format!(
            "enrolled as {name} ({}) in fleet {}, profile {}; start the worker with \
             `bella worker run --data-dir {}`",
            response.machine_id,
            response.fleet_id,
            wire_name(&response.policy.profile),
            data_dir.display()
        )
    });
    Ok(Exit::Success)
}

/// Opens the worker's control connection over [`WorkerSocket`] and bridges
/// it to the channels the daemon expects. Closing either side ends the
/// connection: the socket ending drops the inbound sender, and the daemon
/// dropping either channel closes the socket.
struct SocketConnector {
    url: ControllerUrl,
    fingerprint: Option<Fingerprint>,
    token: String,
}

impl Connector for SocketConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async move {
            let (sink, mut stream) =
                WorkerSocket::connect(&self.url, self.fingerprint.as_ref(), &self.token)
                    .await
                    .map_err(|error| ConnectError::new(describe_connect_error(&error)))?;
            let (to_controller, mut outbound) = mpsc::channel::<WorkerMsg>(CHANNEL_DEPTH);
            let (inbound, from_controller) = mpsc::channel::<ControllerMsg>(CHANNEL_DEPTH);

            let writer = sink.clone();
            tokio::spawn(async move {
                while let Some(message) = outbound.recv().await {
                    if let Err(error) = writer.send(&message).await {
                        tracing::debug!(%error, "the worker socket is closed");
                        break;
                    }
                }
                writer.close().await;
            });
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        item = stream.next() => match item {
                            Some(Ok(message)) => {
                                if inbound.send(message).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(error)) => {
                                tracing::warn!(%error, "the controller connection failed");
                                break;
                            }
                            None => break,
                        },
                        () = inbound.closed() => break,
                    }
                }
                sink.close().await;
            });
            Ok(Connection {
                tx: to_controller,
                rx: from_controller,
            })
        })
    }
}

fn describe_connect_error(error: &ClientError) -> String {
    if error.is_pin_mismatch() {
        "the controller's certificate fingerprint changed: it is not the certificate this \
         machine was enrolled with, so nothing was sent. Enroll again with a new pairing code \
         if the controller was reinstalled on purpose"
            .to_string()
    } else {
        error.to_string()
    }
}

/// The controller's object store, callable from any thread.
///
/// [`HttpObjectStore`] refuses threads that belong to a Tokio runtime. The
/// worker calls its store from plain runner threads, but a call that does
/// come from a runtime thread (a `spawn_blocking` closure) is moved to a
/// scoped plain thread here instead of failing.
struct AnyThreadStore(HttpObjectStore);

impl AnyThreadStore {
    fn off_runtime<T: Send>(&self, call: impl FnOnce(&HttpObjectStore) -> T + Send) -> T {
        if tokio::runtime::Handle::try_current().is_err() {
            return call(&self.0);
        }
        std::thread::scope(|scope| match scope.spawn(|| call(&self.0)).join() {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        })
    }
}

impl ObjectStore for AnyThreadStore {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        self.off_runtime(|store| store.contains(digest))
    }

    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        if tokio::runtime::Handle::try_current().is_err() {
            return self.0.put(reader, expected);
        }
        // The reader cannot cross threads; spool it to a file that can.
        let mut spool = tempfile::tempfile()?;
        std::io::copy(reader, &mut spool)?;
        spool.seek(SeekFrom::Start(0))?;
        self.off_runtime(move |store| store.put(&mut spool, expected))
    }

    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        self.off_runtime(|store| store.open(digest))
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        self.off_runtime(|store| store.size(digest))
    }
}

pub fn run(_ctx: &Ctx, args: &WorkerRunArgs) -> CliResult<Exit> {
    #[cfg(windows)]
    if bella_service::running_as_service() {
        return run_as_windows_service(args.clone());
    }
    let rt = runtime()?;
    let config = worker_config(args, rt.handle().clone())?;
    let work_root = config.work_root.clone();
    rt.block_on(async {
        absorb_ctrl_break();
        eprintln!(
            "worker running (work root {}); Ctrl-C stops it after running work is stopped and \
             cleaned up",
            work_root.display()
        );
        bella_worker::run(config, shutdown_signal())
            .await
            .map_err(|error| CliError::local(format!("the worker stopped: {error}")))
    })?;
    eprintln!("worker stopped");
    Ok(Exit::Success)
}

/// Keep the daemon alive through `CTRL_BREAK_EVENT`.
///
/// On Windows the containment adapter stops a scope politely by attaching
/// to the scope's console and generating `CTRL_BREAK_EVENT` there. While it
/// is attached, this process is on that console too, and an event that
/// Windows delivers to the whole console (which it does for a group id that
/// is not a group leader) would end the daemon through the default handler
/// in the middle of the cleanup it is doing. With a handler installed the
/// event is only logged. Ctrl-C and service stops still shut down
/// gracefully.
#[cfg(windows)]
fn absorb_ctrl_break() {
    match tokio::signal::windows::ctrl_break() {
        Ok(mut events) => {
            tokio::spawn(async move {
                while events.recv().await.is_some() {
                    tracing::debug!("ignored a CTRL_BREAK event");
                }
            });
        }
        Err(error) => tracing::warn!(%error, "cannot install a CTRL_BREAK handler"),
    }
}

#[cfg(not(windows))]
fn absorb_ctrl_break() {}

fn worker_config(args: &WorkerRunArgs, handle: tokio::runtime::Handle) -> CliResult<WorkerConfig> {
    let data_dir = absolute("data-dir", &args.data_dir)?;
    let work_root = match &args.work_root {
        Some(root) => absolute("work-root", root)?,
        None => default_work_root(&data_dir),
    };
    let enrollment = bella_worker::load_enrollment(&data_dir)
        .map_err(|error| CliError::local(error.to_string()))?
        .ok_or_else(|| {
            CliError::local(format!(
                "{} holds no enrollment; run `bella worker enroll <string> --data-dir {}` first",
                data_dir.display(),
                data_dir.display()
            ))
        })?;
    let token = bella_worker::load_machine_token(&data_dir)
        .map_err(|error| CliError::local(error.to_string()))?;
    // The address was checked against the transport rule when the machine
    // enrolled; plain HTTP to another host was an explicit choice then.
    let url = ControllerUrl::parse(&enrollment.controller_url, true)
        .map_err(|error| CliError::local(format!("stored controller address: {error}")))?;
    let fingerprint = if enrollment.fingerprint.is_empty() {
        None
    } else {
        Some(
            Fingerprint::parse(&enrollment.fingerprint)
                .map_err(|error| CliError::local(format!("stored fingerprint: {error}")))?,
        )
    };
    let client = Client::new(&url, fingerprint.as_ref(), Some(token.clone()))?;
    let remote = Arc::new(AnyThreadStore(HttpObjectStore::new(client, handle)));
    let connector = Arc::new(SocketConnector {
        url,
        fingerprint,
        token,
    });
    Ok(WorkerConfig {
        data_dir,
        work_root,
        remote,
        connector,
        containment: None,
        clock: Arc::new(SystemClock),
        tuning: Tuning::default(),
        worker_version: WORKER_VERSION.to_string(),
        hooks: Hooks::default(),
    })
}

/// The worker under the Windows Service Control Manager.
#[cfg(windows)]
fn run_as_windows_service(args: WorkerRunArgs) -> CliResult<Exit> {
    let name = std::env::var(bella_service::ENV_SERVICE_NAME)
        .unwrap_or_else(|_| bella_service::Role::Worker.default_name().to_string());
    bella_service::run_as_service(&name, move |signal| -> Result<(), String> {
        let rt = runtime().map_err(|error| error.message)?;
        let config = worker_config(&args, rt.handle().clone()).map_err(|error| error.message)?;
        rt.block_on(async {
            absorb_ctrl_break();
            let stop = async {
                let _ = tokio::task::spawn_blocking(move || signal.wait()).await;
            };
            bella_worker::run(config, stop)
                .await
                .map_err(|error| format!("the worker stopped: {error}"))
        })
    })
    .map_err(|error| CliError::local(error.to_string()))?;
    Ok(Exit::Success)
}

fn control_text(control: &LocalControl) -> String {
    let boost = match control.boost_until {
        Some(until) if until > UnixMillis::now() => format!("until {}", utc(until)),
        Some(_) => "expired".into(),
        None => "off".into(),
    };
    format!(
        "paused {}, draining {}, boost {boost}",
        control.paused, control.draining
    )
}

fn local_handle(data_dir: &Path) -> CliResult<bella_worker::LocalControlHandle> {
    bella_worker::local_control(data_dir).map_err(|error| {
        CliError::local(format!(
            "cannot open the worker journal in {}: {error} (is the machine enrolled?)",
            data_dir.display()
        ))
    })
}

/// Pause, resume, drain and undrain: a journal setting the running daemon
/// picks up on its next protection pass, with or without a controller.
pub fn local_control(
    ctx: &Ctx,
    args: &DataDirArg,
    change: impl FnOnce(
        &bella_worker::LocalControlHandle,
    ) -> Result<LocalControl, bella_journal::JournalError>,
) -> CliResult<Exit> {
    let handle = local_handle(&args.data_dir)?;
    let control = change(&handle).map_err(|error| CliError::local(error.to_string()))?;
    ctx.out.emit(&control, || control_text(&control));
    Ok(Exit::Success)
}

pub fn boost(ctx: &Ctx, args: &BoostArgs) -> CliResult<Exit> {
    let handle = local_handle(&args.data_dir)?;
    let until = if args.off {
        None
    } else {
        let secs = parse_duration_secs(&args.duration)
            .map_err(|error| CliError::local(format!("--for: {error}")))?;
        Some(UnixMillis(UnixMillis::now().0 + secs * 1000))
    };
    let control = handle
        .set_boost(until)
        .map_err(|error| CliError::local(error.to_string()))?;
    ctx.out.emit(&control, || control_text(&control));
    Ok(Exit::Success)
}

pub fn status(ctx: &Ctx, args: &DataDirArg) -> CliResult<Exit> {
    let enrollment = bella_worker::load_enrollment(&args.data_dir)
        .map_err(|error| CliError::local(error.to_string()))?;
    let Some(enrollment) = enrollment else {
        return Err(CliError::local(format!(
            "{} holds no enrollment",
            args.data_dir.display()
        )));
    };
    let control = local_handle(&args.data_dir)?
        .get()
        .map_err(|error| CliError::local(error.to_string()))?;
    let document = json!({
        "machine_id": enrollment.machine_id,
        "fleet_id": enrollment.fleet_id,
        "controller": enrollment.controller_url,
        "fingerprint": enrollment.fingerprint,
        "policy": enrollment.policy,
        "local_control": control,
    });
    ctx.out.emit(&document, || {
        format!(
            "machine {} in fleet {}\ncontroller {}\nprofile {} (policy revision {})\n{}",
            enrollment.machine_id,
            enrollment.fleet_id,
            enrollment.controller_url,
            wire_name(&enrollment.policy.profile),
            enrollment.policy.revision,
            control_text(&control)
        )
    });
    Ok(Exit::Success)
}

pub fn self_test(ctx: &Ctx, args: &SelfTestArgs) -> CliResult<Exit> {
    let data_dir = absolute("data-dir", &args.data_dir)?;
    let work_root = match &args.work_root {
        Some(root) => absolute("work-root", root)?,
        None => default_work_root(&data_dir),
    };
    std::fs::create_dir_all(&work_root).map_err(|error| {
        CliError::local(format!("cannot create {}: {error}", work_root.display()))
    })?;
    let report = bella_worker::self_test(&work_root, None);
    let passed = report.passed();
    let document = json!({
        "passed": passed,
        "mechanism": report.mechanism,
        "enforcement": report.enforcement.as_ref().ok(),
        "enforcement_error": report.enforcement.as_ref().err(),
        "command": { "ok": report.command.ok, "detail": report.command.detail },
        "hardware": report.hardware,
        "os": report.os,
        "boot_id": report.boot_id,
        "capabilities": report.capabilities,
        "telemetry": report.telemetry,
    });
    ctx.out.emit(&document, || {
        let mut lines = vec![
            format!(
                "{} {} on {} ({} logical CPUs, {} memory)",
                report.os.platform,
                report.os.os_version,
                report.os.hostname,
                report.hardware.logical_cpus,
                crate::output::human_bytes(report.hardware.memory_bytes)
            ),
            format!("containment {}", report.mechanism),
        ];
        match &report.enforcement {
            Ok(enforcement) => {
                lines.push(format!(
                    "  verified {}: cpu {}, memory {}, process tree {}, process count {}, disk {}",
                    enforcement.verified,
                    wire_name(&enforcement.cpu),
                    wire_name(&enforcement.memory),
                    wire_name(&enforcement.process_tree),
                    wire_name(&enforcement.process_count),
                    wire_name(&enforcement.disk)
                ));
                lines.extend(
                    enforcement
                        .notes
                        .iter()
                        .map(|note| format!("  note: {note}")),
                );
            }
            Err(error) => lines.push(format!("  self-test failed: {error}")),
        }
        lines.push(format!(
            "managed command: {} ({})",
            if report.command.ok { "ok" } else { "failed" },
            report.command.detail
        ));
        for capability in &report.capabilities {
            lines.push(format!(
                "capability {} {} ({})",
                capability.name,
                capability.version.as_deref().unwrap_or("-"),
                wire_name(&capability.state)
            ));
        }
        lines.push(if passed {
            "self-test passed".into()
        } else {
            "self-test FAILED: containment is not what the adapter claims".into()
        });
        lines.join("\n")
    });
    Ok(if passed {
        Exit::Success
    } else {
        Exit::JobFailed
    })
}
