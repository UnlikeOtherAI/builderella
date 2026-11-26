//! Parsing and validation of `builderella.toml`.

mod common;

use bella_core::job::{
    CloudMode, CommandSpec, LeaseSpec, Priority, Readiness, ReadinessProbe, RetryClass, StepKind,
};
use bella_core::platform::{Arch, IsolationLevel, Os, PlatformSelector};
use bella_core::resources::ResourceVector;
use bella_recipe::{
    DEFAULT_READY_INTERVAL_MS, DEFAULT_READY_TIMEOUT_SECS, LoadError, MAIN_STEP, MAX_STEPS,
    RECIPE_FILE_NAME, Severity, find_recipe_file, load, parse,
};
use common::*;
use std::collections::BTreeMap;

fn shell(script: &str) -> CommandSpec {
    CommandSpec::Shell {
        script: script.to_string(),
        shell: None,
    }
}

fn argv(args: &[&str]) -> CommandSpec {
    CommandSpec::Argv {
        argv: args.iter().map(|s| s.to_string()).collect(),
    }
}

/// The starting design of the format, with every key in use.
const FULL: &str = r#"
[project]
name = "my-app"

[capture]
exclude = ["node_modules/", "target/", "dist/"]
include_ignored = ["generated/schema.json"]
secrets = ["*.secret"]
allow_secrets = []
roots = { "libs/shared" = "../shared-lib" }

[defaults]
priority = "normal"
timeout = "30m"

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
max_memory = "12GiB"
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
env = { RUST_BACKTRACE = "1" }
run = "cargo test"

[[recipes.e2e.steps]]
name = "build"
run = "cargo build --locked"

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
run = "cargo test"
needs = ["build", "db"]
memory = "6GiB"

[recipes.e2e.environment]
idle_timeout = "15m"
lease = "30m"
max_lifetime = "4h"
"#;

#[test]
fn smallest_useful_file() {
    let file = valid(
        r#"
[project]
name = "my-app"

[recipes.test]
cpu = 2
memory = "4GiB"
run = "cargo test"
"#,
    );
    assert_eq!(file.project.name, "my-app");
    assert!(file.warnings.is_empty(), "{}", render(&file.warnings));
    assert_eq!(file.recipe_names().collect::<Vec<_>>(), ["test"]);

    let recipe = &file.recipes["test"];
    assert_eq!(recipe.name, "test");
    assert_eq!(recipe.steps.len(), 1);
    let step = &recipe.steps[0];
    assert_eq!(step.name, MAIN_STEP);
    assert_eq!(step.kind, StepKind::Task);
    assert_eq!(step.command, shell("cargo test"));
    assert!(step.depends_on.is_empty() && step.resources.is_none());
    assert_eq!(recipe.envelope.peak, ResourceVector::new(2000, 4 * GIB, 0));
    assert!(recipe.compatibility.build_hosts.is_empty());
    assert_eq!(recipe.timeout_secs, None);
    assert_eq!(recipe.environment, None);
}

#[test]
fn a_file_may_define_no_recipes() {
    let file = valid("[project]\nname = \"tools\"\n");
    assert!(file.recipes.is_empty());
    assert_eq!(file.capture, Default::default());
}

#[test]
fn every_key_of_the_format_is_read() {
    let file = valid(FULL);
    assert!(file.warnings.is_empty(), "{}", render(&file.warnings));

    assert_eq!(file.capture.exclude, ["node_modules/", "target/", "dist/"]);
    assert_eq!(file.capture.include_ignored, ["generated/schema.json"]);
    assert_eq!(file.capture.secrets, ["*.secret"]);
    assert!(file.capture.allow_secrets.is_empty());
    assert_eq!(
        file.capture.roots,
        BTreeMap::from([("libs/shared".to_string(), "../shared-lib".to_string())])
    );

    let test = &file.recipes["test"];
    assert_eq!(test.description.as_deref(), Some("Unit tests"));
    assert_eq!(
        test.envelope.peak,
        ResourceVector::new(4000, 8 * GIB, 10 * GIB).with_token("ios-simulator", 1)
    );
    assert_eq!(test.envelope.initial, test.envelope.peak);
    assert_eq!(
        test.envelope.maximum,
        ResourceVector::new(4000, 12 * GIB, 10 * GIB).with_token("ios-simulator", 1)
    );
    assert!(test.envelope.is_well_formed());
    assert_eq!(test.parallelism, Some(8));
    assert_eq!(test.priority, Priority::Interactive);
    assert_eq!(test.retry_class, RetryClass::Pure);
    assert_eq!(test.max_attempts, Some(2));
    assert_eq!(test.timeout_secs, Some(20 * 60));
    assert_eq!(test.cleanup_timeout_secs, Some(120));
    assert_eq!(test.cloud, CloudMode::ExistingOnly);
    assert!(!test.lan_only);
    assert_eq!(test.disconnected_secs, Some(600));
    assert_eq!(test.artifacts, ["target/release/app", "dist/**"]);
    assert_eq!(test.test_reports, ["**/junit.xml"]);
    assert_eq!(
        test.env,
        BTreeMap::from([("RUST_BACKTRACE".to_string(), "1".to_string())])
    );
    assert_eq!(test.steps[0].command, shell("cargo test"));

    let e2e = &file.recipes["e2e"];
    // `[defaults]` reach a recipe that sets neither key.
    assert_eq!(e2e.timeout_secs, Some(30 * 60));
    assert_eq!(e2e.priority, Priority::Normal);
    let names: Vec<&str> = e2e.steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["build", "db", "tests"]);

    let db = &e2e.steps[1];
    assert_eq!(db.kind, StepKind::Service);
    assert_eq!(db.command, argv(&["postgres", "-D", "data"]));
    assert_eq!(db.ports, [5432]);
    assert_eq!(db.resources, Some(ResourceVector::new(1000, GIB, 0)));
    assert_eq!(
        db.readiness,
        Some(Readiness {
            probe: ReadinessProbe::Tcp { port: 5432 },
            timeout_secs: 30,
            interval_ms: DEFAULT_READY_INTERVAL_MS,
        })
    );

    let tests = &e2e.steps[2];
    assert_eq!(tests.kind, StepKind::Test);
    assert_eq!(tests.depends_on, ["build", "db"]);
    assert_eq!(e2e.envelope.peak, ResourceVector::new(1000, 7 * GIB, 0));
    assert_eq!(
        e2e.environment,
        Some(LeaseSpec {
            idle_timeout_secs: 15 * 60,
            lease_secs: 30 * 60,
            max_lifetime_secs: 4 * 60 * 60,
        })
    );
}

#[test]
fn all_problems_are_reported_at_once_with_positions() {
    let text = r#"[project]
name = "demo"
colour = "red"

[defaults]
timeout = "30"
steps = []

[recipes.build]
memroy = "8GiB"
memory = "8 gigs"
cpu = "four"
priority = "asap"
run = "cargo build"

[[recipes.build.steps]]
name = "Build It"
run = "make"
needs = ["missing"]
cwd = "../outside"

[recipes.empty]
description = "nothing to run"
"#;
    let issues = invalid(text);
    assert_eq!(issues.len(), 12, "{}", render(&issues));
    assert!(issues.iter().all(|i| i.severity == Severity::Error));

    let expect = |path: &str, needle: &str, message: &str| {
        let issue = at(&issues, path);
        assert_eq!(issue.line, Some(line_of(text, needle)), "{issue}");
        assert!(issue.column.is_some(), "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    };
    expect("project.colour", "colour", "unknown key `colour`");
    expect("defaults.timeout", "\"30\"", "`30` has no unit");
    expect("defaults.steps", "steps = []", "unknown key `steps`");
    expect(
        "recipes.build.memroy",
        "memroy",
        "unknown key `memroy`; did you mean `memory`?",
    );
    expect("recipes.build.memory", "8 gigs", "unknown size unit `gigs`");
    expect("recipes.build.cpu", "four", "expected a number of CPUs");
    expect("recipes.build.priority", "asap", "unknown value `asap`");
    expect("recipes.build.run", "cargo build", "mutually exclusive");
    expect(
        "recipes.build.steps[0].name",
        "Build It",
        "`Build It` is not a valid step name",
    );
    expect(
        "recipes.build.steps[0].needs[0]",
        "missing",
        "no step named `missing`",
    );
    expect("recipes.build.steps[0].cwd", "outside", "contains `..`");
    expect("recipes.empty", "[recipes.empty]", "nothing to run");

    // Columns point at the value, or at the key when the key is the problem.
    assert_eq!(at(&issues, "project.colour").column, Some(1));
    assert_eq!(
        at(&issues, "recipes.build.memory").column,
        Some(column_of(text, "\"8 gigs\""))
    );
    assert_eq!(
        at(&issues, "recipes.build.steps[0].needs[0]").column,
        Some(column_of(text, "\"missing\""))
    );

    // In reading order.
    let lines: Vec<u32> = issues.iter().filter_map(|i| i.line).collect();
    assert!(lines.is_sorted(), "{lines:?}");
}

#[test]
fn unknown_keys_are_errors_in_every_table() {
    let service = "[[recipes.r.steps]]\nname = \"web\"\nkind = \"service\"\nrun = \"./web\"\n";
    let cases: [(String, &str); 10] = [
        (
            "[project]\nname = \"demo\"\n\n[extra]\nx = 1\n".to_string(),
            "extra",
        ),
        (
            "[project]\nname = \"demo\"\nowner = \"me\"\n".to_string(),
            "project.owner",
        ),
        (
            "[project]\nname = \"demo\"\n\n[capture]\nexcludes = []\n".to_string(),
            "capture.excludes",
        ),
        (
            "[project]\nname = \"demo\"\n\n[defaults]\nrun = \"x\"\n".to_string(),
            "defaults.run",
        ),
        (recipe("tiemout = \"5m\"\nrun = \"x\""), "recipes.r.tiemout"),
        (
            with_steps("[[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\ncommand = \"y\"\n"),
            "recipes.r.steps[0].command",
        ),
        (
            with_steps(&format!("{service}ready = {{ tcp = 80, retries = 3 }}\n")),
            "recipes.r.steps[0].ready.retries",
        ),
        (
            with_steps(&format!(
                "{service}ready = {{ tcp = 80 }}\n\n[recipes.r.environment]\nttl = \"1h\"\n"
            )),
            "recipes.r.environment.ttl",
        ),
        (
            recipe("hosts = [{ os = \"linux\", distro = \"ubuntu\" }]\nrun = \"x\""),
            "recipes.r.hosts[0].distro",
        ),
        (
            recipe("requires = [{ name = \"rust\", min = \"1.85\" }]\nrun = \"x\""),
            "recipes.r.requires[0].min",
        ),
    ];
    for (text, path) in cases {
        let issue = only_error(&text);
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.starts_with("unknown key"), "{issue}");
        assert!(issue.line.is_some(), "{issue}");
    }

    let typo = only_error(&recipe("tiemout = \"5m\"\nrun = \"x\""));
    assert!(typo.message.ends_with("did you mean `timeout`?"), "{typo}");
}

#[test]
fn invalid_toml_reports_every_syntax_error_with_its_position() {
    let text = "[project]\nname = \nother = \"unterminated\n";
    let issues = invalid(text);
    assert!(issues.len() >= 2, "{}", render(&issues));
    for issue in &issues {
        assert!(issue.message.starts_with("invalid TOML:"), "{issue}");
        assert!(issue.path.is_empty(), "{issue}");
        assert!(issue.line.is_some() && issue.column.is_some(), "{issue}");
    }
    assert!(
        issues.iter().any(|i| i.line == Some(2)),
        "{}",
        render(&issues)
    );
    assert!(
        issues.iter().any(|i| i.line == Some(3)),
        "{}",
        render(&issues)
    );

    let duplicate = only_error("[project]\nname = \"a\"\nname = \"b\"\n");
    assert!(duplicate.message.contains("duplicate key"), "{duplicate}");
    assert_eq!(duplicate.line, Some(3));
}

#[test]
fn windows_line_endings_and_a_byte_order_mark_are_accepted() {
    let unix =
        "[project]\nname = \"demo\"\n\n[recipes.r]\ncpu = 1\nmemory = \"1GiB\"\nrun = \"make\"\n";
    let windows = format!("\u{feff}{}", unix.replace('\n', "\r\n"));
    assert_eq!(valid(&windows).recipes, valid(unix).recipes);

    // Positions are the ones an editor shows.
    let broken = windows
        .replace("cpu = 1", "cpu = true")
        .replace("[project]", "[project]\r\nx = 1");
    let issues = invalid(&broken);
    assert_eq!(issues.len(), 2, "{}", render(&issues));
    let unknown = at(&issues, "project.x");
    assert_eq!((unknown.line, unknown.column), (Some(2), Some(1)));
    let cpu = at(&issues, "recipes.r.cpu");
    assert_eq!((cpu.line, cpu.column), (Some(6), Some(7)));

    let first_line = invalid("\u{feff}colour = 1\r\n[project]\r\nname = \"demo\"\r\n");
    let issue = at(&first_line, "colour");
    assert_eq!((issue.line, issue.column), (Some(1), Some(1)));
}

#[test]
fn the_project_section_is_required() {
    let missing = only_error("[recipes.r]\nrun = \"x\"\ncpu = 1\nmemory = \"1GiB\"\n");
    assert_eq!(missing.path, "project");
    assert!(missing.message.contains("missing `[project]`"), "{missing}");
    assert_eq!(missing.line, None);

    let unnamed = only_error("[project]\n");
    assert_eq!(unnamed.path, "project");
    assert!(unnamed.message.contains("missing `name`"), "{unnamed}");

    for bad in ["\"my app\"", "\"\"", "\"-x\"", "7"] {
        let issue = only_error(&format!("[project]\nname = {bad}\n"));
        assert_eq!(issue.path, "project.name", "{issue}");
    }
}

#[test]
fn capture_section_is_validated() {
    let cases = [
        (
            "exclude = \"target/\"",
            "capture.exclude",
            "expected an array of strings",
        ),
        (
            "exclude = [\"\"]",
            "capture.exclude[0]",
            "must not be empty",
        ),
        ("secrets = [1]", "capture.secrets[0]", "expected a string"),
        (
            "roots = { \"/abs\" = \"x\" }",
            "capture.roots.\"/abs\"",
            "not a valid mount path",
        ),
        (
            "roots = { \"libs/shared\" = \"/srv/shared\" }",
            "capture.roots.\"libs/shared\"",
            "absolute path",
        ),
        (
            "roots = { libs = 3 }",
            "capture.roots.libs",
            "expected a string",
        ),
    ];
    for (body, path, message) in cases {
        let issue = only_error(&format!(
            "[project]\nname = \"demo\"\n\n[capture]\n{body}\n"
        ));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    }
}

#[test]
fn wrong_types_and_values_are_precise() {
    let cases = [
        (
            "cpu = \"4\"",
            "recipes.r.cpu",
            "expected a number of CPUs, such as 4 or 0.5, found a string",
        ),
        ("cpu = -1", "recipes.r.cpu", "`-1` is not a CPU count"),
        ("cpu = 0.0001", "recipes.r.cpu", "finer than a thousandth"),
        (
            "lan_only = \"yes\"",
            "recipes.r.lan_only",
            "expected `true` or `false`, found a string",
        ),
        (
            "hosts = \"linux\"",
            "recipes.r.hosts",
            "expected an array of platforms, found a string",
        ),
        ("hosts = []", "recipes.r.hosts", "`hosts` is empty"),
        (
            "hosts = [1]",
            "recipes.r.hosts[0]",
            "expected a platform such as",
        ),
        (
            "hosts = [\"plan9\"]",
            "recipes.r.hosts[0]",
            "unknown operating system `plan9`",
        ),
        (
            "hosts = [{ arch = \"mips\" }]",
            "recipes.r.hosts[0].arch",
            "unknown architecture `mips`",
        ),
        (
            "hosts = [{ os = \"macos\", os_version = \"latest\" }]",
            "recipes.r.hosts[0].os_version",
            "`latest` is not a version requirement",
        ),
        (
            "test_runtime = [\"linux\"]",
            "recipes.r.test_runtime",
            "found an array",
        ),
        ("target = \"\"", "recipes.r.target", "must not be empty"),
        (
            "requires = [\"Rust\"]",
            "recipes.r.requires[0]",
            "`Rust` is not a requirement",
        ),
        (
            "requires = [\"rust>=\"]",
            "recipes.r.requires[0]",
            "is not a version requirement",
        ),
        (
            "requires = [{ version = \">=1\" }]",
            "recipes.r.requires[0]",
            "missing `name`",
        ),
        (
            "requires = [{ name = \"xcode\", attributes = { sdk = 18 } }]",
            "recipes.r.requires[0].attributes.sdk",
            "expected a string",
        ),
        (
            "isolation = \"docker\"",
            "recipes.r.isolation",
            "unknown value `docker`: use `native`, `container` or `vm`",
        ),
        (
            "priority = \"Urgent\"",
            "recipes.r.priority",
            "did you mean `urgent`?",
        ),
        (
            "retry = \"never\"",
            "recipes.r.retry",
            "use `pure`, `isolated` or `external`",
        ),
        (
            "cloud = \"always\"",
            "recipes.r.cloud",
            "use `existing-only`, `manual`, `auto-overflow` or `hybrid`",
        ),
        (
            "env = { A = 1 }",
            "recipes.r.env.A",
            "found an integer; environment values are strings, so quote it",
        ),
        (
            "env = { \"A=B\" = \"x\" }",
            "recipes.r.env.\"A=B\"",
            "not a valid environment variable name",
        ),
        (
            "tokens = { gpu = -1 }",
            "recipes.r.tokens.gpu",
            "`-1` is out of range",
        ),
        (
            "tokens = { GPU = 1 }",
            "recipes.r.tokens.GPU",
            "`GPU` is not a token name",
        ),
        (
            "tokens = { gpu = \"1\" }",
            "recipes.r.tokens.gpu",
            "expected a count, found a string",
        ),
        (
            "parallelism = 0",
            "recipes.r.parallelism",
            "`0` is out of range",
        ),
        (
            "max_attempts = 0",
            "recipes.r.max_attempts",
            "`0` is out of range",
        ),
        (
            "max_attempts = 1.5",
            "recipes.r.max_attempts",
            "expected a number of attempts, found a float",
        ),
        (
            "artifacts = [\"../x\"]",
            "recipes.r.artifacts[0]",
            "contains `..`",
        ),
        (
            "test_reports = [\"/var/log/junit.xml\"]",
            "recipes.r.test_reports[0]",
            "absolute path",
        ),
        ("shell = []", "recipes.r.shell", "`shell` needs a program"),
        (
            "description = 5",
            "recipes.r.description",
            "expected a string, found an integer",
        ),
        ("cwd = \"/abs\"", "recipes.r.cwd", "absolute path"),
        ("cwd = \"a/../..\"", "recipes.r.cwd", "contains `..`"),
    ];
    for (key, path, message) in cases {
        let issue = only_error(&recipe(&format!("{key}\nrun = \"x\"")));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
        assert_eq!(issue.line, Some(5), "{issue}");
    }
}

#[test]
fn unit_errors_name_the_value() {
    let cases = [
        (
            "memory = \"8 gigs\"",
            "recipes.r.memory",
            "`8 gigs` has unknown size unit `gigs`",
        ),
        (
            "memory = \"8G\"",
            "recipes.r.memory",
            "`8G` has unknown size unit `G`",
        ),
        (
            "memory = \"lots\"",
            "recipes.r.memory",
            "`lots` is not a size",
        ),
        (
            "memory = 8192",
            "recipes.r.memory",
            "expected a size in quotes, such as \"8GiB\", found an integer",
        ),
        ("disk = \"10\"", "recipes.r.disk", "`10` has no unit"),
        (
            "max_disk = \"0.1KiB\"",
            "recipes.r.max_disk",
            "`0.1KiB` is not a whole number of bytes",
        ),
        ("timeout = \"30\"", "recipes.r.timeout", "`30` has no unit"),
        (
            "timeout = 30",
            "recipes.r.timeout",
            "expected a duration in quotes, such as \"30m\", found an integer",
        ),
        (
            "timeout = \"3 days\"",
            "recipes.r.timeout",
            "`3 days` has unknown duration unit `days`",
        ),
        (
            "timeout = \"30m1h\"",
            "recipes.r.timeout",
            "`30m1h` is not a duration",
        ),
        (
            "timeout = \"0s\"",
            "recipes.r.timeout",
            "`0s` must be longer than zero",
        ),
        (
            "cleanup_timeout = \"1500ms\"",
            "recipes.r.cleanup_timeout",
            "`1500ms` is not a whole number of seconds",
        ),
        (
            "disconnected = \"soon\"",
            "recipes.r.disconnected",
            "`soon` is not a duration",
        ),
    ];
    for (key, path, message) in cases {
        let issue = only_error(&recipe(&format!("{key}\nrun = \"x\"")));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    }
}

#[test]
fn decimal_and_binary_units_are_different_sizes() {
    let peak = |memory: &str| {
        let text = recipe(&format!("cpu = 1\nmemory = \"{memory}\"\nrun = \"x\""));
        valid(&text).recipes["r"].envelope.peak.memory_bytes
    };
    assert_eq!(peak("8GiB"), 8 * GIB);
    assert_eq!(peak("8GB"), 8_000_000_000);
    assert_eq!(peak("512MiB"), 512 * MIB);
    assert_eq!(peak("512MB"), 512_000_000);
    assert_eq!(peak("1.5GiB"), 3 * GIB / 2);
    assert_eq!(peak("1TiB"), 1024 * GIB);
    assert_eq!(peak("2048KiB"), 2 * MIB);
    assert_eq!(peak("4096B"), 4096);

    let file = valid(&recipe(
        "cpu = 0.5\nmemory = \"1GiB\"\ntimeout = \"1h30m\"\ncleanup_timeout = \"90s\"\n\
         disconnected = \"0s\"\nrun = \"x\"",
    ));
    let recipe = &file.recipes["r"];
    assert_eq!(recipe.envelope.peak.cpu_milli, 500);
    assert_eq!(recipe.timeout_secs, Some(5400));
    assert_eq!(recipe.cleanup_timeout_secs, Some(90));
    assert_eq!(recipe.disconnected_secs, Some(0));
}

#[test]
fn run_is_shorthand_and_excludes_steps() {
    let both = only_error(&with_steps(
        "[recipes.r]\nrun = \"x\"\n\n[[recipes.r.steps]]\nname = \"a\"\nrun = \"y\"\n",
    ));
    assert_eq!(both.path, "recipes.r.run");
    assert!(both.message.contains("mutually exclusive"), "{both}");

    let neither = only_error(&recipe("description = \"empty\""));
    assert_eq!(neither.path, "recipes.r");
    assert!(neither.message.contains("nothing to run"), "{neither}");

    let empty = only_error(&recipe("steps = []"));
    assert_eq!(empty.path, "recipes.r.steps");
    assert!(empty.message.contains("`steps` is empty"), "{empty}");
}

#[test]
fn a_string_runs_in_a_shell_and_an_array_runs_directly() {
    let file = valid(&with_steps(
        r#"
[recipes.r]
cpu = 1
memory = "1GiB"
shell = ["bash", "-euo", "pipefail", "-c"]

[[recipes.r.steps]]
name = "script"
run = "make && make check"

[[recipes.r.steps]]
name = "direct"
run = ["cargo", "build", "--message-format=json"]

[[recipes.r.steps]]
name = "own-shell"
shell = ["pwsh", "-Command"]
run = "Get-ChildItem"
"#,
    ));
    let steps = &file.recipes["r"].steps;
    assert_eq!(
        steps[0].command,
        CommandSpec::Shell {
            script: "make && make check".to_string(),
            shell: Some(vec![
                "bash".into(),
                "-euo".into(),
                "pipefail".into(),
                "-c".into()
            ]),
        }
    );
    assert_eq!(
        steps[1].command,
        argv(&["cargo", "build", "--message-format=json"])
    );
    assert_eq!(
        steps[2].command,
        CommandSpec::Shell {
            script: "Get-ChildItem".to_string(),
            shell: Some(vec!["pwsh".into(), "-Command".into()]),
        }
    );

    let cases = [
        ("run = \"\"", "recipes.r.run", "the command is empty"),
        ("run = \"   \"", "recipes.r.run", "the command is empty"),
        ("run = []", "recipes.r.run", "the command is empty"),
        ("run = [\"\"]", "recipes.r.run", "the command is empty"),
        ("run = 5", "recipes.r.run", "expected a command"),
        (
            "run = [\"a\", 1]",
            "recipes.r.run[1]",
            "expected a string, found an integer",
        ),
    ];
    for (body, path, message) in cases {
        let issue = only_error(&recipe(body));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    }

    let stray_shell = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"a\"\nshell = [\"bash\", \"-c\"]\nrun = [\"make\"]\n",
    ));
    assert_eq!(stray_shell.path, "recipes.r.steps[0].shell");
    assert!(
        stray_shell
            .message
            .contains("only applies when `run` is a string")
    );
}

#[test]
fn working_directories_are_relative_and_stay_inside() {
    let file = valid(&with_steps(
        r#"
[recipes.r]
cpu = 1
memory = "1GiB"
cwd = "web"

[[recipes.r.steps]]
name = "inherits"
run = "npm test"

[[recipes.r.steps]]
name = "own"
cwd = "services/api/"
run = "cargo test"

[[recipes.r.steps]]
name = "root"
cwd = "."
run = "ls"
"#,
    ));
    let cwds: Vec<Option<&str>> = file.recipes["r"]
        .steps
        .iter()
        .map(|s| s.cwd.as_deref())
        .collect();
    assert_eq!(cwds, [Some("web"), Some("services/api"), None]);

    let shorthand = valid(&recipe(
        "cpu = 1\nmemory = \"1GiB\"\ncwd = \"web\"\nrun = \"npm test\"",
    ));
    assert_eq!(shorthand.recipes["r"].steps[0].cwd.as_deref(), Some("web"));

    for bad in ["..", "../sibling", "a/../../b", "/abs", "C:/abs", "a\\\\b"] {
        let issue = only_error(&with_steps(&format!(
            "[[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\ncwd = \"{bad}\"\n"
        )));
        assert_eq!(issue.path, "recipes.r.steps[0].cwd", "{issue}");
    }
}

#[test]
fn step_names_are_unique_and_well_formed() {
    let text = with_steps(
        "[[recipes.r.steps]]\nname = \"build\"\nrun = \"a\"\n\n\
         [[recipes.r.steps]]\nname = \"build\"\nrun = \"b\"\n",
    );
    let duplicate = only_error(&text);
    assert_eq!(duplicate.path, "recipes.r.steps[1].name");
    assert!(
        duplicate.message.contains("duplicate step name `build`"),
        "{duplicate}"
    );
    assert_eq!(duplicate.line, Some(9));

    for bad in ["Build", "has space", "", "ünit", "a.b"] {
        let issue = only_error(&with_steps(&format!(
            "[[recipes.r.steps]]\nname = \"{bad}\"\nrun = \"x\"\n"
        )));
        assert_eq!(issue.path, "recipes.r.steps[0].name", "{issue}");
        assert!(
            issue.message.contains("is not a valid step name"),
            "{issue}"
        );
    }

    let unnamed = only_error(&with_steps("[[recipes.r.steps]]\nrun = \"x\"\n"));
    assert_eq!(unnamed.path, "recipes.r.steps[0]");
    assert!(unnamed.message.contains("missing `name`"), "{unnamed}");

    let no_command = only_error(&with_steps("[[recipes.r.steps]]\nname = \"a\"\n"));
    assert_eq!(no_command.path, "recipes.r.steps[0]");
    assert!(no_command.message.contains("missing `run`"), "{no_command}");

    let bad_recipe = only_error("[project]\nname = \"demo\"\n\n[recipes.Build]\nrun = \"x\"\n");
    assert_eq!(bad_recipe.path, "recipes.Build");
    assert!(
        bad_recipe
            .message
            .contains("`Build` is not a valid recipe name"),
        "{bad_recipe}"
    );
    assert_eq!(bad_recipe.line, Some(4));
}

#[test]
fn needs_must_name_existing_steps_without_cycles() {
    let unknown = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"build\"\nrun = \"a\"\n\n\
         [[recipes.r.steps]]\nname = \"test\"\nrun = \"b\"\nneeds = [\"biuld\"]\n",
    ));
    assert_eq!(unknown.path, "recipes.r.steps[1].needs[0]");
    assert_eq!(
        unknown.message,
        "no step named `biuld` in this recipe; did you mean `build`?"
    );
    assert_eq!(unknown.line, Some(11));

    let itself = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\nneeds = [\"a\"]\n",
    ));
    assert_eq!(itself.path, "recipes.r.steps[0].needs[0]");
    assert!(itself.message.contains("cannot need itself"), "{itself}");

    let cycle = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\nneeds = [\"c\"]\n\n\
         [[recipes.r.steps]]\nname = \"b\"\nrun = \"x\"\nneeds = [\"a\"]\n\n\
         [[recipes.r.steps]]\nname = \"c\"\nrun = \"x\"\nneeds = [\"b\"]\n",
    ));
    assert_eq!(cycle.path, "recipes.r.steps[0].needs[0]");
    assert_eq!(
        cycle.message,
        "steps need each other in a cycle: a -> c -> b -> a"
    );
    assert_eq!(cycle.line, Some(7));

    let not_names = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\nneeds = \"b\"\n",
    ));
    assert_eq!(not_names.path, "recipes.r.steps[0].needs");
    assert!(
        not_names
            .message
            .contains("expected an array of step names"),
        "{not_names}"
    );
}

#[test]
fn a_step_may_not_depend_on_a_cleanup_step() {
    let issue = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"wipe\"\nkind = \"cleanup\"\nrun = \"rm -rf tmp\"\n\n\
         [[recipes.r.steps]]\nname = \"test\"\nrun = \"make test\"\nneeds = [\"wipe\"]\n",
    ));
    assert_eq!(issue.path, "recipes.r.steps[1].needs[0]");
    assert!(
        issue.message.contains("`wipe` is a cleanup step"),
        "{issue}"
    );
}

#[test]
fn steps_come_out_in_a_stable_topological_order() {
    let text = with_steps(
        r#"
[recipes.r]
cpu = 1
memory = "1GiB"

[[recipes.r.steps]]
name = "wipe"
kind = "cleanup"
run = "rm -rf tmp"

[[recipes.r.steps]]
name = "tests"
kind = "test"
run = "make test"
needs = ["build", "db"]

[[recipes.r.steps]]
name = "lint"
run = "make lint"

[[recipes.r.steps]]
name = "db"
kind = "service"
run = "./db"
ready = { tcp = 5432 }

[[recipes.r.steps]]
name = "build"
run = "make"
needs = ["gen"]

[[recipes.r.steps]]
name = "gen"
run = "make gen"
"#,
    );
    let order = |text: &str| -> Vec<String> {
        valid(text).recipes["r"]
            .steps
            .iter()
            .map(|s| s.name.clone())
            .collect()
    };
    let first = order(&text);
    // Each step follows what it needs; otherwise declaration order holds,
    // and the cleanup step is moved to the end.
    assert_eq!(first, ["lint", "db", "gen", "build", "tests", "wipe"]);
    for _ in 0..3 {
        assert_eq!(order(&text), first);
    }
    // `needs` is carried over as written.
    let file = valid(&text);
    let tests = file.recipes["r"]
        .steps
        .iter()
        .find(|s| s.name == "tests")
        .unwrap();
    assert_eq!(tests.depends_on, ["build", "db"]);
}

fn service(ready: &str) -> String {
    with_steps(&format!(
        "[recipes.r]\ncpu = 1\nmemory = \"1GiB\"\n\n\
         [[recipes.r.steps]]\nname = \"web\"\nkind = \"service\"\nrun = \"./web\"\n{ready}\n\n\
         [[recipes.r.steps]]\nname = \"check\"\nrun = \"true\"\nneeds = [\"web\"]\n"
    ))
}

#[test]
fn readiness_probes() {
    let probe = |ready: &str| -> Readiness {
        let file = valid(&service(&format!("ready = {ready}")));
        assert!(file.warnings.is_empty(), "{}", render(&file.warnings));
        file.recipes["r"].steps[0].readiness.clone().unwrap()
    };
    assert_eq!(
        probe("{ tcp = 5432, timeout = \"30s\" }"),
        Readiness {
            probe: ReadinessProbe::Tcp { port: 5432 },
            timeout_secs: 30,
            interval_ms: DEFAULT_READY_INTERVAL_MS,
        }
    );
    assert_eq!(
        probe("{ http = 8080, path = \"/health\", status = 200 }"),
        Readiness {
            probe: ReadinessProbe::Http {
                port: 8080,
                path: "/health".to_string(),
                status: Some(200),
            },
            timeout_secs: DEFAULT_READY_TIMEOUT_SECS,
            interval_ms: DEFAULT_READY_INTERVAL_MS,
        }
    );
    assert_eq!(
        probe("{ http = 8080 }").probe,
        ReadinessProbe::Http {
            port: 8080,
            path: "/".to_string(),
            status: None,
        }
    );
    assert_eq!(
        probe("{ log = \"ready to accept\" }").probe,
        ReadinessProbe::LogLine {
            contains: "ready to accept".to_string(),
        }
    );
    assert_eq!(
        probe("{ run = \"pg_isready\" }").probe,
        ReadinessProbe::Command {
            command: shell("pg_isready"),
        }
    );
    let slow = probe("{ run = [\"pg_isready\", \"-q\"], interval = \"2s\", timeout = \"2m\" }");
    assert_eq!(
        slow.probe,
        ReadinessProbe::Command {
            command: argv(&["pg_isready", "-q"]),
        }
    );
    assert_eq!((slow.timeout_secs, slow.interval_ms), (120, 2000));

    let cases = [
        (
            "{ tcp = 80, http = 80 }",
            "recipes.r.steps[0].ready",
            "exactly one probe",
        ),
        (
            "{ timeout = \"5s\" }",
            "recipes.r.steps[0].ready",
            "exactly one probe",
        ),
        (
            "{ tcp = 80, path = \"/x\" }",
            "recipes.r.steps[0].ready.path",
            "only applies to an `http` probe",
        ),
        (
            "{ log = \"up\", status = 200 }",
            "recipes.r.steps[0].ready.status",
            "only applies to an `http` probe",
        ),
        (
            "{ http = 8080, path = \"health\" }",
            "recipes.r.steps[0].ready.path",
            "must start with `/`",
        ),
        (
            "{ http = 8080, status = 99 }",
            "recipes.r.steps[0].ready.status",
            "`99` is out of range",
        ),
        (
            "{ tcp = 0 }",
            "recipes.r.steps[0].ready.tcp",
            "`0` is out of range",
        ),
        (
            "{ tcp = 70000 }",
            "recipes.r.steps[0].ready.tcp",
            "expected a port number, 1 to 65535",
        ),
        (
            "{ log = \"\" }",
            "recipes.r.steps[0].ready.log",
            "must not be empty",
        ),
        (
            "{ tcp = 80, timeout = \"0s\" }",
            "recipes.r.steps[0].ready.timeout",
            "longer than zero",
        ),
        (
            "{ tcp = 80, interval = \"0ms\" }",
            "recipes.r.steps[0].ready.interval",
            "longer than zero",
        ),
        (
            "\"tcp:80\"",
            "recipes.r.steps[0].ready",
            "expected a table, found a string",
        ),
    ];
    for (ready, path, message) in cases {
        let issue = only_error(&service(&format!("ready = {ready}")));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    }
}

#[test]
fn ready_is_only_for_services_and_a_service_without_it_is_a_warning() {
    let on_task = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"build\"\nrun = \"make\"\nready = { tcp = 8080 }\n",
    ));
    assert_eq!(on_task.path, "recipes.r.steps[0].ready");
    assert!(
        on_task
            .message
            .contains("`ready` only applies to a step with `kind = \"service\"`"),
        "{on_task}"
    );
    assert_eq!(on_task.line, Some(7));

    let text = service("");
    let file = valid(&text);
    assert_eq!(file.warnings.len(), 1, "{}", render(&file.warnings));
    let warning = warning_at(&file.warnings, "recipes.r.steps[0]");
    assert!(
        warning
            .message
            .contains("service `web` has no `ready` probe"),
        "{warning}"
    );
    assert_eq!(warning.line, Some(line_of(&text, "[[recipes.r.steps]]")));
    assert_eq!(file.recipes["r"].warnings, file.warnings);
    assert_eq!(file.recipes["r"].steps[0].readiness, None);
}

#[test]
fn duplicate_ports_are_errors() {
    let within = only_error(&service("ready = { tcp = 8080 }\nports = [8080, 8080]"));
    assert_eq!(within.path, "recipes.r.steps[0].ports[1]");
    assert!(
        within
            .message
            .contains("port 8080 is already used by this step"),
        "{within}"
    );

    let across = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"web\"\nkind = \"service\"\nrun = \"./web\"\n\
         ready = { tcp = 8080 }\nports = [8080]\n\n\
         [[recipes.r.steps]]\nname = \"api\"\nkind = \"service\"\nrun = \"./api\"\n\
         ready = { tcp = 9000 }\nports = [9000, 8080]\n\n\
         [[recipes.r.steps]]\nname = \"check\"\nrun = \"true\"\n",
    ));
    assert_eq!(across.path, "recipes.r.steps[1].ports[1]");
    assert!(
        across
            .message
            .contains("port 8080 is already used by step `web`"),
        "{across}"
    );

    let on_task = only_error(&with_steps(
        "[[recipes.r.steps]]\nname = \"build\"\nrun = \"make\"\nports = [8080]\n",
    ));
    assert_eq!(on_task.path, "recipes.r.steps[0].ports[0]");
    assert!(
        on_task.message.contains("`ports` only applies"),
        "{on_task}"
    );

    for bad in ["0", "65536", "\"http\""] {
        let issue = only_error(&service(&format!(
            "ready = {{ tcp = 80 }}\nports = [{bad}]"
        )));
        assert_eq!(issue.path, "recipes.r.steps[0].ports[0]", "{issue}");
    }

    let fine = valid(&service("ready = { tcp = 8080 }\nports = [8080, 8443]"));
    assert_eq!(fine.recipes["r"].steps[0].ports, [8080, 8443]);
}

/// A19: an application that stays up while its tests run needs both at
/// once. Reserving 2 GiB for the service plus 6 GiB for the test step up
/// front means the app can never take the last slot its own tests need.
#[test]
fn a19_a_service_and_its_tests_reserve_their_combined_peak() {
    let file = valid(&with_steps(
        r#"
[[recipes.r.steps]]
name = "app"
kind = "service"
run = "./app"
cpu = 1
memory = "2GiB"
ready = { tcp = 8080 }

[[recipes.r.steps]]
name = "tests"
kind = "test"
run = "make test"
needs = ["app"]
cpu = 2
memory = "6GiB"
"#,
    ));
    let envelope = &file.recipes["r"].envelope;
    assert_eq!(envelope.peak.memory_bytes, 8 * GIB);
    assert_eq!(envelope.peak.cpu_milli, 3000);
    assert_eq!(envelope.initial, envelope.peak);
    assert_eq!(envelope.maximum, envelope.peak);
    assert!(envelope.is_well_formed());
    assert!(file.warnings.is_empty(), "{}", render(&file.warnings));
}

/// A19: services add up; steps that never overlap contribute only the
/// largest of them, dimension by dimension.
#[test]
fn a19_services_sum_and_other_steps_take_the_maximum() {
    let file = valid(&with_steps(
        r#"
[[recipes.r.steps]]
name = "build"
run = "make"
cpu = 8
memory = "4GiB"
disk = "20GiB"

[[recipes.r.steps]]
name = "db"
kind = "service"
run = "./db"
cpu = 1
memory = "1GiB"
tokens = { "pg-connections" = 10 }
ready = { tcp = 5432 }

[[recipes.r.steps]]
name = "app"
kind = "service"
run = "./app"
cpu = 1
memory = "2GiB"
tokens = { "pg-connections" = 5, "gui-session" = 1 }
ready = { tcp = 8080 }

[[recipes.r.steps]]
name = "unit"
kind = "test"
run = "make unit"
needs = ["build"]
cpu = 2
memory = "6GiB"
tokens = { "ios-simulator" = 2 }

[[recipes.r.steps]]
name = "ui"
kind = "test"
run = "make ui"
needs = ["build", "db", "app"]
cpu = 4
memory = "3GiB"
tokens = { "ios-simulator" = 1, "gui-session" = 1 }

[[recipes.r.steps]]
name = "report"
kind = "cleanup"
run = "make report"
"#,
    ));
    let peak = &file.recipes["r"].envelope.peak;
    // CPU: services 1 + 1, plus the largest other step (build, 8).
    assert_eq!(peak.cpu_milli, 10_000);
    // Memory: services 1 + 2 GiB, plus the largest other step (unit, 6 GiB).
    assert_eq!(peak.memory_bytes, 9 * GIB);
    // Disk: only `build` declares any.
    assert_eq!(peak.disk_bytes, 20 * GIB);
    // Tokens follow the same rule per name.
    assert_eq!(peak.token("pg-connections"), 15);
    assert_eq!(peak.token("ios-simulator"), 2);
    assert_eq!(peak.token("gui-session"), 2);
    assert!(file.recipes["r"].envelope.is_well_formed());
}

/// A19: the recipe-level declaration is a lower bound on the reservation,
/// and a declaration smaller than what the steps need is called out.
#[test]
fn a19_the_recipe_declaration_is_a_lower_bound() {
    let steps = r#"
[[recipes.r.steps]]
name = "app"
kind = "service"
run = "./app"
memory = "2GiB"
ready = { tcp = 8080 }

[[recipes.r.steps]]
name = "tests"
kind = "test"
run = "make test"
needs = ["app"]
memory = "6GiB"
"#;
    let build = |settings: &str| with_steps(&format!("[recipes.r]\n{settings}\n{steps}"));

    // Declared above the steps: the declaration wins, silently.
    let roomy = valid(&build("cpu = 4\nmemory = \"16GiB\"\ndisk = \"5GiB\""));
    assert_eq!(
        roomy.recipes["r"].envelope.peak,
        ResourceVector::new(4000, 16 * GIB, 5 * GIB)
    );
    assert!(roomy.warnings.is_empty(), "{}", render(&roomy.warnings));

    // Declared below the steps: the steps win, with a warning at the key.
    let text = build("cpu = 4\nmemory = \"4GiB\"");
    let tight = valid(&text);
    assert_eq!(
        tight.recipes["r"].envelope.peak,
        ResourceVector::new(4000, 8 * GIB, 0)
    );
    assert_eq!(tight.warnings.len(), 1, "{}", render(&tight.warnings));
    let warning = warning_at(&tight.warnings, "recipes.r.memory");
    assert!(warning.message.contains("8GiB of memory"), "{warning}");
    assert!(warning.message.contains("4GiB declared here"), "{warning}");
    assert_eq!(warning.line, Some(line_of(&text, "memory = \"4GiB\"")));

    // Steps without numbers of their own contribute nothing.
    let plain = valid(&with_steps(
        "[recipes.r]\ncpu = 2\nmemory = \"3GiB\"\n\n\
         [[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\n\n\
         [[recipes.r.steps]]\nname = \"b\"\nrun = \"y\"\nneeds = [\"a\"]\n",
    ));
    assert_eq!(
        plain.recipes["r"].envelope.peak,
        ResourceVector::new(2000, 3 * GIB, 0)
    );
    assert!(
        plain.recipes["r"]
            .steps
            .iter()
            .all(|s| s.resources.is_none())
    );

    // A declaration inherited from `[defaults]` is a lower bound too, and
    // the warning points at where it was written.
    let inherited = valid(&format!(
        "[project]\nname = \"demo\"\n\n[defaults]\ncpu = 1\nmemory = \"1GiB\"\n\
         tokens = {{ gpu = 1 }}\n{steps}"
    ));
    let peak = &inherited.recipes["r"].envelope.peak;
    assert_eq!(
        (peak.cpu_milli, peak.memory_bytes, peak.token("gpu")),
        (1000, 8 * GIB, 1)
    );
    warning_at(&inherited.warnings, "defaults.memory");
}

#[test]
fn growth_room_comes_from_the_max_keys() {
    let file = valid(&recipe(
        "cpu = 2\nmemory = \"4GiB\"\ndisk = \"1GiB\"\ntokens = { gpu = 1 }\n\
         max_cpu = 8\nmax_memory = \"16GiB\"\nmax_disk = \"1GiB\"\nrun = \"x\"",
    ));
    let envelope = &file.recipes["r"].envelope;
    assert_eq!(
        envelope.peak,
        ResourceVector::new(2000, 4 * GIB, GIB).with_token("gpu", 1)
    );
    assert_eq!(envelope.initial, envelope.peak);
    assert_eq!(
        envelope.maximum,
        ResourceVector::new(8000, 16 * GIB, GIB).with_token("gpu", 1)
    );
    assert!(envelope.is_well_formed());

    // A maximum below the peak cannot be honoured.
    let cases = [
        (
            "cpu = 4\nmax_cpu = 2",
            "recipes.r.max_cpu",
            "`max_cpu` of 2 CPUs is below the 4 CPUs",
        ),
        (
            "memory = \"8GiB\"\nmax_memory = \"4GiB\"",
            "recipes.r.max_memory",
            "`max_memory` of 4GiB is below the 8GiB",
        ),
        (
            "disk = \"2GiB\"\nmax_disk = \"1GiB\"",
            "recipes.r.max_disk",
            "`max_disk` of 1GiB is below the 2GiB",
        ),
    ];
    for (body, path, message) in cases {
        let issue = only_error(&recipe(&format!("{body}\nrun = \"x\"")));
        assert_eq!(issue.path, path, "{issue}");
        assert!(issue.message.contains(message), "{issue}");
    }

    // The peak that counts includes the steps, not just the declaration.
    let issue = only_error(&with_steps(
        "[recipes.r]\nmemory = \"1GiB\"\nmax_memory = \"2GiB\"\n\n\
         [[recipes.r.steps]]\nname = \"a\"\nrun = \"x\"\nmemory = \"3GiB\"\n",
    ));
    assert_eq!(issue.path, "recipes.r.max_memory");
    assert!(issue.message.contains("below the 3GiB"), "{issue}");
}

#[test]
fn zero_tokens_are_the_same_as_no_tokens() {
    let with_zero = valid(&recipe(
        "cpu = 1\nmemory = \"1GiB\"\ntokens = { gpu = 0 }\nrun = \"x\"",
    ));
    let without = valid(&recipe("cpu = 1\nmemory = \"1GiB\"\nrun = \"x\""));
    assert_eq!(
        with_zero.recipes["r"].envelope,
        without.recipes["r"].envelope
    );
    assert!(with_zero.recipes["r"].envelope.peak.tokens.is_empty());
}

#[test]
fn defaults_fill_what_a_recipe_leaves_out() {
    let file = valid(
        r#"
[project]
name = "demo"

[defaults]
hosts = ["linux"]
requires = ["rust>=1.85"]
cpu = 2
memory = "2GiB"
priority = "background"
timeout = "30m"
retry = "isolated"
lan_only = true
cloud = "auto_overflow"
env = { CI = "1", LOG = "info" }
shell = ["bash", "-c"]

[recipes.plain]
run = "make"

[recipes.custom]
hosts = ["macos"]
requires = []
memory = "8GiB"
priority = "interactive"
timeout = "5m"
lan_only = false
env = { LOG = "debug", EXTRA = "x" }
run = "make all"
"#,
    );
    assert!(file.warnings.is_empty(), "{}", render(&file.warnings));

    let plain = &file.recipes["plain"];
    assert_eq!(
        plain.compatibility.build_hosts,
        [PlatformSelector::os(Os::Linux)]
    );
    assert_eq!(plain.compatibility.requires.len(), 1);
    assert_eq!(plain.envelope.peak, ResourceVector::new(2000, 2 * GIB, 0));
    assert_eq!(plain.priority, Priority::Background);
    assert_eq!(plain.timeout_secs, Some(1800));
    assert_eq!(plain.retry_class, RetryClass::IsolatedSideEffects);
    assert!(plain.lan_only);
    assert_eq!(plain.cloud, CloudMode::AutoOverflow);
    assert_eq!(plain.env.len(), 2);
    assert_eq!(
        plain.steps[0].command,
        CommandSpec::Shell {
            script: "make".to_string(),
            shell: Some(vec!["bash".into(), "-c".into()]),
        }
    );

    // A key set in the recipe replaces the default whole; `env` merges.
    let custom = &file.recipes["custom"];
    assert_eq!(
        custom.compatibility.build_hosts,
        [PlatformSelector::os(Os::Macos)]
    );
    assert!(custom.compatibility.requires.is_empty());
    assert_eq!(custom.envelope.peak, ResourceVector::new(2000, 8 * GIB, 0));
    assert_eq!(custom.priority, Priority::Interactive);
    assert_eq!(custom.timeout_secs, Some(300));
    assert!(!custom.lan_only);
    assert_eq!(
        custom.env,
        BTreeMap::from([
            ("CI".to_string(), "1".to_string()),
            ("EXTRA".to_string(), "x".to_string()),
            ("LOG".to_string(), "debug".to_string()),
        ])
    );
}

#[test]
fn platform_and_requirement_forms() {
    let file = valid(&recipe(
        "cpu = 1\nmemory = \"1GiB\"\n\
         hosts = [\"any-aarch64\", { os = \"macos\", os_version = \">= 14, < 16\" }, { arch = \"x64\" }, {}]\n\
         test_runtime = { os = \"linux\", arch = \"arm64\", os_version = \"=22\" }\n\
         requires = [\"cmake\", \"jdk 21\", \"xcode >=16,<17\", { name = \"rust\", version = \"=1.89\", attributes = { targets = \"wasm32-unknown-unknown\" } }]\n\
         isolation = \"vm\"\nrun = \"x\"",
    ));
    let compat = &file.recipes["r"].compatibility;
    assert_eq!(
        compat.build_hosts,
        [
            PlatformSelector {
                os: None,
                arch: Some(Arch::Aarch64),
                os_version: None
            },
            PlatformSelector {
                os: Some(Os::Macos),
                arch: None,
                os_version: Some(">=14,<16".to_string())
            },
            PlatformSelector {
                os: None,
                arch: Some(Arch::X86_64),
                os_version: None
            },
            PlatformSelector::default(),
        ]
    );
    assert_eq!(
        compat.test_runtime,
        Some(PlatformSelector {
            os: Some(Os::Linux),
            arch: Some(Arch::Aarch64),
            os_version: Some("22".to_string())
        })
    );
    let requires: Vec<(&str, Option<&str>)> = compat
        .requires
        .iter()
        .map(|r| (r.name.as_str(), r.version.as_deref()))
        .collect();
    assert_eq!(
        requires,
        [
            ("cmake", None),
            ("jdk", Some("21")),
            ("xcode", Some(">=16,<17")),
            ("rust", Some("1.89")),
        ]
    );
    assert_eq!(
        compat.requires[3].attributes,
        BTreeMap::from([("targets".to_string(), "wasm32-unknown-unknown".to_string())])
    );
    assert_eq!(compat.isolation, IsolationLevel::Vm);
}

#[test]
fn environments_keep_services_alive() {
    let with_env = |environment: &str| {
        with_steps(&format!(
            "[recipes.r]\ncpu = 1\nmemory = \"1GiB\"\n\n\
             [[recipes.r.steps]]\nname = \"web\"\nkind = \"service\"\nrun = \"./web\"\n\
             ready = {{ tcp = 8080 }}\n\n{environment}\n"
        ))
    };

    // An empty table takes the defaults.
    let defaults = valid(&with_env("[recipes.r.environment]"));
    assert!(
        defaults.warnings.is_empty(),
        "{}",
        render(&defaults.warnings)
    );
    assert_eq!(
        defaults.recipes["r"].environment,
        Some(LeaseSpec::default())
    );

    let custom = valid(&with_env(
        "[recipes.r.environment]\nidle_timeout = \"5m\"\nlease = \"1h\"\nmax_lifetime = \"2h\"",
    ));
    assert_eq!(
        custom.recipes["r"].environment,
        Some(LeaseSpec {
            idle_timeout_secs: 300,
            lease_secs: 3600,
            max_lifetime_secs: 7200,
        })
    );

    let too_long = only_error(&with_env(
        "[recipes.r.environment]\nlease = \"3h\"\nmax_lifetime = \"2h\"",
    ));
    assert_eq!(too_long.path, "recipes.r.environment.lease");
    assert!(
        too_long
            .message
            .contains("`lease` of 3h is longer than `max_lifetime` of 2h"),
        "{too_long}"
    );

    let zero = only_error(&with_env("[recipes.r.environment]\nidle_timeout = \"0s\""));
    assert_eq!(zero.path, "recipes.r.environment.idle_timeout");

    // Without a service there is nothing to keep alive.
    let nothing = only_error(&with_steps(
        "[recipes.r]\nrun = \"make\"\n\n[recipes.r.environment]\nlease = \"1h\"\n",
    ));
    assert_eq!(nothing.path, "recipes.r.environment");
    assert!(
        nothing
            .message
            .contains("no step with `kind = \"service\"`"),
        "{nothing}"
    );

    // Services alone, with no environment, stop as soon as they are up.
    let pointless = valid(&with_env(""));
    let warning = warning_at(&pointless.warnings, "recipes.r");
    assert!(
        warning.message.contains("every step is a service"),
        "{warning}"
    );
}

#[test]
fn likely_mistakes_are_warnings_not_errors() {
    // Nothing reserved.
    let text = recipe("run = \"make\"");
    let file = valid(&text);
    let warning = warning_at(&file.warnings, "recipes.r");
    assert!(
        warning.message.contains("no `cpu` or `memory` is declared"),
        "{warning}"
    );
    assert_eq!(warning.line, Some(line_of(&text, "[recipes.r]")));
    assert_eq!(file.recipes["r"].warnings.len(), 1);

    let cpu_only = valid(&recipe("cpu = 2\nrun = \"make\""));
    assert!(
        warning_at(&cpu_only.warnings, "recipes.r")
            .message
            .contains("no `memory` is declared")
    );
    let memory_only = valid(&recipe("memory = \"1GiB\"\nrun = \"make\""));
    assert!(
        warning_at(&memory_only.warnings, "recipes.r")
            .message
            .contains("no `cpu` is declared")
    );

    // A test runtime that no build host can be: v0.1 never splits a job.
    let split = valid(&recipe(
        "cpu = 1\nmemory = \"1GiB\"\nhosts = [\"linux-x86_64\", \"macos\"]\n\
         test_runtime = \"linux-aarch64\"\nrun = \"make\"",
    ));
    let warning = warning_at(&split.warnings, "recipes.r.test_runtime");
    assert!(warning.message.contains("cannot be placed"), "{warning}");

    // Warnings come back with the errors when the file is refused.
    let issues = invalid(&with_steps(
        "[[recipes.r.steps]]\nname = \"web\"\nkind = \"service\"\nrun = \"./web\"\n\n\
         [[recipes.r.steps]]\nname = \"check\"\nrun = \"true\"\nneeds = [\"wbe\"]\n",
    ));
    assert_eq!(errors(&issues).len(), 1, "{}", render(&issues));
    assert_eq!(issues.len(), 2, "{}", render(&issues));
    warning_at(&issues, "recipes.r.steps[0]");
}

#[test]
fn a_recipe_has_a_bounded_number_of_steps() {
    let many = |count: usize| {
        let mut text =
            String::from("[project]\nname = \"demo\"\n\n[recipes.r]\ncpu = 1\nmemory = \"1GiB\"\n");
        for i in 0..count {
            text.push_str(&format!(
                "\n[[recipes.r.steps]]\nname = \"s{i}\"\nrun = \"true\"\n"
            ));
        }
        text
    };
    assert_eq!(valid(&many(MAX_STEPS)).recipes["r"].steps.len(), MAX_STEPS);
    let issue = only_error(&many(MAX_STEPS + 1));
    assert_eq!(issue.path, "recipes.r.steps");
    assert!(
        issue.message.contains("257 steps is more than the 256"),
        "{issue}"
    );
}

#[test]
fn parse_never_panics_on_odd_input() {
    for text in [
        "",
        "\u{feff}",
        "= = =",
        "[",
        "[[recipes]]",
        "recipes = 1",
        "project = []",
        "[project]\nname = 1979-05-27",
        "[recipes]\nr = \"x\"\n[project]\nname=\"a\"",
        "[project]\nname = \"a\"\n[recipes.r]\nsteps = [1, \"x\", {}]",
        "[project]\nname = \"a\"\n[recipes.r]\nsteps = [{ name = \"a\", run = \"x\", ready = 1, ports = 2, needs = [1] }]",
        "[project]\nname = \"a\"\n[recipes.r]\nenvironment = 5\nrun = \"x\"",
        "[project]\nname = \"a\"\n[recipes.\"we ird\"]\nrun = \"x\"",
    ] {
        let _ = parse(text);
    }
    let weird = invalid("[project]\nname = \"a\"\n[recipes.\"we ird\"]\nrun = \"x\"\n");
    assert_eq!(errors(&weird)[0].path, "recipes.\"we ird\"");
}

#[test]
fn the_recipe_file_is_found_above_the_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let nested = project.join("crates").join("app").join("src");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(find_recipe_file(&nested), None);

    let file = project.join(RECIPE_FILE_NAME);
    std::fs::write(&file, "[project]\nname = \"demo\"\n").unwrap();
    let found = find_recipe_file(&nested).unwrap();
    assert_eq!(found.canonicalize().unwrap(), file.canonicalize().unwrap());
    assert_eq!(
        find_recipe_file(&project).unwrap().canonicalize().unwrap(),
        file.canonicalize().unwrap()
    );

    // The nearest file wins, and a directory of that name is not a file.
    let inner = project.join("crates").join(RECIPE_FILE_NAME);
    std::fs::write(&inner, "[project]\nname = \"inner\"\n").unwrap();
    std::fs::create_dir(nested.join(RECIPE_FILE_NAME)).unwrap();
    assert_eq!(
        find_recipe_file(&nested).unwrap().canonicalize().unwrap(),
        inner.canonicalize().unwrap()
    );
    assert_eq!(find_recipe_file(root.path()), None);
}

#[test]
fn load_reads_validates_and_explains_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(RECIPE_FILE_NAME);

    assert!(matches!(load(&path), Err(LoadError::Io { .. })));

    std::fs::write(
        &path,
        "[project]\nname = \"demo\"\n\n[recipes.t]\ncpu = 1\nmemory = \"1GiB\"\nrun = \"make\"\n",
    )
    .unwrap();
    let file = load(&path).unwrap();
    assert_eq!(file.project.name, "demo");
    assert_eq!(file.recipe_names().collect::<Vec<_>>(), ["t"]);

    std::fs::write(&path, "[project]\nname = \"demo\"\nbogus = 1\n").unwrap();
    match load(&path) {
        Err(LoadError::Invalid {
            issues,
            path: reported,
        }) => {
            assert_eq!(reported, path);
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].path, "project.bogus");
        }
        other => panic!("expected an invalid file, got {other:?}"),
    }

    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    assert!(matches!(load(&path), Err(LoadError::Io { .. })));

    let padding = "#".repeat(usize::try_from(bella_recipe::MAX_RECIPE_FILE_BYTES).unwrap() + 1);
    std::fs::write(&path, padding).unwrap();
    assert!(matches!(load(&path), Err(LoadError::TooLarge { .. })));
}
