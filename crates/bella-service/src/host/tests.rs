//! The file-based managers against a scripted `systemctl` and `launchctl`.

use super::Backend;
use super::launchd::{Launchd, parse_print};
use super::systemd::{Systemd, parse_show};
use super::unix::{Output, Runner};
use crate::{
    InstallOptions, InstallOutcome, Role, Scope, ServiceError, ServiceSpec, ServiceStatus,
};
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use tempfile::TempDir;

const UID: u32 = 4242;
const RUNNING_SHOW: &str = "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=77\n";

/// What the pretend service manager holds, and every command it was given.
#[derive(Default)]
struct Fake {
    calls: Vec<String>,
    /// launchd: the loaded job's definition.
    loaded: Option<String>,
    no_session: bool,
    /// launchd: the output of `print-disabled`.
    disabled: &'static str,
    /// A command containing this text fails with this error output.
    failing: Option<(&'static str, &'static str)>,
}

#[derive(Clone, Default)]
struct Script(Rc<RefCell<Fake>>);

impl Script {
    /// The commands issued since the last call, without read-only queries.
    fn changes(&self) -> Vec<String> {
        std::mem::take(&mut self.0.borrow_mut().calls)
            .into_iter()
            .filter(|c| {
                !["launchctl print", " show ", "show-user"]
                    .iter()
                    .any(|query| c.contains(query))
            })
            .collect()
    }
}

impl Runner for Script {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output, ServiceError> {
        let mut fake = self.0.borrow_mut();
        let program = program.rsplit('/').next().unwrap_or(program);
        let call = format!("{program} {}", args.join(" "));
        fake.calls.push(call.clone());
        let ok = |stdout: &str| Output {
            success: true,
            stdout: stdout.into(),
            ..Output::default()
        };
        let fail = |stderr: &str| Output {
            success: false,
            status: "exit status: 1".into(),
            stderr: stderr.into(),
            ..Output::default()
        };
        if let Some((text, error)) = fake.failing
            && call.contains(text)
        {
            return Ok(fail(error));
        }
        Ok(match (program, args) {
            ("launchctl", ["print", target]) => {
                if target.matches('/').count() < usize::from(target.starts_with("gui/")) + 1 {
                    if fake.no_session {
                        fail("Could not find domain")
                    } else {
                        ok("domain")
                    }
                } else {
                    match &fake.loaded {
                        Some(job) => ok(&format!("\tstate = running\n\tpid = 9\n{job}")),
                        None => fail("Could not find service"),
                    }
                }
            }
            ("launchctl", ["print-disabled", _]) => ok(fake.disabled),
            ("launchctl", ["bootstrap", _, path]) => {
                fake.loaded = Some(fs::read_to_string(path).expect("bootstrap a written file"));
                ok("")
            }
            ("launchctl", ["bootout", _]) => {
                fake.loaded = None;
                ok("")
            }
            ("systemctl", args) if args.contains(&"show") => ok(RUNNING_SHOW),
            ("loginctl", _) => ok("Linger=no\n"),
            _ => ok(""),
        })
    }
}

struct Rig {
    root: TempDir,
    script: Script,
}

impl Rig {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            script: Script::default(),
        }
    }

    fn systemd(&self, uid: u32) -> Systemd {
        Systemd {
            runner: Box::new(self.script.clone()),
            system_dir: self.root.path().join("system"),
            user_dir: Some(self.root.path().join("user")),
            uid,
        }
    }

    fn launchd(&self, uid: u32) -> Launchd {
        Launchd {
            runner: Box::new(self.script.clone()),
            system_dir: self.root.path().join("daemons"),
            user_dir: Some(self.root.path().join("agents")),
            uid,
        }
    }

    fn spec(&self, role: Role, scope: Scope) -> ServiceSpec {
        ServiceSpec::new(
            role,
            scope,
            "/opt/builderella/bella",
            self.root.path().join("data/work"),
            self.root.path().join("data/logs"),
        )
    }

    fn fail(&self, command: &'static str, error: &'static str) {
        self.script.0.borrow_mut().failing = Some((command, error));
    }
}

fn replace() -> InstallOptions {
    InstallOptions {
        replace: true,
        ..InstallOptions::default()
    }
}

#[test]
fn systemd_install_is_idempotent_and_replacement_is_explicit() {
    let rig = Rig::new();
    let manager = rig.systemd(UID);
    let mut spec = rig.spec(Role::Worker, Scope::User);
    let first = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(first.outcome, InstallOutcome::Created);
    assert_eq!(first.status, ServiceStatus::Running { pid: Some(77) });
    assert!(first.warnings[0].contains("lingering"), "{first:?}");
    assert!(spec.working_dir.is_dir() && spec.log_dir.is_dir());
    let written = fs::read_to_string(&first.definition).unwrap();
    assert_eq!(
        written,
        crate::render(&spec, crate::Os::Linux).unwrap().contents
    );
    assert_eq!(
        rig.script.changes(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable builderella-worker.service",
            "systemctl --user start builderella-worker.service",
        ]
    );

    let second = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(second.outcome, InstallOutcome::Unchanged);
    rig.script.changes();

    spec.args.push("--verbose".into());
    let refused = manager.install(&spec, &InstallOptions::default());
    assert!(matches!(
        refused,
        Err(ServiceError::AlreadyInstalled { .. })
    ));
    assert_eq!(fs::read_to_string(&first.definition).unwrap(), written);
    assert!(rig.script.changes().is_empty());

    let third = manager.install(&spec, &replace()).unwrap();
    assert_eq!(third.outcome, InstallOutcome::Replaced);
    assert!(
        rig.script
            .changes()
            .contains(&"systemctl --user restart builderella-worker.service".to_owned())
    );
}

#[test]
fn systemd_uninstall_stop_and_start() {
    let rig = Rig::new();
    let manager = rig.systemd(0);
    let mut spec = rig.spec(Role::Controller, Scope::System);
    spec.account = None;
    let id = spec.id();
    assert_eq!(manager.status(&id).unwrap(), ServiceStatus::NotInstalled);
    assert!(!manager.uninstall(&id).unwrap());
    assert!(matches!(
        manager.start(&id),
        Err(ServiceError::NotInstalled { .. })
    ));
    let options = InstallOptions {
        start: false,
        ..InstallOptions::default()
    };
    let installed = manager.install(&spec, &options).unwrap();
    assert!(!rig.script.changes().iter().any(|c| c.contains(" start ")));
    manager.start(&id).unwrap();
    manager.stop(&id).unwrap();
    assert!(manager.uninstall(&id).unwrap());
    assert!(!installed.definition.exists());
    assert_eq!(
        rig.script.changes(),
        [
            "systemctl start builderella-controller.service",
            "systemctl stop builderella-controller.service",
            "systemctl disable --now builderella-controller.service",
            "systemctl daemon-reload",
            "systemctl reset-failed builderella-controller.service",
        ]
    );
}

#[test]
fn a_definition_of_someone_else_is_left_alone() {
    let rig = Rig::new();
    let spec = rig.spec(Role::Controller, Scope::User);
    let foreign = "[Service]\nExecStart=/bin/true\n";
    let id = spec.id();
    for (manager, file) in [
        (
            Box::new(rig.systemd(UID)) as Box<dyn Backend>,
            "user/builderella-controller.service",
        ),
        (
            Box::new(rig.launchd(UID)),
            "agents/builderella-controller.plist",
        ),
    ] {
        let path = rig.root.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, foreign).unwrap();
        for result in [
            manager.install(&spec, &replace()).map(drop),
            manager.uninstall(&id).map(drop),
            manager.stop(&id),
            manager.start(&id),
        ] {
            assert!(
                matches!(result, Err(ServiceError::NotManaged { .. })),
                "{result:?}"
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
    }
    assert!(rig.script.changes().is_empty());
}

#[test]
fn system_services_need_root_and_say_so() {
    let rig = Rig::new();
    let spec = rig.spec(Role::Controller, Scope::System);
    for manager in [
        Box::new(rig.systemd(UID)) as Box<dyn Backend>,
        Box::new(rig.launchd(UID)),
    ] {
        let error = manager
            .install(&spec, &InstallOptions::default())
            .unwrap_err();
        assert!(error.to_string().contains("needs root"), "{error}");
    }
    assert!(!spec.log_dir.exists());
    assert!(rig.script.changes().is_empty());
}

#[test]
fn systemd_permission_and_bus_errors_are_explained() {
    let rig = Rig::new();
    let manager = rig.systemd(UID);
    let spec = rig.spec(Role::Worker, Scope::User);
    rig.fail("daemon-reload", "Failed to connect to bus: No medium found");
    let error = manager
        .install(&spec, &InstallOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("lingering"), "{error}");
    rig.fail("daemon-reload", "Failed to reload daemon: Access denied");
    assert!(matches!(
        manager.install(&spec, &InstallOptions::default()),
        Err(ServiceError::NeedsPrivilege { .. })
    ));
    // The same call succeeds once the cause is gone.
    rig.script.0.borrow_mut().failing = None;
    let healed = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(healed.outcome, InstallOutcome::Unchanged);
}

#[test]
fn a_system_worker_owns_the_directories_created_for_it() {
    let rig = Rig::new();
    let manager = rig.systemd(0);
    let mut spec = rig.spec(Role::Worker, Scope::System);
    spec.account = Some("builder".into());
    manager.install(&spec, &InstallOptions::default()).unwrap();
    let data = rig.root.path().join("data");
    let owned: Vec<String> = rig
        .script
        .changes()
        .into_iter()
        .filter(|c| c.starts_with("chown"))
        .collect();
    // `work` creates `data` too; the deeper directory is handed over first.
    let expected = [data.join("work"), data.clone(), data.join("logs")]
        .map(|dir| format!("chown -h -- builder {}", dir.display()));
    assert_eq!(owned, expected);
    manager.install(&spec, &InstallOptions::default()).unwrap();
    assert!(!rig.script.changes().iter().any(|c| c.starts_with("chown")));
}

#[test]
fn launchd_lifecycle() {
    let rig = Rig::new();
    let manager = rig.launchd(UID);
    let mut spec = rig.spec(Role::Worker, Scope::User);
    let id = spec.id();
    let first = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(first.outcome, InstallOutcome::Created);
    assert_eq!(first.status, ServiceStatus::Running { pid: Some(9) });
    let bootstrap = format!(
        "launchctl bootstrap gui/{UID} {}",
        first.definition.display()
    );
    assert_eq!(rig.script.changes(), std::slice::from_ref(&bootstrap));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&first.definition)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);

    let again = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(again.outcome, InstallOutcome::Unchanged);
    // Loaded and running: neither loaded again nor restarted.
    assert!(rig.script.changes().is_empty());

    spec.args.push("--verbose".into());
    assert!(matches!(
        manager.install(&spec, &InstallOptions::default()),
        Err(ServiceError::AlreadyInstalled { .. })
    ));
    manager.install(&spec, &replace()).unwrap();
    let changes = rig.script.changes();
    assert_eq!(
        changes[0],
        format!("launchctl bootout gui/{UID}/builderella-worker")
    );
    assert_eq!(changes.last(), Some(&bootstrap));

    manager.stop(&id).unwrap();
    assert_eq!(manager.status(&id).unwrap(), ServiceStatus::Stopped);
    manager.stop(&id).unwrap();
    manager.start(&id).unwrap();
    assert!(matches!(
        manager.status(&id).unwrap(),
        ServiceStatus::Running { .. }
    ));
    assert!(manager.uninstall(&id).unwrap());
    assert!(rig.script.0.borrow().loaded.is_none());
    assert_eq!(manager.status(&id).unwrap(), ServiceStatus::NotInstalled);
    assert!(!manager.uninstall(&id).unwrap());
}

#[test]
fn launchd_enables_only_a_disabled_job() {
    for disabled in [
        "\t\t\"builderella-worker\" => disabled\n",
        "\t\t\"builderella-worker\" => true\n",
    ] {
        let rig = Rig::new();
        rig.script.0.borrow_mut().disabled = disabled;
        let manager = rig.launchd(UID);
        let spec = rig.spec(Role::Worker, Scope::User);
        manager.install(&spec, &InstallOptions::default()).unwrap();
        assert_eq!(
            rig.script.changes()[0],
            format!("launchctl enable gui/{UID}/builderella-worker")
        );
    }
}

#[test]
fn launchd_removes_a_job_whose_file_was_deleted() {
    let rig = Rig::new();
    let manager = rig.launchd(UID);
    let spec = rig.spec(Role::Worker, Scope::User);
    let installed = manager.install(&spec, &InstallOptions::default()).unwrap();
    fs::remove_file(installed.definition).unwrap();
    assert!(manager.uninstall(&spec.id()).unwrap());
    assert!(rig.script.0.borrow().loaded.is_none());
}

#[test]
fn launchd_agent_without_a_graphical_session_changes_nothing() {
    let rig = Rig::new();
    rig.script.0.borrow_mut().no_session = true;
    let manager = rig.launchd(UID);
    let spec = rig.spec(Role::Worker, Scope::User);
    assert!(matches!(
        manager.install(&spec, &InstallOptions::default()),
        Err(ServiceError::NoGuiSession { uid: UID })
    ));
    assert!(!rig.root.path().join("agents").exists());
    assert!(!spec.log_dir.exists());
}

#[test]
fn launchd_daemon_uses_the_system_domain() {
    let rig = Rig::new();
    rig.script.0.borrow_mut().no_session = true;
    let manager = rig.launchd(0);
    let spec = rig.spec(Role::Controller, Scope::System);
    let installed = manager.install(&spec, &InstallOptions::default()).unwrap();
    assert!(
        installed
            .definition
            .starts_with(rig.root.path().join("daemons"))
    );
    assert!(
        rig.script
            .changes()
            .iter()
            .any(|c| c.starts_with("launchctl bootstrap system "))
    );
}

#[test]
fn systemd_status_explains_an_unreachable_instance() {
    let rig = Rig::new();
    let manager = rig.systemd(UID);
    let spec = rig.spec(Role::Worker, Scope::User);
    manager.install(&spec, &InstallOptions::default()).unwrap();
    rig.fail(" show ", "Failed to connect to bus: No medium found");
    assert!(matches!(
        manager.status(&spec.id()),
        Err(ServiceError::Unsupported(_))
    ));
}

#[test]
fn systemctl_show_states() {
    let show = |active: &str, sub: &str, pid: u32| {
        parse_show(&format!(
            "LoadState=loaded\nActiveState={active}\nSubState={sub}\nMainPID={pid}\n\
             Result=exit-code\nExecMainStatus=3\n"
        ))
    };
    assert_eq!(
        show("active", "running", 5),
        ServiceStatus::Running { pid: Some(5) }
    );
    assert_eq!(
        show("deactivating", "stop-sigterm", 5),
        ServiceStatus::Running { pid: Some(5) }
    );
    assert_eq!(show("inactive", "dead", 0), ServiceStatus::Stopped);
    for (active, sub) in [("failed", "failed"), ("activating", "auto-restart")] {
        let ServiceStatus::Failed { reason } = show(active, sub, 0) else {
            panic!("{active}/{sub} is a failure");
        };
        assert!(reason.contains("exit status 3"), "{reason}");
    }
    assert!(matches!(
        parse_show("LoadState=bad-setting\nActiveState=inactive\n"),
        ServiceStatus::Failed { .. }
    ));
}

#[test]
fn launchctl_print_states() {
    let report = |body: &str| {
        parse_print(&format!(
            "gui/501/x = {{\n{body}\tendpoints = {{\n\t\tstate = running\n\t\tpid = 1\n\t}}\n}}\n"
        ))
    };
    assert_eq!(
        report("\tstate = running\n\tpid = 31\n\tlast exit code = 1\n"),
        ServiceStatus::Running { pid: Some(31) }
    );
    assert_eq!(
        report("\tstate = not running\n\tlast exit code = (never exited)\n"),
        ServiceStatus::Stopped
    );
    assert_eq!(
        report("\tstate = not running\n\tlast exit code = 0\n"),
        ServiceStatus::Stopped
    );
    assert_eq!(
        report("\tstate = spawn scheduled\n\tlast exit code = 78: EX_CONFIG\n"),
        ServiceStatus::Failed {
            reason: "spawn scheduled; last exit code 78: EX_CONFIG".into()
        }
    );
    assert_eq!(
        report("\tstate = spawn scheduled\n\tlast terminating signal = Killed: 9\n"),
        ServiceStatus::Failed {
            reason: "spawn scheduled; ended by signal: Killed: 9".into()
        }
    );
}
