//! systemd units.
//!
//! # Stopping: `KillMode=mixed`
//!
//! A stop sends `SIGTERM` to the main process only; whatever is still in the
//! unit's cgroup after the main process has exited, or after `TimeoutStopSec`,
//! is killed. The default, `control-group`, would signal every attempt scope
//! at the same moment as the worker and so break its ordered cleanup. With
//! `process`, attempts would outlive a stopped or crashed worker with nobody
//! enforcing their deadlines; v0.1 recovery stops leftover scopes when the
//! worker starts again anyway (their output pipes cannot be recovered), so
//! keeping them alive would gain nothing.
//!
//! # The worker unit
//!
//! `Delegate=yes` hands the unit's cgroup to the worker, which the cgroup v2
//! adapter of `bella-contain` requires. `OOMPolicy=continue` keeps one
//! attempt running out of memory from stopping the whole unit, and
//! `TasksMax=infinity` leaves process limits to the attempt scopes.

use crate::render::Rendered;
use crate::spec::invalid;
use crate::{ENV_SERVICE_NAME, Role, STOP_TIMEOUT, Scope, ServiceError, ServiceSpec};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Seconds between a failure and the restart.
const RESTART_SEC: u64 = 5;

pub(crate) fn unit_name(name: &str) -> String {
    format!("{name}.service")
}

pub(crate) fn render(spec: &ServiceSpec) -> Result<Rendered, ServiceError> {
    let paths = spec.validate(crate::Os::Linux)?;
    // systemd refuses quotes and backslashes in a program path however they are
    // escaped, and would expand `$` in it.
    if paths.executable.contains(['"', '\'', '\\', '$']) {
        return Err(invalid(
            "systemd cannot run a program whose path contains a quote, a backslash or `$`",
        ));
    }
    // systemd reads the rest of the line: surrounding blanks are dropped and a
    // final backslash continues onto the next line.
    let dir = paths.working_dir;
    if dir.trim() != dir || dir.ends_with('\\') {
        return Err(invalid(
            "systemd cannot express a working directory with surrounding spaces or a trailing \
             backslash",
        ));
    }
    match (spec.scope, spec.account.as_deref()) {
        (Scope::User, Some(_)) => {
            return Err(invalid(
                "a unit of a user's own systemd instance always runs as that user; leave the \
                 account unset",
            ));
        }
        (Scope::System, None) if spec.role == Role::Worker => {
            return Err(invalid(
                "a system worker unit needs the account the worker runs as",
            ));
        }
        _ => {}
    }

    let mut unit = String::from("# Written by Builderella. Reinstall the service to change it.\n");
    unit.push_str("[Unit]\n");
    line(&mut unit, "Description", &specifiers(&spec.display_name()));
    if spec.scope == Scope::System {
        unit.push_str("After=network-online.target\nWants=network-online.target\n");
    }

    unit.push_str("\n[Service]\nType=exec\n");
    let mut command = quote(paths.executable);
    for arg in &spec.args {
        command.push(' ');
        // Arguments, unlike the program path, expand `$VARIABLE`.
        command.push_str(&quote(&arg.replace('$', "$$")));
    }
    line(&mut unit, "ExecStart", &command);
    line(&mut unit, "WorkingDirectory", &specifiers(dir));
    let marker = (ENV_SERVICE_NAME, spec.name.as_str());
    let env = spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    for (key, value) in std::iter::once(marker).chain(env) {
        line(&mut unit, "Environment", &quote(&format!("{key}={value}")));
    }
    if let Some(account) = &spec.account {
        line(&mut unit, "User", &specifiers(account));
    }
    let _ = write!(
        unit,
        "Restart=on-failure\nRestartSec={RESTART_SEC}\nKillMode=mixed\nTimeoutStopSec={}\n",
        STOP_TIMEOUT.as_secs()
    );
    if spec.role == Role::Worker {
        unit.push_str("Delegate=yes\nOOMPolicy=continue\nTasksMax=infinity\n");
    }

    let target = match spec.scope {
        Scope::System => "multi-user.target",
        Scope::User => "default.target",
    };
    let _ = write!(unit, "\n[Install]\nWantedBy={target}\n");

    let directory = match spec.scope {
        Scope::System => "/etc/systemd/system",
        Scope::User => "~/.config/systemd/user",
    };
    Ok(Rendered {
        path: PathBuf::from(format!("{directory}/{}", unit_name(&spec.name))),
        contents: unit,
    })
}

fn line(unit: &mut String, key: &str, value: &str) {
    let _ = writeln!(unit, "{key}={value}");
}

/// `%` introduces a specifier in unit files.
fn specifiers(value: &str) -> String {
    value.replace('%', "%%")
}

/// One word of a unit file line, in double quotes with C-style escapes.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', r"\\").replace('"', "\\\"");
    format!("\"{}\"", specifiers(&escaped))
}
