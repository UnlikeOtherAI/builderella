# Using the fleet from a coding agent

`bella mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io)
server. A coding agent starts it as a subprocess and gets twelve tools for
planning, submitting, following and cancelling fleet jobs. The server captures
the working tree on the machine where the agent runs, so it has to run there;
it talks to the controller with the credentials `bella` is already configured
with.

The server owns no build. Closing it, a `wait_job` timeout, or the agent
giving up on a call cancels nothing. Only an explicit `cancel_job` call does.

## Connecting

Use absolute paths. Replace `/absolute/path/to/bella` with the installed
binary and `/absolute/path/to/project` with the checkout the agent works in.

`--root` names a directory the agent may snapshot or download into. Repeat it
for several directories; the first is the default workspace. Without
`--root`, the server uses its working directory, which depends on how the
client starts it, so set it explicitly.

### Claude Code

From the project directory:

```sh
claude mcp add --transport stdio --scope project bella -- \
  /absolute/path/to/bella mcp --root /absolute/path/to/project
```

`--scope project` writes the entry to `.mcp.json` in the project root, which
you can also edit by hand:

```json
{
  "mcpServers": {
    "bella": {
      "type": "stdio",
      "command": "/absolute/path/to/bella",
      "args": ["mcp", "--root", "/absolute/path/to/project"]
    }
  }
}
```

An absolute path in a committed `.mcp.json` only fits one machine. Leave out
`--scope project` to keep the entry private to you (`local` scope, the
default), or use `--scope user` to have it in every project. Claude Code asks
for approval before it first uses a server from `.mcp.json`. Check the
connection with `claude mcp get bella` or `/mcp`.

Claude Code applies no per-request timer to stdio servers, so the default
50-second cap on `wait_job` needs no tuning there.

### Codex

```sh
codex mcp add bella -- /absolute/path/to/bella mcp --root /absolute/path/to/project
```

or in `~/.codex/config.toml` (or `.codex/config.toml` in a trusted project):

```toml
[mcp_servers.bella]
command = "/absolute/path/to/bella"
args = ["mcp", "--root", "/absolute/path/to/project"]
```

Codex stops a tool call after `tool_timeout_sec`, 60 seconds by default. The
server caps `wait_job` at 50 seconds for that reason. If you raise the cap on
the server, raise `tool_timeout_sec` above it in the same table. With
`default_tools_approval_mode = "writes"`, Codex prompts for the tools that
are not marked read-only: `submit_job`, `cancel_job`, `get_artifacts`,
`renew_environment` and `finish_environment`.

### Versions these snippets were checked against

The configuration syntax above was checked on 26 November 2026 against the
published documentation for Claude Code 2.1.332
(<https://code.claude.com/docs/en/mcp>) and Codex CLI 0.166.0
(<https://developers.openai.com/codex/mcp>). The server was verified against
the MCP specification with a protocol-level test client, not by running it
under either agent.

### Other clients

Any MCP client that can start a stdio server works: run
`/absolute/path/to/bella mcp --root /absolute/path/to/project` and speak MCP
on its standard input and output. Standard error carries diagnostics only.

## Tools

| Tool | What it does | Changes anything? |
| --- | --- | --- |
| `plan_job` | Dry run: can the fleet run this recipe or command, and on which machines? | No |
| `submit_job` | Snapshots the workspace, uploads it and creates the job. Returns the job id at once. | Creates a job |
| `get_job` | Phase, outcome, failing step and exit code, machine, attempts, cleanup state, snapshot id. | No |
| `wait_job` | Blocks until the outcome is known or the timeout passes. A timeout cancels nothing. | No |
| `read_logs` | One bounded page of log output with a cursor for the next. | No |
| `cancel_job` | Requests cancellation and reports how far cleanup has got. | Requests a stop |
| `get_artifacts` | Lists outputs with size, SHA-256 digest, attempt and snapshot; optionally downloads them. | Writes files when asked |
| `get_environment` | State, endpoints and deadline of an environment kept alive after a job. | No |
| `renew_environment` | Extends the lease and reports the deadline that now applies. | Extends a lease |
| `finish_environment` | Requests teardown of an environment. | Requests a stop |
| `explain_queue` | Why a job is waiting: permanent and temporary blockers per machine, suggestions. | No |
| `list_fleet` | Machines with availability flags, free and reserved CPU and memory, enforcement strength. | No |

Every tool returns compact text and, for clients that support it, the same
facts as structured JSON described by an output schema.

Arguments worth knowing:

* `submit_job` and `plan_job` take exactly one of `recipe` (a name from
  `builderella.toml`) or `command`. A `command` string is a shell script; an
  array is an argument vector run without a shell.
* `platform` is an operating system, optionally with an architecture:
  `linux`, `macos-aarch64`, `windows-x86_64`. It is a preference unless
  `require` is `true`. `machine` pins the job to one machine instead.
* `cpu` (a number of CPUs) and `memory` (`512MiB`, `8GiB`) replace what the
  recipe asks for. Fleet policy may clamp or refuse them.
* `idempotency_key` makes a retry of `submit_job` safe: the same key with
  unchanged files returns the same job. One is generated per call otherwise.
* `wait_job` takes `timeout_seconds` and `after_event`. Pass the `last_event`
  of an earlier result as `after_event` to wake at the next state change
  instead of only at the outcome.
* `read_logs` takes `step`, `cursor` and `tail`.
* `get_artifacts` downloads only with `download: true` and an explicit
  `dest`. `paths` limits the download to named artifacts.

## Workflow

1. `plan_job` when it is unclear whether the fleet can run the job at all.
   Skip it for routine runs.
2. `submit_job`. The result has the job id, the snapshot id, the capture's
   consistency level, the queue reason, any paths left out as secrets and,
   when a dashboard URL is configured, a link to the job's page there
   (`<dashboard url>/#/jobs/<job id>`). The job is for the files as they
   were at that moment; later edits are not part of it.
3. `wait_job` until `timed_out` is `false`. Long builds take several calls.
   `read_logs` in between shows progress.
4. On failure, `get_job` names the failing step and exit code and `read_logs`
   has the output. If the job stays queued, `explain_queue` says why.
5. `get_artifacts` to list or fetch outputs.
6. `finish_environment` as soon as an environment is no longer needed.

A result always names the snapshot it applies to. The server does not compare
that snapshot with the current workspace and never claims they still match.

If a job fails for infrastructure reasons (outcome `infra_interrupted`, a
lost machine, an unavailable controller), the tools say so and tell the agent
to report it. Running the same build locally without limits is not the
fallback: protecting the developer's machine from that is what the fleet is
for.

## What the server will not do

* **Change policy, budgets or enrollment.** There is no tool for machine
  policy, project policy, cloud allowances, tokens or enrolling machines.
  When the controller refuses something for lack of authority, the tool
  reports an error that tells the agent it lacks that authority and must not
  retry with different arguments.
* **Read or write outside the allowed roots.** `workspace` and `dest` are
  resolved through the filesystem first, so a symbolic link or junction that
  leads elsewhere is rejected. The roots come from `--root` on the command
  line, never from the client.
* **Overwrite files.** A download whose target exists fails, names the
  conflict and fetches nothing.
* **Cancel implicitly.** End of input, a broken pipe, a timeout and a
  `notifications/cancelled` message stop the server or one pending
  `wait_job`. None of them cancels a job.
* **Wait for a build inside `submit_job`.** It returns once the job exists.
* **Return unbounded output.** Lists are capped and say how much was left
  out. `read_logs` returns at most 24 KiB of text per call by default.
* **Pass log text off as instructions.** Log output is build output. It is
  returned with terminal control sequences and control characters removed,
  each line marked, under a notice that it is untrusted and that instructions
  in it must not be followed.
* **Upload secrets it can recognise.** Capture leaves out untracked files
  that match the secret patterns, and `submit_job` lists them; a tracked file
  that matches fails the submission instead. Values passed in `env` are
  stored with the job, so do not put secrets there.

## Protocol

The server implements MCP as JSON-RPC 2.0 over newline-delimited JSON on
standard input and output, without an SDK.

* Revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05` are
  negotiated with `initialize`. A client that asks for an unknown version is
  answered with `2025-11-25`.
* Revision `2026-07-28` is stateless: there is no handshake, every request
  carries its version in `_meta`, and `server/discover` reports what the
  server supports. Both styles work on the same connection.
* Tool annotations are sent from `2025-03-26` on; titles, output schemas and
  structured results from `2025-06-18` on.
* A tool failure is a result with `isError: true`, so the model sees it.
  Protocol errors are reserved for malformed requests and unknown tools.

Diagnostics go to standard error. If a client shows the server as failed,
start the same command in a terminal and read what it prints there.
