//! Jobs across cancellation and restarts of the worker and the controller,
//! and the machine owner's offline pause.

use crate::support::{Fleet, Options, PATIENCE, eventually, sleep_script};
use serde_json::Value;
use std::time::{Duration, Instant};

fn sleeper_repo(fleet: &Fleet, name: &str, secs: u32, extra: &str) -> std::path::PathBuf {
    let run = sleep_script(secs, "sleeper-started");
    fleet.repo(
        name,
        &[(
            "builderella.toml",
            &format!("[project]\nname = \"{name}\"\n\n[recipes.sleep]\n{extra}run = {run}\n"),
        )],
    )
}

fn submit(fleet: &Fleet, repo: &std::path::Path) -> String {
    let document = fleet
        .bella_in(repo, &["--json", "submit"], None)
        .ok()
        .json();
    document["job"]["id"].as_str().unwrap().to_string()
}

/// Wait until the job's current attempt has started its step.
fn wait_until_running(fleet: &Fleet, id: &str) -> Value {
    eventually(&format!("job {id} to run its step"), PATIENCE, || {
        let job = fleet.job(id);
        let attempt = job["attempts"].as_array()?.last()?.clone();
        let running = attempt["steps"]
            .as_array()?
            .iter()
            .any(|step| step["state"] == "running");
        running.then_some(job)
    })
}

/// A11/A13-style cancellation end to end: cancelling a running job is a
/// request; the worker stops the step and cleans up, and only then is the
/// job cancelled and its resources released.
#[test]
fn a11_cancel_of_a_running_job_ends_after_the_workers_cleanup() {
    let fleet = Fleet::start(Options::default());
    let repo = sleeper_repo(&fleet, "cancel", 120, "");
    let id = submit(&fleet, &repo);
    wait_until_running(&fleet, &id);

    let cancelled = fleet.bella_json(&["cancel", &id]);
    assert_eq!(cancelled["cancel_requested"], true);

    let started = Instant::now();
    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "1m"]);
    assert_eq!(
        waited.code, 1,
        "a cancelled job is not a success: {}",
        waited.stderr
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    let job = waited.json()["job"].clone();
    assert_eq!(job["phase"], "finished");
    assert_eq!(job["outcome"]["type"], "cancelled");
    assert_eq!(job["cleanup"], "released");
    // The newest attempt is the one that ran; an earlier one may be an
    // offer the worker rejected.
    let attempt = job["attempts"].as_array().unwrap().last().unwrap();
    assert_eq!(attempt["cleanup"], "released");
    assert!(attempt["released_at"].is_number());

    // The machine holds nothing any more.
    let fleet_view = fleet.fleet_view();
    let machine = &fleet_view["machines"][0];
    assert_eq!(machine["reserved"]["cpu_milli"], 0, "{machine}");
    assert!(machine["attempts"].as_array().unwrap().is_empty());
}

/// A16/A17-style worker restart end to end: a worker killed mid-job leaves
/// its attempt held, not released; once it starts again it stops the old
/// process, reports the attempt interrupted and proves the release. The
/// retry runs only after that, so the job never runs twice at once.
#[test]
fn a16_a_worker_killed_mid_job_releases_the_attempt_only_after_its_restart() {
    let mut fleet = Fleet::start(Options::default());
    let repo = sleeper_repo(
        &fleet,
        "restart",
        6,
        "max_attempts = 2\ncpu = 1\nmemory = \"128MiB\"\n",
    );
    let id = submit(&fleet, &repo);
    wait_until_running(&fleet, &id);

    fleet.kill_worker(0);
    // Without the worker, nothing can prove the process is gone: the
    // attempt stays held.
    std::thread::sleep(Duration::from_secs(3));
    let job = fleet.job(&id);
    assert_ne!(job["phase"], "finished", "{job}");
    let attempts = job["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0]["released_at"].is_null(), "{job}");
    let machine = fleet.fleet_view()["machines"][0].clone();
    assert_eq!(machine["reserved"], job["envelope"]["peak"], "{machine}");
    assert_eq!(machine["attempts"][0]["job_id"], id.as_str(), "{machine}");

    fleet.start_worker(0);
    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "2m"]);
    let job = waited.json()["job"].clone();
    let attempts = job["attempts"].as_array().unwrap().clone();
    assert_eq!(
        attempts[0]["outcome"]["type"], "infra_interrupted",
        "the first attempt was interrupted: {job}"
    );
    let first_released = attempts[0]["released_at"]
        .as_u64()
        .expect("the first attempt was released");
    if let Some(second) = attempts.get(1) {
        let second_offered = second["offered_at"].as_u64().unwrap();
        assert!(
            second_offered >= first_released,
            "the retry was offered only after the first attempt's release: {job}"
        );
        assert_eq!(waited.code, 0, "the retry succeeded: {job}");
        assert_eq!(job["outcome"]["type"], "succeeded");
    } else {
        assert_eq!(waited.code, 1, "{job}");
        assert_eq!(job["outcome"]["type"], "infra_interrupted");
    }
    assert_eq!(job["cleanup"], "released");
}

/// A60 end to end: the controller restarts while a worker is connected and
/// a job is running. The new epoch reconciles the worker before offering it
/// anything: the running attempt continues under the same generation, no
/// second attempt is created, and new work runs afterwards.
#[test]
fn a60_a_restarted_controller_reconciles_before_any_new_offer() {
    let mut fleet = Fleet::start(Options::default());
    let repo = sleeper_repo(&fleet, "reconcile", 6, "");
    let id = submit(&fleet, &repo);
    wait_until_running(&fleet, &id);
    let epoch_before = fleet.fleet_view()["controller_epoch"].as_u64().unwrap();

    fleet.restart_controller();
    let view = fleet.fleet_view();
    assert_eq!(view["controller_epoch"].as_u64().unwrap(), epoch_before + 1);
    fleet.wait_for_connected(1);

    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "2m"]);
    assert_eq!(waited.code, 0, "{}\n{}", waited.stderr, fleet.daemon_logs());
    let job = waited.json()["job"].clone();
    let attempts = job["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1, "no second attempt was offered: {job}");
    assert_eq!(attempts[0]["generation"], 1);
    assert_eq!(job["outcome"]["type"], "succeeded");

    // New work is offered once the worker has reconciled.
    let quick = sleeper_repo(&fleet, "after-restart", 0, "");
    let run = fleet.bella_in(&quick, &["submit", "--wait"], None);
    assert_eq!(run.code, 0, "{}", run.stderr);
}

/// `bella worker pause` works with the daemon stopped; once the worker is
/// back, it is offered nothing until it is resumed.
#[test]
fn a_pause_made_offline_stops_new_offers_after_the_worker_reconnects() {
    let mut fleet = Fleet::start(Options::default());
    fleet.stop_worker(0);
    let dir = fleet.worker_dir(0);
    let paused = fleet.bella_json(&["worker", "pause", "--data-dir", &dir]);
    assert_eq!(paused["paused"], true);
    fleet.start_worker(0);
    fleet.wait_for_connected(1);

    let repo = sleeper_repo(&fleet, "paused", 0, "");
    let id = submit(&fleet, &repo);
    let machine = eventually("the pause to reach the controller", PATIENCE, || {
        let view = fleet.fleet_view();
        let machine = view["machines"][0].clone();
        (machine["local_control"]["paused"] == true).then_some(machine)
    });
    assert_eq!(machine["connected"], true);
    std::thread::sleep(Duration::from_secs(3));
    let job = fleet.job(&id);
    assert_eq!(
        job["phase"], "queued",
        "nothing is offered to a paused worker: {job}"
    );
    assert!(job["attempts"].as_array().unwrap().is_empty());
    let explained = fleet.bella_json(&["explain", &id]);
    let text = explained.to_string();
    assert!(
        text.contains("machine_paused") || text.contains("paused"),
        "{text}"
    );

    fleet.bella_json(&["worker", "resume", "--data-dir", &dir]);
    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "1m"]);
    assert_eq!(waited.code, 0, "{}", waited.stderr);
    let status = fleet.bella_json(&["worker", "status", "--data-dir", &dir]);
    assert_eq!(status["local_control"]["paused"], false);
}

/// Ctrl-C during `bella wait` stops waiting only: exit 130, the job id and
/// the cancel command are printed, and the job keeps running to its end.
/// Unix only: a console Ctrl-C cannot be sent to one child process on
/// Windows from a test.
#[cfg(unix)]
#[test]
fn a21_ctrl_c_during_a_wait_stops_waiting_and_cancels_nothing() {
    let fleet = Fleet::start(Options::default());
    let repo = sleeper_repo(&fleet, "interrupt", 5, "");
    let id = submit(&fleet, &repo);
    wait_until_running(&fleet, &id);

    let mut waiting = fleet.spawn("wait", &["wait", &id]);
    std::thread::sleep(Duration::from_secs(1));
    waiting.interrupt();
    let code = waiting.wait_exit(Duration::from_secs(20));
    let output = waiting.output();
    assert_eq!(code, Some(130), "{output}");
    assert!(output.contains(&format!("bella cancel {id}")), "{output}");
    assert!(output.contains("keeps running"), "{output}");

    let job = fleet.job(&id);
    assert_eq!(job["cancel_requested"], false, "{job}");
    let waited = fleet.bella(&["--json", "wait", &id, "--timeout", "1m"]);
    assert_eq!(waited.code, 0, "{}", waited.stderr);
    assert_eq!(waited.json()["job"]["outcome"]["type"], "succeeded");
}
