//! Credentials, the pinned certificate, roles and exit codes.

use crate::support::{Fleet, Options};
use std::path::Path;

/// A pinned fingerprint that is not the controller's: the message says the
/// certificate changed and how to pair again, the exit status is 5, and
/// nothing is saved.
#[test]
fn a_wrong_fingerprint_is_reported_as_a_changed_certificate_with_exit_5() {
    let fleet = Fleet::start(Options {
        workers: 0,
        ..Options::default()
    });
    let wrong = "ab".repeat(32);
    let other = fleet.path().join("other-credentials.json");
    let token = fleet.controller_dir.join("admin-token");
    let run = fleet.bella(&[
        "--credentials",
        &other.display().to_string(),
        "login",
        &fleet.url,
        "--fingerprint",
        &wrong,
        "--token-file",
        &token.display().to_string(),
    ]);
    assert_eq!(run.code, 5, "{}", run.stderr);
    assert!(
        run.stderr.contains("certificate fingerprint changed"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("bella login"), "{}", run.stderr);
    assert!(!other.exists(), "nothing is saved after a failed check");

    // Stored credentials whose pin no longer matches: the same message.
    let saved = std::fs::read_to_string(fleet.path().join("credentials.json")).unwrap();
    let tampered = saved.replace(&fleet.fingerprint, &wrong);
    assert_ne!(saved, tampered);
    write_private(&other, &tampered);
    let run = fleet.bella(&["--credentials", &other.display().to_string(), "fleet"]);
    assert_eq!(run.code, 5, "{}", run.stderr);
    assert!(
        run.stderr.contains("certificate fingerprint changed"),
        "{}",
        run.stderr
    );
    let run = fleet.bella(&[
        "--credentials",
        &other.display().to_string(),
        "--json",
        "fleet",
    ]);
    assert_eq!(run.code, 5);
    assert_eq!(run.json()["exit_code"], 5);
}

fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Exit codes: usage errors are 2, a refusal by the controller is 4, an
/// unreachable controller is 5.
#[test]
fn exit_codes_tell_usage_refusal_and_unreachable_apart() {
    let mut fleet = Fleet::start(Options {
        workers: 0,
        ..Options::default()
    });
    assert_eq!(fleet.bella(&["submit", "--no-such-flag"]).code, 2);
    assert_eq!(fleet.bella(&["job", "not-a-job-id"]).code, 2);
    // Tokens are never accepted as arguments.
    assert_eq!(fleet.bella(&["login", &fleet.url, "--token", "x"]).code, 2);

    let missing = "job_0123456789abcdef0123456789abcdef";
    let run = fleet.bella(&["--json", "job", missing]);
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert_eq!(run.json()["error"]["code"], "not_found");

    fleet.stop_controller();
    let run = fleet.bella(&["fleet"]);
    assert_eq!(run.code, 5, "{}", run.stderr);
}

/// A61 end to end: a developer token can submit but not mint tokens,
/// pair machines or change machine or project policy.
#[test]
fn a61_a_developer_cannot_change_policy_or_enrollment() {
    let fleet = Fleet::start(Options::default());
    let token = fleet
        .bella(&["token", "create", "--role", "developer", "--name", "agent"])
        .ok()
        .stdout
        .trim()
        .to_string();
    assert!(token.starts_with("bella_"), "{token}");
    let developer = fleet.path().join("developer.json");
    let developer_flag = developer.display().to_string();
    fleet
        .bella_in(
            fleet.path(),
            &[
                "--credentials",
                &developer_flag,
                "login",
                &fleet.url,
                "--fingerprint",
                &fleet.fingerprint,
                "--token-stdin",
            ],
            Some(&format!("{token}\n")),
        )
        .ok();
    let saved = std::fs::read_to_string(&developer).unwrap();
    assert!(saved.contains(&token));

    let as_developer = |args: &[&str]| {
        let mut full = vec!["--credentials", developer_flag.as_str()];
        full.extend_from_slice(args);
        fleet.bella(&full)
    };
    let machine = fleet.fleet_view()["machines"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for args in [
        vec!["token", "create", "--role", "admin", "--name", "escalate"],
        vec!["controller", "pair"],
        vec!["machine", "policy", &machine, "--foreground-cpu", "0"],
        vec!["machine", "drain", &machine],
        vec!["project", "set", "anything", "--allow-urgent", "true"],
    ] {
        let run = as_developer(&args);
        assert_eq!(run.code, 4, "{args:?} must be refused: {}", run.stderr);
    }
    // Reading is allowed.
    assert_eq!(as_developer(&["fleet"]).code, 0);
    let version = as_developer(&["--json", "version"]).ok().json();
    assert_eq!(version["controller"]["protocol_version"], 1);
}

/// `service install --print` renders the definition for this host from the
/// absolute path of the binary and the flags of `run`, and installs
/// nothing.
#[test]
fn service_install_print_shows_the_definition_without_installing() {
    let fleet = Fleet::start(Options {
        workers: 0,
        ..Options::default()
    });
    let data_dir = fleet.path().join("svc-data");
    let data = data_dir.display().to_string();
    let mut args = vec![
        "--json",
        "worker",
        "service",
        "install",
        "--data-dir",
        &data,
        "--print",
    ];
    if cfg!(unix) {
        args.push("--user");
    } else {
        // A Windows worker service names the account it runs as.
        args.extend(["--account", "builder"]);
    }
    let document = fleet.bella(&args).ok().json();
    let contents = document["contents"].as_str().unwrap();
    assert!(contents.contains("worker"), "{contents}");
    assert!(contents.contains("run"), "{contents}");
    assert!(contents.contains("svc-data"), "{contents}");
    assert!(contents.contains("bella"), "{contents}");
    assert!(!data_dir.exists(), "printing creates nothing");
}

/// The commissioning self-test passes on this host and says how.
#[test]
fn worker_self_test_reports_the_containment_it_verified() {
    let fleet = Fleet::start(Options {
        workers: 0,
        ..Options::default()
    });
    let dir = fleet.path().join("selftest").display().to_string();
    let root = fleet.path().join("selftest-work").display().to_string();
    let run = fleet.bella(&[
        "--json",
        "worker",
        "self-test",
        "--data-dir",
        &dir,
        "--work-root",
        &root,
    ]);
    let report = run.json();
    assert_eq!(run.code, 0, "{report}");
    assert_eq!(report["passed"], true);
    assert!(report["mechanism"].as_str().is_some_and(|m| !m.is_empty()));
}
