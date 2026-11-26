//! `bella controller service …` and `bella worker service …`: the
//! operating-system service that runs `bella controller run` or
//! `bella worker run` with the same flags.

use crate::cli::{
    ControllerRunArgs, ControllerServiceCmd, InstallFlags, ServiceIdArgs, WorkerRunArgs,
    WorkerServiceCmd,
};
use crate::ctx::Ctx;
use crate::error::{CliError, CliResult, Exit};
use bella_service::{InstallOptions, Os, Role, Scope, ServiceId, ServiceSpec};
use serde_json::json;
use std::path::{Path, PathBuf};

fn absolute(path: &Path) -> CliResult<String> {
    std::path::absolute(path)
        .map(|path| path.display().to_string())
        .map_err(|error| CliError::local(format!("{}: {error}", path.display())))
}

fn controller_args(run: &ControllerRunArgs) -> CliResult<Vec<String>> {
    let mut args = vec![
        "controller".to_string(),
        "run".into(),
        "--data-dir".into(),
        absolute(&run.data_dir)?,
        "--listen".into(),
        run.listen.to_string(),
    ];
    if let Some(url) = &run.public_url {
        args.extend(["--public-url".into(), url.clone()]);
    }
    for name in &run.subject_alt_names {
        args.extend(["--san".into(), name.clone()]);
    }
    if run.insecure_http {
        args.push("--insecure-http".into());
    }
    if run.allow_insecure {
        args.push("--allow-insecure".into());
    }
    if let Some(file) = &run.ready_file {
        args.extend(["--ready-file".into(), absolute(file)?]);
    }
    Ok(args)
}

fn worker_args(run: &WorkerRunArgs) -> CliResult<Vec<String>> {
    let mut args = vec![
        "worker".to_string(),
        "run".into(),
        "--data-dir".into(),
        absolute(&run.data_dir)?,
    ];
    if let Some(root) = &run.work_root {
        args.extend(["--work-root".into(), absolute(root)?]);
    }
    Ok(args)
}

fn id(role: Role, service: &ServiceIdArgs) -> ServiceId {
    let scope = if service.user {
        Scope::User
    } else {
        Scope::System
    };
    match &service.name {
        Some(name) => ServiceId::new(name.clone(), scope),
        None => ServiceId::for_role(role, scope),
    }
}

fn host_os() -> CliResult<Os> {
    bella_core::platform::Platform::current()
        .map(|platform| platform.os)
        .ok_or_else(|| CliError::local("services are not supported on this platform"))
}

fn spec(
    role: Role,
    service: &ServiceIdArgs,
    install: &InstallFlags,
    data_dir: &Path,
    args: Vec<String>,
) -> CliResult<ServiceSpec> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| CliError::local(format!("cannot find this program's path: {error}")))?;
    let data_dir = PathBuf::from(absolute(data_dir)?);
    let log_dir = match &install.log_dir {
        Some(dir) => PathBuf::from(absolute(dir)?),
        None => data_dir.join("logs"),
    };
    let id = id(role, service);
    let mut spec = ServiceSpec::new(role, id.scope, executable, data_dir, log_dir);
    spec.name = id.name;
    spec.args = args;
    spec.account = install.account.clone();
    Ok(spec)
}

fn install(ctx: &Ctx, spec: &ServiceSpec, install: &InstallFlags) -> CliResult<Exit> {
    let os = host_os()?;
    if install.print {
        let rendered =
            bella_service::render(spec, os).map_err(|error| CliError::local(error.to_string()))?;
        ctx.out.emit(
            &json!({ "path": rendered.path, "contents": rendered.contents }),
            || format!("# {}\n{}", rendered.path.display(), rendered.contents),
        );
        return Ok(Exit::Success);
    }
    let password = password(spec, os)?;
    if os == Os::Windows && spec.role == Role::Worker && spec.account.is_some() {
        ctx.out.note(
            "note: the account needs the \"Log on as a service\" right, which this command does \
             not grant. Grant it in Local Security Policy (secpol.msc → Local Policies → User \
             Rights Assignment → Log on as a service) if the service fails to start.",
        );
    }
    let installed = bella_service::install(
        spec,
        &InstallOptions {
            replace: install.replace,
            start: !install.no_start,
            password,
        },
    )
    .map_err(|error| CliError::local(error.to_string()))?;
    for warning in &installed.warnings {
        ctx.out.note(format!("warning: {warning}"));
    }
    let outcome = format!("{:?}", installed.outcome).to_lowercase();
    ctx.out.emit(
        &json!({
            "name": installed.name,
            "definition": installed.definition,
            "outcome": outcome,
            "status": installed.status.to_string(),
        }),
        || {
            format!(
                "service {} {outcome} ({}); {}",
                installed.name,
                installed.definition.display(),
                installed.status
            )
        },
    );
    Ok(Exit::Success)
}

/// The Windows account password, asked for on the terminal without echo.
/// Never taken as an argument.
#[cfg(windows)]
fn password(spec: &ServiceSpec, os: Os) -> CliResult<Option<bella_service::Password>> {
    let ordinary = spec
        .account
        .as_deref()
        .is_some_and(|account| !account.eq_ignore_ascii_case("LocalSystem"));
    if os != Os::Windows || !ordinary {
        return Ok(None);
    }
    let prompt = format!(
        "password of {} (passed to the Service Control Manager only): ",
        spec.account.as_deref().unwrap_or_default()
    );
    let password = rpassword::prompt_password(prompt)
        .map_err(|error| CliError::local(format!("cannot read the password: {error}")))?;
    Ok(Some(bella_service::Password::new(password)))
}

#[cfg(not(windows))]
fn password(_spec: &ServiceSpec, _os: Os) -> CliResult<Option<bella_service::Password>> {
    Ok(None)
}

fn manage(ctx: &Ctx, id: ServiceId, action: &str) -> CliResult<Exit> {
    let failed = |error: bella_service::ServiceError| CliError::local(error.to_string());
    match action {
        "uninstall" => {
            let removed = bella_service::uninstall(&id).map_err(failed)?;
            ctx.out
                .emit(&json!({ "name": id.name, "removed": removed }), || {
                    if removed {
                        format!("service {} removed; logs and data are kept", id.name)
                    } else {
                        format!("no service named {} is installed", id.name)
                    }
                });
        }
        "start" => {
            bella_service::start(&id).map_err(failed)?;
            ctx.out
                .emit(&json!({ "name": id.name, "started": true }), || {
                    format!("service {} started", id.name)
                });
        }
        "stop" => {
            bella_service::stop(&id).map_err(failed)?;
            ctx.out
                .emit(&json!({ "name": id.name, "stopped": true }), || {
                    format!("service {} stopped", id.name)
                });
        }
        _ => {
            let status = bella_service::status(&id).map_err(failed)?;
            ctx.out.emit(
                &json!({ "name": id.name, "status": status.to_string() }),
                || format!("service {}: {status}", id.name),
            );
        }
    }
    Ok(Exit::Success)
}

pub fn controller(ctx: &Ctx, command: &ControllerServiceCmd) -> CliResult<Exit> {
    let role = Role::Controller;
    match command {
        ControllerServiceCmd::Install(args) => {
            let spec = spec(
                role,
                &args.service,
                &args.install,
                &args.run.data_dir,
                controller_args(&args.run)?,
            )?;
            install(ctx, &spec, &args.install)
        }
        ControllerServiceCmd::Uninstall(service) => manage(ctx, id(role, service), "uninstall"),
        ControllerServiceCmd::Status(service) => manage(ctx, id(role, service), "status"),
        ControllerServiceCmd::Start(service) => manage(ctx, id(role, service), "start"),
        ControllerServiceCmd::Stop(service) => manage(ctx, id(role, service), "stop"),
    }
}

pub fn worker(ctx: &Ctx, command: &WorkerServiceCmd) -> CliResult<Exit> {
    let role = Role::Worker;
    match command {
        WorkerServiceCmd::Install(args) => {
            let spec = spec(
                role,
                &args.service,
                &args.install,
                &args.run.data_dir,
                worker_args(&args.run)?,
            )?;
            install(ctx, &spec, &args.install)
        }
        WorkerServiceCmd::Uninstall(service) => manage(ctx, id(role, service), "uninstall"),
        WorkerServiceCmd::Status(service) => manage(ctx, id(role, service), "status"),
        WorkerServiceCmd::Start(service) => manage(ctx, id(role, service), "start"),
        WorkerServiceCmd::Stop(service) => manage(ctx, id(role, service), "stop"),
    }
}
