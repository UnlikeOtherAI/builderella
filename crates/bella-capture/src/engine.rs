//! The capture itself: list, read once, verify, describe.

use crate::enumerate::{Candidate, Class, Listing, Rules, enumerate, first_difference};
use crate::error::CaptureError;
use crate::git;
use crate::read::{DirCache, Observed, Sig, SpoolError, Spooled, observe};
use crate::{CaptureBarrier, CapturePolicy, CaptureResult, CaptureStats, RootSpec};
use bella_core::snapshot::{
    Consistency, Entry, Manifest, RootManifest, RootProvenance, SnapshotMeta, find_collisions,
    validate_symlink, windows_incompatibility,
};
use bella_core::store::{ObjectStore, StoreError};
use bella_core::time::Clock;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Reads of one file before giving up: the first and three retries.
pub(crate) const READ_ATTEMPTS: u32 = 4;
/// Passes over the whole tree before giving up.
pub(crate) const MAX_PASSES: u32 = 3;
/// A regular file standing in for a symlink holds a path, not content.
const SYMLINK_PLACEHOLDER_MAX_BYTES: u64 = 4096;

/// Points at which a test can change the tree mid-capture.
pub(crate) trait Hooks: Sync {
    /// A file was opened for reading and no byte has been read yet.
    fn file_opened(&self, _path: &Path, _rel: &str, _attempt: u32) {}
    /// Every file of a pass was read; verification is about to start.
    fn before_verify(&self, _pass: u32) {}
}

pub(crate) struct Tuning<'a> {
    /// Bytes of one file kept in memory before spooling to a temporary file.
    pub spool_memory: usize,
    pub hooks: Option<&'a dyn Hooks>,
}

impl Default for Tuning<'_> {
    fn default() -> Self {
        Self {
            spool_memory: 4 * 1024 * 1024,
            hooks: None,
        }
    }
}

/// What one pass learned about one root.
struct RootPass {
    listing: Listing,
    /// What was at each candidate's path when it was captured, in the order
    /// of `listing.candidates`.
    observed: Vec<Observed>,
    entries: Vec<Entry>,
    provenance: RootProvenance,
    warnings: Vec<String>,
}

/// Counters that survive a restarted pass: the work was really done.
#[derive(Default)]
struct Carried {
    objects_stored: u64,
    bytes_stored: u64,
    objects_reused: u64,
    read_retries: u32,
}

/// The text put in front of a root-relative path in messages.
fn shown_prefix(root: &RootSpec) -> String {
    if root.mount == "." {
        String::new()
    } else {
        format!("{}/", root.mount)
    }
}

pub(crate) fn run(
    roots: &[RootSpec],
    policy: &CapturePolicy,
    store: &dyn ObjectStore,
    barrier: Option<&dyn CaptureBarrier>,
    clock: &dyn Clock,
    tuning: &Tuning<'_>,
) -> Result<CaptureResult, CaptureError> {
    if roots.is_empty() {
        return Err(CaptureError::NoRoots);
    }
    // The manifest rules for mounts, applied before anything is read.
    Manifest::new(
        roots
            .iter()
            .map(|root| RootManifest {
                mount: root.mount.clone(),
                entries: Vec::new(),
            })
            .collect(),
    )
    .validate()?;
    let rules = Rules::compile(policy)?;

    let mut warnings = Vec::new();
    let guard = match barrier.map(|barrier| barrier.acquire()) {
        Some(Ok(guard)) => Some(guard),
        Some(Err(reason)) => {
            warnings.push(format!("the capture barrier was not acquired: {reason}"));
            None
        }
        None => None,
    };
    let consistency = if guard.is_some() {
        Consistency::Coordinated
    } else {
        Consistency::BestEffort
    };
    if consistency < policy.required_consistency {
        return Err(CaptureError::Consistency {
            required: policy.required_consistency,
            achieved: consistency,
        });
    }

    for root in roots {
        let invalid = |reason: String| CaptureError::InvalidRoot {
            path: root.path.display().to_string(),
            reason,
        };
        match fs::metadata(&root.path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(invalid("not a directory".into())),
            Err(e) => return Err(invalid(e.to_string())),
        }
    }

    let mut carried = Carried::default();
    let mut last_change = String::new();
    for pass in 1..=MAX_PASSES {
        let done = capture_pass(roots, policy, &rules, store, tuning, &mut carried)?;
        if let Some(hooks) = tuning.hooks {
            hooks.before_verify(pass);
        }
        match changed_since(roots, &rules, &done)? {
            Some(path) => {
                tracing::debug!(pass, path = %path, "tree changed during capture; starting over");
                last_change = path;
            }
            None => {
                let result = describe(done, consistency, clock, &carried, pass, warnings)?;
                // The barrier is released only now, after verification.
                drop(guard);
                return Ok(result);
            }
        }
    }
    Err(CaptureError::TreeChanged {
        path: last_change,
        passes: MAX_PASSES,
    })
}

fn capture_pass(
    roots: &[RootSpec],
    policy: &CapturePolicy,
    rules: &Rules,
    store: &dyn ObjectStore,
    tuning: &Tuning<'_>,
    carried: &mut Carried,
) -> Result<Vec<RootPass>, CaptureError> {
    let mut listings = Vec::with_capacity(roots.len());
    for root in roots {
        listings.push(enumerate(&root.path, rules, &shown_prefix(root))?);
    }
    require_declared_inputs(roots, rules, &listings)?;

    let mut total_bytes = 0u64;
    let mut done = Vec::with_capacity(roots.len());
    for (root, listing) in roots.iter().zip(listings) {
        let git = if listing.git {
            Some(git::provenance(&root.path, rules.include_untracked)?)
        } else {
            None
        };
        let label = fs::canonicalize(&root.path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut reader = Reader {
            root: &root.path,
            shown_prefix: shown_prefix(root),
            listing: &listing,
            policy,
            store,
            tuning,
            carried: &mut *carried,
            total_bytes: &mut total_bytes,
            dirs: DirCache::default(),
            symlink_support: vec![None; listing.repos.len()],
            warnings: Vec::new(),
        };
        let mut observed = Vec::with_capacity(listing.candidates.len());
        let mut entries = Vec::with_capacity(listing.candidates.len());
        for candidate in &listing.candidates {
            let (entry, seen) = reader.capture(candidate)?;
            entries.extend(entry);
            observed.push(seen);
        }
        let warnings = reader.warnings;
        done.push(RootPass {
            listing,
            observed,
            entries,
            provenance: RootProvenance {
                mount: root.mount.clone(),
                label,
                git,
            },
            warnings,
        });
    }
    Ok(done)
}

/// A wildcard-free `include_ignored` entry is a required input: it must be
/// on disk in some root or among the selected paths.
fn require_declared_inputs(
    roots: &[RootSpec],
    rules: &Rules,
    listings: &[Listing],
) -> Result<(), CaptureError> {
    for pattern in rules.include_ignored.patterns() {
        let Some(literal) = pattern.literal_path() else {
            continue;
        };
        let on_disk = roots
            .iter()
            .any(|root| root.path.join(&literal).symlink_metadata().is_ok());
        let selected = || {
            listings
                .iter()
                .any(|listing| listing.candidates.iter().any(|c| pattern.covers(&c.rel)))
        };
        if !on_disk && !selected() {
            return Err(CaptureError::MissingDeclaredInput {
                pattern: pattern.raw().to_string(),
            });
        }
    }
    Ok(())
}

struct Reader<'a> {
    root: &'a Path,
    shown_prefix: String,
    listing: &'a Listing,
    policy: &'a CapturePolicy,
    store: &'a dyn ObjectStore,
    tuning: &'a Tuning<'a>,
    carried: &'a mut Carried,
    total_bytes: &'a mut u64,
    dirs: DirCache,
    /// Per repository: whether its checkout uses real symlinks.
    symlink_support: Vec<Option<bool>>,
    warnings: Vec<String>,
}

impl Reader<'_> {
    /// Capture one candidate: the manifest entry, if the path holds
    /// something to record, and what was observed there.
    fn capture(
        &mut self,
        candidate: &Candidate,
    ) -> Result<(Option<Entry>, Observed), CaptureError> {
        let shown = format!("{}{}", self.shown_prefix, candidate.rel);
        let io_error = |source| CaptureError::Io {
            path: shown.clone(),
            source,
        };
        for attempt in 1..=READ_ATTEMPTS {
            let before = observe(self.root, candidate, &mut self.dirs).map_err(io_error)?;
            let sig = match &before {
                Observed::Missing | Observed::Dir => return Ok((None, before)),
                Observed::Other => {
                    self.warnings.push(format!(
                        "`{shown}` is not a regular file, directory or symlink and was not captured"
                    ));
                    return Ok((None, before));
                }
                Observed::Symlink(target) => {
                    refuse_tracked_secret(candidate, &shown)?;
                    let target = target
                        .to_str()
                        .ok_or_else(|| CaptureError::Symlink {
                            path: shown.clone(),
                            target: target.to_string_lossy().into_owned(),
                            reason: "target is not valid UTF-8".into(),
                        })?
                        .to_string();
                    // Windows spells the separator of a link target `\`.
                    #[cfg(windows)]
                    let target = target.replace('\\', "/");
                    let entry = symlink_entry(candidate, &shown, target)?;
                    return Ok((Some(entry), before));
                }
                Observed::File(sig) => sig.clone(),
            };
            refuse_tracked_secret(candidate, &shown)?;
            let limit = self.policy.max_file_bytes;
            let too_large = |size| CaptureError::FileTooLarge {
                path: shown.clone(),
                size,
                limit,
            };
            if sig.len > limit {
                return Err(too_large(sig.len));
            }
            let hooks = self.tuning.hooks;
            let opened = || {
                if let Some(hooks) = hooks {
                    hooks.file_opened(&candidate.abs, &candidate.rel, attempt);
                }
            };
            let read = Spooled::read(&candidate.abs, limit, self.tuning.spool_memory, opened);
            let mut spooled = match read {
                Ok(spooled) => Some(spooled),
                Err(SpoolError::Vanished) => None,
                Err(SpoolError::TooLarge(size)) => return Err(too_large(size)),
                Err(SpoolError::Io(e)) => return Err(io_error(e)),
            };
            let after = observe(self.root, candidate, &mut self.dirs).map_err(io_error)?;
            let stable = spooled
                .as_ref()
                .is_some_and(|spooled| after == before && spooled.len == sig.len);
            let Some(spooled) = spooled.as_mut().filter(|_| stable) else {
                tracing::debug!(path = %shown, attempt, "file changed while it was read");
                if attempt < READ_ATTEMPTS {
                    self.carried.read_retries += 1;
                }
                self.dirs.clear();
                continue;
            };
            let entry = self.record(candidate, &shown, &sig, spooled)?;
            return Ok((Some(entry), before));
        }
        Err(CaptureError::FileChanged {
            path: shown,
            attempts: READ_ATTEMPTS,
        })
    }

    /// Turn one stable read into a manifest entry, storing its object.
    fn record(
        &mut self,
        candidate: &Candidate,
        shown: &str,
        sig: &Sig,
        spooled: &mut Spooled,
    ) -> Result<Entry, CaptureError> {
        let io_error = |source| CaptureError::Io {
            path: shown.to_string(),
            source,
        };
        let repo = candidate
            .repo
            .map(|index| (index, &self.listing.repos[index]));

        // A checkout without symlink support writes a link as a small file
        // holding its target; git still treats that file as the link.
        if candidate.index_mode == Some(git::MODE_SYMLINK)
            && spooled.len <= SYMLINK_PLACEHOLDER_MAX_BYTES
            && let Some((index, repo)) = repo
        {
            let supported = match self.symlink_support[index] {
                Some(known) => known,
                None => {
                    let known = git::config_bool(&repo.dir, "core.symlinks")? != Some(false);
                    self.symlink_support[index] = Some(known);
                    known
                }
            };
            if !supported {
                let bytes = spooled.bytes().map_err(io_error)?;
                let target = String::from_utf8(bytes).map_err(|e| CaptureError::Symlink {
                    path: shown.to_string(),
                    target: String::from_utf8_lossy(e.as_bytes()).into_owned(),
                    reason: "target is not valid UTF-8".into(),
                })?;
                return symlink_entry(candidate, shown, target);
            }
        }

        if spooled.len <= git::LFS_POINTER_MAX_BYTES
            && let Some((_, repo)) = repo
            && git::is_lfs_pointer(&spooled.bytes().map_err(io_error)?)
            && git::has_lfs_filter(&repo.dir, &candidate.rel[candidate.prefix_len..])?
        {
            return Err(CaptureError::LfsNotHydrated {
                path: shown.to_string(),
            });
        }

        *self.total_bytes += spooled.len;
        if *self.total_bytes > self.policy.max_total_bytes {
            return Err(CaptureError::TotalTooLarge {
                path: shown.to_string(),
                limit: self.policy.max_total_bytes,
            });
        }

        let store_error = |source| CaptureError::Store {
            path: shown.to_string(),
            source,
        };
        if self.store.contains(&spooled.digest).map_err(store_error)? {
            self.carried.objects_reused += 1;
        } else {
            let expected = spooled.digest.clone();
            let store = self.store;
            let (digest, len) = spooled
                .with_reader(|reader| store.put(reader, Some(&expected)))
                .map_err(io_error)?
                .map_err(store_error)?;
            if digest != expected || len != spooled.len {
                return Err(store_error(StoreError::DigestMismatch {
                    expected,
                    actual: digest,
                }));
            }
            self.carried.objects_stored += 1;
            self.carried.bytes_stored += len;
        }

        Ok(Entry::File {
            path: candidate.rel.clone(),
            digest: spooled.digest.clone(),
            size: spooled.len,
            // Without mode bits on disk the index is the only record.
            executable: sig
                .executable()
                .unwrap_or(candidate.index_mode == Some(git::MODE_EXECUTABLE)),
        })
    }
}

/// A tracked path that matches a secret pattern fails the capture once it
/// is known to exist.
fn refuse_tracked_secret(candidate: &Candidate, shown: &str) -> Result<(), CaptureError> {
    match (&candidate.secret, candidate.class) {
        (Some(pattern), Class::Tracked) => Err(CaptureError::TrackedSecret {
            path: shown.to_string(),
            pattern: pattern.clone(),
        }),
        _ => Ok(()),
    }
}

fn symlink_entry(
    candidate: &Candidate,
    shown: &str,
    target: String,
) -> Result<Entry, CaptureError> {
    match validate_symlink(&candidate.rel, &target) {
        Ok(()) => Ok(Entry::Symlink {
            path: candidate.rel.clone(),
            target,
        }),
        Err(reason) => Err(CaptureError::Symlink {
            path: shown.to_string(),
            target,
            reason,
        }),
    }
}

/// List and examine every root again. Returns the first path that is not
/// what the pass captured: created, removed, modified or reclassified.
fn changed_since(
    roots: &[RootSpec],
    rules: &Rules,
    done: &[RootPass],
) -> Result<Option<String>, CaptureError> {
    for (root, pass) in roots.iter().zip(done) {
        let prefix = shown_prefix(root);
        let now = enumerate(&root.path, rules, &prefix)?;
        if let Some(rel) = first_difference(&pass.listing.candidates, &now.candidates) {
            return Ok(Some(format!("{prefix}{rel}")));
        }
        if now.excluded_secrets != pass.listing.excluded_secrets {
            let before: BTreeSet<&String> = pass.listing.excluded_secrets.iter().collect();
            let after: BTreeSet<&String> = now.excluded_secrets.iter().collect();
            let path = before.symmetric_difference(&after).next();
            return Ok(Some(path.map(|p| p.to_string()).unwrap_or_default()));
        }
        let mut dirs = DirCache::default();
        for (candidate, captured) in pass.listing.candidates.iter().zip(&pass.observed) {
            let shown = format!("{prefix}{}", candidate.rel);
            let current =
                observe(&root.path, candidate, &mut dirs).map_err(|source| CaptureError::Io {
                    path: shown.clone(),
                    source,
                })?;
            if current != *captured {
                return Ok(Some(shown));
            }
        }
    }
    Ok(None)
}

/// Facts about a tree that decide where it can be materialized: the first
/// reason it cannot exist on Windows, whether it needs a case-sensitive
/// filesystem, and whether it has symlinks.
pub(crate) fn tree_facts(manifest: &Manifest) -> (Option<String>, bool, bool) {
    let mut windows_incompatible = None;
    let mut has_symlinks = false;
    // Every path and every directory above it: `Dir/a` and `dir/b` collide
    // through their directories although the full paths differ.
    let mut paths = BTreeSet::new();
    let mut note = |path: &str, windows_incompatible: &mut Option<String>| {
        if windows_incompatible.is_none() {
            *windows_incompatible =
                windows_incompatibility(path).map(|reason| format!("`{path}`: {reason}"));
        }
        for (end, _) in path.match_indices('/') {
            paths.insert(path[..end].to_string());
        }
        paths.insert(path.to_string());
    };
    for root in &manifest.roots {
        let base = if root.mount == "." {
            String::new()
        } else {
            note(&root.mount, &mut windows_incompatible);
            format!("{}/", root.mount)
        };
        for entry in &root.entries {
            let path = format!("{base}{}", entry.path());
            note(&path, &mut windows_incompatible);
            if let Entry::Symlink { target, .. } = entry {
                has_symlinks = true;
                // `.` and `..` steer the target; only its names can be
                // unrepresentable.
                let names: Vec<&str> = target
                    .split('/')
                    .filter(|c| !matches!(*c, "" | "." | ".."))
                    .collect();
                if windows_incompatible.is_none()
                    && let Some(reason) = windows_incompatibility(&names.join("/"))
                {
                    windows_incompatible =
                        Some(format!("symlink `{path}` -> `{target}`: {reason}"));
                }
            }
        }
    }
    let case_sensitive_only = !find_collisions(paths.iter().map(String::as_str)).is_empty();
    (windows_incompatible, case_sensitive_only, has_symlinks)
}

fn describe(
    done: Vec<RootPass>,
    consistency: Consistency,
    clock: &dyn Clock,
    carried: &Carried,
    passes: u32,
    mut warnings: Vec<String>,
) -> Result<CaptureResult, CaptureError> {
    let mut excluded_secrets = Vec::new();
    let mut provenance = Vec::with_capacity(done.len());
    let mut roots = Vec::with_capacity(done.len());
    for pass in done {
        excluded_secrets.extend(pass.listing.excluded_secrets);
        warnings.extend(pass.listing.warnings);
        warnings.extend(pass.warnings);
        roots.push(RootManifest {
            mount: pass.provenance.mount.clone(),
            entries: pass.entries,
        });
        provenance.push(pass.provenance);
    }
    excluded_secrets.sort();
    provenance.sort_by(|a, b| a.mount.as_bytes().cmp(b.mount.as_bytes()));

    let manifest = Manifest::new(roots);
    manifest.validate()?;
    let (windows_incompatible, case_sensitive_only, has_symlinks) = tree_facts(&manifest);
    let symlinks = manifest
        .roots
        .iter()
        .flat_map(|root| &root.entries)
        .filter(|entry| matches!(entry, Entry::Symlink { .. }))
        .count() as u64;
    Ok(CaptureResult {
        snapshot_id: manifest.id(),
        stats: CaptureStats {
            files: manifest.file_count(),
            symlinks,
            bytes: manifest.total_bytes(),
            objects_stored: carried.objects_stored,
            bytes_stored: carried.bytes_stored,
            objects_reused: carried.objects_reused,
            read_retries: carried.read_retries,
            passes,
        },
        meta: SnapshotMeta {
            consistency,
            captured_at: clock.now(),
            roots: provenance,
            windows_incompatible,
            case_sensitive_only,
            has_symlinks,
        },
        manifest,
        excluded_secrets,
        warnings,
    })
}
