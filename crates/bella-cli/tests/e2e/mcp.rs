//! `bella mcp` driven over its standard streams as an agent would.

use crate::support::{Fleet, McpSession, Options, script};
use serde_json::{Value, json};

/// A21 and the MCP tools end to end: initialize, list the tools, plan,
/// submit, wait, read the log and download the artifact into an allowed
/// root. Standard output carries only JSON-RPC, and closing the session
/// cancels nothing.
#[test]
fn a21_mcp_plans_submits_waits_reads_logs_and_downloads_over_stdio() {
    let fleet = Fleet::start(Options::default());
    let build = script(
        "mkdir -p out && printf 'mcp-artifact' > out/a.txt && echo from-the-mcp-job",
        r"mkdir out & (echo|set /p=mcp-artifact) > out\a.txt & echo from-the-mcp-job",
    );
    let repo = fleet.repo(
        "agent",
        &[(
            "builderella.toml",
            &format!(
                "[project]\nname = \"agent\"\n\n[recipes.build]\nartifacts = [\"out/a.txt\"]\nrun = {build}\n"
            ),
        )],
    );
    let mut session = McpSession::start(&fleet, &repo);

    let init = session.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "bella-e2e", "version": "1" }
        }),
    );
    assert_eq!(init["protocolVersion"], "2025-06-18");
    session.notify("notifications/initialized", json!({}));

    let tools = session.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for wanted in [
        "plan_job",
        "submit_job",
        "wait_job",
        "read_logs",
        "get_artifacts",
        "cancel_job",
    ] {
        assert!(names.contains(&wanted), "{wanted} is offered: {names:?}");
    }

    let plan = session.call("plan_job", json!({ "recipe": "build" }));
    assert_eq!(plan["feasible"], true, "{plan}");

    let submitted = session.call("submit_job", json!({ "recipe": "build" }));
    let id = submitted["job_id"].as_str().expect("a job id").to_string();
    assert!(
        submitted["dashboard_url"]
            .as_str()
            .is_some_and(|link| link.ends_with(&format!("/#/jobs/{id}"))),
        "{submitted}"
    );

    let mut waited = Value::Null;
    for _ in 0..10 {
        waited = session.call("wait_job", json!({ "job_id": id, "timeout_seconds": 20 }));
        if waited["timed_out"] == false {
            break;
        }
    }
    assert_eq!(waited["timed_out"], false, "{waited}");
    assert_eq!(waited["outcome"]["type"], "succeeded", "{waited}");

    let logs = session.call("read_logs", json!({ "job_id": id }));
    let text: String = logs["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["text"].as_str())
        .collect();
    assert!(text.contains("from-the-mcp-job"), "{logs}");

    let dest = repo.join("fetched");
    std::fs::create_dir_all(&dest).unwrap();
    let artifacts = session.call(
        "get_artifacts",
        json!({ "job_id": id, "download": true, "dest": dest.display().to_string() }),
    );
    assert_eq!(artifacts["downloaded_count"], 1, "{artifacts}");
    let bytes = std::fs::read(dest.join("out").join("a.txt")).expect("downloaded");
    assert_eq!(bytes, b"mcp-artifact");

    // A download outside the allowed roots is refused.
    let outside = fleet.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let refused = session.request(
        "tools/call",
        json!({
            "name": "get_artifacts",
            "arguments": { "job_id": id, "download": true, "dest": outside.display().to_string() }
        }),
    );
    assert_eq!(refused["isError"], true, "{refused}");

    // Every line on standard output was a JSON-RPC message.
    assert!(!session.transcript.is_empty());
    for line in &session.transcript {
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
    }
    assert_eq!(session.close(), 0);

    // Closing the session cancelled nothing.
    let job = fleet.job(&id);
    assert_eq!(job["cancel_requested"], false);
    assert_eq!(job["outcome"]["type"], "succeeded");
}
