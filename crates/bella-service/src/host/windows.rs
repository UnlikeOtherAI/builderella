//! Managing services through the Service Control Manager.
//!
//! The environment lives in the service's registry key, where the Service
//! Control Manager reads it when it starts the process. Account rights are not
//! changed: an ordinary account needs the "Log on as a service" right, which
//! an administrator grants in the local security policy.

use super::Backend;
use crate::render::windows::{
    Entry, FAILURE_RESET, RESTART_DELAYS, entry, is_passwordless, registry_key,
};
use crate::service_main::describe;
use crate::{
    ENV_SERVICE_NAME, InstallOptions, InstallOutcome, Installed, STOP_TIMEOUT, Scope, ServiceError,
    ServiceId, ServiceSpec, ServiceStatus,
};
use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceExitCode,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};

const ACCESS_DENIED: i32 = 5;
const ALREADY_RUNNING: i32 = 1056;
const DOES_NOT_EXIST: i32 = 1060;
const NOT_ACTIVE: i32 = 1062;
const LOGON_FAILED: i32 = 1069;
const MARKED_FOR_DELETE: i32 = 1072;
const NEVER_STARTED: u32 = 1077;

const START_WAIT: Duration = Duration::from_secs(60);
/// Extra wait after the service's own stop timeout.
const STOP_WAIT_MARGIN: Duration = Duration::from_secs(30);
const DELETE_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(200);

pub(crate) struct Scm;

fn code(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn scm_error(action: impl Into<String>, error: &windows_service::Error) -> ServiceError {
    let action = action.into();
    if code(error) == Some(ACCESS_DENIED) {
        needs_admin(action)
    } else {
        ServiceError::Manager {
            action,
            reason: describe(error),
        }
    }
}

fn needs_admin(action: String) -> ServiceError {
    ServiceError::NeedsPrivilege {
        action,
        hint: "needs administrator rights; run the command from an elevated prompt",
    }
}

fn registry_error(action: &str, error: io::Error) -> ServiceError {
    if error.raw_os_error() == Some(ACCESS_DENIED) {
        needs_admin(action.to_owned())
    } else {
        ServiceError::Manager {
            action: action.to_owned(),
            reason: error.to_string(),
        }
    }
}

fn system_scope(id: &ServiceId) -> Result<(), ServiceError> {
    if id.scope == Scope::System {
        Ok(())
    } else {
        Err(ServiceError::Unsupported(
            "Windows services exist only in the system scope".into(),
        ))
    }
}

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, ServiceError> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| scm_error("open the service manager", &e))
}

/// Open a service; `None` when there is none.
fn open(name: &str, access: ServiceAccess) -> Result<Option<Service>, ServiceError> {
    match manager(ServiceManagerAccess::CONNECT)?.open_service(name, access) {
        Ok(service) => Ok(Some(service)),
        Err(error) if code(&error) == Some(DOES_NOT_EXIST) => Ok(None),
        Err(error) => Err(scm_error(format!("open service `{name}`"), &error)),
    }
}

fn require(name: &str, access: ServiceAccess) -> Result<Service, ServiceError> {
    open(name, access)?.ok_or_else(|| ServiceError::NotInstalled {
        name: name.to_owned(),
    })
}

fn service_key(name: &str, access: u32) -> io::Result<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"SYSTEM\CurrentControlSet\Services\{name}"), access)
}

/// A registry value of the service, empty when absent.
fn value<T: winreg::types::FromRegValue + Default>(
    name: &str,
    value: &str,
) -> Result<T, ServiceError> {
    let read = service_key(name, KEY_QUERY_VALUE).and_then(|key| key.get_value(value));
    match read {
        Ok(found) => Ok(found),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(registry_error("read the service's registry key", error)),
    }
}

fn require_managed(name: &str) -> Result<(), ServiceError> {
    let environment: Vec<String> = value(name, "Environment")?;
    let marker = format!("{ENV_SERVICE_NAME}=");
    if environment.iter().any(|v| v.starts_with(&marker)) {
        Ok(())
    } else {
        Err(ServiceError::NotManaged {
            name: name.to_owned(),
        })
    }
}

fn recovery() -> ServiceFailureActions {
    let actions = RESTART_DELAYS
        .iter()
        .map(|delay| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: *delay,
        })
        .collect();
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET),
        reboot_msg: None,
        command: None,
        actions: Some(actions),
    }
}

/// What differs between the installed service and the wanted entry.
fn differences(
    name: &str,
    service: &Service,
    wanted: &Entry,
) -> Result<Vec<&'static str>, ServiceError> {
    let query = |e| scm_error(format!("read the configuration of `{name}`"), &e);
    let config = service.query_config().map_err(query)?;
    let actions = service.get_failure_actions().map_err(query)?;
    let on_exit_code = service
        .get_failure_actions_on_non_crash_failures()
        .map_err(query)?;
    let description: String = value(name, "Description")?;
    let mut environment: Vec<String> = value(name, "Environment")?;
    environment.retain(|v| !v.is_empty());
    let account = config.account_name.unwrap_or_default();
    let expected = recovery();

    let mut found = Vec::new();
    let mut differ = |same: bool, what| {
        if !same {
            found.push(what);
        }
    };
    differ(
        config.executable_path.as_os_str() == wanted.command_line.as_str(),
        "command line",
    );
    differ(
        account.to_string_lossy().to_lowercase() == wanted.account.to_lowercase(),
        "account",
    );
    differ(
        config.display_name == wanted.display_name.as_str(),
        "display name",
    );
    differ(
        config.start_type == ServiceStartType::AutoStart,
        "start type",
    );
    differ(environment == wanted.environment, "environment");
    differ(description == wanted.description, "description");
    differ(
        on_exit_code
            && actions.reset_period == expected.reset_period
            && actions.actions == expected.actions,
        "recovery actions",
    );
    Ok(found)
}

/// Set everything `CreateService` cannot. The environment, which carries the
/// ownership marker, goes first.
fn configure(name: &str, service: &Service, wanted: &Entry) -> Result<(), ServiceError> {
    service_key(name, KEY_SET_VALUE)
        .and_then(|key| key.set_value("Environment", &wanted.environment))
        .map_err(|e| registry_error("write the service's environment", e))?;
    let set = |e| scm_error(format!("configure `{name}`"), &e);
    service.update_failure_actions(recovery()).map_err(set)?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(set)?;
    service.set_description(&wanted.description).map_err(set)
}

fn state(name: &str, service: &Service) -> Result<ServiceState, ServiceError> {
    service
        .query_status()
        .map(|status| status.current_state)
        .map_err(|e| scm_error(format!("query `{name}`"), &e))
}

fn start_and_wait(name: &str, service: &Service) -> Result<(), ServiceError> {
    match service.start::<OsString>(&[]) {
        Ok(()) => {}
        Err(error) if code(&error) == Some(ALREADY_RUNNING) => {}
        Err(error) if code(&error) == Some(ACCESS_DENIED) => {
            return Err(needs_admin(format!("start `{name}`")));
        }
        Err(error) => {
            let mut reason = describe(&error);
            if code(&error) == Some(LOGON_FAILED) {
                reason.push_str(
                    " Check the account's password, and that it has the \"Log on as a service\" \
                     right (Local Security Policy, User Rights Assignment).",
                );
            }
            return Err(ServiceError::StartFailed {
                name: name.to_owned(),
                reason,
            });
        }
    }
    let deadline = Instant::now() + START_WAIT;
    loop {
        match state(name, service)? {
            ServiceState::Running => return Ok(()),
            ServiceState::Stopped => {
                return Err(ServiceError::StartFailed {
                    name: name.to_owned(),
                    reason: "it stopped right after starting; see its log files".into(),
                });
            }
            _ if Instant::now() >= deadline => {
                return Err(ServiceError::Timeout {
                    name: name.to_owned(),
                    action: "start",
                    after: START_WAIT,
                });
            }
            _ => std::thread::sleep(POLL),
        }
    }
}

fn stop_and_wait(name: &str, service: &Service) -> Result<(), ServiceError> {
    let limit = STOP_TIMEOUT + STOP_WAIT_MARGIN;
    let deadline = Instant::now() + limit;
    loop {
        match state(name, service)? {
            ServiceState::Stopped => return Ok(()),
            ServiceState::Running | ServiceState::Paused => match service.stop() {
                Ok(_) => {}
                Err(error) if code(&error) == Some(NOT_ACTIVE) => {}
                Err(error) => return Err(scm_error(format!("stop `{name}`"), &error)),
            },
            // A pending state accepts no control; wait for it to settle.
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::Timeout {
                name: name.to_owned(),
                action: "stop",
                after: limit,
            });
        }
        std::thread::sleep(POLL);
    }
}

impl Backend for Scm {
    fn install(
        &self,
        spec: &ServiceSpec,
        options: &InstallOptions,
    ) -> Result<Installed, ServiceError> {
        let wanted = entry(spec)?;
        let name = spec.name.as_str();
        // Reading needs no elevation, so an unchanged service is recognised
        // without it.
        let read = ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS;
        let outcome = match open(name, read)? {
            None => InstallOutcome::Created,
            Some(service) => {
                let found = differences(name, &service, &wanted)?;
                if found.is_empty() {
                    InstallOutcome::Unchanged
                } else {
                    require_managed(name)?;
                    if !options.replace {
                        return Err(ServiceError::AlreadyInstalled {
                            name: name.to_owned(),
                            differences: found.join(", "),
                        });
                    }
                    InstallOutcome::Replaced
                }
            }
        };
        if outcome != InstallOutcome::Unchanged
            && options.password.is_none()
            && !is_passwordless(&wanted.account)
        {
            return Err(ServiceError::PasswordRequired {
                account: wanted.account,
            });
        }
        for directory in [&spec.working_dir, &spec.log_dir] {
            std::fs::create_dir_all(directory).map_err(|source| ServiceError::Io {
                action: "create",
                path: directory.clone(),
                source,
            })?;
        }

        let info = ServiceInfo {
            name: name.into(),
            display_name: wanted.display_name.as_str().into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: spec.executable.clone(),
            launch_arguments: spec.args.iter().map(OsString::from).collect(),
            dependencies: Vec::new(),
            account_name: Some(wanted.account.as_str().into()),
            account_password: options
                .password
                .as_ref()
                .map(|password| password.expose().into()),
        };
        // Restart actions can only be set through a handle that may start.
        let write = ServiceAccess::QUERY_CONFIG
            | ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::DELETE;
        let service = match outcome {
            InstallOutcome::Created => {
                let service =
                    manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?
                        .create_service(&info, write)
                        .map_err(|e| scm_error(format!("create service `{name}`"), &e))?;
                if let Err(error) = configure(name, &service, &wanted) {
                    // A half-configured service must not stay behind. The
                    // configuration error is the one worth reporting.
                    let _ = service.delete();
                    return Err(error);
                }
                Some(service)
            }
            InstallOutcome::Replaced => {
                let service = require(name, write)?;
                stop_and_wait(name, &service)?;
                service
                    .change_config(&info)
                    .map_err(|e| scm_error(format!("change service `{name}`"), &e))?;
                configure(name, &service, &wanted)?;
                Some(service)
            }
            InstallOutcome::Unchanged => None,
        };
        if options.start {
            // An unchanged, running service is left as it is, which needs no
            // elevation.
            let query = require(name, ServiceAccess::QUERY_STATUS)?;
            if state(name, &query)? != ServiceState::Running {
                let service = match service {
                    Some(service) => service,
                    None => require(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?,
                };
                start_and_wait(name, &service)?;
            }
        }
        let id = spec.id();
        Ok(Installed {
            status: self.status(&id)?,
            name: id.name,
            definition: registry_key(name),
            outcome,
            warnings: Vec::new(),
        })
    }

    fn uninstall(&self, id: &ServiceId) -> Result<bool, ServiceError> {
        system_scope(id)?;
        let name = id.name.as_str();
        if open(name, ServiceAccess::QUERY_STATUS)?.is_none() {
            return Ok(false);
        }
        require_managed(name)?;
        {
            let service = require(
                name,
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
            )?;
            stop_and_wait(name, &service)?;
            match service.delete() {
                Ok(()) => {}
                Err(error) if code(&error) == Some(MARKED_FOR_DELETE) => {}
                Err(error) => return Err(scm_error(format!("delete `{name}`"), &error)),
            }
            // The entry goes away only once every handle is closed.
        }
        let deadline = Instant::now() + DELETE_WAIT;
        loop {
            match open(name, ServiceAccess::QUERY_STATUS) {
                Ok(None) => return Ok(true),
                Ok(Some(_)) => {}
                Err(ServiceError::Manager { .. }) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(ServiceError::Timeout {
                    name: name.to_owned(),
                    action: "disappear (another program, such as the Services console, holds it \
                             open)",
                    after: DELETE_WAIT,
                });
            }
            std::thread::sleep(POLL);
        }
    }

    fn status(&self, id: &ServiceId) -> Result<ServiceStatus, ServiceError> {
        system_scope(id)?;
        let Some(service) = open(&id.name, ServiceAccess::QUERY_STATUS)? else {
            return Ok(ServiceStatus::NotInstalled);
        };
        let status = service
            .query_status()
            .map_err(|e| scm_error(format!("query `{}`", id.name), &e))?;
        Ok(match (status.current_state, status.exit_code) {
            (ServiceState::Stopped, ServiceExitCode::Win32(0 | NEVER_STARTED)) => {
                ServiceStatus::Stopped
            }
            (ServiceState::Stopped, ServiceExitCode::Win32(error)) => ServiceStatus::Failed {
                reason: format!(
                    "stopped with Windows error {error}: {}",
                    io::Error::from_raw_os_error(error as i32)
                ),
            },
            (ServiceState::Stopped, ServiceExitCode::ServiceSpecific(error)) => {
                ServiceStatus::Failed {
                    reason: format!("stopped with service exit code {error}"),
                }
            }
            _ => ServiceStatus::Running {
                pid: status.process_id.filter(|pid| *pid != 0),
            },
        })
    }

    fn start(&self, id: &ServiceId) -> Result<(), ServiceError> {
        system_scope(id)?;
        let name = id.name.as_str();
        require(name, ServiceAccess::QUERY_STATUS)?;
        require_managed(name)?;
        let service = require(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
        start_and_wait(name, &service)
    }

    fn stop(&self, id: &ServiceId) -> Result<(), ServiceError> {
        system_scope(id)?;
        let name = id.name.as_str();
        require(name, ServiceAccess::QUERY_STATUS)?;
        require_managed(name)?;
        let service = require(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
        stop_and_wait(name, &service)
    }
}
