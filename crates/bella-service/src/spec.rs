//! What to run, and the rules a specification has to meet.

use crate::{ENV_SERVICE_LOG_DIR, ENV_SERVICE_NAME, ENV_SERVICE_WORKING_DIR, Os, ServiceError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Longest accepted service name.
const MAX_NAME_LEN: usize = 80;

/// Which part of the fleet the service is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The fleet controller.
    Controller,
    /// The build worker of this machine.
    Worker,
}

impl Role {
    /// `controller` or `worker`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::Worker => "worker",
        }
    }

    /// `builderella-controller` or `builderella-worker`.
    pub fn default_name(self) -> &'static str {
        match self {
            Self::Controller => "builderella-controller",
            Self::Worker => "builderella-worker",
        }
    }
}

/// Where a definition is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Machine-wide; starts at boot.
    System,
    /// Belongs to the calling user's own service manager.
    User,
}

/// Names an installed service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceId {
    /// Service name.
    pub name: String,
    /// Where it is installed.
    pub scope: Scope,
}

impl ServiceId {
    /// A service with an explicit name.
    pub fn new(name: impl Into<String>, scope: Scope) -> Self {
        Self {
            name: name.into(),
            scope,
        }
    }

    /// The role's service under its default name.
    pub fn for_role(role: Role, scope: Scope) -> Self {
        Self::new(role.default_name(), scope)
    }

    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        validate_name(&self.name)
    }
}

/// Everything needed to run `bella` as a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Controller or worker.
    pub role: Role,
    /// ASCII letters, digits, `.`, `_` and `-`, starting with a letter or
    /// digit.
    pub name: String,
    /// Absolute path of the program.
    pub executable: PathBuf,
    /// Its arguments, passed through unchanged.
    pub args: Vec<String>,
    /// Where to install the definition.
    pub scope: Scope,
    /// User to run as. `None` is the platform default: the calling user for
    /// [`Scope::User`], root or `LocalSystem` for a system controller. A
    /// system worker must name its account, and that account has to own the
    /// worker's data directory, work root and toolchains.
    pub account: Option<String>,
    /// Absolute working directory.
    pub working_dir: PathBuf,
    /// Absolute directory for log files. Linux logs to the journal instead.
    pub log_dir: PathBuf,
    /// Extra environment. Never tokens: keys that look like secrets are
    /// refused, as are the `BELLA_SERVICE_*` keys this crate sets.
    pub env: BTreeMap<String, String>,
}

impl ServiceSpec {
    /// A specification with the role's default name, no arguments, no extra
    /// environment and the platform's default account.
    pub fn new(
        role: Role,
        scope: Scope,
        executable: impl Into<PathBuf>,
        working_dir: impl Into<PathBuf>,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            role,
            name: role.default_name().to_owned(),
            executable: executable.into(),
            args: Vec::new(),
            scope,
            account: None,
            working_dir: working_dir.into(),
            log_dir: log_dir.into(),
            env: BTreeMap::new(),
        }
    }

    /// The id under which this specification is installed.
    pub fn id(&self) -> ServiceId {
        ServiceId::new(self.name.clone(), self.scope)
    }

    /// Human-readable name. It contains the service name when that is not the
    /// default, because Windows requires display names to be unique.
    pub(crate) fn display_name(&self) -> String {
        let base = format!("Builderella {}", self.role.as_str());
        if self.name == self.role.default_name() {
            base
        } else {
            format!("{base} ({})", self.name)
        }
    }

    /// Check everything that does not depend on the running host, and return
    /// the paths as text.
    pub(crate) fn validate(&self, os: Os) -> Result<Paths<'_>, ServiceError> {
        validate_name(&self.name)?;
        let paths = Paths {
            executable: absolute("executable", &self.executable, os)?,
            working_dir: absolute("working directory", &self.working_dir, os)?,
            log_dir: absolute("log directory", &self.log_dir, os)?,
        };
        for arg in &self.args {
            printable("argument", arg)?;
        }
        if let Some(account) = &self.account {
            printable("account", account)?;
            if account.trim().is_empty() || account.trim() != account {
                return Err(invalid("the account is empty or padded with spaces"));
            }
        }
        let mut seen = BTreeSet::new();
        for (key, value) in &self.env {
            validate_env(key, value)?;
            // Windows environment names ignore case.
            if os == Os::Windows && !seen.insert(key.to_ascii_uppercase()) {
                return Err(invalid(format!(
                    "environment key `{key}` appears twice with different case"
                )));
            }
        }
        Ok(paths)
    }
}

/// The validated paths of a specification.
pub(crate) struct Paths<'a> {
    pub executable: &'a str,
    pub working_dir: &'a str,
    pub log_dir: &'a str,
}

pub(crate) fn invalid(reason: impl Into<String>) -> ServiceError {
    ServiceError::InvalidSpec(reason.into())
}

fn validate_name(name: &str) -> Result<(), ServiceError> {
    let ok = name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "service name `{}` must be at most {MAX_NAME_LEN} ASCII letters, digits, `.`, `_` \
             or `-`, starting with a letter or digit",
            name.escape_debug()
        )))
    }
}

/// No definition format here can carry control characters safely.
fn printable(what: &str, value: &str) -> Result<(), ServiceError> {
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}'))
    {
        Err(invalid(format!(
            "the {what} `{}` contains a control character",
            value.escape_debug()
        )))
    } else {
        Ok(())
    }
}

/// Absoluteness by the rules of the target system, not of the host.
fn absolute<'a>(what: &str, path: &'a Path, os: Os) -> Result<&'a str, ServiceError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid(format!("the {what} is not valid UTF-8")))?;
    printable(what, text)?;
    let bytes = text.as_bytes();
    let ok = match os {
        Os::Linux | Os::Macos => text.starts_with('/'),
        Os::Windows => {
            text.starts_with(r"\\")
                || (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'\\' | b'/'))
        }
    };
    if ok {
        Ok(text)
    } else {
        Err(invalid(format!(
            "the {what} `{text}` is not an absolute {} path",
            os.as_str()
        )))
    }
}

fn validate_env(key: &str, value: &str) -> Result<(), ServiceError> {
    let well_formed = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !well_formed {
        return Err(invalid(format!(
            "environment key `{}` is not a variable name",
            key.escape_debug()
        )));
    }
    let upper = key.to_ascii_uppercase();
    if [
        ENV_SERVICE_NAME,
        ENV_SERVICE_LOG_DIR,
        ENV_SERVICE_WORKING_DIR,
    ]
    .contains(&upper.as_str())
    {
        return Err(invalid(format!(
            "environment key `{key}` is set by the service definition itself"
        )));
    }
    if looks_secret(&upper) {
        return Err(invalid(format!(
            "environment key `{key}` looks like a secret; service definitions are readable by \
             other users, so keep it in a private file"
        )));
    }
    if has_url_password(value) {
        return Err(invalid(format!(
            "environment key `{key}` holds a URL with a password; service definitions are \
             readable by other users"
        )));
    }
    // The value is not echoed: a rejected value may still be sensitive.
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}'))
    {
        return Err(invalid(format!(
            "the value of environment key `{key}` contains a control character"
        )));
    }
    Ok(())
}

fn looks_secret(upper: &str) -> bool {
    const ANYWHERE: [&str; 9] = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASSPHRASE",
        "CREDENTIAL",
        "APIKEY",
        "PRIVATEKEY",
        "AUTHORIZATION",
    ];
    const WORDS: [&str; 5] = ["KEY", "PASS", "PAT", "COOKIE", "PAIRING"];
    // `PWD` alone is the working directory; as a suffix it means password.
    ANYWHERE.iter().any(|marker| upper.contains(marker))
        || upper.split('_').any(|word| WORDS.contains(&word))
        || upper.ends_with("_PWD")
}

/// `scheme://user:password@host`.
fn has_url_password(value: &str) -> bool {
    value.split("://").skip(1).any(|rest| {
        let authority = rest.split(['/', '?', '#', ' ']).next().unwrap_or(rest);
        authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}
