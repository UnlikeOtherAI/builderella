# Recipes: `builderella.toml`

A recipe is a named job: what to run, where it may run and how much of a
machine it needs. Recipes live in one file, `builderella.toml`, at the root
of your project. Builderella looks for it in the directory you work in and
then in each directory above it; the nearest one wins.

This page is the complete reference for the file. Every example on it is
checked by the test suite.

## A first recipe

```toml
[project]
name = "my-app"

[recipes.test]
description = "Unit tests"
cpu = 4
memory = "8GiB"
run = "cargo test"
```

That is a whole file. It says: this project is called `my-app` on the fleet,
and it has one recipe, `test`, which runs `cargo test` and needs four CPUs
and 8 GiB of memory while it does.

Because the recipe does not say which platforms it can build on, the job
runs only on machines with the same operating system and architecture as
the one you submit it from. Add `hosts` when the recipe is portable:

```toml
[project]
name = "my-app"

[recipes.test]
hosts = ["linux", "macos"]
requires = ["rust>=1.85"]
cpu = 4
memory = "8GiB"
timeout = "20m"
run = "cargo test"
```

## Layout of the file

| Table | Required | Purpose |
| --- | --- | --- |
| `[project]` | yes | The project's name. |
| `[capture]` | no | What goes into the snapshot of your working tree. |
| `[defaults]` | no | Values shared by every recipe. |
| `[recipes.<name>]` | no | One table per recipe. |

Recipe and step names use lowercase letters, digits, `-` and `_`, up to 64
characters. A key the file format does not know is an error, not ignored, so
a typo cannot silently change what a job does.

### `[project]`

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string, required | The project's name on the fleet: letters, digits, `-`, `_` and `.`, starting with a letter or digit, up to 64 characters. |

### `[capture]`

A job runs on a snapshot of your working tree as it is on disk, including
uncommitted changes. This table adjusts what the snapshot contains. All keys
are optional.

| Key | Type | Meaning |
| --- | --- | --- |
| `exclude` | array of strings | Path patterns left out of the snapshot. |
| `include_ignored` | array of strings | Git-ignored paths that are captured anyway, such as a generated file the build needs. A path listed here that does not exist fails the capture. |
| `secrets` | array of strings | Extra secret patterns, on top of the built-in ones (`.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `.npmrc`, `.netrc`). A matching file is left out when untracked and stops the capture when tracked. |
| `allow_secrets` | array of strings | Paths that match a secret pattern and are captured regardless. |
| `roots` | table | Additional source directories. Each key is where the directory appears inside the snapshot (a relative path such as `libs/shared`); each value is the directory, relative to this file. |

```toml
[project]
name = "my-app"

[capture]
exclude = ["node_modules/", "target/", "dist/"]
include_ignored = ["generated/schema.json"]
secrets = ["*.secret"]
roots = { "libs/shared" = "../shared-lib" }
```

### `[defaults]`

`[defaults]` takes every recipe key except `description`, `run`, `steps`
and `environment`. A recipe that does not set a key gets the default.

A key set in a recipe replaces the default as a whole: a recipe with its own
`requires` does not also get the default `requires`. `env` is the one
exception; the two tables are merged and the recipe wins for a variable set
in both.

`[defaults]` applies to recipes only, not to a bare command submitted
without a recipe.

## Recipe keys

### What runs

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `description` | string | none | One line shown when recipes are listed. |
| `run` | string or array | | The command. Shorthand for a single step named `main`. Use either `run` or `steps`, not both. |
| `steps` | array of tables | | Several commands with dependencies; see [Steps](#steps). |
| `shell` | array of strings | per platform | The program that runs string commands, such as `["bash", "-c"]`. The script is added as the last argument. |
| `cwd` | string | source root | Working directory for the steps, relative to the root of the snapshot. It must stay inside it: no `..`, no absolute path. |
| `env` | table of strings | none | Environment variables for every step. Values are strings, so write `PORT = "8080"`. |

Steps do not inherit the worker's environment. They start with the system
essentials and Builderella's own `BELLA_*` variables, then get the recipe's
`env` and the step's. Anything else the build needs has to be set in `env`.

A command is written in one of two ways:

* A **string** is a script run by a shell: `sh -c` on Linux and macOS,
  `powershell -Command` on Windows, or the recipe's `shell`.
* An **array** is a program and its arguments, run directly with no shell:
  `run = ["cargo", "test", "--locked"]`. Nothing is split, expanded or
  quoted for you.

### Where it runs

Four separate things are declared, because cross-compiling for a platform
does not prove the result runs there.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `hosts` | array of platforms | the submitting platform | Platforms the build may run on. Without it the job is pinned to the operating system and architecture of the machine that submits it. |
| `target` | string | none | What the output is built for, such as `aarch64-apple-ios`. It does not affect placement; it is part of the job's identity. |
| `test_runtime` | platform | none | Where the tests must execute. |
| `requires` | array of requirements | none | Toolchains and capabilities the machine must have. |
| `isolation` | `native`, `container` or `vm` | `native` | The isolation the job demands. Workers in v0.1 only run native processes, so a recipe asking for more finds no machine. |

A job runs on one machine from start to finish. If `test_runtime` is set,
that machine must match both it and one of the `hosts`; a recipe where no
host can also be the test runtime gets a warning, because it cannot be
placed.

A **platform** is a string or a table:

| Form | Matches |
| --- | --- |
| `"linux"` | Any Linux machine. Also `"macos"` and `"windows"`. |
| `"macos-aarch64"` | That operating system and architecture. Architectures are `x86_64` and `aarch64`. |
| `"any-x86_64"`, `"any"` | `any` leaves the operating system, the architecture or both open. |
| `{ os = "windows", arch = "x86_64", os_version = ">=10" }` | The table form adds a version requirement on the operating system. Each key is optional. |

A **requirement** is a string or a table:

| Form | Matches |
| --- | --- |
| `"rust"` | Any installed version. |
| `"rust>=1.85"` | Version 1.85 or newer. Also `>`, `<=` and `<`. |
| `"node=22"` | A 22 release: 22.0, 22.11.3. Write `"node=22.11"` to accept 22.11.x only. |
| `"xcode>=16,<17"` | Every comma-separated term must hold. |
| `{ name = "xcode", version = ">=16", attributes = { sdk = "iphoneos18.0" } }` | The table form can also require attributes, such as an installed SDK or compilation target. |

Capability names are the ones workers report, for example `rust`, `cargo`,
`node`, `pnpm`, `jdk`, `gradle`, `xcode`, `swift`, `msvc`, `cmake`, `ninja`,
`docker`, `android-sdk` and `git`.

```toml
[project]
name = "my-app"

[recipes.android]
description = "Cross-compile for Android, test on a Linux host"
hosts = ["linux-x86_64", "macos-aarch64"]
target = "aarch64-linux-android"
test_runtime = { os = "linux", arch = "x86_64" }
requires = ["android-sdk", "rust>=1.85"]
cpu = 4
memory = "8GiB"
run = "cargo ndk test"
```

### What it reserves

A job reserves resources on the machine before it starts and keeps them
until its cleanup is confirmed. Declare what the job needs at its busiest.

| Key | Type | Meaning |
| --- | --- | --- |
| `cpu` | number | Logical CPUs. Fractions are allowed: `0.5`. |
| `memory` | size | Memory, such as `"8GiB"`. |
| `disk` | size | Scratch space on the worker's work volume. |
| `tokens` | table of counts | Countable things a machine has a fixed number of, such as `{ "ios-simulator" = 1 }` or `{ gpu = 1 }`. |
| `max_cpu`, `max_memory`, `max_disk` | number, size, size | How far the job may ask to grow while running. Defaults to the reservation: no growth. Must not be below it. |
| `parallelism` | whole number, at least 1 | Suggested build parallelism, given to the steps as `BELLA_JOBS`. It is a hint to your build tool and separate from `cpu`. |

A recipe that declares no `cpu` or no `memory` reserves none, and gets a
warning: the scheduler would be free to place it on a machine that has no
room for it.

With [steps](#steps), each step can declare its own `cpu`, `memory`, `disk`
and `tokens`. The job then reserves the most that can be in use at once:

* **services** stay up while other steps run, so their numbers are added
  together;
* **the other steps** run one after another, so only the largest of them
  counts, separately for CPU, memory, disk and each token;
* the two are added, and the result is never below the recipe-level
  `cpu`, `memory`, `disk` and `tokens`.

An app that needs 2 GiB and stays up while a test step needs 6 GiB therefore
reserves 8 GiB, so starting the app can never use up the memory its own
tests need. If the steps add up to more than the recipe-level numbers, the
steps win and you get a warning pointing at the number that is too small.

### How it is scheduled

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `priority` | `maintenance`, `background`, `normal`, `interactive` or `urgent` | `normal` | The queue band. Lower bands still make progress under load. `urgent` needs permission from fleet policy. |
| `timeout` | duration | `1h` | Wall-clock limit for the whole job. |
| `cleanup_timeout` | duration | `2m` | Time allowed for cleanup after the steps end. |
| `retry` | `pure`, `isolated` or `external` | `pure` | What the job touches, which decides whether a lost attempt may be repeated. `pure`: nothing outside its workspace (compiling, linting, isolated tests). `isolated`: state that belongs to the attempt and is replaced before a retry. `external`: something shared, such as publishing or migrating a database; never repeated automatically. |
| `max_attempts` | whole number, at least 1 | `1` | Attempts in total, including the first. Only attempts lost to infrastructure (a worker that vanished, a machine that slept) are repeated. A failing test is a result, not a reason to retry. |
| `cloud` | `existing-only`, `manual`, `auto-overflow` or `hybrid` | `existing-only` | Whether paid capacity may be created for the job. `existing-only`: enrolled machines only. `manual`: a person may authorise cloud capacity for this job. `auto-overflow`: standing policy may add it. `hybrid`: both. |
| `lan_only` | boolean | `false` | The source may not leave your network: no cloud or remote workers. |
| `disconnected` | duration | `0s` | How long a running attempt may continue when its worker loses contact with the controller. The machine's own limit also applies. |

### What it produces

| Key | Type | Meaning |
| --- | --- | --- |
| `artifacts` | array of strings | Path patterns, relative to the source root, collected as artifacts when the job ends: `["target/release/app", "dist/**"]`. |
| `test_reports` | array of strings | Path patterns for test reports such as JUnit XML: `["**/junit.xml"]`. |

Patterns must stay inside the source root: no `..`, no absolute path.

## Steps

Use `steps` when a job is more than one command: build, then start a
database, then run tests against it.

```toml
[project]
name = "my-app"

[recipes.e2e]
description = "End-to-end tests against a real database"
hosts = ["linux"]
requires = ["rust>=1.85"]
timeout = "30m"
test_reports = ["target/junit.xml"]

[[recipes.e2e.steps]]
name = "build"
run = "cargo build --locked"
cpu = 8
memory = "6GiB"

[[recipes.e2e.steps]]
name = "db"
kind = "service"
run = ["postgres", "-D", "data"]
cpu = 1
memory = "1GiB"
ports = [5432]
ready = { tcp = 5432, timeout = "30s" }

[[recipes.e2e.steps]]
name = "tests"
kind = "test"
run = "cargo test --test e2e"
needs = ["build", "db"]
cpu = 4
memory = "6GiB"

[[recipes.e2e.steps]]
name = "collect-logs"
kind = "cleanup"
run = "tar czf logs.tgz data/log"
```

This job reserves 9 CPUs and 7 GiB: the database (1 CPU, 1 GiB) plus the
larger of the other steps in each dimension (8 CPUs from `build`, 6 GiB from
either).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string, required | | Unique within the recipe. |
| `run` | string or array, required | | The command, as for a recipe. |
| `kind` | `task`, `service`, `test` or `cleanup` | `task` | See below. |
| `needs` | array of step names | none | Steps that must have finished, or for a service become ready, before this one starts. |
| `shell`, `cwd` | | the recipe's | As for a recipe, for this step only. |
| `env` | table of strings | none | Variables for this step, on top of the recipe's `env`. |
| `timeout` | duration | none | Limit for this step alone. The job's `timeout` still applies. |
| `cpu`, `memory`, `disk`, `tokens` | | none | What this step needs; see [What it reserves](#what-it-reserves). |
| `ports` | array of port numbers | none | Services only: loopback ports the service listens on. They are published as the endpoints of an [environment](#environments). |
| `ready` | table | none | Services only: when the service counts as started. |

Step kinds:

| Kind | Behaviour |
| --- | --- |
| `task` | Runs to completion. A failure fails the job. |
| `service` | Started in the background and kept alive until the job's other steps have finished. |
| `test` | Runs to completion like a task; its result is reported as test results. |
| `cleanup` | Always runs, even when an earlier step failed or the job was cancelled. |

Steps run in the order they are written, except that a step always comes
after the steps it `needs`, and cleanup steps come last. The rules that are
checked:

* step names are unique;
* every name in `needs` is a step of the same recipe, listed once, and not
  the step itself;
* there are no cycles;
* no step needs a cleanup step;
* a port is used by one service only, because the whole job shares one
  machine;
* `ready` and `ports` appear only on services.

### Readiness

A step that `needs` a service waits until the service is ready. `ready` has
exactly one probe:

| Probe | Ready when |
| --- | --- |
| `{ tcp = 5432 }` | A TCP connection to that loopback port succeeds. |
| `{ http = 8080, path = "/health", status = 200 }` | An HTTP GET returns that status. `path` defaults to `/`. Without `status`, any status below 500 counts. |
| `{ log = "ready to accept" }` | A line of the service's output contains the text. |
| `{ run = "pg_isready" }` | The command exits with status zero. A string or an array, as for `run`. |

Two more keys apply to any probe: `timeout` (default `60s`) is how long the
service may take before the job fails, and `interval` (default `500ms`) is
how often the probe is tried.

A service without `ready` gets a warning: steps that need it start as soon
as its process does, which is usually before it can answer.

## Environments

Normally services stop when the other steps have finished. Add an
`environment` table to keep them running afterwards, as something you can
connect to: a development server, or an app left up to debug a failing
test.

```toml
[project]
name = "my-app"

[recipes.dev]
description = "The app with its database, left running"
cpu = 2
memory = "3GiB"

[[recipes.dev.steps]]
name = "db"
kind = "service"
run = ["postgres", "-D", "data"]
ports = [5432]
ready = { run = ["pg_isready", "-q"] }

[[recipes.dev.steps]]
name = "app"
kind = "service"
run = "cargo run --release"
needs = ["db"]
env = { DATABASE_URL = "postgres://localhost:5432/app" }
ports = [8080]
ready = { http = 8080, path = "/health" }

[recipes.dev.environment]
idle_timeout = "15m"
lease = "30m"
max_lifetime = "4h"
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `idle_timeout` | duration | `15m` | The environment ends after this long without use. |
| `lease` | duration | `30m` | How long it lives unless renewed. |
| `max_lifetime` | duration | `4h` | It ends by then no matter how often it is renewed. Not shorter than `lease`. |

An environment needs at least one service. A recipe that has only services
and no `environment` gets a warning, because its services would be stopped
the moment they are ready. The reservation is held for as long as the
environment lives.

## Units

**Sizes** are a number and a unit, in quotes: `"8GiB"`, `"512MiB"`,
`"1.5GB"`.

| Units | Factor |
| --- | --- |
| `B` | bytes |
| `KB`, `MB`, `GB`, `TB` | powers of 1000 |
| `KiB`, `MiB`, `GiB`, `TiB` | powers of 1024 |

`"8GB"` is 8,000,000,000 bytes and `"8GiB"` is 8,589,934,592; they are not
interchangeable. Memory is usually meant in the binary units. The unit is
required and may be written in any case.

**Durations** are whole numbers with units, in quotes, largest unit first:
`"45s"`, `"30m"`, `"1h30m"`, `"500ms"`. Units are `h`, `m`, `s` and `ms`.
Everything except `ready.interval` is counted in whole seconds.

**CPU counts** are plain numbers without quotes: `4`, `0.5`. The finest
step is a thousandth of a CPU.

## What a job may change, and what policy may change

A recipe is the same for everyone. Two things can still alter a job when it
is submitted.

**The submitter** may override, for one job: `priority`, the placement (a
preferred or required platform, a named machine, or a pool), `parallelism`,
`cloud`, `timeout`, and extra `env` variables, which win over the recipe's.
They may also turn `lan_only` on, but never off. Nothing else can be
overridden: the commands, the reservation and the platform declaration are
the recipe's.

**Fleet policy** sets limits, and a recipe or override can only stay within
them:

| Asked for | Result |
| --- | --- |
| `timeout`, `cleanup_timeout`, `max_attempts`, `disconnected` or an environment lifetime above the limit | Lowered to the limit, with a warning. |
| `cloud` above what policy allows | Lowered to what is allowed, with a warning. |
| `urgent` priority without permission | The job is refused. |
| A reservation larger than policy allows | The job is refused. |
| `max_cpu`, `max_memory` or `max_disk` above the largest reservation allowed | Lowered, with a warning. |
| `lan_only = false` where policy requires LAN-only | The job is LAN-only. |

The job's digest identifies what it does. It covers the steps, `env`, the
platform declaration, the reservation, the outputs, the timeouts, the
environment terms and `parallelism`. It does not cover `priority`, placement
or `cloud`, nor the recipe's name, `retry`, `max_attempts`, `lan_only` and
`disconnected`: two jobs that differ only in those do the same work.

## Errors and warnings

The whole file is checked at once, and every problem is reported with its
line and column, so you do not fix them one run at a time. An **error**
means the file cannot be used. A **warning** means it can, but probably
does not do what you meant.

This file has three mistakes:

```toml invalid
[project]
name = "my-app"

[recipes.test]
memroy = "8GiB"
timeout = "30"
run = "cargo test"

[[recipes.e2e.steps]]
name = "tests"
run = "cargo test"
needs = ["db"]
```

and is reported as:

```text
error: 5:1: recipes.test.memroy: unknown key `memroy`; did you mean `memory`?
error: 6:11: recipes.test.timeout: `30` has no unit: use h, m, s or ms, as in `30m`
error: 12:10: recipes.e2e.steps[0].needs[0]: no step named `db` in this recipe
```

## A full example

Every key of the format in one file.

```toml
[project]
name = "my-app"                      # the project's name on the fleet

[capture]
exclude = ["node_modules/", "target/", "dist/"]
include_ignored = ["generated/schema.json"]
secrets = ["*.secret"]               # in addition to the built-in patterns
allow_secrets = []
roots = { "libs/shared" = "../shared-lib" }

[defaults]                           # merged into every recipe
priority = "normal"
timeout = "30m"
shell = ["bash", "-euo", "pipefail", "-c"]

[recipes.test]
description = "Unit tests"
hosts = ["linux", "macos-aarch64", { os = "windows", arch = "x86_64", os_version = ">=10" }]
target = "x86_64-unknown-linux-gnu"
test_runtime = "linux"
requires = ["rust>=1.85", "node=22", { name = "xcode", version = ">=16", attributes = { sdk = "iphoneos18.0" } }]
isolation = "native"
cpu = 4
memory = "8GiB"
disk = "10GiB"
max_cpu = 8
max_memory = "12GiB"
max_disk = "20GiB"
tokens = { "ios-simulator" = 1 }
parallelism = 8
priority = "interactive"
retry = "pure"
max_attempts = 2
timeout = "20m"
cleanup_timeout = "2m"
cloud = "existing-only"
lan_only = false
disconnected = "10m"
artifacts = ["target/release/app", "dist/**"]
test_reports = ["**/junit.xml"]
cwd = "."
env = { RUST_BACKTRACE = "1" }
run = "cargo test"

[recipes.e2e]
description = "End-to-end tests, then leave the app running"
hosts = ["linux"]
retry = "isolated"

[[recipes.e2e.steps]]
name = "build"
run = "cargo build --locked"
cpu = 8
memory = "6GiB"

[[recipes.e2e.steps]]
name = "db"
kind = "service"
run = ["postgres", "-D", "data"]      # an array runs without a shell
cwd = "services/db"
cpu = 1
memory = "1GiB"
tokens = { "pg-connections" = 20 }
ports = [5432]
ready = { tcp = 5432, timeout = "30s", interval = "1s" }

[[recipes.e2e.steps]]
name = "app"
kind = "service"
run = "cargo run --release"
needs = ["build", "db"]
env = { DATABASE_URL = "postgres://localhost:5432/app" }
cpu = 1
memory = "2GiB"
ports = [8080]
ready = { http = 8080, path = "/health", status = 200 }

[[recipes.e2e.steps]]
name = "tests"
kind = "test"
run = "cargo test --test e2e"
shell = ["sh", "-c"]
needs = ["app"]
timeout = "10m"
cpu = 4
memory = "6GiB"

[[recipes.e2e.steps]]
name = "collect-logs"
kind = "cleanup"
run = "tar czf logs.tgz services/db/log"

[recipes.e2e.environment]            # keep `db` and `app` alive afterwards
idle_timeout = "15m"
lease = "30m"
max_lifetime = "4h"
```
