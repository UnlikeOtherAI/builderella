//! What the file-based managers share: running commands and writing
//! definition files.

use crate::{InstallOutcome, Scope, ServiceError};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Output of a management command.
#[derive(Clone, Debug, Default)]
pub(crate) struct Output {
    pub success: bool,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output, ServiceError>;

    /// Run a command that has to succeed.
    fn check(&self, program: &str, args: &[&str]) -> Result<Output, ServiceError> {
        let output = self.run(program, args)?;
        if output.success {
            Ok(output)
        } else {
            Err(command_error(program, args, &output))
        }
    }
}

pub(crate) fn command_error(program: &str, args: &[&str], output: &Output) -> ServiceError {
    let text = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    ServiceError::Command {
        command: format!("{program} {}", args.join(" ")),
        status: output.status.clone(),
        output: text.trim().to_owned(),
    }
}

/// Runs real processes.
pub(crate) struct Processes;

impl Runner for Processes {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output, ServiceError> {
        let output = Command::new(program)
            .args(args)
            // The parsers read the untranslated output.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|source| ServiceError::Io {
                action: "run",
                path: program.into(),
                source,
            })?;
        Ok(Output {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Effective user id of this process.
pub(crate) fn effective_uid(runner: &dyn Runner) -> Result<u32, ServiceError> {
    let args = ["-u"];
    let output = runner.check("/usr/bin/id", &args)?;
    output
        .stdout
        .trim()
        .parse()
        .map_err(|_| command_error("/usr/bin/id", &args, &output))
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

/// The directory holding definitions of `scope`.
pub(crate) fn definition_dir<'a>(
    scope: Scope,
    system: &'a Path,
    user: Option<&'a Path>,
) -> Result<&'a Path, ServiceError> {
    match scope {
        Scope::System => Ok(system),
        Scope::User => user.ok_or_else(|| {
            ServiceError::Unsupported(
                "the user scope needs HOME to be set to an absolute path".into(),
            )
        }),
    }
}

/// Refuse to change a system definition without being root.
pub(crate) fn require_root(scope: Scope, uid: u32, action: &str) -> Result<(), ServiceError> {
    if scope == Scope::System && uid != 0 {
        Err(ServiceError::NeedsPrivilege {
            action: format!("{action} a system service"),
            hint: "needs root; run the command with sudo, or use the user scope",
        })
    } else {
        Ok(())
    }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> ServiceError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        ServiceError::NeedsPrivilege {
            action: format!("{action} {}", path.display()),
            hint: "check who owns it, or run the command with sudo for a system service",
        }
    } else {
        ServiceError::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

pub(crate) fn read_definition(path: &Path) -> Result<Option<Vec<u8>>, ServiceError> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("read", path, source)),
    }
}

/// Whether the definition carries this crate's marker.
pub(crate) fn is_managed(definition: &[u8]) -> bool {
    String::from_utf8_lossy(definition).contains(crate::ENV_SERVICE_NAME)
}

/// Decide what installing `wanted` over `existing` means.
pub(crate) fn plan(
    name: &str,
    existing: Option<&[u8]>,
    wanted: &str,
    replace: bool,
) -> Result<InstallOutcome, ServiceError> {
    match existing {
        None => Ok(InstallOutcome::Created),
        Some(contents) if contents == wanted.as_bytes() => Ok(InstallOutcome::Unchanged),
        Some(contents) if !is_managed(contents) => Err(ServiceError::NotManaged {
            name: name.to_owned(),
        }),
        Some(_) if !replace => Err(ServiceError::AlreadyInstalled {
            name: name.to_owned(),
            differences: "definition".into(),
        }),
        Some(_) => Ok(InstallOutcome::Replaced),
    }
}

/// Replace `path` atomically with a world-readable file that only its owner
/// can write, which both managers require.
pub(crate) fn write_definition(path: &Path, contents: &str) -> Result<(), ServiceError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).map_err(|e| io_error("create", directory, e))?;
    let mut temporary = OsString::from(path);
    // Neither manager loads a file with this ending.
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let written = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(contents.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    written.map_err(|source| {
        let _ = fs::remove_file(&temporary);
        io_error("write", path, source)
    })
}

pub(crate) fn remove_definition(path: &Path) -> Result<(), ServiceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove", path, source)),
    }
}

/// Create `directory` if needed and give what was created to `account`.
/// Existing directories keep their owner.
pub(crate) fn prepare_dir(
    runner: &dyn Runner,
    directory: &Path,
    account: Option<&str>,
) -> Result<(), ServiceError> {
    let missing: Vec<&Path> = directory
        .ancestors()
        .take_while(|path| !path.exists())
        .collect();
    fs::create_dir_all(directory).map_err(|e| io_error("create", directory, e))?;
    if let Some(account) = account {
        // Deepest first, while the directories above are still ours, and never
        // through a link.
        for path in &missing {
            let path = path.to_string_lossy();
            runner.check("chown", &["-h", "--", account, &path])?;
        }
    }
    Ok(())
}

/// A service that runs as root must not execute a file others can change.
pub(crate) fn privileged_executable_warning(executable: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(executable).ok()?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Some(format!(
                "the service runs as root, but {} can be changed by other users; install it in a \
                 root-owned location",
                executable.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = executable;
    None
}

/// Poll `done` until it is true or `limit` has passed.
pub(crate) fn wait_until(
    limit: Duration,
    mut done: impl FnMut() -> Result<bool, ServiceError>,
) -> Result<bool, ServiceError> {
    const STEP: Duration = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    loop {
        if done()? {
            return Ok(true);
        }
        if waited >= limit {
            return Ok(false);
        }
        std::thread::sleep(STEP);
        waited += STEP;
    }
}
