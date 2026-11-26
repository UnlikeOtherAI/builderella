//! Identity and administration: login, tokens, the fleet and queue views,
//! machines, projects and versions.

use crate::cli::{
    CloudArg, LoginArgs, MachineCmd, MachinePolicyArgs, Profile, ProjectCmd, ProjectSetArgs,
    RoleArg, TokenCmd,
};
use crate::ctx::Ctx;
use crate::error::{CliError, CliResult, Exit, wire_name};
use crate::output::{ago, job_line};
use bella_client::BlockingClient;
use bella_core::MachineId;
use bella_core::api::{
    CreateTokenRequest, MachineView, ProjectPolicy, ProjectView, Role, UpdateMachineRequest,
};
use bella_core::job::CloudMode;
use bella_core::machine::MachineProfile;
use bella_core::protocol::PROTOCOL_VERSION;
use bella_net::{ClientCredentials, ControllerUrl, Fingerprint, ServerIdentity};
use bella_recipe::units::{format_cpus, parse_duration_secs};
use serde_json::json;
use std::io::Read;
use std::path::Path;

pub fn profile(profile: Profile) -> MachineProfile {
    match profile {
        Profile::ProtectedWorkstation => MachineProfile::ProtectedWorkstation,
        Profile::DedicatedBuilder => MachineProfile::DedicatedBuilder,
        Profile::PortableBuilder => MachineProfile::PortableBuilder,
        Profile::InfrastructureOnly => MachineProfile::InfrastructureOnly,
    }
}

fn read_token(args: &LoginArgs) -> CliResult<String> {
    let text = if args.token_stdin {
        let mut text = String::new();
        std::io::stdin()
            .take(64 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| {
                CliError::local(format!(
                    "cannot read the token from standard input: {error}"
                ))
            })?;
        text
    } else if let Some(path) = &args.token_file {
        read_token_file(path)?
    } else {
        return Err(CliError::local(
            "give the token with --token-stdin or --token-file; tokens are never taken as \
             arguments, which would leave them in shell history",
        ));
    };
    let token = text.trim().to_string();
    if token.is_empty() {
        return Err(CliError::local("the token is empty"));
    }
    Ok(token)
}

fn read_token_file(path: &Path) -> CliResult<String> {
    let file = bella_net::read_private(path)
        .map_err(|error| CliError::local(format!("cannot read {}: {error}", path.display())))?;
    String::from_utf8(file.bytes)
        .map_err(|_| CliError::local(format!("{} does not hold a token", path.display())))
}

pub fn login(ctx: &Ctx, args: &LoginArgs) -> CliResult<Exit> {
    let credentials = if args.local {
        let Some(data_dir) = &args.data_dir else {
            return Err(CliError::local("--local needs --data-dir"));
        };
        let token = read_token_file(&data_dir.join(bella_controller::ADMIN_TOKEN_FILE))?
            .trim()
            .to_string();
        let identity = ServerIdentity::load(&data_dir.join("tls")).map_err(|error| {
            CliError::local(format!("cannot read the controller's certificate: {error}"))
        })?;
        let fingerprint = identity.map(|identity| identity.fingerprint());
        let url_text = match &args.controller_url {
            Some(url) => url.clone(),
            None => format!(
                "{}://127.0.0.1:{}",
                if fingerprint.is_some() {
                    "https"
                } else {
                    "http"
                },
                bella_core::defaults::CONTROLLER_PORT
            ),
        };
        let url = ControllerUrl::parse(&url_text, args.allow_insecure)
            .map_err(|error| CliError::local(format!("controller URL: {error}")))?;
        let fingerprint = if url.is_tls() { fingerprint } else { None };
        ClientCredentials::new(url, fingerprint, token)
            .map_err(|error| CliError::local(error.to_string()))?
    } else {
        let Some(url_text) = &args.url else {
            return Err(CliError::local("give the controller URL"));
        };
        let url = ControllerUrl::parse(url_text, args.allow_insecure)
            .map_err(|error| CliError::local(format!("controller URL: {error}")))?;
        let fingerprint = match (&args.fingerprint, url.is_tls()) {
            (Some(text), true) => Some(
                Fingerprint::parse(text)
                    .map_err(|error| CliError::local(format!("--fingerprint: {error}")))?,
            ),
            (None, true) => {
                return Err(CliError::local(
                    "an https controller needs --fingerprint: the value `bella controller run` \
                     prints when it starts",
                ));
            }
            (Some(_), false) => {
                return Err(CliError::local(
                    "a plain http controller has no certificate; leave out --fingerprint",
                ));
            }
            (None, false) => None,
        };
        let token = read_token(args)?;
        ClientCredentials::new(url, fingerprint, token)
            .map_err(|error| CliError::local(error.to_string()))?
    };

    let client = BlockingClient::from_credentials(&credentials)?;
    let who = client.whoami()?;
    let path = ctx.credentials_path()?;
    credentials
        .save(&path)
        .map_err(|error| CliError::local(format!("cannot save the credentials: {error}")))?;
    let document = json!({
        "url": credentials.url.to_string(),
        "fingerprint": credentials.fingerprint.map(|fp| fp.to_string()),
        "name": who.name,
        "role": who.role,
        "fleet_id": who.fleet_id,
        "credentials": path,
    });
    ctx.out.emit(&document, || {
        format!(
            "logged in to {} as {} ({}); credentials saved to {}",
            credentials.url,
            who.name,
            wire_name(&who.role),
            path.display()
        )
    });
    Ok(Exit::Success)
}

pub fn token(ctx: &Ctx, command: &TokenCmd) -> CliResult<Exit> {
    let TokenCmd::Create(args) = command;
    let role = match args.role {
        RoleArg::Viewer => Role::Viewer,
        RoleArg::Developer => Role::Developer,
        RoleArg::Admin => Role::Admin,
    };
    let client = ctx.blocking()?;
    let created = client.create_token(&CreateTokenRequest {
        name: args.name.clone(),
        role,
    })?;
    ctx.out.note(format!(
        "token for {} ({}): shown once, store it now",
        args.name,
        wire_name(&role)
    ));
    ctx.out.emit(
        &json!({ "principal_id": created.principal_id, "token": created.token }),
        || created.token.clone(),
    );
    Ok(Exit::Success)
}

fn machine_line(machine: &MachineView) -> String {
    let flags: Vec<String> = machine.flags.iter().map(wire_name).collect();
    let state = if machine.connected {
        "connected"
    } else {
        "disconnected"
    };
    format!(
        "{:<20} {}  {:<12} {:<10} cpu {}/{}  mem {}/{}  {}{}",
        machine.name,
        machine.id,
        state,
        wire_name(&machine.lifecycle),
        format_cpus(machine.reserved.cpu_milli),
        format_cpus(machine.managed_ceiling.cpu_milli),
        crate::output::human_bytes(machine.reserved.memory_bytes),
        crate::output::human_bytes(machine.managed_ceiling.memory_bytes),
        if machine.enforcement.verified {
            format!("enforcement {}", wire_name(&machine.enforcement.memory))
        } else {
            "enforcement unverified".into()
        },
        if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join(", "))
        }
    )
}

pub fn fleet(ctx: &Ctx) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let fleet = client.fleet()?;
    ctx.out.emit(&fleet, || {
        let mut lines = vec![format!(
            "fleet {} (controller {}, epoch {}){}: {} queued, {} running, {} environments",
            fleet.fleet_id,
            fleet.controller_version,
            fleet.controller_epoch,
            if fleet.reconciling {
                ", reconciling"
            } else {
                ""
            },
            fleet.queued,
            fleet.running,
            fleet.environments
        )];
        if fleet.machines.is_empty() {
            lines.push("no machines enrolled".into());
        }
        lines.extend(fleet.machines.iter().map(machine_line));
        lines.join("\n")
    });
    Ok(Exit::Success)
}

pub fn queue(ctx: &Ctx) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let queue = client.queue()?;
    ctx.out.emit(&queue, || {
        let mut lines = Vec::new();
        if queue.entries.is_empty() {
            lines.push("the queue is empty".to_string());
        }
        for entry in &queue.entries {
            let reason = entry
                .job
                .queue_reason
                .as_ref()
                .map(|reason| format!(" - {}", reason.message))
                .unwrap_or_default();
            lines.push(format!(
                "{:>3}. {}{reason}",
                entry.position,
                job_line(&entry.job)
            ));
        }
        if !queue.shares.is_empty() {
            lines.push("projects:".into());
            for share in &queue.shares {
                lines.push(format!(
                    "  {:<20} weight {:<4} queued {:<3} running {:<3} reserved {}, {}",
                    share.name,
                    share.weight,
                    share.queued,
                    share.running,
                    format_cpus(share.reserved.cpu_milli),
                    crate::output::human_bytes(share.reserved.memory_bytes)
                ));
            }
        }
        lines.join("\n")
    });
    Ok(Exit::Success)
}

/// A machine named by id or by its unique display name.
fn resolve_machine(client: &BlockingClient, text: &str) -> CliResult<MachineId> {
    if let Ok(id) = text.parse::<MachineId>() {
        return Ok(id);
    }
    let fleet = client.fleet()?;
    let matches: Vec<&MachineView> = fleet
        .machines
        .iter()
        .filter(|machine| machine.name == text)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(CliError::local(format!("no machine is named `{text}`"))),
        _ => Err(CliError::local(format!(
            "several machines are named `{text}`; use the machine id"
        ))),
    }
}

fn machine_detail(machine: &MachineView) -> String {
    let mut lines = vec![machine_line(machine)];
    let os = &machine.inventory.os;
    lines.push(format!(
        "  {} {} ({}), {} logical CPUs, {} memory",
        os.platform,
        os.os_version,
        os.hostname,
        machine.inventory.hardware.logical_cpus,
        crate::output::human_bytes(machine.inventory.hardware.memory_bytes)
    ));
    let policy = &machine.policy;
    lines.push(format!(
        "  policy revision {}: {}, builds {}, foreground {}% CPU / {}% memory, low priority {}",
        policy.revision,
        wire_name(&policy.profile),
        if policy.accept_builds {
            "accepted"
        } else {
            "refused"
        },
        policy.foreground_cpu_percent,
        policy.foreground_memory_percent,
        policy.low_priority
    ));
    let control = &machine.local_control;
    if control.paused || control.draining || control.boost_until.is_some() {
        lines.push(format!(
            "  local control: paused {}, draining {}, boost {}",
            control.paused,
            control.draining,
            control
                .boost_until
                .map(crate::output::utc)
                .unwrap_or_else(|| "off".into())
        ));
    }
    if let Some(seen) = machine.last_seen_at {
        lines.push(format!("  last seen {}", ago(seen)));
    }
    for attempt in &machine.attempts {
        lines.push(format!(
            "  running {} {} / {} ({}){}",
            attempt.job_id,
            attempt.project_name,
            attempt.recipe,
            wire_name(&attempt.phase),
            if attempt.uncertain { ", uncertain" } else { "" }
        ));
    }
    lines.extend(
        machine
            .enforcement
            .notes
            .iter()
            .map(|note| format!("  note: {note}")),
    );
    lines.join("\n")
}

pub fn machine(ctx: &Ctx, command: &MachineCmd) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let (id, request) = match command {
        MachineCmd::Show(arg) => {
            let id = resolve_machine(&client, &arg.machine)?;
            let view = client.machine(&id)?;
            ctx.out.emit(&view, || machine_detail(&view));
            return Ok(Exit::Success);
        }
        MachineCmd::Rename(args) => (
            resolve_machine(&client, &args.machine)?,
            UpdateMachineRequest {
                name: Some(args.name.clone()),
                ..UpdateMachineRequest::default()
            },
        ),
        MachineCmd::Drain(arg) => (
            resolve_machine(&client, &arg.machine)?,
            UpdateMachineRequest {
                drain: Some(true),
                ..UpdateMachineRequest::default()
            },
        ),
        MachineCmd::Undrain(arg) => (
            resolve_machine(&client, &arg.machine)?,
            UpdateMachineRequest {
                drain: Some(false),
                ..UpdateMachineRequest::default()
            },
        ),
        MachineCmd::Retire(arg) => (
            resolve_machine(&client, &arg.machine)?,
            UpdateMachineRequest {
                retire: Some(true),
                ..UpdateMachineRequest::default()
            },
        ),
        MachineCmd::Policy(args) => {
            let id = resolve_machine(&client, &args.machine)?;
            let current = client.machine(&id)?;
            let policy = patched_policy(current.policy, args)?;
            (
                id,
                UpdateMachineRequest {
                    policy: Some(policy),
                    ..UpdateMachineRequest::default()
                },
            )
        }
    };
    let view = client.update_machine(&id, &request)?;
    ctx.out.emit(&view, || machine_detail(&view));
    Ok(Exit::Success)
}

fn patched_policy(
    mut policy: bella_core::machine::MachinePolicy,
    args: &MachinePolicyArgs,
) -> CliResult<bella_core::machine::MachinePolicy> {
    if let Some(value) = args.profile {
        policy.profile = profile(value);
    }
    if let Some(value) = args.accept_builds {
        policy.accept_builds = value;
    }
    for (flag, value) in [
        ("foreground-cpu", args.foreground_cpu),
        ("foreground-memory", args.foreground_memory),
    ] {
        if value.is_some_and(|percent| percent > 100) {
            return Err(CliError::local(format!(
                "--{flag} is a percentage up to 100"
            )));
        }
    }
    if let Some(value) = args.foreground_cpu {
        policy.foreground_cpu_percent = value;
    }
    if let Some(value) = args.foreground_memory {
        policy.foreground_memory_percent = value;
    }
    if let Some(value) = args.max_concurrent {
        policy.max_concurrent_attempts = (value > 0).then_some(value);
    }
    if let Some(value) = args.low_priority {
        policy.low_priority = value;
    }
    if let Some(text) = &args.max_disconnected {
        policy.max_disconnected_secs = parse_duration_secs(text)
            .map_err(|error| CliError::local(format!("--max-disconnected: {error}")))?;
    }
    Ok(policy)
}

fn project_text(project: &ProjectView) -> String {
    let policy = &project.policy;
    format!(
        "{:<24} {}  weight {}, concurrent {}, environments {}, cloud {}, lan-only {}, urgent {}",
        project.name,
        project.id,
        policy.weight,
        policy
            .max_concurrent_attempts
            .map_or("unlimited".into(), |n| n.to_string()),
        policy
            .max_environments
            .map_or("unlimited".into(), |n| n.to_string()),
        wire_name(&policy.cloud),
        policy.source_lan_only,
        if policy.allow_urgent {
            "allowed"
        } else {
            "refused"
        }
    )
}

pub fn project(ctx: &Ctx, command: &ProjectCmd) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    match command {
        ProjectCmd::List => {
            let projects = client.projects()?;
            ctx.out.emit(&projects, || {
                if projects.is_empty() {
                    return "no projects".into();
                }
                projects
                    .iter()
                    .map(project_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        ProjectCmd::Show(arg) => {
            let project = client
                .projects()?
                .into_iter()
                .find(|project| project.name == arg.name)
                .ok_or_else(|| CliError::local(format!("no project is named `{}`", arg.name)))?;
            ctx.out.emit(&project, || project_text(&project));
        }
        ProjectCmd::Set(args) => {
            let current = client
                .projects()?
                .into_iter()
                .find(|project| project.name == args.name)
                .map(|project| project.policy)
                .unwrap_or_default();
            let policy = patched_project(current, args);
            let project = client.put_project(&args.name, &policy)?;
            ctx.out.emit(&project, || project_text(&project));
        }
    }
    Ok(Exit::Success)
}

fn patched_project(mut policy: ProjectPolicy, args: &ProjectSetArgs) -> ProjectPolicy {
    if let Some(weight) = args.weight {
        policy.weight = weight;
    }
    if let Some(value) = args.max_concurrent {
        policy.max_concurrent_attempts = (value > 0).then_some(value);
    }
    if let Some(value) = args.max_environments {
        policy.max_environments = (value > 0).then_some(value);
    }
    if let Some(cloud) = args.cloud {
        policy.cloud = match cloud {
            CloudArg::ExistingOnly => CloudMode::ExistingOnly,
            CloudArg::Manual => CloudMode::Manual,
            CloudArg::AutoOverflow => CloudMode::AutoOverflow,
            CloudArg::Hybrid => CloudMode::Hybrid,
        };
    }
    if let Some(value) = args.lan_only {
        policy.source_lan_only = value;
    }
    if let Some(value) = args.allow_urgent {
        policy.allow_urgent = value;
    }
    policy
}

pub fn version(ctx: &Ctx) -> CliResult<Exit> {
    let binary = env!("CARGO_PKG_VERSION");
    let controller = match ctx.try_credentials() {
        Ok(Some(credentials)) => match BlockingClient::from_credentials(&credentials)
            .and_then(|client| client.version())
        {
            Ok(version) => Some(version),
            Err(error) => {
                let error = CliError::client(error);
                ctx.out
                    .note(format!("could not ask the controller: {}", error.message));
                Some(json!({ "error": error.message }))
            }
        },
        Ok(None) => None,
        Err(error) => {
            ctx.out.note(format!("credentials: {}", error.message));
            None
        }
    };
    let document = json!({
        "version": binary,
        "protocol_version": PROTOCOL_VERSION,
        "controller": controller,
    });
    ctx.out.emit(&document, || {
        let mut lines = vec![format!("bella {binary} (protocol {PROTOCOL_VERSION})")];
        if let Some(controller) = &controller
            && let Some(version) = controller.get("version").and_then(|v| v.as_str())
        {
            lines.push(format!(
                "controller {version} (protocol {})",
                controller
                    .get("protocol_version")
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            ));
        }
        lines.join("\n")
    });
    Ok(Exit::Success)
}
