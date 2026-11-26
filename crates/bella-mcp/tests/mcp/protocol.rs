//! Lifecycle, version negotiation and JSON-RPC error handling.

use crate::common::{Client, FakeFleet, check_schema, job_view, options};
use bella_core::JobId;
use bella_core::job::JobPhase;
use bella_mcp::{SUPPORTED_PROTOCOL_VERSIONS, TOOL_NAMES};
use serde_json::{Value, json};
use std::time::Duration;

const HANDSHAKE_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const STATELESS: &str = "2026-07-28";

fn start() -> (Client, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let client = Client::start(FakeFleet::new(), options(root.path()));
    (client, root)
}

fn stateless_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": STATELESS,
        "io.modelcontextprotocol/clientInfo": {"name": "test-client", "version": "1.0.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn error_code(reply: &Value) -> i64 {
    reply["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("not an error: {reply}"))
}

#[test]
fn the_supported_versions_are_the_documented_ones() {
    let mut expected = vec![STATELESS];
    expected.extend(HANDSHAKE_VERSIONS);
    assert_eq!(SUPPORTED_PROTOCOL_VERSIONS.to_vec(), expected);
}

#[test]
fn handshake_works_for_every_handshake_revision() {
    for version in HANDSHAKE_VERSIONS {
        let root = tempfile::tempdir().unwrap();
        let fleet = FakeFleet::new();
        let job = fleet.add_job(job_view(&JobId::generate(), JobPhase::Executing));
        let mut client = Client::start(fleet, options(root.path()));

        let reply = client.initialize(version);
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], version);
        assert!(result["capabilities"]["tools"].is_object(), "{reply}");
        assert_eq!(result["serverInfo"]["name"], "builderella");
        assert!(result["serverInfo"]["version"].is_string());
        let instructions = result["instructions"].as_str().unwrap();
        assert!(instructions.contains("instead of running it on this machine"));
        assert!(instructions.contains("Do not fall back to an unbounded local build"));
        assert!(result.get("resultType").is_none(), "{version}: {reply}");

        assert_eq!(client.request("ping", json!({}))["result"], json!({}));

        let listed = client.request("tools/list", json!({}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES, "{version}");
        let structured = version >= "2025-06-18";
        for tool in tools {
            assert!(tool["description"].as_str().unwrap().len() > 40);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool.get("outputSchema").is_some(), structured, "{version}");
            assert_eq!(tool.get("title").is_some(), structured, "{version}");
            assert_eq!(
                tool.get("annotations").is_some(),
                version >= "2025-03-26",
                "{version}"
            );
        }

        let result = client.call("get_job", json!({"job_id": job.as_str()}));
        assert_eq!(result["isError"], false);
        assert!(crate::common::text(&result).contains("phase: executing"));
        assert_eq!(result.get("structuredContent").is_some(), structured, "{version}");

        let (served, rest) = client.close();
        assert!(served.is_ok());
        assert!(rest.is_empty(), "{rest:?}");
    }
}

#[test]
fn an_unknown_version_is_answered_with_the_newest_handshake_revision() {
    for requested in ["1999-01-01", "2030-01-01", STATELESS] {
        let (mut client, _root) = start();
        let reply = client.initialize(requested);
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25", "{requested}");
        // The session then runs under the revision that was answered.
        let listed = client.request("tools/list", json!({}));
        assert!(listed["result"]["tools"][0].get("outputSchema").is_some());
    }
}

#[test]
fn initialize_needs_a_version_and_happens_once() {
    let (mut client, _root) = start();
    let reply = client.request("initialize", json!({"capabilities": {}}));
    assert_eq!(error_code(&reply), -32602);
    // The failed attempt did not initialise anything.
    assert_eq!(error_code(&client.request("tools/list", json!({}))), -32600);

    client.initialize("2025-06-18");
    let again = client.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
    );
    assert_eq!(error_code(&again), -32600);
    assert!(again["error"]["message"].as_str().unwrap().contains("already initialized"));
}

#[test]
fn requests_before_initialize_are_refused() {
    let (mut client, _root) = start();
    for (method, params) in [
        ("tools/list", json!({})),
        ("tools/call", json!({"name": "list_fleet", "arguments": {}})),
        ("server/discover", json!({})),
    ] {
        let reply = client.request(method, params);
        assert_eq!(error_code(&reply), -32600, "{method}: {reply}");
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(message.contains("not initialized"), "{message}");
    }
    // Pings are allowed at any time; unknown methods stay unknown.
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert_eq!(error_code(&client.request("resources/list", json!({}))), -32601);
    assert_eq!(error_code(&client.request("no/such", json!({}))), -32601);
}

#[test]
fn unknown_methods_after_initialize_are_method_not_found() {
    let (mut client, _root) = start();
    client.initialize("2025-06-18");
    for method in ["prompts/list", "resources/read", "logging/setLevel", "bogus"] {
        let reply = client.request(method, json!({}));
        assert_eq!(error_code(&reply), -32601, "{method}");
    }
}

#[test]
fn stateless_revision_needs_no_handshake() {
    let (mut client, _root) = start();
    client.meta = Some(stateless_meta());

    let discovered = client.request("server/discover", json!({}));
    let result = &discovered["result"];
    assert_eq!(result["resultType"], "complete", "{discovered}");
    assert_eq!(result["supportedVersions"], json!(SUPPORTED_PROTOCOL_VERSIONS));
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "builderella"
    );
    assert!(result["instructions"].as_str().unwrap().contains("fleet"));
    assert!(result["ttlMs"].as_u64().is_some());
    assert_eq!(result["cacheScope"], "public");

    let listed = client.request("tools/list", json!({}));
    let result = &listed["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["tools"].as_array().unwrap().len(), 12);
    assert!(result["ttlMs"].as_u64().is_some());
    assert_eq!(result["cacheScope"], "public");
    assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());

    let called = client.request("tools/call", json!({"name": "list_fleet", "arguments": {}}));
    let result = &called["result"];
    assert_eq!(result["resultType"], "complete", "{called}");
    assert_eq!(result["isError"], false);
    assert!(result["structuredContent"].is_object());
    assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());

    // A tool failure is still a complete result in this revision.
    let failed = client.request("tools/call", json!({"name": "get_job", "arguments": {}}));
    assert_eq!(failed["result"]["resultType"], "complete");
    assert_eq!(failed["result"]["isError"], true);

    // `ping` does not exist in the stateless revision.
    assert_eq!(error_code(&client.request("ping", json!({}))), -32601);
}

#[test]
fn stateless_requests_are_validated_one_by_one() {
    let (mut client, _root) = start();

    client.meta = Some(json!({
        "io.modelcontextprotocol/protocolVersion": "2031-01-01",
        "io.modelcontextprotocol/clientCapabilities": {}
    }));
    let unsupported = client.request("tools/list", json!({}));
    assert_eq!(error_code(&unsupported), -32022, "{unsupported}");
    assert_eq!(unsupported["error"]["data"]["requested"], "2031-01-01");
    assert_eq!(
        unsupported["error"]["data"]["supported"],
        json!(SUPPORTED_PROTOCOL_VERSIONS)
    );
    assert_eq!(error_code(&client.request("server/discover", json!({}))), -32022);

    client.meta = Some(json!({"io.modelcontextprotocol/protocolVersion": STATELESS}));
    let incomplete = client.request("tools/list", json!({}));
    assert_eq!(error_code(&incomplete), -32602, "{incomplete}");

    client.meta = Some(json!({"io.modelcontextprotocol/protocolVersion": 20260728}));
    assert_eq!(error_code(&client.request("tools/list", json!({}))), -32602);

    // A handshake revision named per request does not replace the handshake.
    client.meta = Some(json!({
        "io.modelcontextprotocol/protocolVersion": "2025-11-25",
        "io.modelcontextprotocol/clientCapabilities": {}
    }));
    assert_eq!(error_code(&client.request("tools/list", json!({}))), -32600);

    // Nothing above left any state behind.
    client.meta = None;
    assert_eq!(error_code(&client.request("tools/list", json!({}))), -32600);
}

#[test]
fn both_styles_are_served_on_one_connection() {
    let (mut client, _root) = start();
    client.initialize("2025-06-18");
    let handshake_style = client.request("tools/list", json!({}));
    assert!(handshake_style["result"].get("resultType").is_none());
    assert!(handshake_style["result"].get("ttlMs").is_none());

    client.meta = Some(stateless_meta());
    let stateless_style = client.request("tools/list", json!({}));
    assert_eq!(stateless_style["result"]["resultType"], "complete");

    client.meta = None;
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    // Discovery always needs the per-request metadata.
    assert_eq!(error_code(&client.request("server/discover", json!({}))), -32602);
}

#[test]
fn malformed_json_is_a_parse_error_without_an_id() {
    let (mut client, _root) = start();
    for garbage in [
        &b"{not json\n"[..],
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"\n",
        b"\xff\xfe{}\n",
        b"}\n",
    ] {
        client.send_bytes(garbage);
        let reply = client.next();
        assert_eq!(error_code(&reply), -32700, "{reply}");
        assert!(reply.get("id").is_none(), "{reply}");
    }
    // The connection survives all of it.
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn invalid_requests_are_rejected_and_the_connection_survives() {
    let (mut client, _root) = start();
    let cases = [
        (json!(42), None),
        (json!("ping"), None),
        (json!({"jsonrpc": "1.0", "id": 5, "method": "ping"}), Some(json!(5))),
        (json!({"id": 6, "method": "ping"}), Some(json!(6))),
        (json!({"jsonrpc": "2.0", "id": null, "method": "ping"}), None),
        (json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}), None),
        (json!({"jsonrpc": "2.0", "id": [1], "method": "ping"}), None),
        (json!({"jsonrpc": "2.0", "id": 7}), Some(json!(7))),
        (json!({"jsonrpc": "2.0", "id": 8, "method": 9}), Some(json!(8))),
        (json!([]), None),
        (json!([{"jsonrpc": "2.0", "id": 9, "method": "ping"}]), None),
    ];
    for (message, id) in cases {
        client.send(&message);
        let reply = client.next();
        assert_eq!(error_code(&reply), -32600, "{message} -> {reply}");
        assert_eq!(reply.get("id"), id.as_ref(), "{message} -> {reply}");
    }
    client.send(&json!({"jsonrpc": "2.0", "id": 10, "method": "ping", "params": [1]}));
    assert_eq!(error_code(&client.next()), -32602);

    // String ids work and are echoed exactly.
    client.send(&json!({"jsonrpc": "2.0", "id": "abc", "method": "ping"}));
    assert_eq!(client.next(), json!({"jsonrpc": "2.0", "id": "abc", "result": {}}));
}

#[test]
fn blank_lines_and_crlf_are_tolerated() {
    let (mut client, _root) = start();
    client.send_bytes(b"\n   \n\r\n");
    client.send_bytes(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n");
    assert_eq!(client.next()["id"], 1);
    // A message may arrive in pieces, and the last one may lack a newline.
    client.send_bytes(b"{\"jsonrpc\":\"2.0\",");
    client.send_bytes(b"\"id\":2,\"method\":\"ping\"}");
    let (served, rest) = client.close();
    assert!(served.is_ok());
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["id"], 2);
}

#[test]
fn an_oversized_message_is_refused_without_ending_the_session() {
    let (mut client, _root) = start();
    let mut huge = Vec::with_capacity(5 * 1024 * 1024);
    huge.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{\"x\":\"");
    huge.resize(5 * 1024 * 1024, b'a');
    huge.extend_from_slice(b"\"}}\n");
    client.send_bytes(&huge);
    let reply = client.next();
    assert_eq!(error_code(&reply), -32600, "{reply}");
    assert!(reply["error"]["message"].as_str().unwrap().contains("exceeds"));
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn notifications_are_never_answered() {
    let (mut client, _root) = start();
    client.initialize("2025-11-25");
    client.notify("notifications/initialized", json!({}));
    client.notify("notifications/something/unknown", json!({"a": 1}));
    client.notify("notifications/cancelled", json!({"requestId": 999, "reason": "nothing there"}));
    client.notify("notifications/cancelled", json!({}));
    // A request method without an id is a notification too.
    client.notify("tools/list", json!({}));
    client.send(&json!({"jsonrpc": "2.0", "method": "tools/call", "params": [1]}));
    // A stray response is ignored.
    client.send(&json!({"jsonrpc": "2.0", "id": 77, "result": {}}));
    client.send(&json!({"jsonrpc": "2.0", "id": 78, "error": {"code": -1, "message": "x"}}));

    let id = client.begin("ping", json!({}));
    // The very next message is the answer to the ping: nothing was sent
    // for anything above.
    assert_eq!(client.next()["id"], id);
}

#[test]
fn tool_call_protocol_errors_are_distinct_from_tool_errors() {
    let (mut client, _root) = start();
    client.initialize("2025-11-25");

    let unknown = client.request("tools/call", json!({"name": "set_machine_policy", "arguments": {}}));
    assert_eq!(error_code(&unknown), -32602);
    assert!(unknown["error"]["message"].as_str().unwrap().starts_with("Unknown tool"));

    assert_eq!(error_code(&client.request("tools/call", json!({}))), -32602);
    assert_eq!(error_code(&client.request("tools/call", json!({"name": 5}))), -32602);
    let bad_arguments = client.request("tools/call", json!({"name": "list_fleet", "arguments": [1]}));
    assert_eq!(error_code(&bad_arguments), -32602);
    assert_eq!(
        error_code(&client.request("tools/list", json!({"cursor": "page-2"}))),
        -32602
    );

    // Arguments that are missing or wrong are for the model to fix: a
    // result with isError, not a protocol error.
    let omitted = client.request("tools/call", json!({"name": "get_job"}));
    assert_eq!(omitted["result"]["isError"], true, "{omitted}");
    let explicit_null = client.request("tools/call", json!({"name": "list_fleet", "arguments": null}));
    assert_eq!(explicit_null["result"]["isError"], false, "{explicit_null}");
}

#[test]
fn batches_are_answered_as_one_array_under_2025_03_26() {
    let root = tempfile::tempdir().unwrap();
    let fleet = FakeFleet::new();
    let job = fleet.add_job(job_view(&JobId::generate(), JobPhase::Queued));
    let mut client = Client::initialized(fleet, options(root.path()), "2025-03-26");

    client.send(&json!([
        {"jsonrpc": "2.0", "id": 1, "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "get_job", "arguments": {"job_id": job.as_str()}}},
        5,
        {"jsonrpc": "2.0", "id": 3, "method": "nope"},
        {"jsonrpc": "2.0", "id": 90, "result": {}}
    ]));
    let reply = client.next();
    let items = reply.as_array().unwrap_or_else(|| panic!("not a batch: {reply}"));
    assert_eq!(items.len(), 4, "{reply}");
    let by_id = |id: i64| items.iter().find(|item| item["id"] == id).unwrap();
    assert_eq!(by_id(1)["result"], json!({}));
    assert_eq!(by_id(2)["result"]["isError"], false);
    assert_eq!(by_id(3)["error"]["code"], -32601);
    let unidentified: Vec<&Value> = items.iter().filter(|item| item.get("id").is_none()).collect();
    assert_eq!(unidentified.len(), 1);
    assert_eq!(unidentified[0]["error"]["code"], -32600);

    // A batch of notifications gets no reply at all.
    client.send(&json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]));
    let id = client.begin("ping", json!({}));
    assert_eq!(client.next()["id"], id);

    client.send(&json!([]));
    assert_eq!(error_code(&client.next()), -32600);
}

#[test]
fn batches_are_refused_under_other_revisions() {
    for version in ["2024-11-05", "2025-06-18", "2025-11-25"] {
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::initialized(FakeFleet::new(), options(root.path()), version);
        client.send(&json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]));
        let reply = client.next();
        assert_eq!(error_code(&reply), -32600, "{version}: {reply}");
    }
}

#[test]
fn a_request_id_in_use_cannot_be_reused_until_it_is_answered() {
    let root = tempfile::tempdir().unwrap();
    let fleet = FakeFleet::new();
    let job = fleet.add_job(job_view(&JobId::generate(), JobPhase::Executing));
    let mut client = Client::initialized(fleet.clone(), options(root.path()), "2025-11-25");

    let call = json!({"jsonrpc": "2.0", "id": "w", "method": "tools/call",
        "params": {"name": "wait_job", "arguments": {"job_id": job.as_str(), "timeout_seconds": 30}}});
    client.send(&call);
    fleet.wait_until_waiting(1);
    client.send(&call);
    let duplicate = client.next();
    assert_eq!(error_code(&duplicate), -32600, "{duplicate}");

    fleet.finish_job(&job, bella_core::job::Outcome::Succeeded);
    let answered = client.response(&json!("w"));
    assert_eq!(answered["result"]["structuredContent"]["timed_out"], false);

    // Once answered, the id is free again.
    client.send(&call);
    assert_eq!(client.response(&json!("w"))["result"]["isError"], false);
}

#[test]
fn published_schemas_are_valid_and_match_sample_inputs() {
    let (mut client, _root) = start();
    client.initialize("2025-11-25");
    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for key in ["inputSchema", "outputSchema"] {
            let schema = &tool[key];
            assert_eq!(schema["type"], "object", "{name}.{key}");
            assert!(schema["properties"].is_object(), "{name}.{key}");
            walk(schema, &format!("{name}.{key}"));
        }
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
    }

    let schema = |name: &str| {
        tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"].clone()
    };
    let id = "job_0190f3a2b4c57d8e9f00112233445566";
    let valid = [
        ("submit_job", json!({"recipe": "test"})),
        ("submit_job", json!({"command": "cargo test", "priority": "interactive", "platform": "linux", "require": true})),
        ("submit_job", json!({"command": ["cargo", "test"], "env": {"CI": "1"}, "cpu": 4, "memory": "8GiB", "idempotency_key": "retry-0001"})),
        ("plan_job", json!({"recipe": "build", "machine": "mini"})),
        ("wait_job", json!({"job_id": id, "timeout_seconds": 30, "after_event": 12})),
        ("read_logs", json!({"job_id": id, "step": "build", "tail": true})),
        ("get_artifacts", json!({"job_id": id, "download": true, "dest": "/tmp/out", "paths": ["bin/app"]})),
        ("renew_environment", json!({"environment_id": "env_0190f3a2b4c57d8e9f00112233445566"})),
        ("list_fleet", json!({})),
    ];
    for (name, sample) in valid {
        check_schema(&schema(name), &sample, name).unwrap_or_else(|e| panic!("{sample}: {e}"));
    }
    let invalid = [
        ("submit_job", json!({"recipe": "test", "wait": true})),
        ("submit_job", json!({"command": 5})),
        ("submit_job", json!({"command": []})),
        ("submit_job", json!({"recipe": "test", "priority": "highest"})),
        ("submit_job", json!({"recipe": "test", "env": {"A": 1}})),
        ("submit_job", json!({"recipe": "test", "cpu": 0})),
        ("wait_job", json!({"timeout_seconds": 5})),
        ("wait_job", json!({"job_id": id, "timeout_seconds": -1})),
        ("wait_job", json!({"job_id": id, "timeout_seconds": "30"})),
        ("read_logs", json!({"job_id": id, "tail": "yes"})),
        ("get_artifacts", json!({"job_id": id, "paths": []})),
        ("list_fleet", json!({"verbose": true})),
        ("cancel_job", json!({})),
    ];
    for (name, sample) in invalid {
        assert!(check_schema(&schema(name), &sample, name).is_err(), "{name} accepted {sample}");
        // The server enforces the same contract it publishes.
        let result = client.call(name, sample.clone());
        assert_eq!(result["isError"], true, "{name} ran with {sample}");
        let text = crate::common::text(&result);
        assert!(text.starts_with(&format!("Invalid arguments for {name}")), "{text}");
    }
}

/// Every schema node is an object with a `type`, uses only plain JSON Schema
/// keywords, and `required` names only declared properties.
fn walk(schema: &Value, at: &str) {
    const KEYWORDS: [&str; 16] = [
        "type", "description", "title", "default", "enum", "properties", "required",
        "additionalProperties", "maxProperties", "items", "minItems", "maxItems", "minLength",
        "maxLength", "minimum", "maximum",
    ];
    let object = schema.as_object().unwrap_or_else(|| panic!("{at} is not an object"));
    assert!(object.contains_key("type"), "{at} has no type");
    for keyword in object.keys() {
        assert!(KEYWORDS.contains(&keyword.as_str()), "{at}: keyword `{keyword}`");
    }
    let properties = object.get("properties").and_then(Value::as_object);
    for name in object.get("required").and_then(Value::as_array).into_iter().flatten() {
        let name = name.as_str().unwrap();
        assert!(
            properties.is_some_and(|known| known.contains_key(name)),
            "{at}: required `{name}` is not declared"
        );
    }
    for (name, sub) in properties.into_iter().flatten() {
        walk(sub, &format!("{at}.{name}"));
    }
    if let Some(items) = object.get("items") {
        walk(items, &format!("{at}[]"));
    }
    if let Some(extra) = object.get("additionalProperties").filter(|extra| extra.is_object()) {
        walk(extra, &format!("{at}{{}}"));
    }
}

#[test]
fn nothing_is_written_unprompted() {
    let (mut client, _root) = start();
    assert!(client.next_within(Duration::from_millis(100)).is_none());
    let (served, rest) = client.close();
    assert!(served.is_ok());
    assert!(rest.is_empty());
}
