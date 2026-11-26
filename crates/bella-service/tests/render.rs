//! Definitions for every operating system, produced on whichever one runs the
//! tests.

use bella_core::defaults::{CLEANUP_TIMEOUT, TERMINATE_GRACE};
use bella_service::{
    ENV_SERVICE_NAME, Os, Role, STOP_MARGIN, STOP_TIMEOUT, Scope, ServiceError, ServiceSpec, render,
};
use std::path::PathBuf;

const ALL: [Os; 3] = [Os::Linux, Os::Macos, Os::Windows];

fn unix_spec(role: Role, scope: Scope) -> ServiceSpec {
    let mut spec = ServiceSpec::new(
        role,
        scope,
        "/home/builder/tools dir é/bella",
        "/home/builder/work dir",
        "/home/builder/logs & more",
    );
    spec.args = [
        role.as_str(),
        "run",
        "--data-dir",
        "/home/builder/data ü",
        "say \"hi\"",
        r"back\slash",
        "100% $HOME",
        "<a&'b'>",
        "",
        ";",
    ]
    .map(String::from)
    .to_vec();
    spec.env.insert("LANG".into(), "cs_CZ.UTF-8".into());
    spec.env
        .insert("NOTE".into(), "50% \"off\" $x <&> \\ ž".into());
    spec
}

fn windows_spec(role: Role) -> ServiceSpec {
    let mut spec = ServiceSpec::new(
        role,
        Scope::System,
        r"C:\Users\builder\Program Files é\bella.exe",
        r"C:\Users\builder\work dir",
        r"C:\Users\builder\logs",
    );
    spec.args = [
        role.as_str(),
        "run",
        "--data-dir",
        r"C:\Users\builder\data ü\",
        "say \"hi\"",
        r#"a\"b"#,
        r"plain\path\",
        "",
        "100%",
    ]
    .map(String::from)
    .to_vec();
    spec.env.insert("NOTE".into(), "a b \"c\"".into());
    spec
}

#[test]
fn linux_system_worker_unit() {
    let mut spec = unix_spec(Role::Worker, Scope::System);
    spec.account = Some("builder".into());
    let unit = render(&spec, Os::Linux).unwrap();
    assert_eq!(
        unit.path,
        PathBuf::from("/etc/systemd/system/builderella-worker.service")
    );
    assert_eq!(
        unit.contents,
        r#"# Written by Builderella. Reinstall the service to change it.
[Unit]
Description=Builderella worker
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart="/home/builder/tools dir é/bella" "worker" "run" "--data-dir" "/home/builder/data ü" "say \"hi\"" "back\\slash" "100%% $$HOME" "<a&'b'>" "" ";"
WorkingDirectory=/home/builder/work dir
Environment="BELLA_SERVICE_NAME=builderella-worker"
Environment="LANG=cs_CZ.UTF-8"
Environment="NOTE=50%% \"off\" $x <&> \\ ž"
User=builder
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=160
Delegate=yes
OOMPolicy=continue
TasksMax=infinity

[Install]
WantedBy=multi-user.target
"#
    );
}

#[test]
fn linux_user_controller_unit() {
    let mut spec = ServiceSpec::new(
        Role::Controller,
        Scope::User,
        "/home/builder/bin/bella",
        "/home/builder",
        "/home/builder/logs",
    );
    spec.args = vec!["controller".into(), "run".into()];
    let unit = render(&spec, Os::Linux).unwrap();
    assert_eq!(
        unit.path,
        PathBuf::from("~/.config/systemd/user/builderella-controller.service")
    );
    assert_eq!(
        unit.contents,
        r#"# Written by Builderella. Reinstall the service to change it.
[Unit]
Description=Builderella controller

[Service]
Type=exec
ExecStart="/home/builder/bin/bella" "controller" "run"
WorkingDirectory=/home/builder
Environment="BELLA_SERVICE_NAME=builderella-controller"
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=160

[Install]
WantedBy=default.target
"#
    );
}

#[test]
fn macos_worker_agent() {
    let plist = render(&unix_spec(Role::Worker, Scope::User), Os::Macos).unwrap();
    assert_eq!(
        plist.path,
        PathBuf::from("~/Library/LaunchAgents/builderella-worker.plist")
    );
    assert_eq!(
        plist.contents,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by Builderella. Reinstall the service to change it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>builderella-worker</string>
	<key>ProgramArguments</key>
	<array>
		<string>/home/builder/tools dir é/bella</string>
		<string>worker</string>
		<string>run</string>
		<string>--data-dir</string>
		<string>/home/builder/data ü</string>
		<string>say &quot;hi&quot;</string>
		<string>back\slash</string>
		<string>100% $HOME</string>
		<string>&lt;a&amp;&apos;b&apos;&gt;</string>
		<string></string>
		<string>;</string>
	</array>
	<key>WorkingDirectory</key>
	<string>/home/builder/work dir</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>BELLA_SERVICE_NAME</key>
		<string>builderella-worker</string>
		<key>LANG</key>
		<string>cs_CZ.UTF-8</string>
		<key>NOTE</key>
		<string>50% &quot;off&quot; $x &lt;&amp;&gt; \ ž</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>ExitTimeOut</key>
	<integer>160</integer>
	<key>StandardOutPath</key>
	<string>/home/builder/logs &amp; more/builderella-worker.out.log</string>
	<key>StandardErrorPath</key>
	<string>/home/builder/logs &amp; more/builderella-worker.err.log</string>
</dict>
</plist>
"#
    );
}

#[test]
fn macos_controller_daemon_runs_as_the_named_user() {
    let mut spec = unix_spec(Role::Controller, Scope::System);
    spec.account = Some("builder".into());
    let plist = render(&spec, Os::Macos).unwrap();
    assert_eq!(
        plist.path,
        PathBuf::from("/Library/LaunchDaemons/builderella-controller.plist")
    );
    assert!(
        plist
            .contents
            .contains("<key>UserName</key>\n\t<string>builder</string>")
    );
}

/// Apple's own parser reads back exactly what was put in.
#[cfg(target_os = "macos")]
#[test]
fn macos_plutil_reads_back_every_value() {
    use std::process::Command;

    let spec = unix_spec(Role::Worker, Scope::User);
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("job.plist");
    std::fs::write(&file, render(&spec, Os::Macos).unwrap().contents).unwrap();
    let extract = |key: &str| {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(output.status.success(), "{key}: {output:?}");
        let mut text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.pop(), Some('\n'));
        text
    };
    assert_eq!(
        extract("ProgramArguments.0"),
        spec.executable.to_str().unwrap()
    );
    for (index, argument) in spec.args.iter().enumerate() {
        assert_eq!(
            &extract(&format!("ProgramArguments.{}", index + 1)),
            argument
        );
    }
    assert_eq!(extract("EnvironmentVariables.NOTE"), spec.env["NOTE"]);
    assert_eq!(extract("ExitTimeOut"), "160");
    assert_eq!(extract("KeepAlive.SuccessfulExit"), "false");
}

#[test]
fn windows_worker_entry() {
    let mut spec = windows_spec(Role::Worker);
    spec.account = Some("builder".into());
    let entry = render(&spec, Os::Windows).unwrap();
    assert_eq!(
        entry.path,
        PathBuf::from(r"HKLM\SYSTEM\CurrentControlSet\Services\builderella-worker")
    );
    assert_eq!(
        entry.contents,
        r#"service: builderella-worker
display name: Builderella worker
description: Builderella worker: part of the Builderella build and test fleet.
start: automatic
command: "C:\Users\builder\Program Files é\bella.exe" worker run --data-dir "C:\Users\builder\data ü\\" "say \"hi\"" "a\\\"b" plain\path\ "" 100%
account: .\builder
on failure: restart after 5 s, then 30 s, then every 60 s; the count resets after 86400 s
stop timeout: 160 s
environment:
  BELLA_SERVICE_NAME=builderella-worker
  BELLA_SERVICE_LOG_DIR=C:\Users\builder\logs
  BELLA_SERVICE_WORKING_DIR=C:\Users\builder\work dir
  NOTE=a b "c"
"#
    );
}

#[test]
fn windows_accounts_are_normalised_and_defaulted() {
    let account = |role, account: Option<&str>| {
        let mut spec = windows_spec(role);
        spec.account = account.map(String::from);
        render(&spec, Os::Windows).map(|r| {
            r.contents
                .lines()
                .find_map(|l| l.strip_prefix("account: "))
                .unwrap()
                .to_owned()
        })
    };
    assert_eq!(account(Role::Controller, None).unwrap(), "LocalSystem");
    for (given, expected) in [
        ("system", "LocalSystem"),
        (r"NT AUTHORITY\SYSTEM", "LocalSystem"),
        ("NetworkService", r"NT AUTHORITY\NetworkService"),
        (r"DOMAIN\builder", r"DOMAIN\builder"),
        ("builder@example.com", "builder@example.com"),
        ("builder", r".\builder"),
    ] {
        assert_eq!(account(Role::Worker, Some(given)).unwrap(), expected);
    }
    // The worker's account is never guessed.
    assert!(matches!(
        account(Role::Worker, None),
        Err(ServiceError::InvalidSpec(_))
    ));
}

#[test]
fn a_custom_name_is_part_of_the_display_name() {
    let mut spec = windows_spec(Role::Controller);
    spec.name = "builderella-second".into();
    let text = render(&spec, Os::Windows).unwrap().contents;
    assert!(text.contains("display name: Builderella controller (builderella-second)\n"));
}

/// A specification every platform accepts as it stands.
fn valid(os: Os) -> ServiceSpec {
    match os {
        Os::Windows => windows_spec(Role::Controller),
        Os::Linux | Os::Macos => unix_spec(Role::Controller, Scope::User),
    }
}

fn rejected(os: Os, change: impl FnOnce(&mut ServiceSpec)) -> String {
    let mut spec = valid(os);
    render(&spec, os).unwrap();
    change(&mut spec);
    match render(&spec, os) {
        Err(ServiceError::InvalidSpec(reason)) => reason,
        other => panic!("{os:?} accepted or misreported: {other:?}"),
    }
}

#[test]
fn secret_looking_environment_keys_are_refused() {
    for os in ALL {
        for key in [
            "BELLA_TOKEN",
            "github_token",
            "CLIENT_SECRET",
            "DB_PASSWORD",
            "MYSQL_PWD",
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
            "APIKEY",
            "SIGNING_KEY",
            "GH_PAT",
            "PAIRING_CODE",
            "SESSION_COOKIE",
        ] {
            let reason = rejected(os, |spec| {
                spec.env.insert(key.into(), "s3cr3t-value".into());
            });
            assert!(reason.contains(key), "{reason}");
            assert!(!reason.contains("s3cr3t"), "{reason}");
        }
        let reason = rejected(os, |spec| {
            spec.env.insert(
                "HTTPS_PROXY".into(),
                "http://builder:s3cr3t@proxy.example:3128".into(),
            );
        });
        assert!(!reason.contains("s3cr3t"), "{reason}");
        let mut spec = valid(os);
        for key in ["PWD", "SSH_AUTH_SOCK", "MONKEY", "PATH", "KEYBOARD"] {
            spec.env
                .insert(key.into(), "http://proxy.example:3128/a@b".into());
        }
        render(&spec, os).unwrap();
    }
}

#[test]
fn relative_paths_are_refused() {
    for os in ALL {
        for path in ["bella", "./bin/bella", "", r"C:bella.exe"] {
            let reason = rejected(os, |spec| spec.executable = path.into());
            assert!(reason.contains("executable"), "{reason}");
        }
        rejected(os, |spec| spec.working_dir = "work".into());
        rejected(os, |spec| spec.log_dir = "logs".into());
    }
    // Absolute means absolute for the target, whatever the host is.
    rejected(Os::Linux, |spec| spec.executable = r"C:\bella.exe".into());
    rejected(Os::Windows, |spec| {
        spec.executable = "/usr/bin/bella".into()
    });
}

#[test]
fn values_that_could_forge_a_definition_are_refused() {
    for os in ALL {
        rejected(os, |spec| spec.args.push("a\nUser=root".into()));
        rejected(os, |spec| {
            spec.env.insert("NOTE".into(), "a\r\nb".into());
        });
        rejected(os, |spec| {
            spec.env.insert("A=B".into(), "c".into());
        });
        rejected(os, |spec| spec.account = Some("a\0b".into()));
        for name in ["", "../x", "a b", "-x", "a/b", &"x".repeat(81)] {
            rejected(os, |spec| spec.name = name.into());
        }
        for key in [ENV_SERVICE_NAME, "bella_service_log_dir"] {
            rejected(os, |spec| {
                spec.env.insert(key.into(), "other".into());
            });
        }
    }
    for path in [
        "/opt/$x/bella",
        "/opt/\"x\"/bella",
        "/opt/x's/bella",
        r"/opt/a\b",
    ] {
        rejected(Os::Linux, |spec| spec.executable = path.into());
    }
    for os in [Os::Linux, Os::Macos] {
        rejected(os, |spec| {
            spec.scope = Scope::System;
            spec.account = Some("builder:staff".into());
        });
    }
    rejected(Os::Linux, |spec| spec.working_dir = "/work\\".into());
    rejected(Os::Windows, |spec| {
        spec.env.insert("Path".into(), "a".into());
        spec.env.insert("PATH".into(), "b".into());
    });
}

#[test]
fn scope_and_account_rules() {
    // Linux: a system worker needs an account, a user unit cannot have one.
    let mut spec = unix_spec(Role::Worker, Scope::System);
    assert!(render(&spec, Os::Linux).is_err());
    spec.scope = Scope::User;
    spec.account = Some("builder".into());
    assert!(render(&spec, Os::Linux).is_err());
    assert!(render(&spec, Os::Macos).is_err());
    // macOS never runs the worker as a daemon.
    spec.scope = Scope::System;
    assert!(matches!(
        render(&spec, Os::Macos),
        Err(ServiceError::Unsupported(_))
    ));
    let mut windows = windows_spec(Role::Controller);
    windows.scope = Scope::User;
    assert!(matches!(
        render(&windows, Os::Windows),
        Err(ServiceError::Unsupported(_))
    ));
}

#[test]
fn stop_timeouts_cover_cleanup_and_grace() {
    assert_eq!(
        STOP_TIMEOUT,
        CLEANUP_TIMEOUT + TERMINATE_GRACE + STOP_MARGIN
    );
    assert!(!STOP_MARGIN.is_zero());
    let seconds = STOP_TIMEOUT.as_secs();
    for (os, expected) in [
        (Os::Linux, format!("TimeoutStopSec={seconds}\n")),
        (
            Os::Macos,
            format!("<key>ExitTimeOut</key>\n\t<integer>{seconds}</integer>"),
        ),
        (Os::Windows, format!("stop timeout: {seconds} s\n")),
    ] {
        let text = render(&valid(os), os).unwrap().contents;
        assert!(text.contains(&expected), "{text}");
    }
}
