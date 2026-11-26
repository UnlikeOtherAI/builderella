//! Managing jobs with `launchctl`.
//!
//! Stopping a job unloads it (`bootout`), because a loaded job can be started
//! again by launchd at any time. The property list stays, so it is loaded
//! again at the next boot or logon.

use super::Backend;
use super::unix::{self, Output, Processes, Runner};
use crate::render::launchd::{file_name, render};
use crate::{
    InstallOptions, InstallOutcome, Installed, STOP_TIMEOUT, Scope, ServiceError, ServiceId,
    ServiceSpec, ServiceStatus,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LAUNCHCTL: &str = "/bin/launchctl";
/// Extra wait after launchd's own exit timeout.
const UNLOAD_MARGIN: Duration = Duration::from_secs(20);
/// How long a started job may take to show a process.
const START_WAIT: Duration = Duration::from_secs(5);

pub(crate) struct Launchd {
    pub(crate) runner: Box<dyn Runner>,
    pub(crate) system_dir: PathBuf,
    pub(crate) user_dir: Option<PathBuf>,
    pub(crate) uid: u32,
}

impl Launchd {
    pub(crate) fn host() -> Result<Self, ServiceError> {
        let runner = Processes;
        Ok(Self {
            uid: unix::effective_uid(&runner)?,
            runner: Box::new(runner),
            system_dir: "/Library/LaunchDaemons".into(),
            user_dir: unix::home_dir().map(|home| home.join("Library/LaunchAgents")),
        })
    }

    fn path(&self, id: &ServiceId) -> Result<PathBuf, ServiceError> {
        let dir = unix::definition_dir(id.scope, &self.system_dir, self.user_dir.as_deref())?;
        Ok(dir.join(file_name(&id.name)))
    }

    fn domain(&self, scope: Scope) -> String {
        match scope {
            Scope::System => "system".into(),
            Scope::User => format!("gui/{}", self.uid),
        }
    }

    fn target(&self, id: &ServiceId) -> String {
        format!("{}/{}", self.domain(id.scope), id.name)
    }

    /// The `launchctl print` report of a loaded job.
    fn loaded(&self, id: &ServiceId) -> Result<Option<Output>, ServiceError> {
        let output = self.runner.run(LAUNCHCTL, &["print", &self.target(id)])?;
        Ok(output.success.then_some(output))
    }

    fn require_session(&self, scope: Scope) -> Result<(), ServiceError> {
        if scope == Scope::User {
            let domain = self.domain(scope);
            if !self.runner.run(LAUNCHCTL, &["print", &domain])?.success {
                return Err(ServiceError::NoGuiSession { uid: self.uid });
            }
        }
        Ok(())
    }

    /// Unload the job and wait until its process is gone.
    fn unload(&self, id: &ServiceId) -> Result<(), ServiceError> {
        if self.loaded(id)?.is_none() {
            return Ok(());
        }
        let target = self.target(id);
        let args = ["bootout", target.as_str()];
        // `bootout` may report "in progress"; what counts is that the job goes.
        let output = self.runner.run(LAUNCHCTL, &args)?;
        let limit = STOP_TIMEOUT + UNLOAD_MARGIN;
        let limit = if output.success { limit } else { UNLOAD_MARGIN };
        if unix::wait_until(limit, || Ok(self.loaded(id)?.is_none()))? {
            Ok(())
        } else if output.success {
            Err(ServiceError::Timeout {
                name: id.name.clone(),
                action: "stop",
                after: limit,
            })
        } else {
            Err(unix::command_error(LAUNCHCTL, &args, &output))
        }
    }

    /// Lift a `launchctl disable` or `unload -w`, which would stop the job from
    /// loading. `enable` is only called then, because launchd keeps an
    /// override entry for every label it was ever given.
    fn enable_if_disabled(&self, id: &ServiceId) -> Result<(), ServiceError> {
        let domain = self.domain(id.scope);
        let output = self.runner.run(LAUNCHCTL, &["print-disabled", &domain])?;
        if is_disabled(&output.stdout, &id.name) {
            self.runner
                .check(LAUNCHCTL, &["enable", &self.target(id)])?;
        }
        Ok(())
    }

    /// Load the job, or start it if it is loaded and idle.
    fn run_job(&self, id: &ServiceId, path: &Path) -> Result<(), ServiceError> {
        self.require_session(id.scope)?;
        self.enable_if_disabled(id)?;
        match self.loaded(id)? {
            None => {
                let path = path.to_string_lossy();
                self.runner
                    .check(LAUNCHCTL, &["bootstrap", &self.domain(id.scope), &path])?;
            }
            Some(report) => {
                if !matches!(parse_print(&report.stdout), ServiceStatus::Running { .. }) {
                    self.runner
                        .check(LAUNCHCTL, &["kickstart", &self.target(id)])?;
                }
            }
        }
        Ok(())
    }

    fn require_installed(&self, id: &ServiceId) -> Result<PathBuf, ServiceError> {
        let path = self.path(id)?;
        let name = id.name.clone();
        match unix::read_definition(&path)? {
            None => Err(ServiceError::NotInstalled { name }),
            Some(contents) if !unix::is_managed(&contents) => {
                Err(ServiceError::NotManaged { name })
            }
            Some(_) => Ok(path),
        }
    }
}

impl Backend for Launchd {
    fn install(
        &self,
        spec: &ServiceSpec,
        options: &InstallOptions,
    ) -> Result<Installed, ServiceError> {
        let rendered = render(spec)?;
        let id = spec.id();
        unix::require_root(id.scope, self.uid, "install")?;
        // Checked first, so a machine without a session is left untouched.
        self.require_session(id.scope)?;
        let path = self.path(&id)?;
        let existing = unix::read_definition(&path)?;
        let outcome = unix::plan(
            &id.name,
            existing.as_deref(),
            &rendered.contents,
            options.replace,
        )?;
        // A job loaded from somewhere else must not be silently adopted.
        if existing.is_none()
            && let Some(report) = self.loaded(&id)?
        {
            if !unix::is_managed(report.stdout.as_bytes()) {
                return Err(ServiceError::NotManaged { name: id.name });
            }
            self.unload(&id)?;
        }
        let account = spec.account.as_deref();
        unix::prepare_dir(self.runner.as_ref(), &spec.working_dir, account)?;
        unix::prepare_dir(self.runner.as_ref(), &spec.log_dir, account)?;
        if outcome != InstallOutcome::Unchanged {
            unix::write_definition(&path, &rendered.contents)?;
        }
        if outcome == InstallOutcome::Replaced {
            // launchd keeps the definition it loaded.
            self.unload(&id)?;
        }

        let mut warnings = Vec::new();
        if options.start {
            self.run_job(&id, &path)?;
            unix::wait_until(START_WAIT, || {
                Ok(matches!(self.status(&id)?, ServiceStatus::Running { .. }))
            })?;
        } else {
            self.enable_if_disabled(&id)?;
        }
        if id.scope == Scope::System && matches!(account, None | Some("root")) {
            warnings.extend(unix::privileged_executable_warning(&spec.executable));
        }
        Ok(Installed {
            status: self.status(&id)?,
            name: id.name,
            definition: path,
            outcome,
            warnings,
        })
    }

    fn uninstall(&self, id: &ServiceId) -> Result<bool, ServiceError> {
        let path = self.path(id)?;
        match unix::read_definition(&path)? {
            Some(contents) if !unix::is_managed(&contents) => {
                return Err(ServiceError::NotManaged {
                    name: id.name.clone(),
                });
            }
            Some(_) => {}
            // The file was removed by hand while the job stayed loaded.
            None => match self.loaded(id)? {
                Some(report) if unix::is_managed(report.stdout.as_bytes()) => {}
                _ => return Ok(false),
            },
        }
        unix::require_root(id.scope, self.uid, "remove")?;
        self.unload(id)?;
        unix::remove_definition(&path)?;
        Ok(true)
    }

    fn status(&self, id: &ServiceId) -> Result<ServiceStatus, ServiceError> {
        let path = self.path(id)?;
        match self.loaded(id)? {
            Some(report) => Ok(parse_print(&report.stdout)),
            // Without a graphical session nothing of the user's is loaded.
            None if unix::read_definition(&path)?.is_some() => Ok(ServiceStatus::Stopped),
            None => Ok(ServiceStatus::NotInstalled),
        }
    }

    fn start(&self, id: &ServiceId) -> Result<(), ServiceError> {
        let path = self.require_installed(id)?;
        unix::require_root(id.scope, self.uid, "start")?;
        self.run_job(id, &path)
    }

    fn stop(&self, id: &ServiceId) -> Result<(), ServiceError> {
        self.require_installed(id)?;
        unix::require_root(id.scope, self.uid, "stop")?;
        self.unload(id)
    }
}

/// Whether `launchctl print-disabled` lists the label as disabled. Older
/// releases write `true` instead of `disabled`.
fn is_disabled(report: &str, label: &str) -> bool {
    let quoted = format!("\"{label}\"");
    report
        .lines()
        .filter_map(|line| line.trim().split_once(" => "))
        .any(|(name, value)| name == quoted && matches!(value, "disabled" | "true"))
}

/// Read the top level of a `launchctl print` report of a loaded job. Nested
/// blocks are indented further and are ignored.
pub(crate) fn parse_print(text: &str) -> ServiceStatus {
    let value = |key: &str| {
        text.lines()
            .filter_map(|line| line.strip_prefix('\t'))
            .filter(|line| !line.starts_with(['\t', ' ']))
            .filter_map(|line| line.split_once(" = "))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
    };
    let state = value("state").unwrap_or("unknown");
    if state == "running" {
        return ServiceStatus::Running {
            pid: value("pid").and_then(|pid| pid.parse().ok()),
        };
    }
    if let Some(signal) = value("last terminating signal") {
        return ServiceStatus::Failed {
            reason: format!("{state}; ended by signal: {signal}"),
        };
    }
    match value("last exit code") {
        Some(code) if code != "0" && !code.starts_with('(') => ServiceStatus::Failed {
            reason: format!("{state}; last exit code {code}"),
        },
        _ => ServiceStatus::Stopped,
    }
}
