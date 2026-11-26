//! Resolving recipes, overrides and policy limits into job specifications.

mod common;

use bella_core::api::ProjectPolicy;
use bella_core::defaults;
use bella_core::job::{
    CloudMode, CommandSpec, JobSpec, LeaseSpec, OutputSpec, Priority, RetryClass, StepKind,
};
use bella_core::platform::{
    Arch, CapabilityRequirement, Compatibility, IsolationLevel, Os, PlacementChoice,
    PlatformSelector,
};
use bella_core::resources::ResourceVector;
use bella_recipe::{
    AD_HOC_RECIPE, DEFAULT_MAX_ATTEMPTS, Issue, MAIN_STEP, Overrides, PolicyLimits, Resolved,
    Severity, ad_hoc, resolve,
};
use common::*;
use std::collections::BTreeMap;

const APP: &str = r#"
[project]
name = "demo"

[recipes.test]
hosts = ["linux"]
cpu = 4
memory = "8GiB"
disk = "10GiB"
max_memory = "16GiB"
tokens = { gpu = 1 }
priority = "interactive"
timeout = "2h"
cleanup_timeout = "10m"
max_attempts = 5
retry = "isolated"
cloud = "hybrid"
disconnected = "30m"
parallelism = 8
artifacts = ["dist/**"]
test_reports = ["**/junit.xml"]
env = { A = "1", B = "2" }
run = "make test"
"#;

const PLAIN: &str = r#"
[project]
name = "demo"

[recipes.test]
cpu = 1
memory = "1GiB"
run = "make test"
"#;

fn free() -> PolicyLimits {
    PolicyLimits::unrestricted()
}

fn none() -> Overrides {
    Overrides::default()
}

fn resolved(text: &str, recipe: &str, overrides: &Overrides, limits: &PolicyLimits) -> Resolved {
    resolve(&valid(text), recipe, overrides, limits)
        .unwrap_or_else(|issues| panic!("expected a job, got:\n{}", render(&issues)))
}

fn refused(text: &str, recipe: &str, overrides: &Overrides, limits: &PolicyLimits) -> Vec<Issue> {
    match resolve(&valid(text), recipe, overrides, limits) {
        Ok(_) => panic!("expected the job to be refused"),
        Err(issues) => {
            assert!(issues.iter().any(Issue::is_error));
            issues
        }
    }
}

/// The `test` recipe of `text`, with no overrides and no limits.
fn spec_of(text: &str) -> JobSpec {
    resolved(text, "test", &none(), &free()).spec
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_recipe_resolves_as_written_when_nothing_intervenes() {
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &free());
    assert!(warnings.is_empty(), "{}", render(&warnings));

    assert_eq!(spec.recipe, "test");
    assert_eq!(spec.steps.len(), 1);
    assert_eq!(spec.steps[0].name, MAIN_STEP);
    assert_eq!(spec.steps[0].kind, StepKind::Task);
    assert_eq!(
        spec.steps[0].command,
        CommandSpec::Shell {
            script: "make test".to_string(),
            shell: None
        }
    );
    let peak = ResourceVector::new(4000, 8 * GIB, 10 * GIB).with_token("gpu", 1);
    assert_eq!(spec.envelope.initial, peak);
    assert_eq!(spec.envelope.peak, peak);
    assert_eq!(
        spec.envelope.maximum,
        ResourceVector::new(4000, 16 * GIB, 10 * GIB).with_token("gpu", 1)
    );
    assert!(spec.envelope.is_well_formed());
    assert_eq!(spec.placement, PlacementChoice::Any);
    assert_eq!(spec.priority, Priority::Interactive);
    assert_eq!(spec.retry_class, RetryClass::IsolatedSideEffects);
    assert_eq!(spec.max_attempts, 5);
    assert_eq!(spec.timeout_secs, 7200);
    assert_eq!(spec.cleanup_timeout_secs, 600);
    assert_eq!(spec.cloud, CloudMode::Hybrid);
    assert!(!spec.source_lan_only);
    assert_eq!(
        spec.outputs,
        OutputSpec {
            artifacts: vec!["dist/**".to_string()],
            test_reports: vec!["**/junit.xml".to_string()],
            max_log_bytes: defaults::MAX_LOG_BYTES,
            max_artifact_bytes: defaults::MAX_ARTIFACT_BYTES,
        }
    );
    assert_eq!(spec.environment, None);
    assert_eq!(spec.env, env(&[("A", "1"), ("B", "2")]));
    assert_eq!(spec.disconnected_continuation_secs, 1800);
    assert_eq!(spec.parallelism, Some(8));
}

#[test]
fn a_silent_recipe_gets_the_fleet_defaults() {
    // The cautious default limits do not get in the way of an ordinary job.
    let Resolved { spec, warnings } = resolved(PLAIN, "test", &none(), &PolicyLimits::default());
    assert!(warnings.is_empty(), "{}", render(&warnings));
    assert_eq!(spec.timeout_secs, defaults::JOB_TIMEOUT.as_secs());
    assert_eq!(
        spec.cleanup_timeout_secs,
        defaults::CLEANUP_TIMEOUT.as_secs()
    );
    assert_eq!(spec.max_attempts, DEFAULT_MAX_ATTEMPTS);
    assert_eq!(spec.priority, Priority::Normal);
    assert_eq!(spec.retry_class, RetryClass::Pure);
    assert_eq!(spec.cloud, CloudMode::ExistingOnly);
    assert_eq!(spec.placement, PlacementChoice::Any);
    assert_eq!(spec.disconnected_continuation_secs, 0);
    assert_eq!(spec.parallelism, None);
    assert_eq!(spec.outputs, OutputSpec::default());
    assert_eq!(spec.compatibility, Compatibility::default());
    assert!(!spec.source_lan_only);
    assert!(spec.env.is_empty());
}

#[test]
fn the_resolved_spec_survives_the_wire() {
    for text in [APP, PLAIN] {
        let spec = spec_of(text);
        let json = serde_json::to_string(&spec).unwrap();
        let back: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}

#[test]
fn overrides_replace_what_a_job_may_change() {
    let overrides = Overrides {
        priority: Some(Priority::Background),
        placement: Some(PlacementChoice::Prefer {
            platform: PlatformSelector {
                os: Some(Os::Linux),
                arch: Some(Arch::Aarch64),
                os_version: Some("=22".to_string()),
            },
        }),
        env: env(&[("B", "override"), ("C", "3")]),
        parallelism: Some(2),
        cloud: Some(CloudMode::Manual),
        timeout_secs: Some(90),
        lan_only: true,
    };
    let Resolved { spec, warnings } = resolved(APP, "test", &overrides, &free());
    assert!(warnings.is_empty(), "{}", render(&warnings));
    assert_eq!(spec.priority, Priority::Background);
    assert_eq!(
        spec.placement,
        PlacementChoice::Prefer {
            platform: PlatformSelector {
                os: Some(Os::Linux),
                arch: Some(Arch::Aarch64),
                // Normalised the same way as in the file.
                os_version: Some("22".to_string()),
            },
        }
    );
    assert_eq!(spec.env, env(&[("A", "1"), ("B", "override"), ("C", "3")]));
    assert_eq!(spec.parallelism, Some(2));
    assert_eq!(spec.cloud, CloudMode::Manual);
    assert_eq!(spec.timeout_secs, 90);
    assert!(spec.source_lan_only);

    // Everything else is the recipe's.
    let written = spec_of(APP);
    assert_eq!(spec.steps, written.steps);
    assert_eq!(spec.envelope, written.envelope);
    assert_eq!(spec.compatibility, written.compatibility);
    assert_eq!(spec.max_attempts, written.max_attempts);
    assert_eq!(spec.retry_class, written.retry_class);
    assert_eq!(spec.outputs, written.outputs);

    for placement in [
        PlacementChoice::Any,
        PlacementChoice::Require {
            platform: PlatformSelector::os(Os::Windows),
        },
        PlacementChoice::Machine {
            machine: "studio".to_string(),
        },
        PlacementChoice::Pool {
            pool: "ci".to_string(),
        },
    ] {
        let overrides = Overrides {
            placement: Some(placement.clone()),
            ..none()
        };
        let spec = resolved(APP, "test", &overrides, &free()).spec;
        assert_eq!(spec.placement, placement);
        // A placement choice never rewrites what the recipe declared.
        assert_eq!(spec.compatibility, written.compatibility);
    }
}

#[test]
fn bad_overrides_are_all_reported() {
    let overrides = Overrides {
        env: env(&[("A=B", "x"), ("", "y"), ("OK", "z")]),
        parallelism: Some(0),
        timeout_secs: Some(0),
        placement: Some(PlacementChoice::Machine {
            machine: "  ".to_string(),
        }),
        ..none()
    };
    let issues = refused(APP, "test", &overrides, &free());
    assert_eq!(errors(&issues).len(), 5, "{}", render(&issues));
    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    for path in [
        "overrides.env",
        "overrides.parallelism",
        "overrides.timeout_secs",
        "overrides.placement",
    ] {
        assert!(paths.contains(&path), "{path} missing from {paths:?}");
    }
    assert!(issues.iter().all(|i| i.line.is_none()));

    let overrides = Overrides {
        placement: Some(PlacementChoice::Require {
            platform: PlatformSelector {
                os: Some(Os::Macos),
                arch: None,
                os_version: Some("newest".to_string()),
            },
        }),
        ..none()
    };
    let issues = refused(APP, "test", &overrides, &free());
    assert_eq!(at(&issues, "overrides.placement").severity, Severity::Error);
    assert!(
        at(&issues, "overrides.placement")
            .message
            .contains("`newest`")
    );

    let overrides = Overrides {
        placement: Some(PlacementChoice::Pool {
            pool: String::new(),
        }),
        ..none()
    };
    assert_eq!(errors(&refused(APP, "test", &overrides, &free())).len(), 1);
}

#[test]
fn a_timeout_above_the_limit_is_lowered_with_a_warning() {
    let limits = PolicyLimits {
        max_timeout_secs: Some(3600),
        ..free()
    };
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &limits);
    assert_eq!(spec.timeout_secs, 3600);
    assert_eq!(warnings.len(), 1, "{}", render(&warnings));
    let warning = warning_at(&warnings, "recipes.test.timeout");
    assert_eq!(
        warning.message,
        "a timeout of 2h is above the limit of 1h; the job will use 1h"
    );
    assert_eq!(warning.line, Some(line_of(APP, "timeout = \"2h\"")));
    assert_eq!(warning.column, Some(column_of(APP, "\"2h\"")));

    // At or below the limit nothing is said.
    for max in [7200, 9000] {
        let limits = PolicyLimits {
            max_timeout_secs: Some(max),
            ..free()
        };
        let Resolved { spec, warnings } = resolved(APP, "test", &none(), &limits);
        assert_eq!(spec.timeout_secs, 7200);
        assert!(warnings.is_empty());
    }

    // An override is limited like the recipe, and the warning says whose
    // value it was.
    let overrides = Overrides {
        timeout_secs: Some(86_400),
        ..none()
    };
    let Resolved { spec, warnings } = resolved(APP, "test", &overrides, &limits);
    assert_eq!(spec.timeout_secs, 3600);
    let warning = warning_at(&warnings, "overrides.timeout_secs");
    assert!(warning.message.contains("24h"), "{warning}");
    assert_eq!(warning.line, None);

    // A default that is lowered is not the author's doing: no warning.
    let limits = PolicyLimits {
        max_timeout_secs: Some(600),
        max_cleanup_timeout_secs: Some(30),
        ..free()
    };
    let Resolved { spec, warnings } = resolved(PLAIN, "test", &none(), &limits);
    assert_eq!((spec.timeout_secs, spec.cleanup_timeout_secs), (600, 30));
    assert!(warnings.is_empty(), "{}", render(&warnings));

    // A timeout inherited from `[defaults]` is reported where it was written.
    let inherited = "[project]\nname = \"demo\"\n\n[defaults]\ntimeout = \"3h\"\n\n\
                     [recipes.test]\ncpu = 1\nmemory = \"1GiB\"\nrun = \"make\"\n";
    let limits = PolicyLimits {
        max_timeout_secs: Some(3600),
        ..free()
    };
    let Resolved { spec, warnings } = resolved(inherited, "test", &none(), &limits);
    assert_eq!(spec.timeout_secs, 3600);
    assert_eq!(warning_at(&warnings, "defaults.timeout").line, Some(5));
}

#[test]
fn other_durations_and_attempts_are_lowered_the_same_way() {
    let limits = PolicyLimits {
        max_cleanup_timeout_secs: Some(60),
        max_attempts: Some(2),
        max_disconnected_secs: Some(300),
        ..free()
    };
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &limits);
    assert_eq!(spec.cleanup_timeout_secs, 60);
    assert_eq!(spec.max_attempts, 2);
    assert_eq!(spec.disconnected_continuation_secs, 300);
    assert_eq!(warnings.len(), 3, "{}", render(&warnings));
    assert!(
        warning_at(&warnings, "recipes.test.cleanup_timeout")
            .message
            .contains("10m")
    );
    assert!(
        warning_at(&warnings, "recipes.test.max_attempts")
            .message
            .contains("limit of 2")
    );
    assert!(
        warning_at(&warnings, "recipes.test.disconnected")
            .message
            .contains("limit of 5m")
    );

    // A limit of zero attempts cannot mean "never run".
    let limits = PolicyLimits {
        max_attempts: Some(0),
        ..free()
    };
    assert_eq!(resolved(APP, "test", &none(), &limits).spec.max_attempts, 1);
    assert_eq!(
        resolved(PLAIN, "test", &none(), &limits).spec.max_attempts,
        1
    );
}

#[test]
fn urgent_needs_permission() {
    let urgent = APP.replace("priority = \"interactive\"", "priority = \"urgent\"");
    let denied = PolicyLimits {
        allow_urgent: false,
        ..free()
    };

    let issues = refused(&urgent, "test", &none(), &denied);
    assert_eq!(errors(&issues).len(), 1, "{}", render(&issues));
    let issue = at(&issues, "recipes.test.priority");
    assert!(
        issue.message.contains("`urgent` priority is not allowed"),
        "{issue}"
    );
    assert_eq!(issue.line, Some(line_of(&urgent, "priority = \"urgent\"")));

    let ask = Overrides {
        priority: Some(Priority::Urgent),
        ..none()
    };
    let issues = refused(APP, "test", &ask, &denied);
    assert_eq!(at(&issues, "overrides.priority").severity, Severity::Error);

    // An override can step down from an urgent recipe, and permission
    // allows urgent from either source.
    let calm = Overrides {
        priority: Some(Priority::Normal),
        ..none()
    };
    assert_eq!(
        resolved(&urgent, "test", &calm, &denied).spec.priority,
        Priority::Normal
    );
    assert_eq!(
        resolved(&urgent, "test", &none(), &free()).spec.priority,
        Priority::Urgent
    );
    assert_eq!(
        resolved(APP, "test", &ask, &free()).spec.priority,
        Priority::Urgent
    );
}

#[test]
fn cloud_use_is_lowered_to_the_ceiling() {
    let ceiling = |mode| PolicyLimits {
        cloud_ceiling: mode,
        ..free()
    };

    // The recipe asks for `hybrid`.
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &ceiling(CloudMode::Manual));
    assert_eq!(spec.cloud, CloudMode::Manual);
    let warning = warning_at(&warnings, "recipes.test.cloud");
    assert_eq!(
        warning.message,
        "cloud mode `hybrid` is more than policy allows (`manual`); the job will use `manual`"
    );
    assert_eq!(warning.line, Some(line_of(APP, "cloud = ")));

    let Resolved { spec, warnings } =
        resolved(APP, "test", &none(), &ceiling(CloudMode::ExistingOnly));
    assert_eq!(spec.cloud, CloudMode::ExistingOnly);
    assert_eq!(warnings.len(), 1);

    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &ceiling(CloudMode::Hybrid));
    assert_eq!(spec.cloud, CloudMode::Hybrid);
    assert!(warnings.is_empty());

    // An override cannot get past the ceiling either. Automatic overflow
    // is not something a `manual` ceiling grants.
    let ask = Overrides {
        cloud: Some(CloudMode::AutoOverflow),
        ..none()
    };
    let Resolved { spec, warnings } = resolved(PLAIN, "test", &ask, &ceiling(CloudMode::Manual));
    assert_eq!(spec.cloud, CloudMode::ExistingOnly);
    assert!(
        warning_at(&warnings, "overrides.cloud")
            .message
            .contains("`auto-overflow`")
    );

    // Asking for less than the recipe is always fine.
    let less = Overrides {
        cloud: Some(CloudMode::ExistingOnly),
        ..none()
    };
    let Resolved { spec, warnings } = resolved(APP, "test", &less, &ceiling(CloudMode::Manual));
    assert_eq!(spec.cloud, CloudMode::ExistingOnly);
    assert!(warnings.is_empty());
}

#[test]
fn project_policy_becomes_limits() {
    // A new project's policy is the cautious default.
    let fresh = PolicyLimits::for_project(&ProjectPolicy::default());
    assert_eq!(fresh, PolicyLimits::default());

    let policy = ProjectPolicy {
        cloud: CloudMode::Manual,
        source_lan_only: true,
        allow_urgent: true,
        ..ProjectPolicy::default()
    };
    let limits = PolicyLimits::for_project(&policy);
    let urgent = APP.replace("priority = \"interactive\"", "priority = \"urgent\"");
    let Resolved { spec, warnings } = resolved(&urgent, "test", &none(), &limits);
    assert_eq!(spec.priority, Priority::Urgent);
    assert_eq!(spec.cloud, CloudMode::Manual);
    assert!(spec.source_lan_only);
    assert_eq!(warnings.len(), 1, "{}", render(&warnings));
    warning_at(&warnings, "recipes.test.cloud");
}

#[test]
fn overrides_and_limits_travel_as_json() {
    let overrides: Overrides = serde_json::from_str(
        r#"{ "priority": "interactive", "env": { "CI": "1" }, "timeout_secs": 600,
             "placement": { "type": "prefer", "platform": { "os": "macos" } } }"#,
    )
    .unwrap();
    assert_eq!(
        overrides,
        Overrides {
            priority: Some(Priority::Interactive),
            env: env(&[("CI", "1")]),
            timeout_secs: Some(600),
            placement: Some(PlacementChoice::Prefer {
                platform: PlatformSelector::os(Os::Macos),
            }),
            ..none()
        }
    );
    assert_eq!(serde_json::from_str::<Overrides>("{}").unwrap(), none());
    let json = serde_json::to_string(&overrides).unwrap();
    assert_eq!(serde_json::from_str::<Overrides>(&json).unwrap(), overrides);
    // A misspelt or unsupported override is refused, not dropped.
    assert!(serde_json::from_str::<Overrides>(r#"{ "prority": "urgent" }"#).is_err());
    assert!(serde_json::from_str::<Overrides>(r#"{ "max_attempts": 9 }"#).is_err());

    let limits = PolicyLimits {
        max_timeout_secs: Some(3600),
        max_envelope: Some(ResourceVector::new(8000, 16 * GIB, 0).with_token("gpu", 1)),
        cloud_ceiling: CloudMode::AutoOverflow,
        ..PolicyLimits::default()
    };
    let json = serde_json::to_string(&limits).unwrap();
    assert_eq!(serde_json::from_str::<PolicyLimits>(&json).unwrap(), limits);
    // Absent limits are the cautious defaults.
    assert_eq!(
        serde_json::from_str::<PolicyLimits>("{}").unwrap(),
        PolicyLimits::default()
    );
}

#[test]
fn forced_lan_only_wins() {
    let forced = PolicyLimits {
        forced_lan_only: true,
        ..free()
    };
    let explicit_false = APP.replace("run = ", "lan_only = false\nrun = ");
    let explicit_true = APP.replace("run = ", "lan_only = true\nrun = ");
    let ask = Overrides {
        lan_only: true,
        ..none()
    };

    assert!(
        resolved(&explicit_false, "test", &none(), &forced)
            .spec
            .source_lan_only
    );
    assert!(resolved(APP, "test", &none(), &forced).spec.source_lan_only);
    assert!(
        !resolved(&explicit_false, "test", &none(), &free())
            .spec
            .source_lan_only
    );
    // An override tightens, and cannot loosen what the recipe requires.
    assert!(
        resolved(&explicit_false, "test", &ask, &free())
            .spec
            .source_lan_only
    );
    assert!(
        resolved(&explicit_true, "test", &none(), &free())
            .spec
            .source_lan_only
    );
}

#[test]
fn an_envelope_above_the_limit_is_an_error() {
    let limit = |v: ResourceVector| PolicyLimits {
        max_envelope: Some(v),
        ..free()
    };

    // Exactly the peak fits; growth room is cut back to the limit.
    let exact = ResourceVector::new(4000, 8 * GIB, 10 * GIB).with_token("gpu", 1);
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &limit(exact.clone()));
    assert_eq!(spec.envelope.peak, exact);
    assert_eq!(spec.envelope.maximum, exact);
    assert!(spec.envelope.is_well_formed());
    let warning = warning_at(&warnings, "recipes.test.max_memory");
    assert!(
        warning.message.contains("growth up to 16GiB of memory"),
        "{warning}"
    );
    assert!(
        warning.message.contains("limit of 8GiB of memory"),
        "{warning}"
    );

    // Room for the declared growth: untouched and silent.
    let roomy = ResourceVector::new(64_000, 64 * GIB, 100 * GIB).with_token("gpu", 4);
    let Resolved { spec, warnings } = resolved(APP, "test", &none(), &limit(roomy));
    assert_eq!(spec.envelope.maximum.memory_bytes, 16 * GIB);
    assert!(warnings.is_empty(), "{}", render(&warnings));

    // Too small in memory only.
    let small = ResourceVector::new(4000, 4 * GIB, 10 * GIB).with_token("gpu", 1);
    let issues = refused(APP, "test", &none(), &limit(small));
    assert_eq!(errors(&issues).len(), 1, "{}", render(&issues));
    let issue = at(&issues, "recipes.test.memory");
    assert_eq!(
        issue.message,
        "the job needs 8GiB of memory at its peak, above the limit of 4GiB of memory"
    );
    assert_eq!(issue.line, Some(line_of(APP, "memory = \"8GiB\"")));

    // Every dimension that is over is reported, not just the first.
    let tiny = ResourceVector::new(1000, GIB, GIB);
    let issues = refused(APP, "test", &none(), &limit(tiny));
    assert_eq!(errors(&issues).len(), 4, "{}", render(&issues));
    assert!(at(&issues, "recipes.test.cpu").message.contains("4 CPUs"));
    assert!(
        at(&issues, "recipes.test.disk")
            .message
            .contains("10GiB of disk")
    );
    // A token the limit does not list is not available at all.
    assert!(
        at(&issues, "recipes.test.tokens")
            .message
            .contains("needs 1 `gpu`, which policy does not make available")
    );

    let few = ResourceVector::new(8000, 32 * GIB, 100 * GIB).with_token("gpu", 1);
    let greedy = APP.replace("tokens = { gpu = 1 }", "tokens = { gpu = 2 }");
    let issues = refused(&greedy, "test", &none(), &limit(few));
    assert!(
        at(&issues, "recipes.test.tokens")
            .message
            .contains("needs 2 `gpu`, above the limit of 1")
    );
}

#[test]
fn an_environment_cannot_outlive_the_limit() {
    let text = r#"
[project]
name = "demo"

[recipes.test]
cpu = 1
memory = "1GiB"

[[recipes.test.steps]]
name = "web"
kind = "service"
run = "./web"
ready = { http = 8080, path = "/health" }

[recipes.test.environment]
idle_timeout = "20m"
lease = "45m"
max_lifetime = "3h"
"#;
    let unlimited = resolved(text, "test", &none(), &free()).spec;
    assert_eq!(
        unlimited.environment,
        Some(LeaseSpec {
            idle_timeout_secs: 1200,
            lease_secs: 2700,
            max_lifetime_secs: 10_800,
        })
    );

    let limits = PolicyLimits {
        max_environment_lifetime_secs: Some(1800),
        ..free()
    };
    let Resolved { spec, warnings } = resolved(text, "test", &none(), &limits);
    assert_eq!(
        spec.environment,
        Some(LeaseSpec {
            idle_timeout_secs: 1200,
            lease_secs: 1800,
            max_lifetime_secs: 1800,
        })
    );
    let warning = warning_at(&warnings, "recipes.test.environment.max_lifetime");
    assert!(
        warning.message.contains("3h") && warning.message.contains("30m"),
        "{warning}"
    );
    assert_eq!(warning.line, Some(line_of(text, "max_lifetime")));
}

/// Whatever a recipe and its overrides ask for, the result stays inside
/// every limit: values are lowered or the job is refused, never widened.
#[test]
fn a_recipe_can_never_widen_a_limit() {
    let greedy = APP
        .replace("priority = \"interactive\"", "priority = \"urgent\"")
        .replace("run = ", "lan_only = false\nrun = ");
    let recipes = [APP.to_string(), PLAIN.to_string(), greedy];
    let overrides = [
        none(),
        Overrides {
            priority: Some(Priority::Urgent),
            cloud: Some(CloudMode::Hybrid),
            timeout_secs: Some(u64::MAX),
            parallelism: Some(u32::MAX),
            ..none()
        },
        Overrides {
            priority: Some(Priority::Maintenance),
            cloud: Some(CloudMode::AutoOverflow),
            timeout_secs: Some(1),
            lan_only: true,
            ..none()
        },
    ];
    let all_limits = [
        PolicyLimits::default(),
        free(),
        PolicyLimits {
            max_timeout_secs: Some(60),
            max_cleanup_timeout_secs: Some(10),
            max_attempts: Some(1),
            allow_urgent: false,
            max_envelope: Some(ResourceVector::new(4000, 8 * GIB, 10 * GIB).with_token("gpu", 1)),
            forced_lan_only: true,
            cloud_ceiling: CloudMode::Manual,
            max_disconnected_secs: Some(0),
            max_environment_lifetime_secs: Some(60),
        },
        PolicyLimits {
            max_envelope: Some(ResourceVector::new(500, GIB / 2, 0)),
            cloud_ceiling: CloudMode::AutoOverflow,
            ..free()
        },
    ];

    let mut accepted = 0;
    let mut refused = 0;
    for text in &recipes {
        let file = valid(text);
        for overrides in &overrides {
            for limits in &all_limits {
                match resolve(&file, "test", overrides, limits) {
                    Err(issues) => {
                        assert!(issues.iter().any(Issue::is_error));
                        refused += 1;
                    }
                    Ok(Resolved { spec, .. }) => {
                        accepted += 1;
                        if let Some(max) = limits.max_timeout_secs {
                            assert!(spec.timeout_secs <= max);
                        }
                        if let Some(max) = limits.max_cleanup_timeout_secs {
                            assert!(spec.cleanup_timeout_secs <= max);
                        }
                        if let Some(max) = limits.max_attempts {
                            assert!(spec.max_attempts <= max.max(1));
                        }
                        if let Some(max) = limits.max_disconnected_secs {
                            assert!(spec.disconnected_continuation_secs <= max);
                        }
                        assert!(limits.allow_urgent || spec.priority != Priority::Urgent);
                        assert!(!limits.forced_lan_only || spec.source_lan_only);
                        let allowed: &[CloudMode] = match limits.cloud_ceiling {
                            CloudMode::ExistingOnly => &[CloudMode::ExistingOnly],
                            CloudMode::Manual => &[CloudMode::ExistingOnly, CloudMode::Manual],
                            CloudMode::AutoOverflow => {
                                &[CloudMode::ExistingOnly, CloudMode::AutoOverflow]
                            }
                            CloudMode::Hybrid => &[
                                CloudMode::ExistingOnly,
                                CloudMode::Manual,
                                CloudMode::AutoOverflow,
                                CloudMode::Hybrid,
                            ],
                        };
                        assert!(allowed.contains(&spec.cloud), "{:?}", spec.cloud);
                        assert!(spec.envelope.is_well_formed());
                        if let Some(max) = &limits.max_envelope {
                            assert!(spec.envelope.peak.fits_within(max));
                            assert!(spec.envelope.maximum.fits_within(max));
                        }
                    }
                }
            }
        }
    }
    // Both outcomes must actually occur for the checks above to mean much.
    assert!(
        accepted >= 10 && refused >= 10,
        "{accepted} accepted, {refused} refused"
    );
}

#[test]
fn the_digest_ignores_how_the_file_is_written() {
    let one = r#"
[project]
name = "demo"

[recipes.test]
hosts = ["linux", "macos-aarch64"]
requires = ["rust>=1.85"]
cpu = 4
memory = "8GiB"
timeout = "20m"
env = { A = "1", B = "2" }
artifacts = ["dist/**"]

[[recipes.test.steps]]
name = "build"
run = "make"
cpu = 2

[[recipes.test.steps]]
name = "check"
kind = "test"
needs = ["build"]
run = ["make", "check"]
env = { X = "1", Y = "2" }
"#;
    // The same recipe: keys and tables reordered, other spellings of the
    // same values, comments, and an unrelated recipe added.
    let two = r#"
# Reordered.
[recipes.other]
run = "true"
cpu = 1
memory = "1GiB"

[recipes.test]
artifacts = [
    "dist/**",
]
timeout = "0h20m"
memory = "8192MiB"
cpu = 4.0
requires = [{ name = "rust", version = ">= 1.85" }]
hosts = [{ os = "linux" }, { arch = "arm64", os = "darwin" }]

[recipes.test.env]
B = "2"
A = "1"

[[recipes.test.steps]]
run = "make"
cpu = 2
name = "build"

[[recipes.test.steps]]
env.Y = "2"
env.X = "1"
run = ["make", "check"]
needs = ["build"]
kind = "test"
name = "check"

[project]
name = "renamed-project"
"#;
    let (a, b) = (spec_of(one), spec_of(two));
    assert_eq!(a.recipe_digest, b.recipe_digest);
    assert_eq!(a.steps, b.steps);

    // Resolving twice gives the same digest.
    assert_eq!(spec_of(one).recipe_digest, a.recipe_digest);
    // The recipe's name is a label, not content.
    let renamed = one.replace("recipes.test", "recipes.verify");
    let renamed = resolved(&renamed, "verify", &none(), &free()).spec;
    assert_eq!(renamed.recipe_digest, a.recipe_digest);
    assert_ne!(renamed.recipe, a.recipe);
}

#[test]
fn the_digest_changes_with_anything_that_affects_the_result() {
    let base = r#"
[project]
name = "demo"

[recipes.test]
hosts = ["linux"]
target = "x86_64-unknown-linux-gnu"
requires = ["rust>=1.85"]
cpu = 4
memory = "8GiB"
timeout = "20m"
cleanup_timeout = "2m"
env = { A = "1" }
artifacts = ["dist/**"]
test_reports = ["**/junit.xml"]

[[recipes.test.steps]]
name = "db"
kind = "service"
run = "./db"
ports = [5432]
ready = { tcp = 5432 }

[[recipes.test.steps]]
name = "check"
kind = "test"
needs = ["db"]
run = "make check"
"#;
    let digest = spec_of(base).recipe_digest;
    let changes = [
        ("run = \"make check\"", "run = \"make check -k\""),
        ("run = \"./db\"", "run = [\"./db\"]"),
        ("env = { A = \"1\" }", "env = { A = \"2\" }"),
        ("hosts = [\"linux\"]", "hosts = [\"linux\", \"macos\"]"),
        (
            "target = \"x86_64-unknown-linux-gnu\"",
            "target = \"aarch64-unknown-linux-gnu\"",
        ),
        ("requires = [\"rust>=1.85\"]", "requires = [\"rust>=1.89\"]"),
        ("cpu = 4", "cpu = 8"),
        ("memory = \"8GiB\"", "memory = \"8GB\""),
        ("timeout = \"20m\"", "timeout = \"21m\""),
        ("cleanup_timeout = \"2m\"", "cleanup_timeout = \"3m\""),
        ("artifacts = [\"dist/**\"]", "artifacts = [\"dist/*\"]"),
        ("test_reports = [\"**/junit.xml\"]", "test_reports = []"),
        (
            "ready = { tcp = 5432 }",
            "ready = { tcp = 5432, timeout = \"5s\" }",
        ),
        ("ports = [5432]", "ports = [5432, 5433]"),
        ("kind = \"test\"", "kind = \"task\""),
        ("needs = [\"db\"]", "cwd = \"sub\"\nneeds = [\"db\"]"),
        ("name = \"check\"", "name = \"verify\""),
        ("cpu = 4", "cpu = 4\nparallelism = 4"),
        ("cpu = 4", "cpu = 4\nisolation = \"container\""),
        ("cpu = 4", "cpu = 4\ntest_runtime = \"linux\""),
    ];
    for (from, to) in changes {
        assert!(base.contains(from), "{from}");
        let changed = spec_of(&base.replacen(from, to, 1)).recipe_digest;
        assert_ne!(changed, digest, "`{to}` did not change the digest");
    }

    // Overrides the steps can observe change it too.
    let file = valid(base);
    for overrides in [
        Overrides {
            env: env(&[("EXTRA", "1")]),
            ..none()
        },
        Overrides {
            parallelism: Some(3),
            ..none()
        },
        Overrides {
            timeout_secs: Some(30),
            ..none()
        },
    ] {
        let changed = resolve(&file, "test", &overrides, &free())
            .unwrap()
            .spec
            .recipe_digest;
        assert_ne!(changed, digest, "{overrides:?}");
    }
    // A limit that lowers the timeout changes what runs, and so the digest.
    let limits = PolicyLimits {
        max_timeout_secs: Some(60),
        ..free()
    };
    assert_ne!(
        resolve(&file, "test", &none(), &limits)
            .unwrap()
            .spec
            .recipe_digest,
        digest
    );
}

#[test]
fn the_digest_excludes_priority_placement_and_cloud_mode() {
    let digest = spec_of(APP).recipe_digest;

    // Set in the recipe.
    for (from, to) in [
        ("priority = \"interactive\"", "priority = \"background\""),
        ("cloud = \"hybrid\"", "cloud = \"manual\""),
        // The other scheduling terms are left out as well.
        ("retry = \"isolated\"", "retry = \"external\""),
        ("max_attempts = 5", "max_attempts = 1"),
        ("disconnected = \"30m\"", "disconnected = \"1m\""),
        ("run = ", "lan_only = true\nrun = "),
        ("run = ", "description = \"words\"\nrun = "),
    ] {
        assert!(APP.contains(from), "{from}");
        let same = spec_of(&APP.replacen(from, to, 1)).recipe_digest;
        assert_eq!(same, digest, "`{to}` changed the digest");
    }

    // Set by an override.
    let file = valid(APP);
    for overrides in [
        Overrides {
            priority: Some(Priority::Urgent),
            ..none()
        },
        Overrides {
            placement: Some(PlacementChoice::Machine {
                machine: "studio".to_string(),
            }),
            ..none()
        },
        Overrides {
            placement: Some(PlacementChoice::Require {
                platform: PlatformSelector::os(Os::Linux),
            }),
            ..none()
        },
        Overrides {
            cloud: Some(CloudMode::ExistingOnly),
            ..none()
        },
        Overrides {
            lan_only: true,
            ..none()
        },
    ] {
        let spec = resolve(&file, "test", &overrides, &free()).unwrap().spec;
        assert_eq!(spec.recipe_digest, digest, "{overrides:?}");
    }

    // Lowered by policy.
    let limits = PolicyLimits {
        cloud_ceiling: CloudMode::ExistingOnly,
        forced_lan_only: true,
        max_attempts: Some(1),
        ..free()
    };
    assert_eq!(
        resolve(&file, "test", &none(), &limits)
            .unwrap()
            .spec
            .recipe_digest,
        digest
    );
}

#[test]
fn an_unknown_recipe_is_named_with_the_ones_that_exist() {
    let file = valid(FULLISH);
    let issues = resolve(&file, "tset", &none(), &free()).unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "recipes");
    assert_eq!(
        issues[0].message,
        "no recipe named `tset`; did you mean `test`? (this file has `build` or `test`)"
    );

    let unrelated = resolve(&file, "deploy", &none(), &free()).unwrap_err();
    assert_eq!(
        unrelated[0].message,
        "no recipe named `deploy` (this file has `build` or `test`)"
    );

    let empty = valid("[project]\nname = \"demo\"\n");
    let issues = resolve(&empty, "test", &none(), &free()).unwrap_err();
    assert!(issues[0].message.contains("this file defines no recipes"));
}

const FULLISH: &str = r#"
[project]
name = "demo"

[recipes.build]
cpu = 1
memory = "1GiB"
run = "make"

[recipes.test]
run = "make test"
"#;

#[test]
fn recipe_warnings_travel_with_the_job() {
    // `test` declares no resources: a warning from parsing.
    let file = valid(FULLISH);
    let Resolved { warnings, .. } = resolve(&file, "test", &none(), &free()).unwrap();
    assert_eq!(warnings.len(), 1, "{}", render(&warnings));
    assert!(
        warning_at(&warnings, "recipes.test")
            .message
            .contains("no `cpu` or `memory`")
    );
    // Only this recipe's.
    assert!(
        resolve(&file, "build", &none(), &free())
            .unwrap()
            .warnings
            .is_empty()
    );

    // They are still there when the job is refused.
    let ask = Overrides {
        priority: Some(Priority::Urgent),
        ..none()
    };
    let issues = resolve(&file, "test", &ask, &PolicyLimits::default()).unwrap_err();
    assert_eq!(issues.len(), 2, "{}", render(&issues));
    assert_eq!(errors(&issues).len(), 1);
}

/// A19: the reservation of a job whose app stays up during its tests is the
/// sum of both, so starting the app can never consume what the tests need.
#[test]
fn a19_the_job_reserves_the_app_and_its_tests_together() {
    let text = r#"
[project]
name = "demo"

[[recipes.test.steps]]
name = "app"
kind = "service"
run = "./app --port 8080"
memory = "2GiB"
cpu = 1
ports = [8080]
ready = { http = 8080, path = "/health" }

[[recipes.test.steps]]
name = "tests"
kind = "test"
run = "make e2e"
needs = ["app"]
memory = "6GiB"
cpu = 2
"#;
    let Resolved { spec, warnings } = resolved(text, "test", &none(), &free());
    assert!(warnings.is_empty(), "{}", render(&warnings));
    assert_eq!(spec.envelope.peak.memory_bytes, 8 * GIB);
    assert_eq!(spec.envelope.peak.cpu_milli, 3000);
    assert_eq!(spec.envelope.initial, spec.envelope.peak);
    assert!(spec.envelope.is_well_formed());
    // The per-step demands are kept for the worker.
    assert_eq!(
        spec.steps[0].resources,
        Some(ResourceVector::new(1000, 2 * GIB, 0))
    );
    assert_eq!(
        spec.steps[1].resources,
        Some(ResourceVector::new(2000, 6 * GIB, 0))
    );

    // A machine with room for either alone but not both must refuse it.
    let limits = PolicyLimits {
        max_envelope: Some(ResourceVector::new(8000, 6 * GIB, 0)),
        ..free()
    };
    let issues = resolve(&valid(text), "test", &none(), &limits).unwrap_err();
    assert!(
        errors(&issues)[0].message.contains("8GiB of memory"),
        "{}",
        render(&issues)
    );
}

/// A38/A39: build host, artifact target, test runtime and extra
/// requirements are four separate declarations and reach the job as such.
#[test]
fn a38_a39_the_platform_fields_arrive_intact_and_separate() {
    let text = r#"
[project]
name = "demo"

[recipes.test]
cpu = 2
memory = "4GiB"
hosts = ["linux", "macos-aarch64", { os = "windows", arch = "x86_64", os_version = ">=10" }]
target = "x86_64-unknown-linux-gnu"
test_runtime = "linux"
requires = ["rust>=1.85", "node=22", { name = "xcode", version = ">=16", attributes = { sdk = "iphoneos18.0" } }]
isolation = "container"
run = "cargo test"
"#;
    let Resolved { spec, warnings } = resolved(text, "test", &none(), &free());
    assert!(warnings.is_empty(), "{}", render(&warnings));
    assert_eq!(
        spec.compatibility,
        Compatibility {
            build_hosts: vec![
                PlatformSelector::os(Os::Linux),
                PlatformSelector {
                    os: Some(Os::Macos),
                    arch: Some(Arch::Aarch64),
                    os_version: None,
                },
                PlatformSelector {
                    os: Some(Os::Windows),
                    arch: Some(Arch::X86_64),
                    os_version: Some(">=10".to_string()),
                },
            ],
            artifact_target: Some("x86_64-unknown-linux-gnu".to_string()),
            test_runtime: Some(PlatformSelector::os(Os::Linux)),
            requires: vec![
                CapabilityRequirement {
                    name: "rust".to_string(),
                    version: Some(">=1.85".to_string()),
                    attributes: BTreeMap::new(),
                },
                CapabilityRequirement {
                    name: "node".to_string(),
                    version: Some("22".to_string()),
                    attributes: BTreeMap::new(),
                },
                CapabilityRequirement {
                    name: "xcode".to_string(),
                    version: Some(">=16".to_string()),
                    attributes: BTreeMap::from([("sdk".to_string(), "iphoneos18.0".to_string())]),
                },
            ],
            isolation: IsolationLevel::Container,
        }
    );

    // Overrides and limits do not touch them.
    let overrides = Overrides {
        placement: Some(PlacementChoice::Require {
            platform: PlatformSelector::os(Os::Macos),
        }),
        priority: Some(Priority::Background),
        ..none()
    };
    let limited = resolved(text, "test", &overrides, &PolicyLimits::default()).spec;
    assert_eq!(limited.compatibility, spec.compatibility);
}

/// A38/A39: a recipe that declares no build hosts makes no portability
/// claim. Its job carries an empty `build_hosts`, which pins it to the
/// platform of the machine that submits it.
#[test]
fn a38_a39_a_recipe_without_hosts_is_pinned_to_the_origin() {
    let text = r#"
[project]
name = "demo"

[recipes.test]
cpu = 2
memory = "4GiB"
target = "aarch64-apple-ios"
requires = ["xcode>=16"]
run = "xcodebuild test"
"#;
    let spec = resolved(text, "test", &none(), &free()).spec;
    assert!(spec.compatibility.build_hosts.is_empty());
    // The other three fields are unaffected by the missing one.
    assert_eq!(
        spec.compatibility.artifact_target.as_deref(),
        Some("aarch64-apple-ios")
    );
    assert_eq!(spec.compatibility.test_runtime, None);
    assert_eq!(spec.compatibility.requires.len(), 1);
    assert_eq!(spec.compatibility.isolation, IsolationLevel::Native);

    // Not even a placement override invents build hosts.
    let overrides = Overrides {
        placement: Some(PlacementChoice::Prefer {
            platform: PlatformSelector::os(Os::Linux),
        }),
        ..none()
    };
    let spec = resolved(text, "test", &overrides, &free()).spec;
    assert!(spec.compatibility.build_hosts.is_empty());

    // `[defaults]` can supply the hosts for every recipe instead.
    let with_defaults = text.replace(
        "[recipes.test]",
        "[defaults]\nhosts = [\"macos\"]\n\n[recipes.test]",
    );
    let spec = resolved(&with_defaults, "test", &none(), &free()).spec;
    assert_eq!(
        spec.compatibility.build_hosts,
        [PlatformSelector::os(Os::Macos)]
    );
}

/// A38/A39: cross-compiling does not prove the output runs on its target,
/// so where the tests run is its own field, distinct from both the build
/// hosts and the artifact target.
#[test]
fn a38_a39_cross_compilation_keeps_a_distinct_test_runtime() {
    let text = r#"
[project]
name = "demo"

[recipes.test]
cpu = 4
memory = "8GiB"
hosts = ["linux-x86_64", "macos-aarch64"]
target = "aarch64-linux-android"
test_runtime = { os = "linux", arch = "x86_64", os_version = ">=22.04" }
requires = ["android-sdk", "rust>=1.85"]
run = "cargo ndk test"
"#;
    let Resolved { spec, warnings } = resolved(text, "test", &none(), &free());
    assert!(warnings.is_empty(), "{}", render(&warnings));
    let compat = &spec.compatibility;
    assert_eq!(
        compat.build_hosts,
        [
            PlatformSelector {
                os: Some(Os::Linux),
                arch: Some(Arch::X86_64),
                os_version: None
            },
            PlatformSelector {
                os: Some(Os::Macos),
                arch: Some(Arch::Aarch64),
                os_version: None
            },
        ]
    );
    assert_eq!(
        compat.artifact_target.as_deref(),
        Some("aarch64-linux-android")
    );
    let runtime = PlatformSelector {
        os: Some(Os::Linux),
        arch: Some(Arch::X86_64),
        os_version: Some(">=22.04".to_string()),
    };
    assert_eq!(compat.test_runtime, Some(runtime.clone()));
    // The runtime is not folded into the hosts, nor the hosts narrowed to it.
    assert!(!compat.build_hosts.contains(&runtime));
    assert_eq!(compat.build_hosts.len(), 2);
    let names: Vec<&str> = compat.requires.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["android-sdk", "rust"]);

    // Built on macOS for Windows and tested on Windows: no build host can
    // be the test runtime. The fields still arrive as written; the recipe
    // gets a warning because v0.1 runs a whole job on one machine.
    let split = r#"
[project]
name = "demo"

[recipes.test]
cpu = 4
memory = "8GiB"
hosts = ["macos"]
target = "x86_64-pc-windows-msvc"
test_runtime = "windows"
run = "make cross"
"#;
    let Resolved { spec, warnings } = resolved(split, "test", &none(), &free());
    assert_eq!(
        spec.compatibility.build_hosts,
        [PlatformSelector::os(Os::Macos)]
    );
    assert_eq!(
        spec.compatibility.artifact_target.as_deref(),
        Some("x86_64-pc-windows-msvc")
    );
    assert_eq!(
        spec.compatibility.test_runtime,
        Some(PlatformSelector::os(Os::Windows))
    );
    assert_eq!(warnings.len(), 1, "{}", render(&warnings));
    warning_at(&warnings, "recipes.test.test_runtime");
}

#[test]
fn an_ad_hoc_command_is_one_step_pinned_to_the_origin() {
    let command = CommandSpec::Argv {
        argv: vec!["cargo".to_string(), "test".to_string()],
    };
    let resources = ResourceVector::new(2000, 4 * GIB, GIB)
        .with_token("gpu", 1)
        .with_token("unused", 0);
    let Resolved { spec, warnings } = ad_hoc(
        command.clone(),
        resources,
        &none(),
        &PolicyLimits::default(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{}", render(&warnings));

    assert_eq!(spec.recipe, AD_HOC_RECIPE);
    assert_eq!(spec.steps.len(), 1);
    assert_eq!(spec.steps[0].name, MAIN_STEP);
    assert_eq!(spec.steps[0].kind, StepKind::Task);
    assert_eq!(spec.steps[0].command, command);
    // A38/A39: no build hosts, so the job is pinned to the origin platform.
    assert_eq!(spec.compatibility, Compatibility::default());
    assert!(spec.compatibility.build_hosts.is_empty());
    let reserved = ResourceVector::new(2000, 4 * GIB, GIB).with_token("gpu", 1);
    assert_eq!(spec.envelope.initial, reserved);
    assert_eq!(spec.envelope.peak, reserved);
    assert_eq!(spec.envelope.maximum, reserved);
    assert_eq!(spec.timeout_secs, defaults::JOB_TIMEOUT.as_secs());
    assert_eq!(spec.max_attempts, DEFAULT_MAX_ATTEMPTS);
    assert_eq!(spec.priority, Priority::Normal);
    assert_eq!(spec.cloud, CloudMode::ExistingOnly);
    assert_eq!(spec.environment, None);
    assert_eq!(spec.outputs, OutputSpec::default());

    // The same command gives the same digest; another command another.
    let again = ad_hoc(command, reserved.clone(), &none(), &free())
        .unwrap()
        .spec;
    assert_eq!(again.recipe_digest, spec.recipe_digest);
    let other = CommandSpec::Shell {
        script: "cargo test".to_string(),
        shell: None,
    };
    let other = ad_hoc(other, reserved, &none(), &free()).unwrap().spec;
    assert_ne!(other.recipe_digest, spec.recipe_digest);
}

#[test]
fn an_ad_hoc_command_obeys_overrides_and_limits() {
    let command = || CommandSpec::Shell {
        script: "make".to_string(),
        shell: None,
    };
    let resources = || ResourceVector::new(4000, 8 * GIB, 0);

    let overrides = Overrides {
        priority: Some(Priority::Interactive),
        env: env(&[("CI", "1")]),
        timeout_secs: Some(7200),
        cloud: Some(CloudMode::Hybrid),
        placement: Some(PlacementChoice::Pool {
            pool: "mac".to_string(),
        }),
        parallelism: Some(4),
        lan_only: false,
    };
    let limits = PolicyLimits {
        max_timeout_secs: Some(600),
        forced_lan_only: true,
        cloud_ceiling: CloudMode::Manual,
        ..PolicyLimits::default()
    };
    let Resolved { spec, warnings } = ad_hoc(command(), resources(), &overrides, &limits).unwrap();
    assert_eq!(spec.priority, Priority::Interactive);
    assert_eq!(spec.env, env(&[("CI", "1")]));
    assert_eq!(spec.timeout_secs, 600);
    assert_eq!(spec.cloud, CloudMode::Manual);
    assert!(spec.source_lan_only);
    assert_eq!(spec.parallelism, Some(4));
    assert_eq!(
        spec.placement,
        PlacementChoice::Pool {
            pool: "mac".to_string()
        }
    );
    assert_eq!(warnings.len(), 2, "{}", render(&warnings));
    warning_at(&warnings, "overrides.timeout_secs");
    warning_at(&warnings, "overrides.cloud");

    // Urgent without permission, and more than the limit allows.
    let overrides = Overrides {
        priority: Some(Priority::Urgent),
        ..none()
    };
    let limits = PolicyLimits {
        max_envelope: Some(ResourceVector::new(2000, 16 * GIB, 0)),
        ..PolicyLimits::default()
    };
    let issues = ad_hoc(command(), resources(), &overrides, &limits).unwrap_err();
    assert_eq!(errors(&issues).len(), 2, "{}", render(&issues));
    assert!(at(&issues, "resources").message.contains("4 CPUs"));
    at(&issues, "overrides.priority");

    // Empty commands are refused before anything else.
    for empty in [
        CommandSpec::Argv { argv: Vec::new() },
        CommandSpec::Argv {
            argv: vec![String::new(), "x".to_string()],
        },
        CommandSpec::Shell {
            script: "  \n".to_string(),
            shell: None,
        },
        CommandSpec::Shell {
            script: "make".to_string(),
            shell: Some(Vec::new()),
        },
    ] {
        let issues = ad_hoc(empty, resources(), &none(), &free()).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "command");
    }
}
