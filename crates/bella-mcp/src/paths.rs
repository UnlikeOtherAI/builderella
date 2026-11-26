//! Confinement of every path argument to the allowed roots.
//!
//! An agent, or text an agent was tricked into acting on, supplies the
//! paths. Each one is resolved through the filesystem first, so a symbolic
//! link or junction that points elsewhere is judged by where it leads, and
//! only then compared with the roots.

use crate::diag;
use crate::sanitize::inline;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// The canonical directories the server may read from and write into.
pub(crate) struct Roots {
    roots: Vec<PathBuf>,
}

impl Roots {
    /// Canonicalise the configured roots. A root that does not resolve to a
    /// directory is dropped with a diagnostic, never widened.
    pub(crate) fn new(configured: &[PathBuf]) -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        for raw in configured {
            match fs::canonicalize(raw) {
                Ok(canonical) if canonical.is_dir() => {
                    if !roots.contains(&canonical) {
                        roots.push(canonical);
                    }
                }
                Ok(_) => diag(format_args!(
                    "ignoring root {}: not a directory",
                    raw.display()
                )),
                Err(error) => diag(format_args!("ignoring root {}: {error}", raw.display())),
            }
        }
        Self { roots }
    }

    /// The default workspace: the first root.
    pub(crate) fn first(&self) -> Option<PathBuf> {
        self.roots.first().map(|root| plain_form(root))
    }

    fn contains(&self, canonical: &Path) -> bool {
        self.roots.iter().any(|root| canonical.starts_with(root))
    }

    fn outside(&self, argument: &str) -> String {
        if self.roots.is_empty() {
            return format!(
                "`{argument}` cannot be used: this server was started without a usable directory. \
                 Ask the user to restart `bella mcp` with `--root <directory>`."
            );
        }
        let allowed: Vec<String> = self
            .roots
            .iter()
            .take(4)
            .map(|root| format!("`{}`", plain_form(root).display()))
            .collect();
        format!(
            "`{argument}` resolves to a location outside the directories this server was started \
             for ({}). Only those directories can be used. If another directory is needed, ask the \
             user to restart `bella mcp` with `--root <directory>`; do not try other paths.",
            allowed.join(", ")
        )
    }

    /// Resolve an existing directory named by the agent. Links are followed
    /// before the containment check.
    pub(crate) fn existing_dir(&self, argument: &str, raw: &str) -> Result<PathBuf, String> {
        let path = absolute(argument, raw)?;
        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("`{argument}` `{}`: {error}", inline(raw, 200)))?;
        if !self.contains(&canonical) {
            return Err(self.outside(argument));
        }
        if !canonical.is_dir() {
            return Err(format!(
                "`{argument}` `{}` is not a directory",
                inline(raw, 200)
            ));
        }
        Ok(plain_form(&canonical))
    }

    /// Resolve a directory to write into, creating it when it does not exist
    /// yet. The existing part of the path decides containment, so nothing is
    /// created outside the roots.
    pub(crate) fn writable_dir(&self, argument: &str, raw: &str) -> Result<PathBuf, String> {
        let path = absolute(argument, raw)?;
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(format!("`{argument}` must not contain `..`"));
        }
        let shown = inline(raw, 200);

        // Split into the deepest part that exists and the tail to create.
        let mut existing = path;
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        loop {
            match fs::symlink_metadata(existing) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let (Some(name), Some(parent)) = (existing.file_name(), existing.parent())
                    else {
                        return Err(format!("`{argument}` `{shown}`: {error}"));
                    };
                    tail.push(name);
                    existing = parent;
                }
                Err(error) => return Err(format!("`{argument}` `{shown}`: {error}")),
            }
        }
        let base = fs::canonicalize(existing)
            .map_err(|error| format!("`{argument}` `{shown}`: {error}"))?;
        if !self.contains(&base) {
            return Err(self.outside(argument));
        }
        if !base.is_dir() {
            return Err(format!("`{argument}` `{shown}` is not a directory"));
        }
        if tail.is_empty() {
            return Ok(plain_form(&base));
        }
        let mut full = base;
        for name in tail.iter().rev() {
            full.push(name);
        }
        fs::create_dir_all(&full).map_err(|error| format!("`{argument}` `{shown}`: {error}"))?;
        // Resolve again: the result must still be inside after creation.
        let created =
            fs::canonicalize(&full).map_err(|error| format!("`{argument}` `{shown}`: {error}"))?;
        if !self.contains(&created) || !created.is_dir() {
            return Err(self.outside(argument));
        }
        Ok(plain_form(&created))
    }
}

fn absolute<'a>(argument: &str, raw: &'a str) -> Result<&'a Path, String> {
    if raw.contains('\0') {
        return Err(format!("`{argument}` contains a NUL character"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!(
            "`{argument}` must be an absolute path, got `{}`",
            inline(raw, 200)
        ));
    }
    Ok(path)
}

/// Turn the relative path of an artifact into a path below `base`, refusing
/// anything that could leave it. Returns the reason on refusal.
pub(crate) fn join_relative(base: &Path, relative: &str) -> Result<PathBuf, String> {
    bella_core::snapshot::validate_rel_path(relative)?;
    if relative.contains('\\') {
        return Err("contains a backslash".into());
    }
    if relative.contains(':') {
        return Err("contains a colon".into());
    }
    let mut out = base.to_path_buf();
    for part in relative.split('/') {
        out.push(part);
    }
    Ok(out)
}

/// What stands in the way of creating `target` below `base`, if anything:
/// the target itself, or a path component that is not a real directory.
pub(crate) fn obstruction(base: &Path, target: &Path) -> Option<PathBuf> {
    let Ok(rest) = target.strip_prefix(base) else {
        return Some(target.to_path_buf());
    };
    let mut current = base.to_path_buf();
    let mut parts = rest.components().peekable();
    while let Some(part) = parts.next() {
        current.push(part);
        let is_last = parts.peek().is_none();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !is_last && metadata.file_type().is_dir() => {}
            Ok(_) => return Some(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(_) => return Some(current),
        }
    }
    None
}

/// The form of a canonical path that ordinary tools accept. On Windows,
/// canonicalisation yields `\\?\C:\…`, which many programs reject; the
/// prefix is dropped when the path means the same without it. Elsewhere the
/// path is returned unchanged.
pub(crate) fn plain_form(canonical: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        windows::plain_form(canonical).unwrap_or_else(|| canonical.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        canonical.to_path_buf()
    }
}

#[cfg(windows)]
mod windows {
    use std::path::{Component, Path, PathBuf, Prefix};

    /// Longest path the non-verbatim form can carry.
    const MAX_PLAIN_LEN: usize = 247;

    pub(super) fn plain_form(canonical: &Path) -> Option<PathBuf> {
        let mut parts = canonical.components();
        let Some(Component::Prefix(prefix)) = parts.next() else {
            return None;
        };
        let Prefix::VerbatimDisk(letter) = prefix.kind() else {
            return None;
        };
        let mut out = PathBuf::from(format!("{}:\\", char::from(letter)));
        for part in parts {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    if !is_ordinary(name.to_str()?) {
                        return None;
                    }
                    out.push(name);
                }
                _ => return None,
            }
        }
        (out.as_os_str().len() <= MAX_PLAIN_LEN).then_some(out)
    }

    /// A name Win32 path parsing leaves alone: no trailing dot or space and
    /// not a device name such as `NUL` or `COM1`.
    fn is_ordinary(name: &str) -> bool {
        if name.ends_with('.') || name.ends_with(' ') {
            return false;
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end();
        let upper = stem.to_ascii_uppercase();
        let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.as_bytes()[3].is_ascii_digit()
                && upper.as_bytes()[3] != b'0');
        !device
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn verbatim_drive_paths_lose_the_prefix() {
            assert_eq!(
                plain_form(Path::new(r"\\?\C:\work\repo")),
                Some(PathBuf::from(r"C:\work\repo"))
            );
        }

        #[test]
        fn names_that_need_the_prefix_keep_it() {
            assert_eq!(plain_form(Path::new(r"\\?\C:\work\NUL")), None);
            assert_eq!(plain_form(Path::new(r"\\?\C:\work\com1.txt")), None);
            assert_eq!(plain_form(Path::new(r"\\?\C:\work\dot.")), None);
            assert_eq!(plain_form(Path::new(r"\\?\UNC\server\share\x")), None);
            let long = format!(r"\\?\C:\{}", "a".repeat(260));
            assert_eq!(plain_form(Path::new(&long)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> Roots {
        Roots::new(&[dir.to_path_buf()])
    }

    #[test]
    fn a_directory_inside_a_root_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("pkg");
        fs::create_dir(&inner).unwrap();
        let resolved = roots(root.path())
            .existing_dir("workspace", inner.to_str().unwrap())
            .unwrap();
        assert_eq!(
            fs::canonicalize(&resolved).unwrap(),
            fs::canonicalize(&inner).unwrap()
        );
    }

    #[test]
    fn a_sibling_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let error = roots(root.path())
            .existing_dir("workspace", other.path().to_str().unwrap())
            .unwrap_err();
        assert!(error.contains("outside the directories"), "{error}");
    }

    #[test]
    fn dot_dot_cannot_climb_out() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("pkg");
        fs::create_dir(&inner).unwrap();
        let climbing = inner.join("..").join("..");
        let error = roots(root.path())
            .existing_dir("workspace", climbing.to_str().unwrap())
            .unwrap_err();
        assert!(error.contains("outside the directories"), "{error}");
    }

    #[test]
    fn relative_missing_and_file_paths_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let roots = roots(root.path());
        assert!(
            roots
                .existing_dir("workspace", "relative/dir")
                .unwrap_err()
                .contains("absolute path")
        );
        let missing = root.path().join("missing");
        assert!(
            roots
                .existing_dir("workspace", missing.to_str().unwrap())
                .is_err()
        );
        let file = root.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert!(
            roots
                .existing_dir("workspace", file.to_str().unwrap())
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[test]
    fn a_prefix_of_the_root_name_is_not_inside_it() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        let lookalike = parent.path().join("repo-secrets");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&lookalike).unwrap();
        assert!(
            roots(&root)
                .existing_dir("workspace", lookalike.to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn a_missing_root_grants_nothing() {
        let parent = tempfile::tempdir().unwrap();
        let roots = Roots::new(&[parent.path().join("gone")]);
        assert!(roots.first().is_none());
        let error = roots
            .existing_dir("workspace", parent.path().to_str().unwrap())
            .unwrap_err();
        assert!(error.contains("without a usable directory"), "{error}");
    }

    #[test]
    fn a_destination_is_created_only_inside_a_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = roots(root.path());

        let nested = root.path().join("out").join("artifacts");
        let created = roots
            .writable_dir("dest", nested.to_str().unwrap())
            .unwrap();
        assert!(created.is_dir());

        let escaping = outside.path().join("out");
        assert!(
            roots
                .writable_dir("dest", escaping.to_str().unwrap())
                .is_err()
        );
        assert!(!escaping.exists());

        let climbing = root.path().join("out").join("..").join("..").join("x");
        assert!(
            roots
                .writable_dir("dest", climbing.to_str().unwrap())
                .unwrap_err()
                .contains("`..`")
        );
    }

    #[test]
    fn artifact_paths_cannot_leave_the_destination() {
        let base = Path::new("base");
        assert_eq!(
            join_relative(base, "bin/app").unwrap(),
            base.join("bin").join("app")
        );
        for bad in ["../x", "/abs", "a//b", "a/./b", "", "a\\b", "c:x", "a/../b"] {
            assert!(join_relative(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn existing_files_and_non_directories_obstruct() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        fs::create_dir(base.join("bin")).unwrap();
        fs::write(base.join("bin").join("app"), b"x").unwrap();
        fs::write(base.join("flat"), b"x").unwrap();

        assert_eq!(obstruction(base, &base.join("bin").join("new")), None);
        assert_eq!(obstruction(base, &base.join("fresh").join("deep")), None);
        assert_eq!(
            obstruction(base, &base.join("bin").join("app")),
            Some(base.join("bin").join("app"))
        );
        assert_eq!(
            obstruction(base, &base.join("flat").join("child")),
            Some(base.join("flat"))
        );
        // A directory where the file should go is an obstruction too.
        assert_eq!(obstruction(base, &base.join("bin")), Some(base.join("bin")));
    }
}
