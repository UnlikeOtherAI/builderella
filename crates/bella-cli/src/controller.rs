//! `bella controller run` and `bella controller pair`.

use crate::cli::{ControllerRunArgs, PairArgs};
use crate::ctx::{Ctx, runtime, shutdown_signal};
use crate::error::{CliError, CliResult, Exit};
use crate::output::utc;
use bella_controller::{ADMIN_TOKEN_FILE, Controller, ControllerConfig, ControllerHandle, Tls};
use bella_core::api::CreatePairingCodeRequest;
use bella_recipe::units::parse_duration_secs;
use serde_json::json;
use std::path::Path;

fn config(args: &ControllerRunArgs) -> CliResult<ControllerConfig> {
    let data_dir = std::path::absolute(&args.data_dir).map_err(|error| {
        CliError::local(format!("--data-dir {}: {error}", args.data_dir.display()))
    })?;
    let mut config = ControllerConfig::new(data_dir, args.listen);
    config.public_url = args.public_url.clone();
    config.allow_insecure = args.allow_insecure;
    config.tls = if args.insecure_http {
        Tls::Disabled
    } else {
        Tls::SelfSigned {
            subject_alt_names: args.subject_alt_names.clone(),
        }
    };
    Ok(config)
}

/// Tell the operator what is running, and hand out the bootstrap token on
/// the first start only.
fn announce(ctx: &Ctx, handle: &ControllerHandle, args: &ControllerRunArgs) -> CliResult<()> {
    let url = handle.public_url();
    let fingerprint = handle.fingerprint.map(|fp| fp.to_string());
    tracing::info!(addr = %handle.local_addr, %url, "controller listening");
    eprintln!("controller listening on {} ({url})", handle.local_addr);
    match &fingerprint {
        Some(fingerprint) => eprintln!("certificate fingerprint {fingerprint}"),
        None => eprintln!(
            "warning: plain HTTP, no TLS: tokens and source cross the network unencrypted"
        ),
    }
    if bella_controller::dashboard_embedded() {
        eprintln!("dashboard at {url}/");
    } else {
        eprintln!(
            "dashboard: not built into this binary; {url}/ serves a placeholder page \
             (the API and the command line work without it)"
        );
    }
    tracing::info!(
        embedded = bella_controller::dashboard_embedded(),
        "dashboard"
    );
    if let Some(token) = &handle.bootstrap_admin_token {
        let token_file = args.data_dir.join(ADMIN_TOKEN_FILE);
        eprintln!();
        eprintln!("first start: the administrator token is shown once below. Store it safely;");
        eprintln!("anyone who holds it controls this fleet.");
        eprintln!();
        eprintln!("    {token}");
        eprintln!();
        eprintln!(
            "It is also kept, readable by this account only, in {}.",
            token_file.display()
        );
        eprintln!(
            "On this host, `bella login --local --data-dir {}` uses it.",
            args.data_dir.display()
        );
        eprintln!();
    }
    if let Some(path) = &args.ready_file {
        write_ready_file(path, &url, fingerprint.as_deref())?;
    }
    if ctx.out.json {
        ctx.out.print_json(&json!({
            "url": url,
            "fingerprint": fingerprint,
            "listen": handle.local_addr.to_string(),
            "epoch": handle.epoch(),
        }));
    }
    Ok(())
}

/// Write `{"url", "fingerprint"}` so that a script can find the controller.
/// Written to a temporary name and renamed, so a reader never sees half.
fn write_ready_file(path: &Path, url: &str, fingerprint: Option<&str>) -> CliResult<()> {
    let body = serde_json::to_vec_pretty(&json!({ "url": url, "fingerprint": fingerprint }))
        .map_err(|error| CliError::local(error.to_string()))?;
    let partial = path.with_extension("partial");
    std::fs::write(&partial, body)
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|error| {
            CliError::local(format!(
                "cannot write the ready file {}: {error}",
                path.display()
            ))
        })
}

pub fn run(ctx: &Ctx, args: &ControllerRunArgs) -> CliResult<Exit> {
    #[cfg(windows)]
    if bella_service::running_as_service() {
        return run_as_windows_service(args.clone());
    }
    let config = config(args)?;
    runtime()?.block_on(async {
        let handle = Controller::start(config)
            .await
            .map_err(|error| CliError::local(format!("the controller could not start: {error}")))?;
        announce(ctx, &handle, args)?;
        shutdown_signal().await;
        eprintln!("stopping the controller");
        handle.shutdown().await;
        Ok(Exit::Success)
    })
}

/// The controller under the Windows Service Control Manager: no console,
/// the stop request comes as a [`bella_service::ShutdownSignal`].
#[cfg(windows)]
fn run_as_windows_service(args: ControllerRunArgs) -> CliResult<Exit> {
    let name = std::env::var(bella_service::ENV_SERVICE_NAME)
        .unwrap_or_else(|_| bella_service::Role::Controller.default_name().to_string());
    bella_service::run_as_service(&name, move |signal| -> Result<(), String> {
        let config = config(&args).map_err(|error| error.message)?;
        let rt = runtime().map_err(|error| error.message)?;
        rt.block_on(async {
            let handle = Controller::start(config)
                .await
                .map_err(|error| format!("the controller could not start: {error}"))?;
            tracing::info!(
                addr = %handle.local_addr,
                url = %handle.public_url(),
                fingerprint = %handle.fingerprint.map(|fp| fp.to_string()).unwrap_or_default(),
                dashboard_embedded = bella_controller::dashboard_embedded(),
                first_start = handle.bootstrap_admin_token.is_some(),
                "controller running as a service; the admin token, on a first start, is in the \
                 data directory"
            );
            let waiter = tokio::task::spawn_blocking(move || signal.wait());
            let _ = waiter.await;
            handle.shutdown().await;
            Ok::<(), String>(())
        })
    })
    .map_err(|error| CliError::local(error.to_string()))?;
    Ok(Exit::Success)
}

pub fn pair(ctx: &Ctx, args: &PairArgs) -> CliResult<Exit> {
    let ttl = parse_duration_secs(&args.ttl)
        .map_err(|error| CliError::local(format!("--ttl: {error}")))?;
    let client = ctx.blocking()?;
    let response = client.create_pairing_code(&CreatePairingCodeRequest {
        profile: crate::admin::profile(args.profile),
        ttl_secs: Some(ttl),
    })?;
    ctx.out.note(format!(
        "single use, valid until {}. Give it to the new machine over a channel you trust:",
        utc(response.expires_at)
    ));
    ctx.out.emit(
        &json!({ "enrollment": response.enrollment, "expires_at": response.expires_at }),
        || response.enrollment.clone(),
    );
    ctx.out
        .note("on the new machine: bella worker enroll '<the string above>' --data-dir <dir>");
    Ok(Exit::Success)
}
