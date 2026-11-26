//! Secrets, ignored inputs, excludes and limits.

use super::*;

fn with(change: impl FnOnce(&mut CapturePolicy)) -> CapturePolicy {
    let mut policy = CapturePolicy::default();
    change(&mut policy);
    policy
}

/// a33: files that look like credentials and that git does not track are
/// left out and listed, so the user learns why a build may miss them.
#[test]
fn a33_untracked_secret_is_excluded_and_reported() {
    let repo = Repo::new();
    repo.write("src/main.rs", b"fn main() {}");
    repo.commit_all("init");
    repo.write(".env", b"TOKEN=1");
    repo.write("config/.env.local", b"TOKEN=2");
    repo.write("keys/server.pem", b"-----BEGIN PRIVATE KEY-----");
    repo.write("deploy/id_rsa", b"key");
    repo.write("deploy/ID_ED25519", b"upper case still matches");
    repo.write("deploy/id_rsa.pub", b"public half");
    repo.write(".env.example", b"TOKEN=");
    repo.write("ok.txt", b"fine");
    repo.write("environment.md", b"not a secret name");

    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        result.excluded_secrets,
        [
            ".env",
            "config/.env.local",
            "deploy/ID_ED25519",
            "deploy/id_rsa",
            "keys/server.pem",
        ]
    );
    // Templates and public keys match a secret pattern but are allowed by
    // default.
    assert_eq!(
        paths(&result),
        [
            ".env.example",
            "deploy/id_rsa.pub",
            "environment.md",
            "ok.txt",
            "src/main.rs"
        ]
    );
    // No secret byte reached the store.
    assert_eq!(store.len(), 5);

    // Explicitly allowed paths are captured and no longer reported.
    let policy = with(|p| p.allow_secrets = vec![".env".into(), "keys".into()]);
    let allowed = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(content(&store, &allowed, ".env"), b"TOKEN=1");
    assert_eq!(
        content(&store, &allowed, "keys/server.pem"),
        b"-----BEGIN PRIVATE KEY-----"
    );
    assert_eq!(
        allowed.excluded_secrets,
        ["config/.env.local", "deploy/ID_ED25519", "deploy/id_rsa"]
    );
}

/// a33: a tracked file that matches a secret pattern is a decision the
/// capture will not make silently in either direction: it fails, naming
/// the file and the pattern, until the policy allows the path.
#[test]
fn a33_tracked_secret_fails_unless_allowed() {
    let repo = Repo::new();
    repo.write("app.txt", b"app");
    repo.write("config/prod.key", b"tracked key material");
    repo.commit_all("init");

    let store = MemStore::new();
    let err = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap_err();
    match &err {
        CaptureError::TrackedSecret { path, pattern } => {
            assert_eq!(path, "config/prod.key");
            assert_eq!(pattern, "*.key");
        }
        other => panic!("unexpected error: {other}"),
    }
    let message = err.to_string();
    assert!(message.contains("config/prod.key") && message.contains("allow_secrets"));

    let policy = with(|p| p.allow_secrets = vec!["config/prod.key".into()]);
    let result = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(
        content(&store, &result, "config/prod.key"),
        b"tracked key material"
    );
    assert!(result.excluded_secrets.is_empty());

    // Allowing a different path does not help.
    let policy = with(|p| p.allow_secrets = vec!["other/prod.key".into()]);
    assert!(matches!(
        snap(&repo.dir, &policy, &store),
        Err(CaptureError::TrackedSecret { .. })
    ));

    // A tracked secret that is gone from the working tree has nothing to
    // leak and nothing to capture.
    fs::remove_file(repo.path("config/prod.key")).unwrap();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(paths(&result), ["app.txt"]);
}

/// a33: a pattern without `/` matches the file name anywhere; a pattern
/// with `/` matches the path relative to the root.
#[test]
fn a33_patterns_match_the_file_name_and_the_relative_path() {
    let repo = Repo::new();
    repo.write("readme.txt", b"r");
    repo.commit_all("init");
    repo.write("deploy/creds.json", b"path match");
    repo.write("other/deploy/creds.json", b"same name, different path");
    repo.write("a/b/token", b"name match, deep");
    repo.write("tokens.txt", b"not the name");

    let policy = with(|p| p.secret_patterns = vec!["deploy/*.json".into(), "token".into()]);
    let result = snap(&repo.dir, &policy, &MemStore::new()).unwrap();
    assert_eq!(result.excluded_secrets, ["a/b/token", "deploy/creds.json"]);
    assert_eq!(
        paths(&result),
        ["other/deploy/creds.json", "readme.txt", "tokens.txt"]
    );
}

/// a33: a directory that is not a git working tree has no tracked files,
/// so every secret match is left out and reported.
#[test]
fn a33_secrets_in_a_plain_directory_are_excluded_and_reported() {
    let Some(tmp) = plain_dir() else { return };
    write(&tmp.path().join("app/main.py"), b"print()");
    write(&tmp.path().join("app/.npmrc"), b"//registry/:_authToken=x");
    write(&tmp.path().join("credentials.json"), b"{}");

    let result = snap_default(tmp.path());
    assert_eq!(result.excluded_secrets, ["app/.npmrc", "credentials.json"]);
    assert_eq!(paths(&result), ["app/main.py"]);
}

fn repo_with_ignored_outputs() -> Repo {
    let repo = Repo::new();
    repo.write(".gitignore", b"build/\n*.gen\n.env.local\n");
    repo.write("src.txt", b"source");
    repo.commit_all("init");
    repo.write("build/out/gen.h", b"#define GENERATED 1\n");
    repo.write("build/tmp/junk.o", b"junk");
    repo.write("x.gen", b"generated");
    repo.write(".env.local", b"LOCAL_TOKEN=1");
    repo
}

/// a34: ignored files stay out unless the policy declares them, by
/// directory, by glob or by exact path.
#[test]
fn a34_ignored_files_are_included_only_when_declared() {
    let repo = repo_with_ignored_outputs();
    let store = MemStore::new();
    let take = |declared: &[&str]| {
        let policy = with(|p| p.include_ignored = declared.iter().map(|d| d.to_string()).collect());
        snap(&repo.dir, &policy, &store).unwrap()
    };

    let none = take(&[]);
    assert_eq!(paths(&none), [".gitignore", "src.txt"]);
    // An ignored secret is not selected, so it is not reported either.
    assert!(none.excluded_secrets.is_empty());

    let directory = take(&["build/out"]);
    assert_eq!(
        paths(&directory),
        [".gitignore", "build/out/gen.h", "src.txt"]
    );
    assert_eq!(
        content(&store, &directory, "build/out/gen.h"),
        b"#define GENERATED 1\n"
    );

    assert_eq!(paths(&take(&["*.gen"])), [".gitignore", "src.txt", "x.gen"]);
    assert_eq!(
        paths(&take(&["build/out/gen.h", "x.gen"])),
        [".gitignore", "build/out/gen.h", "src.txt", "x.gen"]
    );
    // `exclude` wins over a declaration.
    let policy = with(|p| {
        p.include_ignored = vec!["build".into()];
        p.exclude = vec!["build/tmp".into()];
    });
    assert_eq!(
        paths(&snap(&repo.dir, &policy, &store).unwrap()),
        [".gitignore", "build/out/gen.h", "src.txt"]
    );
}

/// a34: a declared input named without wildcards is required. If it does
/// not exist the build would fail later for a reason that is hard to see,
/// so the capture fails now and names it.
#[test]
fn a34_missing_declared_input_fails() {
    let repo = repo_with_ignored_outputs();
    let store = MemStore::new();
    let policy = with(|p| p.include_ignored = vec!["build/out/missing.h".into()]);
    let err = snap(&repo.dir, &policy, &store).unwrap_err();
    assert!(
        matches!(&err, CaptureError::MissingDeclaredInput { pattern } if pattern == "build/out/missing.h"),
        "{err}"
    );
    assert!(store.is_empty(), "the check runs before any file is read");

    let policy = with(|p| p.include_ignored = vec!["missing.gen".into()]);
    assert!(matches!(
        snap(&repo.dir, &policy, &store),
        Err(CaptureError::MissingDeclaredInput { .. })
    ));

    // A wildcard is a selection, not a requirement.
    let policy = with(|p| p.include_ignored = vec!["build/**/*.nothing".into()]);
    assert_eq!(
        paths(&snap(&repo.dir, &policy, &store).unwrap()),
        [".gitignore", "src.txt"]
    );
}

/// a34 meets a33: declaring an ignored file by name is not permission to
/// capture a secret, and dropping a declared input quietly would break the
/// build that asked for it.
#[test]
fn a34_declared_input_that_is_a_secret_needs_allow_secrets() {
    let repo = repo_with_ignored_outputs();
    let store = MemStore::new();
    let policy = with(|p| p.include_ignored = vec![".env.local".into()]);
    let err = snap(&repo.dir, &policy, &store).unwrap_err();
    assert!(
        matches!(&err, CaptureError::DeclaredInputIsSecret { path, .. } if path == ".env.local"),
        "{err}"
    );

    let policy = with(|p| {
        p.include_ignored = vec![".env.local".into()];
        p.allow_secrets = vec![".env.local".into()];
    });
    let result = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(content(&store, &result, ".env.local"), b"LOCAL_TOKEN=1");

    // Swept up by a directory or a glob, a secret is withheld and reported.
    repo.write("build/out/.env", b"SWEPT=1");
    let policy = with(|p| p.include_ignored = vec!["build/out".into()]);
    let result = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(result.excluded_secrets, ["build/out/.env"]);
    assert!(entry(&result, "build/out/.env").is_none());
    assert!(entry(&result, "build/out/gen.h").is_some());
}

#[test]
fn excluded_paths_are_never_captured() {
    let repo = Repo::new();
    repo.write("src/a.rs", b"a");
    repo.write("src/gen/b.rs", b"b");
    repo.write("docs/big.mp4", b"video");
    repo.write("notes.tmp", b"tracked but excluded");
    repo.commit_all("init");
    repo.write("scratch/notes.tmp", b"untracked and excluded");
    repo.write("scratch/keep.txt", b"keep");

    let policy = with(|p| p.exclude = vec!["src/gen".into(), "*.tmp".into(), "docs/".into()]);
    let result = snap(&repo.dir, &policy, &MemStore::new()).unwrap();
    assert_eq!(paths(&result), ["scratch/keep.txt", "src/a.rs"]);
}

#[test]
fn size_limits_fail_with_the_path_and_the_numbers() {
    let repo = Repo::new();
    repo.write("small.txt", b"0123456789");
    repo.write("large.txt", b"0123456789A");
    repo.commit_all("init");

    let store = MemStore::new();
    let policy = with(|p| p.max_file_bytes = 10);
    let err = snap(&repo.dir, &policy, &store).unwrap_err();
    match &err {
        CaptureError::FileTooLarge { path, size, limit } => {
            assert_eq!((path.as_str(), *size, *limit), ("large.txt", 11, 10));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(store.is_empty(), "an oversized file is refused unread");

    let policy = with(|p| p.max_total_bytes = 20);
    let err = snap(&repo.dir, &policy, &store).unwrap_err();
    match &err {
        CaptureError::TotalTooLarge { path, limit } => {
            assert_eq!((path.as_str(), *limit), ("small.txt", 20));
        }
        other => panic!("unexpected error: {other}"),
    }

    let policy = with(|p| {
        p.max_file_bytes = 11;
        p.max_total_bytes = 21;
    });
    let result = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(result.stats.bytes, 21);
    assert_eq!(result.stats.files, 2);
}

#[test]
fn invalid_patterns_are_reported_with_their_list() {
    let repo = Repo::new();
    repo.write("f", b"f");
    let policy = with(|p| p.exclude = vec!["[unclosed".into()]);
    let err = snap(&repo.dir, &policy, &MemStore::new()).unwrap_err();
    assert!(
        matches!(&err, CaptureError::InvalidPattern { list: "exclude", pattern, .. } if pattern == "[unclosed"),
        "{err}"
    );
}

#[test]
fn default_policy_has_the_documented_secret_patterns() {
    let policy = CapturePolicy::default();
    for pattern in [
        ".env",
        ".env.*",
        "*.pem",
        "*.key",
        "id_rsa*",
        "id_ed25519*",
        ".npmrc",
        ".netrc",
        "*.p12",
        "*.pfx",
        "credentials.json",
    ] {
        assert!(
            policy.secret_patterns.iter().any(|p| p == pattern),
            "{pattern}"
        );
    }
    assert!(policy.include_untracked);
    assert_eq!(
        policy.required_consistency,
        bella_core::snapshot::Consistency::BestEffort
    );
}
