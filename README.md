<img src="assets/icon.svg" width="64" height="64" alt="">

# Builderella

Builderella turns the computers you already have into one build-and-test
pool for coding agents and the people who work with them. An agent (or you)
submits a snapshot of a working tree, uncommitted edits included, and a
recipe. The fleet picks a machine that can run it, reserves the CPU, memory
and disk the job needs, runs build, services, tests and cleanup, and frees
the reservation only after the machine proves the work is gone.

It is one program, `bella`, with three roles:

- **controller**: one per fleet. It holds the queue, the scheduler, the
  object store and the dashboard, and keeps its state in SQLite.
- **worker**: one per build machine. It connects out to the controller (no
  inbound port), runs each attempt in a containment scope, and protects the
  machine's own user: a workstation keeps a foreground reserve that fleet
  work never takes.
- **client**: the command line and a local MCP server (`bella mcp`) for
  Claude Code, Codex and other agents.

Traffic between them is HTTPS with a self-signed fleet certificate that
clients and workers pin by fingerprint when they pair.

## Status

v0.1. The controller, worker, command line and MCP server work end to end on
loopback, and the end-to-end tests run on Linux, macOS and Windows. Not yet
done: a long-running trial across real machines, signed release binaries,
container or VM executors, device and GUI runners, pooled database services
and real cloud providers. `docs/architecture.md` §9 lists what v0.1 leaves
out.

| Host | Containment | Notes |
| --- | --- | --- |
| Linux | cgroup v2, hard limits | Needs a delegated cgroup: run the worker as a systemd unit with `Delegate=yes` (`bella worker service install` writes one). |
| Windows | Job Objects, hard limits | Install the worker as a Windows service (`bella worker service install`): it then always restarts in the same session and finds its job objects again. |
| macOS | process groups, advisory | macOS has no cgroup equivalent. Limits are watched and enforced by eviction, and every view labels them advisory. |

The minimum supported Rust version is 1.89, so macOS 12 on Intel can build
and run it.

## Build

```sh
# Optional: build the dashboard so it is embedded in the controller.
(cd dashboard && npm ci && npm run build)

cargo build --release -p bella-cli
# The binary is target/release/bella (bella.exe on Windows).
```

Without `dashboard/dist`, the controller serves a page that says the
dashboard was not built into the binary.

## Use

[docs/cli.md](docs/cli.md) has a quickstart: a controller on one host, a
worker on another, and a developer submitting a job. In short:

```sh
# controller host
bella controller run --data-dir /var/lib/builderella \
  --public-url https://controller.example:7420 --san controller.example
bella login --local --data-dir /var/lib/builderella
bella controller pair --profile dedicated-builder      # prints bella://…

# build machine
bella worker enroll 'bella://controller.example:7420/#…' --data-dir ~/.bella-worker
bella worker run --data-dir ~/.bella-worker

# developer machine, in a project with a builderella.toml
bella submit test --wait --follow
```

## Documentation

| Document | What it covers |
| --- | --- |
| [docs/cli.md](docs/cli.md) | Every command, its flags, exit codes and where files live. |
| [docs/operations.md](docs/operations.md) | Running a fleet: services, ports, backups, upgrades, troubleshooting. |
| [docs/recipes.md](docs/recipes.md) | The `builderella.toml` format. |
| [docs/agents.md](docs/agents.md) | Connecting Claude Code, Codex and other MCP clients. |
| [docs/architecture.md](docs/architecture.md) | Design decisions and the contract each crate is built against. |
| [docs/acceptance.md](docs/acceptance.md) | Every acceptance scenario and the tests that cover it. |
| [docs/cloud.md](docs/cloud.md) | The cloud capacity model (not wired into v0.1). |

## Development

The rules for contributors and coding agents are in [AGENTS.md](AGENTS.md).
CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd dashboard && npm ci && npm run typecheck && npm test -- --run && npm run build)
```

plus `cargo check --workspace --all-targets` on Rust 1.89.

## License

MIT. See [LICENSE](LICENSE).
