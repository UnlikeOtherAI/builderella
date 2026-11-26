# The `bella` command line

`bella` is one program for every role: it runs the controller, runs the
worker on each build machine, and is the command line developers and coding
agents use to submit and follow jobs. This page lists every command, its
flags and its exit codes.

## Quickstart

One controller, one worker on another machine, one developer. Host names are
placeholders.

**On the controller host** (`controller.example`):

```sh
bella controller run --data-dir /var/lib/builderella \
  --listen 0.0.0.0:7420 --public-url https://controller.example:7420 \
  --san controller.example
```

It prints the certificate fingerprint, and on the first start the
administrator token, once. The token is also kept in the data directory,
readable by its owner only. Log in on the same host with it:

```sh
bella login --local --data-dir /var/lib/builderella \
  --controller-url https://controller.example:7420
bella controller pair --profile dedicated-builder
```

`pair` prints an enrollment string such as
`bella://controller.example:7420/#<fingerprint>.<code>`. It is single use and
expires after ten minutes. Pass it to the new machine over a channel you
trust (your own terminal, an SSH session).

**On the build machine:**

```sh
bella worker enroll 'bella://controller.example:7420/#…' --data-dir ~/.bella-worker
bella worker run --data-dir ~/.bella-worker
```

**On a developer machine**, with a token an administrator made with
`bella token create --role developer --name alice`:

```sh
bella login https://controller.example:7420 --fingerprint <fingerprint> --token-stdin
cd my-project            # has a builderella.toml
bella submit test --wait --follow
```

`submit` snapshots the working tree as it is on disk, uncommitted edits
included, uploads what the controller does not have yet, and creates the
job. With `--wait` it waits for the result and exits with its status.

## Global flags

| Flag | Meaning |
| --- | --- |
| `--json` | Print one JSON document on standard output instead of text. Progress and diagnostics still go to standard error. |
| `--credentials <path>` | Credentials file to use. Default: see [Files](#where-files-live). Also `BELLA_CREDENTIALS`. |
| `-v`, `--verbose` | More diagnostics on standard error; repeat for more. `RUST_LOG` overrides it. |

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. For `submit --wait` and `wait`: the job succeeded. |
| 1 | The job finished without succeeding: failed, timed out, cancelled, out of resources, interrupted by an infrastructure failure, or rejected. `worker self-test` also exits 1 when containment is not what the adapter claims. |
| 2 | Usage or local error: bad arguments, no recipe, a capture that failed, a file that already exists. |
| 3 | A wait ran out of time. The job keeps running; a wait timeout never cancels. |
| 4 | The controller refused the request (an API error: not found, forbidden, invalid state, policy). |
| 5 | The controller could not be reached, or its certificate is not the one these credentials pinned. |
| 130 | Ctrl-C during `submit --wait`, `wait` or `logs --follow`. Only the waiting stops; the job keeps running. |

With `--json`, a failure prints `{"error": {"message", "code", "details"},
"exit_code"}` on standard output; `code` is the controller's error code when
it answered.

When the controller presents a certificate other than the pinned one, `bella`
says that the certificate fingerprint changed, sends nothing (not even the
token), and exits 5. Pair again with `bella login … --fingerprint <new>` only
when you know why it changed.

## Controller

### `bella controller run`

Runs the controller in the foreground until Ctrl-C or `SIGTERM`.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--data-dir <dir>` | required | Ledger, objects, fleet certificate and admin token. One controller per directory. |
| `--listen <addr>` | `0.0.0.0:7420` | Address to listen on. Port 0 picks a free port. |
| `--public-url <url>` | from `--listen` | The address workers and clients use, `https://host:port`. Set it when listening on a wildcard address. |
| `--san <name>` | `--listen` address, `localhost` | Name or address the fleet certificate is issued for; repeat for several. Only used when the certificate is first generated. |
| `--insecure-http` | off | Plain HTTP. Refused on anything but loopback unless `--allow-insecure` is also given. |
| `--allow-insecure` | off | With `--insecure-http`: allow a non-loopback address. Tokens and source then cross the network unencrypted. |
| `--ready-file <file>` | none | Once listening, write `{"url": …, "fingerprint": …}` to this file. Scripts use it with `--listen 127.0.0.1:0`. |

It logs whether a built dashboard is embedded or the placeholder page is
served. With `--json` it prints `{url, fingerprint, listen, epoch}` once
listening.

### `bella controller pair`

Administrators. Creates a single-use pairing code and prints the enrollment
string on standard output, its expiry on standard error.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--ttl <duration>` | `10m` | How long the code is valid. |
| `--profile <profile>` | `protected-workstation` | What the machine is for: `protected-workstation`, `dedicated-builder`, `portable-builder`, `infrastructure-only`. |

### `bella controller service install|uninstall|status|start|stop`

Installs the controller as an operating-system service that runs
`bella controller run` with the same flags (`install` takes every flag of
`run`). The service runs this binary by its absolute path.

| Flag | Meaning |
| --- | --- |
| `--print` | `install` only: show the definition that would be written and change nothing. |
| `--user` | A per-user service (systemd user unit, LaunchAgent) instead of a system one. |
| `--name <name>` | Service name instead of `builderella-controller`. |
| `--account <account>` | Account the service runs as. |
| `--log-dir <dir>` | Log directory; default `<data-dir>/logs`. |
| `--replace` | Replace a different definition written earlier by `bella`. |
| `--no-start` | Install without starting. |

Linux services log to the journal; macOS and Windows services write files in
the log directory (on Windows one file per day, `bella-YYYY-MM-DD.log`).

## Worker

### `bella worker enroll <enrollment>`

Probes the machine, runs the containment self-test once, exchanges the
pairing code for a machine identity and stores it in the data directory.

| Flag | Meaning |
| --- | --- |
| `--data-dir <dir>` | Required. The worker's state: journal, machine token, object cache, log spool. A directory that is already enrolled is refused. |
| `--name <name>` | Display name; default the host name. |
| `--allow-insecure` | Accept a `bella+http://` string naming a host other than loopback. |

### `bella worker run`

Runs the worker daemon in the foreground. Ctrl-C or `SIGTERM` (or a service
stop) stops running work, cleans it up and releases it before exiting.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--data-dir <dir>` | required | The enrolled data directory. |
| `--work-root <dir>` | `<data-dir>/work`; `C:\bw` on Windows | Where attempt workspaces are created. Keep it short on Windows, where the path limit applies to everything below it. |

### Local controls

These change the worker's journal directly, so they work whether or not the
daemon or the controller is running. A running daemon picks the change up
within a second; the controller learns it when the worker next reports.

| Command | Effect |
| --- | --- |
| `bella worker pause --data-dir <dir>` | Accept no new work. Running work continues. |
| `bella worker resume --data-dir <dir>` | Accept work again. |
| `bella worker drain --data-dir <dir>` | Accept no new work and let the machine run empty. |
| `bella worker undrain --data-dir <dir>` | End draining. |
| `bella worker boost --data-dir <dir> [--for 30m \| --off]` | Allow managed work up to the static ceiling for a while. Never lowers the foreground floor. |
| `bella worker status --data-dir <dir>` | The enrollment, the policy and the local settings. |

### `bella worker self-test`

Runs the containment adapter's self-test and one small command the way a step
runs, then prints the commissioning report. Exits 1 when the adapter did not
verify itself or the command failed. Takes `--data-dir` and an optional
`--work-root`.

### `bella worker service install|uninstall|status|start|stop`

As for the controller, running `bella worker run` with the given
`--data-dir` and `--work-root`. A system worker on Linux names its
`--account`. On macOS the worker is a per-user LaunchAgent (`--user`). On
Windows an ordinary `--account` needs its password, which is asked for on the
terminal and never taken as an argument, and the "Log on as a service" right,
which `install` does not grant (Local Security Policy → User Rights
Assignment).

## Identity

### `bella login <url>`

Checks the credentials with the controller (`whoami`) and only then saves
them.

| Flag | Meaning |
| --- | --- |
| `--fingerprint <fp>` | The controller's certificate fingerprint, as `controller run` prints it (hex, colon-separated hex or base64url). Required for `https`. |
| `--token-stdin` | Read the token from standard input. |
| `--token-file <file>` | Read the token from a file. |
| `--local --data-dir <dir>` | On the controller host: use the admin token and certificate in the controller's data directory. `--controller-url` names the address when it is not `https://127.0.0.1:7420`. |
| `--allow-insecure` | Accept a plain `http://` address other than loopback. |

Tokens are never accepted as a plain argument, where they would end up in
shell history, and are never printed back.

### `bella token create --role viewer|developer|admin --name <name>`

Administrators. Mints a token and prints it once on standard output.
Viewers read; developers also submit, cancel and manage environments;
administrators also pair machines, mint tokens and change policy.

## Jobs

### `bella submit [RECIPE] [-- COMMAND…]`

Finds `builderella.toml` in the current directory or above, captures the
working tree (tracked files and untracked files that git does not ignore,
from the bytes on disk), uploads the objects the controller lacks, registers
the snapshot, resolves the recipe against the project's policy and submits
it. A recipe file with a single recipe needs no name. A command after `--`
runs instead of a recipe, without a shell, pinned to this machine's platform
unless `--platform` or `--machine` says otherwise.

Untracked files that look like secrets (`.env`, `*.pem`, `*.key`, …) are left
out and listed on standard error; a tracked one stops the capture. `-v` also
reports the capture statistics and the exclusion rules in force. Recipe
warnings are printed with their file positions; recipe errors stop the
submission with every problem listed.

Capture writes to a temporary local spool first and then uploads only the
objects the controller reports missing, so an interrupted upload resumes
where it stopped when the same command is run again.

| Flag | Meaning |
| --- | --- |
| `--wait` | Wait until the job has finished and its resources are released; exit with its status. |
| `--follow` | Wait and print the log as it arrives. |
| `--wait-timeout <duration>` | Stop waiting after this long (exit 3). The job keeps running. |
| `--idempotency-key <key>` | Reuse the key of an earlier submission to get the same job back. A key is generated and printed when none is given. |
| `--project <name>` | Project name; default the recipe file's. |
| `-C`, `--dir <dir>` | Work in this directory instead of the current one. |
| `--priority <band>` | `maintenance`, `background`, `normal`, `interactive`, `urgent`. |
| `--platform <platform>` | Preferred platform, such as `linux` or `macos-aarch64`; with `--require` a hard requirement. |
| `--machine <id or name>` | Run only on this machine. |
| `--env KEY=VALUE` | Extra environment variable for the steps; repeatable. Stored with the job, so no secrets. |
| `--timeout <duration>` | Job timeout, within policy. |
| `--lan-only` | Keep the source on the local network. |
| `--cpu <cpus>`, `--memory <size>` | With a command: the reservation (default 1 CPU and 1GiB). A recipe's reservation cannot be overridden. |

It prints `job <id>` and `dashboard <url>/#/jobs/<id>` on standard output,
the idempotency key on standard error, and with `--wait` the outcome. When a
job fails in a step, the last lines of that step's output follow on standard
error. Ctrl-C while waiting prints the job id and the `bella cancel` command
and cancels nothing.

### `bella plan [RECIPE] [-- COMMAND…]`

Takes the same flags as `submit` (except the waiting ones) and asks the
controller whether any machine could run the job and why each one could or
could not, without capturing or uploading anything.

### Following and inspecting

| Command | Meaning |
| --- | --- |
| `bella jobs [--project P] [--state S] [--limit N]` | List jobs. `--state` is `active`, `finished`, `all` or a phase such as `queued`, `executing`, `cleaning`. |
| `bella job <id>` | Phase, outcome, cleanup, attempts and steps, and the dashboard link. |
| `bella wait <id> [--timeout D]` | Wait until the job is finished and released; exit with its status, or 3 on timeout. |
| `bella logs <id> [--follow] [--step S] [--tail SIZE]` | Print the log. `--follow` keeps printing until the output is complete. Log text is untrusted build output. With `--json`, followed text goes to standard error. |
| `bella cancel <id>` | Request cancellation. The job ends, and its resources are released, once the worker has stopped and cleaned up. |
| `bella artifacts <id> [--download DIR]` | List artifacts, or download them below `DIR`, the newest attempt's copy of each path. Each file is verified against its digest and created new: an existing file is never overwritten (exit 2). |
| `bella explain <id>` | Why a job is waiting, per machine, with suggestions. |
| `bella env list`, `env show <id>`, `env renew <id>`, `env finish <id>` | Environments kept alive after a job. |

## Fleet administration

| Command | Meaning |
| --- | --- |
| `bella fleet` | Machines with their state, flags, reservations, ceilings and enforcement strength. |
| `bella queue` | Queued jobs in order and each project's fair-share standing. |
| `bella machine show <machine>` | One machine. `<machine>` is an id or a unique name. |
| `bella machine rename <machine> <name>` | Administrators. |
| `bella machine drain <machine>`, `undrain <machine>` | Administrators. Stop or resume offering new work. |
| `bella machine retire <machine>` | Administrators. Stop offering work and revoke the machine's credentials. |
| `bella machine policy <machine> [flags]` | Administrators. `--profile`, `--accept-builds true\|false`, `--foreground-cpu PERCENT`, `--foreground-memory PERCENT`, `--max-concurrent N` (0 removes the limit), `--low-priority true\|false`, `--max-disconnected DURATION`. |
| `bella project list`, `project show <name>` | Projects and their policy. |
| `bella project set <name> [flags]` | Administrators. `--weight N`, `--max-concurrent N`, `--max-environments N` (0 removes a limit), `--cloud existing-only\|manual\|auto-overflow\|hybrid`, `--lan-only true\|false`, `--allow-urgent true\|false`. |

## Agents

### `bella mcp [--root DIR]… [--max-wait DURATION]`

Serves the fleet to a coding agent over the Model Context Protocol on
standard input and output, using the stored credentials. `--root` names a
directory the agent may snapshot or download into; repeat it for several,
the first is the default workspace (the current directory without any).
`--max-wait` caps one `wait_job` call (default 50 s). Standard output carries
only JSON-RPC; diagnostics go to standard error. Closing the session cancels
nothing. See [agents.md](agents.md) for client configuration.

### `bella version`

This binary's version and protocol version, and the controller's
`/version` when credentials exist.

## Where files live

| What | Where |
| --- | --- |
| Credentials (default) | Linux: `$XDG_CONFIG_HOME/builderella/credentials.json`, else `~/.config/builderella/credentials.json`. macOS: `$XDG_CONFIG_HOME/builderella/…` if set, else `~/Library/Application Support/builderella/credentials.json`. Windows: `%APPDATA%\builderella\credentials.json`. Written readable by the owner only; a file others can read is used with a warning. |
| Controller data directory | `ledger.sqlite`, `objects/`, `logs/`, `tls/` (the fleet certificate and its key) and `admin-token`. |
| Worker data directory | `journal.sqlite`, `machine-token`, `worker.lock`, `commissioning.json`, `inventory.json`, `cache/`, `spool/`, and `work/` unless `--work-root` says otherwise. |
| Service logs | `<data-dir>/logs` unless `--log-dir` is given; the journal on Linux. |
