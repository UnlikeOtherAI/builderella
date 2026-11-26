//! The stdio server: framing, lifecycle, dispatch and shutdown.
//!
//! One thread reads newline-delimited JSON-RPC messages. Each `tools/call`
//! runs on its own thread, so a long `wait_job` never blocks another call.
//! Writes to the output are serialised and are the only thing ever written
//! there; diagnostics go to standard error.
//!
//! The server owns no build lifetime. End of input, a broken output pipe or
//! a `notifications/cancelled` message stop the server or one pending call;
//! none of them reaches [`FleetOps::cancel`].

use crate::diag;
use crate::ops::FleetOps;
use crate::paths::Roots;
use crate::rpc::{
    self, META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, META_SERVER_INFO, Revision, RpcError,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::sanitize::inline;
use crate::tools::{self, Call, Config, Finished, INSTRUCTIONS};
use bella_core::api::SessionInfo;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Largest accepted message. Longer lines are discarded and answered with
/// an error instead of being buffered.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// Tool calls that may run at once; further calls are refused, not queued.
const MAX_CONCURRENT_CALLS: usize = 32;
/// How long `serve` lets calls that are already running finish after the
/// input ended, so that their replies are not lost.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Hard ceiling for `ServerOptions::max_log_bytes`.
const LOG_BYTES_CEILING: u64 = 1024 * 1024;
const LOG_BYTES_FLOOR: u64 = 256;
/// How long clients may cache the tool list and the discovery result. Both
/// are fixed for the life of the process.
const CACHE_TTL_MS: u64 = 3_600_000;

/// How the server is confined and tuned. The `bella mcp` command fills this
/// in from its flags.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Directories the agent may name as `workspace` or `dest`. Every path
    /// argument is resolved through the filesystem and must end up inside
    /// one of these. The first one is the default workspace.
    pub allowed_roots: Vec<PathBuf>,
    /// Attribution attached to every submission. It grants nothing.
    pub session: SessionInfo,
    /// Longest a single `wait_job` call blocks. Keep it below the client's
    /// tool timeout.
    pub max_wait: Duration,
    /// Most log text one `read_logs` call returns, in bytes. Values outside
    /// 256 bytes to 1 MiB are clamped.
    pub max_log_bytes: u64,
    /// Base URL of the dashboard, for links in results. `{job}` is replaced
    /// by the job id when present; otherwise `/jobs/<id>` is appended.
    pub dashboard_url: Option<String>,
}

impl ServerOptions {
    /// Default cap for one `wait_job` call: under the 60-second tool
    /// timeout common in clients.
    pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(50);
    /// Default page size for `read_logs`.
    pub const DEFAULT_MAX_LOG_BYTES: u64 = 24 * 1024;

    /// Options with the default limits and no dashboard link.
    pub fn new(allowed_roots: Vec<PathBuf>, session: SessionInfo) -> Self {
        Self {
            allowed_roots,
            session,
            max_wait: Self::DEFAULT_MAX_WAIT,
            max_log_bytes: Self::DEFAULT_MAX_LOG_BYTES,
            dashboard_url: None,
        }
    }
}

/// Serve MCP over `input` and `output` until the input ends.
///
/// Returns `Ok(())` at end of input and the error when reading fails.
/// Before returning it gives calls that are still running a few seconds to
/// reply, then closes the output; later replies are dropped. It never
/// cancels a job: jobs submitted through this server keep running after it
/// returns.
pub fn serve(
    mut input: impl BufRead + Send,
    output: impl Write + Send + 'static,
    fleet: Arc<dyn FleetOps>,
    options: ServerOptions,
) -> io::Result<()> {
    let roots = Roots::new(&options.allowed_roots);
    let config = Config {
        roots,
        session: options.session,
        max_wait: options.max_wait,
        wait_slice: (options.max_wait / 20)
            .clamp(Duration::from_millis(100), Duration::from_secs(2)),
        max_log_bytes: options
            .max_log_bytes
            .clamp(LOG_BYTES_FLOOR, LOG_BYTES_CEILING) as usize,
        dashboard_url: options.dashboard_url,
    };
    let server = Arc::new(Server {
        out: Mutex::new(Some(Box::new(output))),
        fleet,
        config,
        shutdown: AtomicBool::new(false),
        session: Mutex::new(None),
        calls: Mutex::new(HashMap::new()),
        active: Mutex::new(0),
        idle: Condvar::new(),
    });

    let mut line = Vec::new();
    let result = loop {
        match read_line(&mut input, &mut line, MAX_MESSAGE_BYTES) {
            Ok(LineRead::Eof) => break Ok(()),
            Ok(LineRead::TooLong) => server.send(&rpc::error_message(
                None,
                &RpcError::invalid_request(format!(
                    "Invalid Request: message exceeds {MAX_MESSAGE_BYTES} bytes"
                )),
            )),
            Ok(LineRead::Line) => Server::handle_line(&server, &line),
            Err(error) => break Err(error),
        }
    };

    // The client is gone. Stop pending waits, let running calls finish
    // briefly, and close the output. Nothing here touches any job.
    server.shutdown.store(true, Ordering::SeqCst);
    {
        let active = lock(&server.active);
        let _ = server
            .idle
            .wait_timeout_while(active, SHUTDOWN_GRACE, |running| *running > 0)
            .unwrap_or_else(PoisonError::into_inner);
    }
    *lock(&server.out) = None;
    result
}

enum LineRead {
    Line,
    TooLong,
    Eof,
}

/// Read up to the next newline into `line` without ever holding more than
/// `max` bytes. A final line without a newline still counts.
fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    line.clear();
    let mut too_long = false;
    let mut read_any = false;
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(match (read_any, too_long) {
                (false, _) => LineRead::Eof,
                (true, true) => LineRead::TooLong,
                (true, false) => LineRead::Line,
            });
        }
        read_any = true;
        let (chunk, consumed, ended) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (&available[..index], index + 1, true),
            None => (available, available.len(), false),
        };
        if !too_long {
            if line.len() + chunk.len() > max {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        input.consume(consumed);
        if ended {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Server {
    /// `None` once the output failed or the server stopped.
    out: Mutex<Option<Box<dyn Write + Send>>>,
    fleet: Arc<dyn FleetOps>,
    config: Config,
    shutdown: AtomicBool,
    /// The revision agreed by `initialize`, for clients that use it.
    session: Mutex<Option<Revision>>,
    /// Running tool calls by request id, with the flag that withdraws them.
    calls: Mutex<HashMap<String, Arc<AtomicBool>>>,
    active: Mutex<usize>,
    idle: Condvar,
}

/// Where the answer to one request goes: straight to the output, or into
/// the single array that answers a batch.
#[derive(Clone)]
enum Sink {
    Direct,
    Batch(Arc<Batch>),
}

struct Batch {
    state: Mutex<BatchState>,
}

struct BatchState {
    pending: usize,
    responses: Vec<Value>,
}

/// The obligation to answer one request, exactly once. Dropping it without
/// answering releases the slot in a batch and sends nothing.
struct Reply {
    server: Arc<Server>,
    id: Value,
    /// Set once the request is known to use a stateless revision, whose
    /// results carry `resultType` and the server's identity.
    stateless: bool,
    sink: Sink,
    delivered: bool,
}

impl Reply {
    fn result(mut self, mut result: Map<String, Value>) {
        if self.stateless {
            result.insert("resultType".into(), json!("complete"));
            let meta = result
                .entry("_meta")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(meta) = meta {
                meta.insert(META_SERVER_INFO.into(), server_info(true));
            }
        }
        let message = rpc::result_message(&self.id, result);
        self.deliver(Some(message));
    }

    fn error(mut self, error: RpcError) {
        let message = rpc::error_message(Some(&self.id), &error);
        self.deliver(Some(message));
    }

    /// The request was withdrawn: say nothing.
    fn silent(mut self) {
        self.deliver(None);
    }

    fn deliver(&mut self, message: Option<Value>) {
        if !std::mem::replace(&mut self.delivered, true) {
            self.server.complete(&self.sink, message);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.deliver(None);
    }
}

fn server_info(with_title: bool) -> Value {
    let mut info = Map::new();
    info.insert("name".into(), json!("builderella"));
    if with_title {
        info.insert("title".into(), json!("Builderella fleet"));
    }
    info.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    Value::Object(info)
}

impl Server {
    /// Write one message and its newline as a single, serialised write.
    fn send(&self, message: &Value) {
        let mut bytes = match serde_json::to_vec(message) {
            Ok(bytes) => bytes,
            Err(error) => {
                diag(format_args!("could not encode a message: {error}"));
                return;
            }
        };
        bytes.push(b'\n');
        let mut out = lock(&self.out);
        let Some(writer) = out.as_mut() else {
            return;
        };
        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            // The client went away. Stop writing and stop pending waits;
            // jobs are left alone.
            diag(format_args!("output closed: {error}"));
            *out = None;
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    fn handle_line(server: &Arc<Self>, line: &[u8]) {
        let text = line.trim_ascii();
        if text.is_empty() {
            return;
        }
        let value: Value = match serde_json::from_slice(text) {
            Ok(value) => value,
            Err(error) => {
                server.send(&rpc::error_message(
                    None,
                    &RpcError::new(rpc::PARSE_ERROR, format!("Parse error: {error}")),
                ));
                return;
            }
        };
        match value {
            Value::Object(message) => Self::handle_message(server, message, Sink::Direct),
            Value::Array(items) => Self::handle_batch(server, items),
            _ => server.send(&rpc::error_message(
                None,
                &RpcError::invalid_request("Invalid Request: a message must be a JSON object"),
            )),
        }
    }

    /// JSON-RPC batches exist only in revision 2025-03-26, which requires
    /// servers to accept them. The whole batch gets one array in reply.
    fn handle_batch(server: &Arc<Self>, items: Vec<Value>) {
        let batching = lock(&server.session).is_some_and(Revision::has_batching);
        if !batching {
            server.send(&rpc::error_message(
                None,
                &RpcError::invalid_request(
                    "Invalid Request: JSON-RPC batches are only part of protocol revision 2025-03-26",
                ),
            ));
            return;
        }
        if items.is_empty() {
            server.send(&rpc::error_message(
                None,
                &RpcError::invalid_request("Invalid Request: empty batch"),
            ));
            return;
        }
        // Everything except a well-formed notification or response yields
        // exactly one entry in the reply.
        let answers = |item: &Value| match item.as_object() {
            Some(message) => {
                !(message.get("jsonrpc") == Some(&json!("2.0"))
                    && !message.contains_key("id")
                    && (message.get("method").is_some_and(Value::is_string)
                        || message.contains_key("result")
                        || message.contains_key("error")))
            }
            None => true,
        };
        let batch = Arc::new(Batch {
            state: Mutex::new(BatchState {
                // One extra slot held by this function, so the reply cannot
                // be flushed before every item was started.
                pending: items.iter().filter(|item| answers(item)).count() + 1,
                responses: Vec::new(),
            }),
        });
        for item in items {
            match item {
                Value::Object(message) => {
                    Self::handle_message(server, message, Sink::Batch(batch.clone()));
                }
                _ => Self::reject(
                    server,
                    None,
                    Sink::Batch(batch.clone()),
                    RpcError::invalid_request("Invalid Request: a message must be a JSON object"),
                ),
            }
        }
        // Release the extra slot.
        server.complete(&Sink::Batch(batch), None);
    }

    /// Answer a message that never became a request with a usable id.
    fn reject(server: &Arc<Self>, id: Option<&Value>, sink: Sink, error: RpcError) {
        server.complete(&sink, Some(rpc::error_message(id, &error)));
    }

    /// Finish one answer: write it, or add it to its batch and write the
    /// batch once every item in it is finished. `None` finishes without a
    /// response.
    fn complete(&self, sink: &Sink, message: Option<Value>) {
        match sink {
            Sink::Direct => {
                if let Some(message) = message {
                    self.send(&message);
                }
            }
            Sink::Batch(batch) => {
                let finished = {
                    let mut state = lock(&batch.state);
                    if let Some(message) = message {
                        state.responses.push(message);
                    }
                    state.pending = state.pending.saturating_sub(1);
                    (state.pending == 0).then(|| std::mem::take(&mut state.responses))
                };
                if let Some(responses) = finished
                    && !responses.is_empty()
                {
                    self.send(&Value::Array(responses));
                }
            }
        }
    }

    fn handle_message(server: &Arc<Self>, message: Map<String, Value>, sink: Sink) {
        let id = message.get("id");
        let usable_id = id.filter(|id| rpc::is_valid_id(id));
        if message.get("jsonrpc") != Some(&json!("2.0")) {
            return Self::reject(
                server,
                usable_id,
                sink,
                RpcError::invalid_request("Invalid Request: `jsonrpc` must be \"2.0\""),
            );
        }
        let method = match message.get("method") {
            Some(Value::String(method)) => method.as_str(),
            Some(_) => {
                return Self::reject(
                    server,
                    usable_id,
                    sink,
                    RpcError::invalid_request("Invalid Request: `method` must be a string"),
                );
            }
            None => {
                if message.contains_key("result") || message.contains_key("error") {
                    // A response. This server sends no requests, so there is
                    // nothing it could belong to.
                    diag("ignoring an unexpected response from the client");
                    if id.is_some() {
                        server.complete(&sink, None);
                    }
                    return;
                }
                return Self::reject(
                    server,
                    usable_id,
                    sink,
                    RpcError::invalid_request("Invalid Request: no `method`"),
                );
            }
        };
        let params = match message.get("params") {
            None | Some(Value::Null) => Ok(Map::new()),
            Some(Value::Object(params)) => Ok(params.clone()),
            Some(_) => Err(RpcError::invalid_params(
                "Invalid params: `params` must be an object",
            )),
        };

        let Some(id) = id else {
            // A notification: never answered.
            if let Ok(params) = params {
                server.notification(method, &params);
            }
            return;
        };
        if usable_id.is_none() {
            return Self::reject(
                server,
                None,
                sink,
                RpcError::invalid_request("Invalid Request: `id` must be a string or an integer"),
            );
        }
        let reply = Reply {
            server: server.clone(),
            id: id.clone(),
            stateless: false,
            sink,
            delivered: false,
        };
        match params {
            Ok(params) => Self::request(server, method, params, reply),
            Err(error) => reply.error(error),
        }
    }

    fn notification(&self, method: &str, params: &Map<String, Value>) {
        match method {
            "notifications/initialized" => {}
            "notifications/cancelled" => {
                let Some(id) = params.get("requestId").filter(|id| rpc::is_valid_id(id)) else {
                    return;
                };
                // Unknown or finished requests are ignored, as the
                // specification asks. Only a pending wait observes the flag.
                if let Some(flag) = lock(&self.calls).get(&id.to_string()) {
                    flag.store(true, Ordering::SeqCst);
                    let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
                    diag(format_args!(
                        "request {} withdrawn by the client: {}",
                        inline(&id.to_string(), 80),
                        inline(reason, 200)
                    ));
                }
            }
            _ => {}
        }
    }

    fn request(server: &Arc<Self>, method: &str, params: Map<String, Value>, mut reply: Reply) {
        if !matches!(
            method,
            "initialize" | "ping" | "server/discover" | "tools/list" | "tools/call"
        ) {
            return reply.error(RpcError::method_not_found(method));
        }
        if method == "initialize" {
            return server.initialize(&params, reply);
        }
        let named_version = params
            .get("_meta")
            .and_then(|meta| meta.get(META_PROTOCOL_VERSION));
        if method == "ping" && named_version.is_none() {
            // Allowed at any time, also before initialisation.
            return reply.result(Map::new());
        }
        let revision = match server.revision_of(&params) {
            Ok(revision) => revision,
            Err(error) => return reply.error(error),
        };
        reply.stateless = revision.is_stateless();
        match method {
            // `ping` was removed in the stateless revision.
            "ping" if revision.is_stateless() => reply.error(RpcError::method_not_found(method)),
            "ping" => reply.result(Map::new()),
            "server/discover" if revision.is_stateless() => reply.result(discover_result()),
            "server/discover" => reply.error(RpcError::invalid_params(format!(
                "Invalid params: server/discover needs `_meta` with `{META_PROTOCOL_VERSION}`"
            ))),
            "tools/list" => match params.get("cursor") {
                None | Some(Value::Null) => reply.result(list_result(revision)),
                Some(_) => reply.error(RpcError::invalid_params(
                    "Invalid params: unknown cursor; the tool list has a single page",
                )),
            },
            _ => Self::call_tool(server, revision, &params, reply),
        }
    }

    /// The revision a request is to be served under: the one named in its
    /// `_meta` when that is a stateless revision, otherwise the one agreed
    /// by `initialize`.
    fn revision_of(&self, params: &Map<String, Value>) -> Result<Revision, RpcError> {
        let meta = params.get("_meta");
        let session = *lock(&self.session);
        let not_initialized = || {
            RpcError::invalid_request(format!(
                "Invalid Request: the server is not initialized. Send `initialize` first, or put `{META_PROTOCOL_VERSION}` and `{META_CLIENT_CAPABILITIES}` in the request's `_meta` (protocol revision {}).",
                SUPPORTED_PROTOCOL_VERSIONS[0]
            ))
        };
        match meta.and_then(|meta| meta.get(META_PROTOCOL_VERSION)) {
            None => session.ok_or_else(not_initialized),
            Some(Value::String(version)) => match Revision::parse(version) {
                Some(revision) if revision.is_stateless() => {
                    let capabilities = meta.and_then(|meta| meta.get(META_CLIENT_CAPABILITIES));
                    if capabilities.is_some_and(Value::is_object) {
                        Ok(revision)
                    } else {
                        Err(RpcError::invalid_params(format!(
                            "Invalid params: `_meta` must contain `{META_CLIENT_CAPABILITIES}` as an object"
                        )))
                    }
                }
                // A handshake revision named per request: only the
                // handshake itself can select it.
                Some(_) => session.ok_or_else(not_initialized),
                None => Err(RpcError::unsupported_version(version)),
            },
            Some(_) => Err(RpcError::invalid_params(format!(
                "Invalid params: `{META_PROTOCOL_VERSION}` must be a string"
            ))),
        }
    }

    fn initialize(&self, params: &Map<String, Value>, reply: Reply) {
        let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
            return reply.error(RpcError::invalid_params(
                "Invalid params: `protocolVersion` must be a string",
            ));
        };
        // Answer with the client's version when it is a handshake revision
        // this server speaks, otherwise with the newest such revision.
        let revision = Revision::parse(requested)
            .filter(|revision| !revision.is_stateless())
            .unwrap_or(Revision::LATEST_HANDSHAKE);
        {
            let mut session = lock(&self.session);
            if session.is_some() {
                drop(session);
                return reply.error(RpcError::invalid_request(
                    "Invalid Request: the server is already initialized",
                ));
            }
            *session = Some(revision);
        }
        let mut result = Map::new();
        result.insert("protocolVersion".into(), json!(revision.as_str()));
        result.insert("capabilities".into(), json!({"tools": {}}));
        result.insert(
            "serverInfo".into(),
            server_info(revision.has_structured_output()),
        );
        result.insert("instructions".into(), json!(INSTRUCTIONS));
        reply.result(result);
    }

    fn call_tool(
        server: &Arc<Self>,
        revision: Revision,
        params: &Map<String, Value>,
        reply: Reply,
    ) {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return reply.error(RpcError::invalid_params(
                "Invalid params: `name` must be a string",
            ));
        };
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return reply.error(RpcError::invalid_params(
                    "Invalid params: `arguments` must be an object",
                ));
            }
        };
        let Some(tool) = tools::find(name) else {
            return reply.error(RpcError::invalid_params(format!(
                "Unknown tool: {}",
                inline(name, 100)
            )));
        };

        let key = reply.id.to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut calls = lock(&server.calls);
            if calls.contains_key(&key) {
                drop(calls);
                return reply.error(RpcError::invalid_request(
                    "Invalid Request: a request with this id is still in progress",
                ));
            }
            if calls.len() >= MAX_CONCURRENT_CALLS {
                drop(calls);
                return reply.result(tool_result(
                    revision,
                    Finished::Error(format!(
                        "{name} was not started: {MAX_CONCURRENT_CALLS} tool calls are already running on this server. Wait for one to finish and call again."
                    )),
                ));
            }
            calls.insert(key.clone(), cancelled.clone());
        }
        *lock(&server.active) += 1;

        // The reply is shared with this thread so that it can still be
        // answered if the worker thread cannot be started.
        let slot = Arc::new(Mutex::new(Some(reply)));
        let worker = server.clone();
        let worker_slot = slot.clone();
        let worker_key = key.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("bella-mcp-{name}"))
            .spawn(move || {
                let finished = catch_unwind(AssertUnwindSafe(|| {
                    tool.invoke(
                        &Call {
                            fleet: worker.fleet.as_ref(),
                            config: &worker.config,
                            cancelled: &cancelled,
                            shutdown: &worker.shutdown,
                        },
                        &arguments,
                    )
                }));
                // Free the id before answering, so the client may reuse it.
                lock(&worker.calls).remove(&worker_key);
                if let Some(reply) = lock(&worker_slot).take() {
                    match finished {
                        Ok(Finished::Abandoned) => reply.silent(),
                        Ok(finished) => reply.result(tool_result(revision, finished)),
                        Err(_) => {
                            diag(format_args!("tool {} panicked", tool.name));
                            reply.error(RpcError::new(
                                rpc::INTERNAL_ERROR,
                                format!("Internal error while running {}", tool.name),
                            ));
                        }
                    }
                }
                worker.call_finished();
            });
        if let Err(error) = spawned {
            diag(format_args!("could not start a thread for {name}: {error}"));
            lock(&server.calls).remove(&key);
            if let Some(reply) = lock(&slot).take() {
                reply.error(RpcError::new(
                    rpc::INTERNAL_ERROR,
                    format!("Internal error: could not start {name}"),
                ));
            }
            server.call_finished();
        }
    }

    fn call_finished(&self) {
        let mut active = lock(&self.active);
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.idle.notify_all();
        }
    }
}

fn discover_result() -> Map<String, Value> {
    let mut result = Map::new();
    result.insert(
        "supportedVersions".into(),
        json!(SUPPORTED_PROTOCOL_VERSIONS),
    );
    result.insert("capabilities".into(), json!({"tools": {}}));
    result.insert("instructions".into(), json!(INSTRUCTIONS));
    result.insert("ttlMs".into(), json!(CACHE_TTL_MS));
    result.insert("cacheScope".into(), json!("public"));
    result
}

fn list_result(revision: Revision) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert(
        "tools".into(),
        Value::Array(
            tools::all()
                .iter()
                .map(|tool| tool.descriptor(revision))
                .collect(),
        ),
    );
    if revision.is_stateless() {
        result.insert("ttlMs".into(), json!(CACHE_TTL_MS));
        result.insert("cacheScope".into(), json!("public"));
    }
    result
}

/// A `tools/call` result. Failures of the tool are results with `isError`,
/// so the model sees them and can react; they are not protocol errors.
fn tool_result(revision: Revision, finished: Finished) -> Map<String, Value> {
    let mut result = Map::new();
    let (text, structured, is_error) = match finished {
        Finished::Success(output) => (output.text, Some(output.structured), false),
        Finished::Error(text) => (text, None, true),
        Finished::Abandoned => (String::new(), None, true),
    };
    result.insert("content".into(), json!([{"type": "text", "text": text}]));
    if let Some(structured) = structured
        && revision.has_structured_output()
    {
        result.insert("structuredContent".into(), structured);
    }
    result.insert("isError".into(), json!(is_error));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lines(input: &[u8], max: usize) -> Vec<Option<String>> {
        let mut reader = io::BufReader::with_capacity(4, Cursor::new(input.to_vec()));
        let mut line = Vec::new();
        let mut out = Vec::new();
        loop {
            match read_line(&mut reader, &mut line, max).unwrap() {
                LineRead::Eof => return out,
                LineRead::TooLong => out.push(None),
                LineRead::Line => out.push(Some(String::from_utf8(line.clone()).unwrap())),
            }
        }
    }

    #[test]
    fn lines_are_split_across_buffer_refills() {
        assert_eq!(
            lines(b"one\ntwo three\n\nlast", 64),
            vec![
                Some("one".into()),
                Some("two three".into()),
                Some(String::new()),
                Some("last".into())
            ]
        );
        assert!(lines(b"", 64).is_empty());
    }

    #[test]
    fn an_oversized_line_is_skipped_without_losing_the_next() {
        assert_eq!(
            lines(b"0123456789abcdef\nok\n0123456789", 8),
            vec![None, Some("ok".into()), None]
        );
        assert_eq!(lines(b"12345678\n", 8), vec![Some("12345678".into())]);
    }
}
