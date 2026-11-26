//! Managing units with `systemctl`.

use super::Backend;
use super::unix::{self, Output, Processes, Runner};
use crate::render::systemd::{render, unit_name};
use crate::{
    InstallOptions, InstallOutcome, Installed, Scope, ServiceError, ServiceId, ServiceSpec,
    ServiceStatus,
};
use std::path::PathBuf;

const PROPERTIES: &str = "--property=LoadState,ActiveState,SubState,MainPID,Result,ExecMainStatus";

pub(crate) struct Systemd {
    pub(crate) runner: Box<dyn Runner>,
    pub(crate) system_dir: PathBuf,
    pub(crate) user_dir: Option<PathBuf>,
    pub(crate) uid: u32,
}

impl Systemd {
    pub(crate) fn host() -> Result<Self, ServiceError> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| unix::home_dir().map(|home| home.join(".config")));
        let runner = Processes;
        Ok(Self {
            uid: unix::effective_uid(&runner)?,
            runner: Box::new(runner),
            system_dir: "/etc/systemd/system".into(),
            user_dir: config.map(|config| config.join("systemd/user")),
        })
    }

    fn path(&self, id: &ServiceId) -> Result<PathBuf, ServiceError> {
        let dir = unix::definition_dir(id.scope, &self.system_dir, self.user_dir.as_deref())?;
        Ok(dir.join(unit_name(&id.name)))
    }

    fn run(&self, scope: Scope, args: &[&str]) -> Result<Output, ServiceError> {
        let mut full = Vec::with_capacity(args.len() + 1);
        if scope == Scope::User {
            full.push("--user");
        }
        full.extend_from_slice(args);
        self.runner.run("systemctl", &full)
    }

    /// Run a command that has to succeed, explaining the usual refusals.
    fn check(&self, scope: Scope, args: &[&str]) -> Result<Output, ServiceError> {
        let output = self.run(scope, args)?;
        if output.success {
            return Ok(output);
        }
        let command = format!("systemctl {}", args.join(" "));
        if output.stderr.contains("Access denied")
            || output.stderr.contains("authentication required")
        {
            return Err(ServiceError::NeedsPrivilege {
                action: format!("run `{command}`"),
                hint: "needs root; run the command with sudo",
            });
        }
        if output.stderr.contains("Failed to connect to bus") {
            return Err(ServiceError::Unsupported(format!(
                "`{command}` cannot reach systemd ({}); a user unit needs that user's systemd \
                 instance, which exists in a login session or with lingering enabled",
                output.stderr.trim()
            )));
        }
        Err(unix::command_error("systemctl", args, &output))
    }

    fn lingering_warning(&self) -> Option<String> {
        let uid = self.uid.to_string();
        let output = self
            .runner
            .run("loginctl", &["show-user", &uid, "--property=Linger"]);
        if output.is_ok_and(|o| o.success && o.stdout.trim() == "Linger=yes") {
            return None;
        }
        Some(
            "lingering is not enabled, so the service starts at login and stops at logout \
             instead of running from boot; enable it with `loginctl enable-linger`"
                .into(),
        )
    }

    /// The unit file must exist and be ours before it is operated on.
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

impl Backend for Systemd {
    fn install(
        &self,
        spec: &ServiceSpec,
        options: &InstallOptions,
    ) -> Result<Installed, ServiceError> {
        let rendered = render(spec)?;
        let id = spec.id();
        unix::require_root(id.scope, self.uid, "install")?;
        let path = self.path(&id)?;
        let existing = unix::read_definition(&path)?;
        let outcome = unix::plan(
            &id.name,
            existing.as_deref(),
            &rendered.contents,
            options.replace,
        )?;
        let account = spec.account.as_deref();
        unix::prepare_dir(self.runner.as_ref(), &spec.working_dir, account)?;
        unix::prepare_dir(self.runner.as_ref(), &spec.log_dir, account)?;
        if outcome != InstallOutcome::Unchanged {
            unix::write_definition(&path, &rendered.contents)?;
        }
        let unit = unit_name(&id.name);
        // Repeated for an unchanged file so that an interrupted install heals.
        self.check(id.scope, &["daemon-reload"])?;
        self.check(id.scope, &["enable", &unit])?;

        let mut warnings = Vec::new();
        if options.start {
            let verb = if outcome == InstallOutcome::Replaced {
                "restart"
            } else {
                "start"
            };
            self.check(id.scope, &[verb, &unit])?;
        } else if outcome == InstallOutcome::Replaced {
            warnings
                .push("a running service keeps the old definition until it is restarted".into());
        }
        match id.scope {
            Scope::User => warnings.extend(self.lingering_warning()),
            Scope::System if matches!(account, None | Some("root")) => {
                warnings.extend(unix::privileged_executable_warning(&spec.executable));
            }
            Scope::System => {}
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
        let path = match self.require_installed(id) {
            Ok(path) => path,
            Err(ServiceError::NotInstalled { .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        unix::require_root(id.scope, self.uid, "remove")?;
        let unit = unit_name(&id.name);
        // `systemctl` waits for the stop, bounded by the unit's own timeout.
        self.check(id.scope, &["disable", "--now", &unit])?;
        unix::remove_definition(&path)?;
        self.check(id.scope, &["daemon-reload"])?;
        // Fails when there is no failed state to forget.
        self.run(id.scope, &["reset-failed", &unit])?;
        Ok(true)
    }

    fn status(&self, id: &ServiceId) -> Result<ServiceStatus, ServiceError> {
        let installed = unix::read_definition(&self.path(id)?)?.is_some();
        if !installed {
            return Ok(ServiceStatus::NotInstalled);
        }
        let unit = unit_name(&id.name);
        let output = self.check(id.scope, &["show", &unit, PROPERTIES])?;
        Ok(parse_show(&output.stdout))
    }

    fn start(&self, id: &ServiceId) -> Result<(), ServiceError> {
        self.require_installed(id)?;
        unix::require_root(id.scope, self.uid, "start")?;
        self.check(id.scope, &["start", &unit_name(&id.name)])?;
        Ok(())
    }

    fn stop(&self, id: &ServiceId) -> Result<(), ServiceError> {
        self.require_installed(id)?;
        unix::require_root(id.scope, self.uid, "stop")?;
        self.check(id.scope, &["stop", &unit_name(&id.name)])?;
        Ok(())
    }
}

/// Read the `key=value` lines of `systemctl show` for an installed unit.
pub(crate) fn parse_show(text: &str) -> ServiceStatus {
    let value = |key: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.trim())
    };
    let pid = value("MainPID").parse().ok().filter(|pid| *pid != 0);
    let failure = || ServiceStatus::Failed {
        reason: format!(
            "{}/{}, result {}, exit status {}",
            value("ActiveState"),
            value("SubState"),
            value("Result"),
            value("ExecMainStatus")
        ),
    };
    match value("ActiveState") {
        "active" | "reloading" | "deactivating" => ServiceStatus::Running { pid },
        "activating" if value("SubState") == "auto-restart" => failure(),
        "activating" => ServiceStatus::Running { pid },
        "failed" => failure(),
        // Installed but not yet loaded, or stopped.
        _ if value("LoadState") == "bad-setting" || value("LoadState") == "error" => {
            ServiceStatus::Failed {
                reason: format!("systemd cannot load the unit ({})", value("LoadState")),
            }
        }
        _ => ServiceStatus::Stopped,
    }
}
