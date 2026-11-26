//! The environment and command line of a step.

use bella_core::job::CommandSpec;
use bella_core::protocol::Offer;
use bella_core::snapshot::validate_rel_path;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// System variables a step inherits from the daemon, by exact name.
#[cfg(unix)]
const INHERITED: &[&str] = &["PATH", "HOME", "USER", "LANG"];
#[cfg(windows)]
const INHERITED: &[&str] = &[
    "PATH",
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Windows only: every variable whose name starts with this is inherited
/// (`ProgramFiles`, `ProgramFiles(x86)`).
#[cfg(windows)]
const INHERITED_PREFIX: &str = "ProgramFiles";

/// The system part of a step environment, read from the daemon's own
/// environment once per attempt.
///
/// Only the names in the list above are taken; on Windows they are stored
/// under the canonical spelling so a recipe's `PATH` replaces the system's
/// `Path`. Variables that are not valid Unicode are left out.
pub(crate) fn system_env() -> BTreeMap<String, String> {
    system_env_from(std::env::vars_os().filter_map(|(name, value)| {
        Some((name.into_string().ok()?, value.into_string().ok()?))
    }))
}

fn system_env_from(vars: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in vars {
        if let Some(canonical) = inherited_name(&name) {
            out.insert(canonical, value);
        }
    }
    out
}

#[cfg(unix)]
fn inherited_name(name: &str) -> Option<String> {
    INHERITED
        .iter()
        .find(|wanted| **wanted == name)
        .map(|wanted| (*wanted).to_string())
}

#[cfg(windows)]
fn inherited_name(name: &str) -> Option<String> {
    if let Some(wanted) = INHERITED
        .iter()
        .find(|wanted| wanted.eq_ignore_ascii_case(name))
    {
        return Some((*wanted).to_string());
    }
    let prefix = name.get(..INHERITED_PREFIX.len())?;
    prefix
        .eq_ignore_ascii_case(INHERITED_PREFIX)
        .then(|| name.to_string())
}

/// Set a variable. Windows treats names case-insensitively, so there an
/// existing spelling of the same name is replaced rather than duplicated.
fn set(env: &mut BTreeMap<String, String>, name: &str, value: &str) {
    if cfg!(windows) {
        let clash: Vec<String> = env
            .keys()
            .filter(|existing| existing.eq_ignore_ascii_case(name) && existing.as_str() != name)
            .cloned()
            .collect();
        for existing in clash {
            env.remove(&existing);
        }
    }
    env.insert(name.to_string(), value.to_string());
}

/// Build the complete environment of one step.
///
/// In this order, later entries replacing earlier ones: the system set
/// (`system`), the temporary directory variables pointing at `tmp_dir`,
/// the `BELLA_*` variables and `CI=true`, the job's `env`, the step's
/// `env`. Nothing else is added; in particular nothing else of the daemon's
/// environment, no token and no controller address.
///
/// `BELLA_WORKSPACE` is the root of the materialized source set.
/// `BELLA_JOBS` is the job's `parallelism`, or the granted CPUs rounded up,
/// at least 1.
pub(crate) fn step_env(
    system: &BTreeMap<String, String>,
    offer: &Offer,
    src: &Path,
    tmp_dir: &Path,
    step: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = system.clone();
    let tmp = tmp_dir.to_string_lossy();
    if cfg!(windows) {
        set(&mut env, "TEMP", &tmp);
        set(&mut env, "TMP", &tmp);
    } else {
        set(&mut env, "TMPDIR", &tmp);
    }
    let cpu_milli = offer.reservation.cpu_milli;
    let jobs = offer
        .spec
        .parallelism
        .map(u64::from)
        .unwrap_or_else(|| cpu_milli.div_ceil(1000))
        .max(1);
    set(&mut env, "BELLA_JOB_ID", offer.job_id.as_str());
    set(&mut env, "BELLA_ATTEMPT_ID", offer.attempt.attempt_id.as_str());
    set(&mut env, "BELLA_SNAPSHOT_ID", offer.snapshot_id.digest().as_str());
    set(&mut env, "BELLA_WORKSPACE", &src.to_string_lossy());
    set(&mut env, "BELLA_JOBS", &jobs.to_string());
    set(&mut env, "BELLA_CPU_MILLI", &cpu_milli.to_string());
    set(
        &mut env,
        "BELLA_MEMORY_BYTES",
        &offer.reservation.memory_bytes.to_string(),
    );
    set(&mut env, "CI", "true");
    for (name, value) in &offer.spec.env {
        set(&mut env, name, value);
    }
    for (name, value) in step {
        set(&mut env, name, value);
    }
    env
}

/// The default shell that runs a `Shell` command without one of its own.
pub(crate) fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command"]
            .map(str::to_string)
            .to_vec()
    } else {
        ["/bin/sh", "-c"].map(str::to_string).to_vec()
    }
}

/// The argument vector for a command: `Argv` as given, `Shell` as the
/// explicit shell (or the default one) with the script as its last argument.
pub(crate) fn resolve_command(command: &CommandSpec) -> Result<Vec<String>, String> {
    match command {
        CommandSpec::Argv { argv } => {
            if argv.first().is_none_or(|program| program.is_empty()) {
                return Err("the command has no program".to_string());
            }
            Ok(argv.clone())
        }
        CommandSpec::Shell { script, shell } => {
            let mut argv = match shell {
                Some(shell) if shell.first().is_some_and(|program| !program.is_empty()) => {
                    shell.clone()
                }
                Some(_) => return Err("the shell of the command has no program".to_string()),
                None => default_shell(),
            };
            argv.push(script.clone());
            Ok(argv)
        }
    }
}

/// The working directory of a step: `src` joined with its relative `cwd`.
///
/// The path must be relative, `/`-separated, without `.`/`..` components,
/// without `\` and, on Windows, without a drive or stream separator, so the
/// join cannot leave `src`.
pub(crate) fn step_cwd(src: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    let Some(cwd) = cwd else {
        return Ok(src.to_path_buf());
    };
    let trimmed = cwd.strip_suffix('/').unwrap_or(cwd);
    if trimmed == "." || trimmed.is_empty() {
        return Ok(src.to_path_buf());
    }
    if trimmed.contains('\\') {
        return Err(format!("working directory `{cwd}` uses `\\`"));
    }
    if cfg!(windows) && trimmed.contains(':') {
        return Err(format!("working directory `{cwd}` contains `:`"));
    }
    validate_rel_path(trimmed)
        .map_err(|reason| format!("working directory `{cwd}` is not usable: {reason}"))?;
    let mut path = src.to_path_buf();
    for component in trimmed.split('/') {
        path.push(component);
    }
    Ok(path)
}

/// Is `name` usable as a step name in spool file names and log streams:
/// `[a-z0-9_-]`, 1 to 64 characters?
pub(crate) fn valid_step_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_system_variables_are_taken() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("SECRET_TOKEN", "x"),
            ("BELLA_CONTROLLER_URL", "https://c"),
            ("LD_PRELOAD", "evil.so"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = system_env_from(vars);
        assert!(env.contains_key("PATH"));
        assert!(!env.contains_key("SECRET_TOKEN"));
        assert!(!env.contains_key("BELLA_CONTROLLER_URL"));
        assert!(!env.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn cwd_cannot_leave_the_source_root() {
        let src = Path::new("ws").join("src");
        assert_eq!(step_cwd(&src, None).unwrap(), src);
        assert_eq!(step_cwd(&src, Some(".")).unwrap(), src);
        assert_eq!(step_cwd(&src, Some("a/b/")).unwrap(), src.join("a").join("b"));
        assert!(step_cwd(&src, Some("../x")).is_err());
        assert!(step_cwd(&src, Some("/abs")).is_err());
        assert!(step_cwd(&src, Some("a\\b")).is_err());
        assert!(step_cwd(&src, Some("a//b")).is_err());
    }

    #[test]
    fn shell_commands_get_an_explicit_interpreter() {
        let argv = resolve_command(&CommandSpec::Shell {
            script: "echo hi".into(),
            shell: Some(vec!["bash".into(), "-c".into()]),
        })
        .unwrap();
        assert_eq!(argv, ["bash", "-c", "echo hi"]);
        let argv = resolve_command(&CommandSpec::Shell {
            script: "echo hi".into(),
            shell: None,
        })
        .unwrap();
        assert_eq!(argv.last().map(String::as_str), Some("echo hi"));
        assert!(argv.len() >= 3);
        assert!(resolve_command(&CommandSpec::Argv { argv: vec![] }).is_err());
    }

    #[test]
    fn step_names_are_restricted() {
        assert!(valid_step_name("build-1_x"));
        assert!(!valid_step_name(""));
        assert!(!valid_step_name("Build"));
        assert!(!valid_step_name("a/b"));
        assert!(!valid_step_name(&"a".repeat(65)));
    }
}
