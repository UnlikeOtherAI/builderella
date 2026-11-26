//! Finding artifact and test-report files in a workspace.
//!
//! Patterns are relative to the source set root and `/`-separated:
//!
//! * `*` matches any run of characters within one name, `?` one character,
//!   `[abc]`, `[a-z]` and `[!a-z]` one character of a set;
//! * `**` as a whole component matches any number of directories,
//!   including none;
//! * a pattern that names a directory collects every file below it.
//!
//! Only regular files are collected. Symbolic links (and on Windows any
//! reparse point) are never followed and never collected, so nothing
//! outside the workspace can be reached through one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One file matched by a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    /// Path relative to the source set root, `/`-separated.
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Why a pattern was not expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PatternError {
    pub pattern: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    /// `**`
    AnyDirs,
    /// A name without wildcard characters.
    Literal(String),
    /// A name with `*`, `?` or a set.
    Wild(Vec<char>),
}

fn parse(pattern: &str) -> Result<Vec<Part>, String> {
    if pattern.contains('\\') {
        return Err("uses `\\`; separate directories with `/`".to_string());
    }
    if pattern.contains('\0') {
        return Err("contains a NUL character".to_string());
    }
    if pattern.starts_with('/') {
        return Err("is an absolute path".to_string());
    }
    if cfg!(windows) && pattern.contains(':') {
        return Err("contains `:`".to_string());
    }
    let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
    let mut parts = Vec::new();
    for component in trimmed.split('/') {
        match component {
            "" => return Err("has an empty path component".to_string()),
            "." => {}
            ".." => return Err("contains `..`".to_string()),
            "**" => {
                if parts.last() != Some(&Part::AnyDirs) {
                    parts.push(Part::AnyDirs);
                }
            }
            name if name.contains(['*', '?', '[']) => {
                parts.push(Part::Wild(name.chars().collect()));
            }
            name => parts.push(Part::Literal(name.to_string())),
        }
    }
    if parts.is_empty() {
        return Err("matches the whole source root".to_string());
    }
    Ok(parts)
}

/// Does `name` match the single-component pattern `pattern`?
fn name_matches(pattern: &[char], name: &[char]) -> bool {
    // Iterative matching with one backtrack point per `*`.
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        let step = match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
                continue;
            }
            Some('?') => Some(p + 1),
            Some('[') => match_set(pattern, p, name[n]),
            Some(c) if *c == name[n] => Some(p + 1),
            _ => None,
        };
        match step {
            Some(next) => {
                p = next;
                n += 1;
            }
            None => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Match `c` against the set that starts at `pattern[open]` (`[`). Returns
/// the index after the closing `]` on a match. An unterminated set matches
/// a literal `[`.
fn match_set(pattern: &[char], open: usize, c: char) -> Option<usize> {
    let mut i = open + 1;
    let negated = matches!(pattern.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let first = i;
    let mut matched = false;
    loop {
        let Some(&low) = pattern.get(i) else {
            return (c == '[').then_some(open + 1);
        };
        if low == ']' && i > first {
            break;
        }
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|end| *end != ']') {
            let high = pattern[i + 2];
            matched |= low <= c && c <= high;
            i += 3;
        } else {
            matched |= low == c;
            i += 1;
        }
    }
    (matched != negated).then_some(i + 1)
}

/// What a directory entry is, without following a link.
enum Kind {
    File(u64),
    Dir,
    Other,
}

fn kind_of(path: &Path) -> Kind {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Kind::Other,
        Ok(meta) if meta.is_file() => Kind::File(meta.len()),
        Ok(meta) if meta.is_dir() => Kind::Dir,
        _ => Kind::Other,
    }
}

#[cfg(windows)]
fn is_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// Sorted names of the entries of `dir` that are valid Unicode.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

struct Walk {
    found: BTreeMap<String, Found>,
    /// Stop collecting once this many files were found.
    limit: usize,
}

impl Walk {
    fn full(&self) -> bool {
        self.found.len() >= self.limit
    }

    fn add(&mut self, rel: String, path: PathBuf, size: u64) {
        if !self.full() {
            self.found
                .entry(rel.clone())
                .or_insert(Found { rel, path, size });
        }
    }

    /// Everything below `dir`.
    fn all(&mut self, dir: &Path, rel: &str) {
        for name in entries(dir) {
            if self.full() {
                return;
            }
            let path = dir.join(&name);
            let child = join_rel(rel, &name);
            match kind_of(&path) {
                Kind::File(size) => self.add(child, path, size),
                Kind::Dir => self.all(&path, &child),
                Kind::Other => {}
            }
        }
    }

    fn entry(&mut self, path: PathBuf, rel: String, rest: &[Part]) {
        match kind_of(&path) {
            Kind::File(size) if rest.is_empty() => self.add(rel, path, size),
            Kind::Dir => self.visit(&path, &rel, rest),
            _ => {}
        }
    }

    /// Match `parts` against what lies below `dir`.
    fn visit(&mut self, dir: &Path, rel: &str, parts: &[Part]) {
        if self.full() {
            return;
        }
        let Some((first, rest)) = parts.split_first() else {
            // The pattern named this directory: take everything in it.
            self.all(dir, rel);
            return;
        };
        match first {
            Part::Literal(name) => self.entry(dir.join(name), join_rel(rel, name), rest),
            Part::Wild(pattern) => {
                for name in entries(dir) {
                    let chars: Vec<char> = name.chars().collect();
                    if name_matches(pattern, &chars) {
                        self.entry(dir.join(&name), join_rel(rel, &name), rest);
                    }
                }
            }
            Part::AnyDirs => {
                // Zero directories, then one more level with `**` kept.
                self.visit(dir, rel, rest);
                for name in entries(dir) {
                    let path = dir.join(&name);
                    if matches!(kind_of(&path), Kind::Dir) {
                        self.visit(&path, &join_rel(rel, &name), parts);
                    }
                }
            }
        }
    }
}

/// Expand `patterns` below `root`. Returns the files sorted by relative
/// path, each once, at most `limit` of them, and the patterns that were
/// refused.
pub(crate) fn expand(
    root: &Path,
    patterns: &[String],
    limit: usize,
) -> (Vec<Found>, Vec<PatternError>) {
    let mut walk = Walk {
        found: BTreeMap::new(),
        limit,
    };
    let mut errors = Vec::new();
    for pattern in patterns {
        match parse(pattern) {
            Ok(parts) => walk.visit(root, "", &parts),
            Err(reason) => errors.push(PatternError {
                pattern: pattern.clone(),
                reason,
            }),
        }
    }
    (walk.found.into_values().collect(), errors)
}

/// Bytes of all regular files below `root`, without following links.
/// Entries that vanish while being measured count as nothing.
pub(crate) fn tree_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(reader) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            match kind_of(&path) {
                Kind::File(size) => total = total.saturating_add(size),
                Kind::Dir => pending.push(path),
                Kind::Other => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, name: &str) -> bool {
        let p: Vec<char> = pattern.chars().collect();
        let n: Vec<char> = name.chars().collect();
        name_matches(&p, &n)
    }

    #[test]
    fn single_component_patterns() {
        assert!(matches("*.xml", "junit.xml"));
        assert!(!matches("*.xml", "junit.xml.bak"));
        assert!(matches("a*b*c", "a--b--c"));
        assert!(!matches("a*b*c", "a--b--"));
        assert!(matches("?.txt", "a.txt"));
        assert!(!matches("?.txt", "ab.txt"));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[a-c]x", "dx"));
        assert!(matches("[!a-c]x", "dx"));
        assert!(matches("*", ""));
        assert!(matches("[", "["));
    }

    fn touch(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn rels(found: &[Found]) -> Vec<&str> {
        found.iter().map(|f| f.rel.as_str()).collect()
    }

    #[test]
    fn expands_globs_directories_and_double_star() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "target/release/app", 3);
        touch(root, "dist/a.js", 1);
        touch(root, "dist/sub/b.js", 2);
        touch(root, "junit.xml", 4);
        touch(root, "crates/x/junit.xml", 5);
        touch(root, "crates/x/other.txt", 6);

        let (found, errors) = expand(root, &["target/release/app".into()], 100);
        assert!(errors.is_empty());
        assert_eq!(rels(&found), ["target/release/app"]);
        assert_eq!(found[0].size, 3);

        let (found, _) = expand(root, &["dist/**".into()], 100);
        assert_eq!(rels(&found), ["dist/a.js", "dist/sub/b.js"]);

        let (found, _) = expand(root, &["dist".into()], 100);
        assert_eq!(rels(&found), ["dist/a.js", "dist/sub/b.js"]);

        let (found, _) = expand(root, &["**/junit.xml".into()], 100);
        assert_eq!(rels(&found), ["crates/x/junit.xml", "junit.xml"]);

        let (found, _) = expand(root, &["dist/*.js".into(), "dist/a.js".into()], 100);
        assert_eq!(rels(&found), ["dist/a.js"]);

        let (found, _) = expand(root, &["**".into()], 2);
        assert_eq!(found.len(), 2);

        assert_eq!(tree_bytes(root), 21);
    }

    #[test]
    fn refuses_patterns_that_leave_the_root() {
        let dir = tempfile::tempdir().unwrap();
        for pattern in ["../x", "/etc/passwd", "a/../../b", "a\\b", "."] {
            let (found, errors) = expand(dir.path(), &[pattern.to_string()], 10);
            assert!(found.is_empty(), "{pattern}");
            assert_eq!(errors.len(), 1, "{pattern}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_neither_followed_nor_collected() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "out/real.txt", 1);
        std::os::unix::fs::symlink(outside.path(), root.join("out/escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("out/link.txt"))
            .unwrap();

        let (found, _) = expand(root, &["out/**".into(), "out/escape/secret".into()], 100);
        assert_eq!(rels(&found), ["out/real.txt"]);
        assert_eq!(tree_bytes(root), 1);
    }
}
