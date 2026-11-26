//! Submitting and following jobs: success, failure, timeouts, what the
//! snapshot contains, idempotency and services.

use crate::support::{Fleet, Options, script, sleep_script, write};
use serde_json::Value;

fn recipe(project: &str, body: &str) -> String {
    format!("[project]\nname = \"{project}\"\n\n{body}")
}

/// The happy path, end to end: login (done by the fleet), submit with
/// `--wait --follow`, the log lines arrive, the artifact downloads
/// byte-identical, the job is released and the dashboard link is printed.
#[test]
fn submit_wait_follow_downloads_the_artifact_and_releases_the_job() {
    let fleet = Fleet::start(Options::default());
    let build = script(
        "mkdir -p out && cat input.txt > out/result.txt && echo line-one && echo line-two",
        r"mkdir out & type input.txt > out\result.txt & echo line-one & echo line-two",
    );
    let repo = fleet.repo(
        "happy",
        &[
            (
                "builderella.toml",
                &recipe(
                    "happy",
                    &format!(
                        "[recipes.build]\ncpu = 1\nmemory = \"256MiB\"\nartifacts = [\"out/result.txt\"]\nrun = {build}\n"
                    ),
                ),
            ),
            ("input.txt", "payload\n"),
        ],
    );

    let run = fleet
        .bella_in(&repo, &["submit", "build", "--wait", "--follow"], None)
        .ok();
    assert!(run.stdout.contains("line-one"), "{}", run.stdout);
    assert!(run.stdout.contains("line-two"), "{}", run.stdout);
    let job_id = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("job "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the job id is printed")
        .to_string();
    assert!(
        run.stdout
            .contains(&format!("{}/#/jobs/{job_id}", fleet.url)),
        "the dashboard link is printed: {}",
        run.stdout
    );
    assert!(run.stdout.contains("succeeded"), "{}", run.stdout);

    let job = fleet.job(&job_id);
    assert_eq!(job["phase"], "finished");
    assert_eq!(job["outcome"]["type"], "succeeded");
    assert_eq!(job["cleanup"], "released");
    // The last attempt is the one that ran: a worker may reject an offer
    // first (for example while its telemetry settles), which adds an
    // attempt that never started.
    assert!(
        last_attempt(&job)["released_at"].is_number(),
        "the attempt was released: {job}"
    );

    let dest = fleet.path().join("downloads");
    fleet
        .bella(&[
            "artifacts",
            &job_id,
            "--download",
            &dest.display().to_string(),
        ])
        .ok();
    let bytes = std::fs::read(dest.join("out").join("result.txt")).expect("downloaded");
    assert_eq!(bytes, b"payload\n");

    // A second download never overwrites what is there.
    let again = fleet.bella(&[
        "artifacts",
        &job_id,
        "--download",
        &dest.display().to_string(),
    ]);
    assert_eq!(again.code, 2, "{}", again.stderr);
    assert!(again.stderr.contains("already exists"), "{}", again.stderr);
    assert_eq!(
        std::fs::read(dest.join("out").join("result.txt")).unwrap(),
        b"payload\n"
    );

    let logs = fleet.bella(&["logs", &job_id]).ok();
    assert!(logs.stdout.contains("line-two"));
}

/// A failing step: exit 1, and the step's last lines on standard error.
#[test]
fn a_failing_step_exits_1_with_its_last_lines() {
    let fleet = Fleet::start(Options::default());
    let fail = script(
        "echo about-to-fail; echo the-reason-is-here; exit 3",
        "echo about-to-fail & echo the-reason-is-here & exit /b 3",
    );
    let repo = fleet.repo(
        "failing",
        &[(
            "builderella.toml",
            &recipe("failing", &format!("[recipes.check]\nrun = {fail}\n")),
        )],
    );
    let run = fleet.bella_in(&repo, &["submit", "--wait"], None);
    assert_eq!(
        run.code, 1,
        "stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    assert!(run.stderr.contains("the-reason-is-here"), "{}", run.stderr);
    assert!(
        run.stdout.contains("failed in step main (exit 3)"),
        "{}",
        run.stdout
    );

    // `--json` gives one document with the outcome.
    let run = fleet.bella_in(&repo, &["--json", "submit", "--wait"], None);
    assert_eq!(run.code, 1);
    let document = run.json();
    assert_eq!(document["job"]["outcome"]["type"], "failed");
    assert_eq!(document["job"]["outcome"]["exit_code"], 3);
}

/// A21 end to end: a wait that times out exits 3 and cancels nothing; the
/// job still runs to completion.
#[test]
fn a21_a_wait_timeout_exits_3_and_the_job_still_finishes() {
    let fleet = Fleet::start(Options::default());
    let slow = sleep_script(4, "started");
    let repo = fleet.repo(
        "slow",
        &[(
            "builderella.toml",
            &recipe("slow", &format!("[recipes.slow]\nrun = {slow}\n")),
        )],
    );
    let run = fleet.bella_in(
        &repo,
        &["--json", "submit", "--wait", "--wait-timeout", "1s"],
        None,
    );
    assert_eq!(
        run.code, 3,
        "stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    let document = run.json();
    assert_eq!(document["timed_out"], true);
    let id = document["job"]["id"].as_str().unwrap().to_string();
    assert!(!document["job"]["cancel_requested"].as_bool().unwrap());

    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "1m"]);
    assert_eq!(waited.code, 0, "{}", waited.stderr);
    let job = waited.json()["job"].clone();
    assert_eq!(job["outcome"]["type"], "succeeded");
    assert_eq!(job["cancel_requested"], false);
}

/// A33-style, end to end: an uncommitted edit and an untracked file reach
/// the worker; an ignored file and a secret-looking file do not, and the
/// secret is reported.
#[test]
fn a33_the_dirty_worktree_reaches_the_worker_without_ignored_files_or_secrets() {
    let fleet = Fleet::start(Options::default());
    let check = script(
        "cat edited.txt; [ -f untracked.txt ] && echo has-untracked; [ -e ignored.txt ] || echo no-ignored; [ -e .env ] || echo no-secret",
        "type edited.txt & (if exist untracked.txt echo has-untracked) & (if not exist ignored.txt echo no-ignored) & (if not exist .env echo no-secret)",
    );
    let repo = fleet.repo(
        "dirty",
        &[
            (
                "builderella.toml",
                &recipe("dirty", &format!("[recipes.tree]\nrun = {check}\n")),
            ),
            ("edited.txt", "committed-content\n"),
            (".gitignore", "ignored.txt\n"),
        ],
    );
    write(&repo.join("edited.txt"), "uncommitted-edit\n");
    write(&repo.join("untracked.txt"), "new\n");
    write(&repo.join("ignored.txt"), "ignored\n");
    write(&repo.join(".env"), "TOKEN=do-not-upload\n");

    let run = fleet.bella_in(&repo, &["--json", "submit", "--wait", "--follow"], None);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let document = run.json();
    let secrets: Vec<&str> = document["excluded_secrets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(secrets, [".env"], "the secret is reported");
    // With --json the followed log goes to standard error.
    for marker in [
        "uncommitted-edit",
        "has-untracked",
        "no-ignored",
        "no-secret",
    ] {
        assert!(
            run.stderr.contains(marker),
            "missing {marker}:\n{}",
            run.stderr
        );
    }
    assert!(!run.stderr.contains("committed-content"));
    assert!(!run.stdout.contains("do-not-upload"));
}

/// The same idempotency key returns the same job.
#[test]
fn resubmitting_with_the_same_key_returns_the_same_job() {
    let fleet = Fleet::start(Options::default());
    let quick = script("echo quick", "echo quick");
    let repo = fleet.repo(
        "idempotent",
        &[(
            "builderella.toml",
            &recipe("idempotent", &format!("[recipes.quick]\nrun = {quick}\n")),
        )],
    );
    let first = fleet
        .bella_in(&repo, &["--json", "submit"], None)
        .ok()
        .json();
    let key = first["idempotency_key"].as_str().unwrap().to_string();
    let again = fleet
        .bella_in(
            &repo,
            &["--json", "submit", "--idempotency-key", &key],
            None,
        )
        .ok()
        .json();
    assert_eq!(first["job"]["id"], again["job"]["id"]);
    let other = fleet
        .bella_in(&repo, &["--json", "submit"], None)
        .ok()
        .json();
    assert_ne!(first["job"]["id"], other["job"]["id"]);
}

/// A19 end to end: a service and the test that runs against it reserve
/// their combined peak, the test waits for the service's readiness, and the
/// job succeeds.
#[test]
fn a19_a_service_and_its_test_run_together_under_their_combined_reservation() {
    let fleet = Fleet::start(Options::default());
    let service = sleep_script(30, "service-ready");
    let test = script("echo tests-ran", "echo tests-ran");
    let body = format!(
        "[recipes.e2e]\n\n[[recipes.e2e.steps]]\nname = \"svc\"\nkind = \"service\"\ncpu = 0.25\nmemory = \"64MiB\"\nrun = {service}\nready = {{ log = \"service-ready\", timeout = \"30s\" }}\n\n[[recipes.e2e.steps]]\nname = \"tests\"\nkind = \"test\"\nneeds = [\"svc\"]\ncpu = 0.5\nmemory = \"128MiB\"\nrun = {test}\n"
    );
    let repo = fleet.repo(
        "service",
        &[("builderella.toml", &recipe("service", &body))],
    );

    let plan = fleet
        .bella_in(&repo, &["--json", "plan", "e2e"], None)
        .ok()
        .json();
    assert_eq!(plan["envelope"]["peak"]["cpu_milli"], 750);
    assert_eq!(plan["envelope"]["peak"]["memory_bytes"], 192u64 << 20);
    assert_eq!(plan["plan"]["feasible"], true);

    let run = fleet.bella_in(
        &repo,
        &["--json", "submit", "e2e", "--wait", "--follow"],
        None,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let job = run.json()["job"].clone();
    assert_eq!(job["envelope"]["peak"]["cpu_milli"], 750);
    assert!(run.stderr.contains("service-ready"), "{}", run.stderr);
    assert!(run.stderr.contains("tests-ran"), "{}", run.stderr);
    let steps = last_attempt(&job)["steps"].as_array().unwrap();
    let state = |name: &str| {
        steps
            .iter()
            .find(|step| step["name"] == name)
            .map(|step| step["state"].clone())
            .unwrap_or_default()
    };
    assert_eq!(state("tests"), "succeeded", "{job}");
    assert_ne!(state("svc"), "failed", "{job}");
}

/// The newest attempt of a job view; earlier ones may be offers the worker
/// rejected before anything started.
fn last_attempt(job: &Value) -> &Value {
    job["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .expect("the job has an attempt")
}
