//! Decide which paths of a source root belong to a capture.
//!
//! Nothing is read here beyond directory listings and git's own index and
//! ignore rules. The result is the sorted list of paths to capture, each
//! with what git knows about it.

use crate::CapturePolicy;
use crate::error::CaptureError;
use crate::git;
use crate::pattern::PatternSet;
use bella_core::snapshot::{ManifestError, validate_rel_path};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Submodules and nested repositories deeper than this are refused rather
/// than followed.
const MAX_REPOSITORY_NESTING: usize = 32;
/// How many missing paths a sparse-checkout error lists.
const SPARSE_MISSING_SHOWN: usize = 20;

/// The policy's pattern lists, compiled once.
pub(crate) struct Rules {
    pub include_untracked: bool,
    pub exclude: PatternSet,
    pub include_ignored: PatternSet,
    pub secrets: PatternSet,
    pub allow_secrets: PatternSet,
}

impl Rules {
    pub(crate) fn compile(policy: &CapturePolicy) -> Result<Self, CaptureError> {
        Ok(Self {
            include_untracked: policy.include_untracked,
            exclude: PatternSet::parse("exclude", &policy.exclude, false)?,
            include_ignored: PatternSet::parse("include_ignored", &policy.include_ignored, false)?,
            // Secret names are matched without regard to case: `.ENV` is the
            // same file as `.env` on most developer machines.
            secrets: PatternSet::parse("secret_patterns", &policy.secret_patterns, true)?,
            allow_secrets: PatternSet::parse("allow_secrets", &policy.allow_secrets, false)?,
        })
    }

    /// The secret pattern that withholds `rel`, unless it is allowed.
    fn secret(&self, rel: &str) -> Option<&str> {
        let pattern = self.secrets.file_match(rel)?;
        self.allow_secrets
            .tree_match(rel)
            .is_none()
            .then(|| pattern.raw())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// In the git index.
    Tracked,
    /// Not in the index and not ignored.
    Untracked,
    /// Ignored by git but declared in `include_ignored`.
    Ignored,
    /// Found by walking a directory that is not a git working tree.
    Plain,
}

/// One path selected for capture.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    /// Relative to the source root, `/`-separated.
    pub rel: String,
    pub abs: PathBuf,
    pub class: Class,
    /// Mode recorded in the git index, for tracked paths.
    pub index_mode: Option<u32>,
    /// Index into [`Listing::repos`] of the repository that owns the path.
    pub repo: Option<usize>,
    /// `rel[prefix_len..]` is the path relative to that repository.
    pub prefix_len: usize,
    /// Secret pattern a tracked path matches without being allowed. It
    /// fails the capture if the path exists.
    pub secret: Option<String>,
}

pub(crate) struct Repo {
    pub dir: PathBuf,
}

/// Everything selected under one source root.
pub(crate) struct Listing {
    /// Sorted by `rel`, unique.
    pub candidates: Vec<Candidate>,
    pub repos: Vec<Repo>,
    /// Untracked files withheld by the secret patterns, as shown to the user.
    pub excluded_secrets: Vec<String>,
    pub warnings: Vec<String>,
    /// The root is a git working tree.
    pub git: bool,
}

/// List the paths of `root` that the policy selects. `shown_prefix` is put
/// in front of root-relative paths in messages (`""` or `"<mount>/"`).
pub(crate) fn enumerate(
    root: &Path,
    rules: &Rules,
    shown_prefix: &str,
) -> Result<Listing, CaptureError> {
    let mut builder = Builder {
        rules,
        shown_prefix,
        found: BTreeMap::new(),
        repos: Vec::new(),
        excluded_secrets: Vec::new(),
        warnings: Vec::new(),
    };
    let kind = git::root_kind(root)?;
    let git = kind == git::RootKind::WorkTree;
    if git {
        builder.list_repo(root.to_path_buf(), String::new(), 0)?;
    } else {
        if kind == git::RootKind::IgnoredByEnclosingTree {
            builder.warnings.push(format!(
                "`{}` is ignored by the git repository that contains it; \
                 it was captured as a plain directory, without ignore rules",
                root.display()
            ));
        }
        builder.walk(root.to_path_buf(), String::new(), Class::Plain, None)?;
    }
    builder.excluded_secrets.sort();
    builder.excluded_secrets.dedup();
    Ok(Listing {
        candidates: builder.found.into_values().collect(),
        repos: builder.repos,
        excluded_secrets: builder.excluded_secrets,
        warnings: builder.warnings,
        git,
    })
}

struct Builder<'a> {
    rules: &'a Rules,
    shown_prefix: &'a str,
    found: BTreeMap<String, Candidate>,
    repos: Vec<Repo>,
    excluded_secrets: Vec<String>,
    warnings: Vec<String>,
}

impl Builder<'_> {
    fn shown(&self, rel: &str) -> String {
        format!("{}{rel}", self.shown_prefix)
    }

    fn io_error(&self, rel: &str, source: std::io::Error) -> CaptureError {
        CaptureError::Io {
            path: self.shown(rel),
            source,
        }
    }

    /// A name from git or the filesystem as text, or the failure that names
    /// it. Nothing with an undecodable name is skipped.
    fn utf8<'b>(&self, prefix: &str, name: &'b [u8]) -> Result<&'b str, CaptureError> {
        std::str::from_utf8(name).map_err(|_| CaptureError::NonUtf8Path {
            path: self.shown(&format!("{prefix}{}", String::from_utf8_lossy(name))),
        })
    }

    fn rel(&self, prefix: &str, path: &str) -> Result<String, CaptureError> {
        let rel = format!("{prefix}{path}");
        validate_rel_path(&rel).map_err(|reason| ManifestError::Path {
            path: self.shown(&rel),
            reason,
        })?;
        Ok(rel)
    }

    fn add(&mut self, candidate: Candidate) {
        self.found.entry(candidate.rel.clone()).or_insert(candidate);
    }

    /// Apply the secret rule for a file git does not track. Returns `true`
    /// when the file is withheld.
    fn withhold_secret(&mut self, rel: &str) -> bool {
        if self.rules.secret(rel).is_none() {
            return false;
        }
        self.excluded_secrets.push(self.shown(rel));
        true
    }

    /// An ignored file is captured only when `include_ignored` declares it.
    fn offer_ignored(
        &mut self,
        rel: String,
        abs: PathBuf,
        repo: Option<(usize, usize)>,
    ) -> Result<(), CaptureError> {
        if self.rules.exclude.tree_match(&rel).is_some()
            || self.rules.include_ignored.tree_match(&rel).is_none()
        {
            return Ok(());
        }
        if let Some(pattern) = self.rules.secret(&rel) {
            // Naming the file itself is a request for it; dropping it
            // quietly would break the build that declared it.
            if self.rules.include_ignored.names_file(&rel) {
                return Err(CaptureError::DeclaredInputIsSecret {
                    path: self.shown(&rel),
                    pattern: pattern.to_string(),
                });
            }
            self.excluded_secrets.push(self.shown(&rel));
            return Ok(());
        }
        self.add(Candidate {
            rel,
            abs,
            class: Class::Ignored,
            index_mode: None,
            repo: repo.map(|(index, _)| index),
            prefix_len: repo.map_or(0, |(_, prefix_len)| prefix_len),
            secret: None,
        });
        Ok(())
    }

    /// List one git working tree: `dir` is captured at `prefix` inside the
    /// source root. Submodules and nested repositories recurse.
    fn list_repo(
        &mut self,
        dir: PathBuf,
        prefix: String,
        depth: usize,
    ) -> Result<(), CaptureError> {
        if depth > MAX_REPOSITORY_NESTING {
            return Err(CaptureError::InvalidRoot {
                path: dir.display().to_string(),
                reason: format!("repositories are nested more than {MAX_REPOSITORY_NESTING} deep"),
            });
        }
        let repo = self.repos.len();
        self.repos.push(Repo { dir: dir.clone() });
        let owner = Some((repo, prefix.len()));

        let args = ["ls-files", "-z", "-s", "-t"];
        let output = git::run(&dir, &args)?;
        let index = git::parse_index(&output).map_err(|detail| CaptureError::Git {
            command: args.join(" "),
            dir: dir.display().to_string(),
            detail,
        })?;
        let mut tracked = Vec::with_capacity(index.len());
        for entry in index {
            let path = self.utf8(&prefix, entry.path)?;
            let rel = self.rel(&prefix, path)?;
            if self.rules.exclude.tree_match(&rel).is_none() {
                tracked.push((rel, path, entry.mode, entry.skip_worktree));
            }
        }
        // An unmerged path is listed once per stage.
        tracked.dedup_by(|later, earlier| later.0 == earlier.0);

        let sparse = tracked.iter().any(|entry| entry.3)
            && git::config_bool(&dir, "core.sparseCheckout")? == Some(true);
        let mut missing = Vec::new();
        let mut submodules = Vec::new();
        for (rel, path, mode, skip_worktree) in tracked {
            let abs = dir.join(path);
            if sparse && skip_worktree && abs.symlink_metadata().is_err() {
                missing.push(self.shown(&rel));
                continue;
            }
            if mode == git::MODE_GITLINK {
                submodules.push((rel, abs));
                continue;
            }
            let secret = self.rules.secret(&rel).map(str::to_string);
            self.add(Candidate {
                rel,
                abs,
                class: Class::Tracked,
                index_mode: Some(mode),
                repo: Some(repo),
                prefix_len: prefix.len(),
                secret,
            });
        }
        if !missing.is_empty() {
            let count = missing.len();
            missing.truncate(SPARSE_MISSING_SHOWN);
            return Err(CaptureError::SparseCheckout { count, missing });
        }

        for (rel, abs) in submodules {
            if abs.join(".git").symlink_metadata().is_err() {
                return Err(CaptureError::SubmoduleNotInitialised {
                    path: self.shown(&rel),
                });
            }
            self.list_repo(abs, format!("{rel}/"), depth + 1)?;
        }

        if self.rules.include_untracked {
            let output = git::run(&dir, &["ls-files", "-z", "--others", "--exclude-standard"])?;
            for name in git::split_nul(&output) {
                let path = self.utf8(&prefix, name)?;
                // git names a directory only when it is another repository.
                if let Some(nested) = path.strip_suffix('/') {
                    let rel = self.rel(&prefix, nested)?;
                    if self.rules.exclude.tree_match(&rel).is_some() {
                        continue;
                    }
                    let abs = dir.join(nested);
                    if abs.join(".git").symlink_metadata().is_ok() {
                        self.warnings.push(format!(
                            "untracked nested repository `{}` was captured from its own working tree",
                            self.shown(&rel)
                        ));
                        self.list_repo(abs, format!("{rel}/"), depth + 1)?;
                    } else {
                        self.warnings.push(format!(
                            "git listed `{}` as a directory without contents; it was not captured",
                            self.shown(&rel)
                        ));
                    }
                    continue;
                }
                let rel = self.rel(&prefix, path)?;
                if self.rules.exclude.tree_match(&rel).is_some() || self.withhold_secret(&rel) {
                    continue;
                }
                self.add(Candidate {
                    abs: dir.join(path),
                    rel,
                    class: Class::Untracked,
                    index_mode: None,
                    repo: Some(repo),
                    prefix_len: prefix.len(),
                    secret: None,
                });
            }
        }

        if !self.rules.include_ignored.is_empty() {
            let output = git::run(
                &dir,
                &[
                    "ls-files",
                    "-z",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                ],
            )?;
            for name in git::split_nul(&output) {
                let path = self.utf8(&prefix, name)?;
                if let Some(nested) = path.strip_suffix('/') {
                    // An ignored repository: everything in it is ignored
                    // from here, so only declared files are taken.
                    let rel = self.rel(&prefix, nested)?;
                    if self.rules.exclude.tree_match(&rel).is_none() {
                        self.walk(dir.join(nested), format!("{rel}/"), Class::Ignored, owner)?;
                    }
                    continue;
                }
                let rel = self.rel(&prefix, path)?;
                self.offer_ignored(rel, dir.join(path), owner)?;
            }
        }
        Ok(())
    }

    /// Walk a directory without git. Symlinks are listed, never followed,
    /// and `.git` is never entered.
    fn walk(
        &mut self,
        dir: PathBuf,
        prefix: String,
        class: Class,
        repo: Option<(usize, usize)>,
    ) -> Result<(), CaptureError> {
        let mut pending = vec![(dir, prefix)];
        while let Some((dir, prefix)) = pending.pop() {
            let here = if prefix.is_empty() {
                "."
            } else {
                prefix.trim_end_matches('/')
            };
            let reader = fs::read_dir(&dir).map_err(|e| self.io_error(here, e))?;
            for item in reader {
                let item = item.map_err(|e| self.io_error(here, e))?;
                let name = item.file_name();
                let Some(name) = name.to_str() else {
                    return Err(CaptureError::NonUtf8Path {
                        path: self.shown(&format!("{prefix}{}", name.to_string_lossy())),
                    });
                };
                if name.eq_ignore_ascii_case(".git") {
                    continue;
                }
                let rel = self.rel(&prefix, name)?;
                if self.rules.exclude.tree_match(&rel).is_some() {
                    continue;
                }
                let kind = item.file_type().map_err(|e| self.io_error(&rel, e))?;
                if kind.is_dir() {
                    pending.push((item.path(), format!("{rel}/")));
                } else if class == Class::Ignored {
                    self.offer_ignored(rel, item.path(), repo)?;
                } else if !self.withhold_secret(&rel) {
                    self.add(Candidate {
                        rel,
                        abs: item.path(),
                        class,
                        index_mode: None,
                        repo: None,
                        prefix_len: 0,
                        secret: None,
                    });
                }
            }
        }
        Ok(())
    }
}

/// The first directory above `rel` that is a link instead of a directory,
/// with the link's target.
pub(crate) fn link_above(root: &Path, rel: &str) -> Option<(String, PathBuf)> {
    for (end, _) in rel.match_indices('/') {
        let dir = root.join(&rel[..end]);
        let kind = fs::symlink_metadata(&dir).ok()?.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(&dir).unwrap_or_default();
            return Some((rel[..end].to_string(), target));
        }
        if !kind.is_dir() {
            return None;
        }
    }
    None
}

/// First path at which two listings of the same root disagree.
pub(crate) fn first_difference(before: &[Candidate], after: &[Candidate]) -> Option<String> {
    let key = |c: &Candidate| (c.class, c.index_mode);
    let (mut b, mut a) = (before.iter().peekable(), after.iter().peekable());
    loop {
        match (b.peek(), a.peek()) {
            (None, None) => return None,
            (Some(only), None) | (None, Some(only)) => return Some(only.rel.clone()),
            (Some(x), Some(y)) if x.rel == y.rel && key(x) == key(y) => {
                b.next();
                a.next();
            }
            (Some(x), Some(y)) => return Some(x.rel.as_str().min(y.rel.as_str()).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(rel: &str, class: Class, index_mode: Option<u32>) -> Candidate {
        Candidate {
            rel: rel.into(),
            abs: PathBuf::from(rel),
            class,
            index_mode,
            repo: None,
            prefix_len: 0,
            secret: None,
        }
    }

    /// A name git reports that is not valid UTF-8 is an error that shows
    /// the name, with the path it was found under.
    #[test]
    fn undecodable_name_from_git_is_an_error_with_the_path() {
        let rules = Rules::compile(&CapturePolicy::default()).unwrap();
        let builder = Builder {
            rules: &rules,
            shown_prefix: "app/",
            found: BTreeMap::new(),
            repos: Vec::new(),
            excluded_secrets: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(
            builder.utf8("vendor/", "ünï.txt".as_bytes()).unwrap(),
            "ünï.txt"
        );
        match builder.utf8("vendor/", b"bad-\xff-name.txt") {
            Err(CaptureError::NonUtf8Path { path }) => {
                assert_eq!(path, "app/vendor/bad-\u{fffd}-name.txt");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(builder.rel("", "ok/path").is_ok());
        for bad in ["../up", "a//b", "/abs", ""] {
            assert!(builder.rel("", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn listings_are_compared_by_path_class_and_mode() {
        let a = candidate("a", Class::Tracked, Some(0o100644));
        let b = candidate("b", Class::Untracked, None);
        let c = candidate("c", Class::Tracked, Some(0o100644));
        let same = [a.clone(), b.clone()];
        assert_eq!(first_difference(&same, &same), None);
        assert_eq!(
            first_difference(&[a.clone(), b.clone()], &[a.clone()]),
            Some("b".into())
        );
        assert_eq!(
            first_difference(&[a.clone(), c.clone()], &[a.clone(), b.clone(), c.clone()]),
            Some("b".into())
        );
        let executable = candidate("a", Class::Tracked, Some(0o100755));
        assert_eq!(
            first_difference(&[a.clone()], &[executable]),
            Some("a".into())
        );
        let now_tracked = candidate("b", Class::Tracked, Some(0o100644));
        assert_eq!(
            first_difference(&[a.clone(), b], &[a, now_tracked]),
            Some("b".into())
        );
    }
}
