//! launchd property lists.
//!
//! `KeepAlive` with `SuccessfulExit=false` restarts the program after a crash
//! or a non-zero exit and leaves it down after a clean one. `ThrottleInterval`
//! spaces the restarts and `ExitTimeOut` is how long launchd waits after
//! `SIGTERM` before `SIGKILL`.
//!
//! macOS containment is advisory (process groups): attempts that outlive a
//! stopped worker are dealt with by the worker's recovery when it starts again.

use crate::render::Rendered;
use crate::spec::invalid;
use crate::{ENV_SERVICE_NAME, Role, STOP_TIMEOUT, Scope, ServiceError, ServiceSpec};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Minimum seconds between two starts.
const THROTTLE_SEC: u64 = 10;

pub(crate) fn file_name(name: &str) -> String {
    format!("{name}.plist")
}

pub(crate) fn render(spec: &ServiceSpec) -> Result<Rendered, ServiceError> {
    let paths = spec.validate(crate::Os::Macos)?;
    match spec.scope {
        Scope::System if spec.role == Role::Worker => {
            return Err(ServiceError::Unsupported(
                "on macOS the worker runs as the logged-in user so that it can use that user's \
                 toolchains; install it in the user scope"
                    .into(),
            ));
        }
        Scope::User if spec.account.is_some() => {
            return Err(invalid(
                "a LaunchAgent always runs as the user it is installed for; leave the account \
                 unset",
            ));
        }
        _ => {}
    }

    let mut plist = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<!-- Written by Builderella. Reinstall the service to change it. -->\n",
        "<plist version=\"1.0\">\n",
        "<dict>\n",
    ));
    string(&mut plist, "Label", &spec.name);
    plist.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    let arguments = std::iter::once(paths.executable).chain(spec.args.iter().map(String::as_str));
    for argument in arguments {
        let _ = writeln!(plist, "\t\t<string>{}</string>", escape(argument));
    }
    plist.push_str("\t</array>\n");
    if let Some(account) = &spec.account {
        string(&mut plist, "UserName", account);
    }
    string(&mut plist, "WorkingDirectory", paths.working_dir);

    let mut env: BTreeMap<&str, &str> = spec
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    env.insert(ENV_SERVICE_NAME, &spec.name);
    plist.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for (key, value) in env {
        let _ = writeln!(
            plist,
            "\t\t<key>{}</key>\n\t\t<string>{}</string>",
            escape(key),
            escape(value)
        );
    }
    plist.push_str("\t</dict>\n");

    let _ = write!(
        plist,
        concat!(
            "\t<key>RunAtLoad</key>\n\t<true/>\n",
            "\t<key>KeepAlive</key>\n\t<dict>\n",
            "\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n",
            "\t</dict>\n",
            "\t<key>ThrottleInterval</key>\n\t<integer>{}</integer>\n",
            "\t<key>ExitTimeOut</key>\n\t<integer>{}</integer>\n",
        ),
        THROTTLE_SEC,
        STOP_TIMEOUT.as_secs()
    );
    let logs = paths.log_dir.trim_end_matches('/');
    string(
        &mut plist,
        "StandardOutPath",
        &format!("{logs}/{}.out.log", spec.name),
    );
    string(
        &mut plist,
        "StandardErrorPath",
        &format!("{logs}/{}.err.log", spec.name),
    );
    plist.push_str("</dict>\n</plist>\n");

    let directory = match spec.scope {
        Scope::System => "/Library/LaunchDaemons",
        Scope::User => "~/Library/LaunchAgents",
    };
    Ok(Rendered {
        path: PathBuf::from(format!("{directory}/{}", file_name(&spec.name))),
        contents: plist,
    })
}

fn string(plist: &mut String, key: &str, value: &str) {
    let _ = writeln!(
        plist,
        "\t<key>{key}</key>\n\t<string>{}</string>",
        escape(value)
    );
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
