//! Windows service entries.
//!
//! The worker is a real service, in session 0 on every start. Its job objects
//! live in the session namespace, so this is what lets a restarted worker find
//! them again, and a service has no console, which `bella-contain` needs to
//! deliver a polite stop to a scope.

use crate::render::Rendered;
use crate::spec::invalid;
use crate::{
    ENV_SERVICE_LOG_DIR, ENV_SERVICE_NAME, ENV_SERVICE_WORKING_DIR, Role, STOP_TIMEOUT, Scope,
    ServiceError, ServiceSpec,
};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const LOCAL_SYSTEM: &str = "LocalSystem";
/// Delays before the first, second and every later restart.
pub(crate) const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Quiet time after which the failure count starts again.
pub(crate) const FAILURE_RESET: Duration = Duration::from_secs(24 * 60 * 60);
/// Longest command line the Service Control Manager can start.
const MAX_COMMAND_LINE: usize = 32_767;

/// The entry the Service Control Manager and the registry should hold.
#[derive(Debug)]
pub(crate) struct Entry {
    pub display_name: String,
    pub description: String,
    pub command_line: String,
    pub account: String,
    /// `KEY=value`, the service's registry `Environment`.
    pub environment: Vec<String>,
}

/// Whether the account starts without a password: built-in accounts, virtual
/// service accounts and group managed service accounts.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_passwordless(account: &str) -> bool {
    let lower = account.to_ascii_lowercase();
    lower == "localsystem"
        || lower.starts_with(r"nt authority\")
        || lower.starts_with(r"nt service\")
        || lower.ends_with('$')
}

/// The name in the form the Service Control Manager expects.
fn account_name(account: &str) -> String {
    match account.to_ascii_lowercase().as_str() {
        "localsystem" | "system" | r"nt authority\system" | r".\localsystem" => LOCAL_SYSTEM.into(),
        "localservice" | r"nt authority\localservice" => r"NT AUTHORITY\LocalService".into(),
        "networkservice" | r"nt authority\networkservice" => r"NT AUTHORITY\NetworkService".into(),
        _ if account.contains(['\\', '@']) => account.into(),
        _ => format!(r".\{account}"),
    }
}

pub(crate) fn entry(spec: &ServiceSpec) -> Result<Entry, ServiceError> {
    let paths = spec.validate(crate::Os::Windows)?;
    if spec.scope == Scope::User {
        return Err(ServiceError::Unsupported(
            "Windows services are machine-wide; use the system scope with the account the \
             service should run as"
                .into(),
        ));
    }
    let account = match (&spec.account, spec.role) {
        (Some(account), _) => account_name(account),
        (None, Role::Controller) => LOCAL_SYSTEM.into(),
        (None, Role::Worker) => {
            return Err(invalid(
                "a Windows worker service needs the account it runs as, which must own the work \
                 root and the toolchains",
            ));
        }
    };
    if paths.executable.contains('"') {
        return Err(invalid("a Windows path cannot contain a double quote"));
    }
    let mut command_line = quote(paths.executable);
    for arg in &spec.args {
        command_line.push(' ');
        command_line.push_str(&quote(arg));
    }
    if command_line.encode_utf16().count() >= MAX_COMMAND_LINE {
        return Err(invalid("the command line is too long for Windows"));
    }
    let mut environment = vec![
        format!("{ENV_SERVICE_NAME}={}", spec.name),
        format!("{ENV_SERVICE_LOG_DIR}={}", paths.log_dir),
        format!("{ENV_SERVICE_WORKING_DIR}={}", paths.working_dir),
    ];
    environment.extend(spec.env.iter().map(|(k, v)| format!("{k}={v}")));
    Ok(Entry {
        display_name: spec.display_name(),
        description: format!(
            "Builderella {}: part of the Builderella build and test fleet.",
            spec.role.as_str()
        ),
        command_line,
        account,
        environment,
    })
}

pub(crate) fn registry_key(name: &str) -> PathBuf {
    PathBuf::from(format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{name}"))
}

pub(crate) fn render(spec: &ServiceSpec) -> Result<Rendered, ServiceError> {
    let entry = entry(spec)?;
    let [first, second, later] = RESTART_DELAYS.map(|delay| delay.as_secs());
    let mut text = format!(
        "service: {}\ndisplay name: {}\ndescription: {}\nstart: automatic\ncommand: {}\n\
         account: {}\non failure: restart after {first} s, then {second} s, then every {later} s; \
         the count resets after {} s\nstop timeout: {} s\nenvironment:\n",
        spec.name,
        entry.display_name,
        entry.description,
        entry.command_line,
        entry.account,
        FAILURE_RESET.as_secs(),
        STOP_TIMEOUT.as_secs()
    );
    for variable in &entry.environment {
        let _ = writeln!(text, "  {variable}");
    }
    Ok(Rendered {
        path: registry_key(&spec.name),
        contents: text,
    })
}

/// Quote one argument for the C runtime's command-line rules. It matches the
/// quoting `windows-service` applies on installation, so an installed command
/// line can be compared with a rendered one.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '"', '\t']) {
        return arg.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, and the quote escaped.
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}
