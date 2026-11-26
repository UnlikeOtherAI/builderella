//! Read-only use of the `git` command line.
//!
//! Only commands that leave the repository untouched are run: `rev-parse`,
//! `ls-files`, `check-attr`, `config --get`, `symbolic-ref` and `status`.
//! `GIT_OPTIONAL_LOCKS=0` keeps `status` from refreshing the index on disk.

use crate::error::CaptureError;
use bella_core::snapshot::GitProvenance;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Index mode of a submodule entry.
pub(crate) const MODE_GITLINK: u32 = 0o160000;
/// Index mode of a symbolic link.
pub(crate) const MODE_SYMLINK: u32 = 0o120000;
/// Index mode of an executable file.
pub(crate) const MODE_EXECUTABLE: u32 = 0o100755;

/// Variables that would point git at a repository other than the one the
/// directory belongs to (set, for example, while a git hook runs).
const REPOSITORY_OVERRIDES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

pub(crate) struct Finished {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for name in REPOSITORY_OVERRIDES {
        cmd.env_remove(name);
    }
    cmd
}

fn attempt(dir: &Path, args: &[&str], input: Option<&[u8]>) -> io::Result<Finished> {
    let mut cmd = command(dir, args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = match input {
        None => cmd.stdin(Stdio::null()).output()?,
        Some(input) => {
            let mut child = cmd.stdin(Stdio::piped()).spawn()?;
            let stdin = child.stdin.take();
            // Feed stdin from a second thread so a full pipe in either
            // direction cannot stall the exchange.
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    if let Some(mut stdin) = stdin {
                        // A failed write shows up as git's own exit status.
                        let _ = stdin.write_all(input);
                    }
                });
                child.wait_with_output()
            })?
        }
    };
    Ok(Finished {
        ok: output.status.success(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn unavailable(dir: &Path, error: io::Error) -> CaptureError {
    CaptureError::GitUnavailable {
        dir: dir.display().to_string(),
        reason: error.to_string(),
    }
}

fn failed(dir: &Path, args: &[&str], done: &Finished) -> CaptureError {
    CaptureError::Git {
        command: args.join(" "),
        dir: dir.display().to_string(),
        detail: if done.stderr.is_empty() {
            "no error output".into()
        } else {
            done.stderr.clone()
        },
    }
}

/// Run a query whose non-zero exit is an answer, not a failure.
pub(crate) fn query(dir: &Path, args: &[&str]) -> Result<Finished, CaptureError> {
    attempt(dir, args, None).map_err(|e| unavailable(dir, e))
}

/// Run a command that must succeed and return its standard output.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<Vec<u8>, CaptureError> {
    let done = query(dir, args)?;
    if done.ok {
        Ok(done.stdout)
    } else {
        Err(failed(dir, args, &done))
    }
}

fn has_git_ancestor(dir: &Path) -> bool {
    let start = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    start
        .ancestors()
        .any(|a| a.join(".git").symlink_metadata().is_ok())
}

/// Whether `dir` is inside a git working tree.
///
/// A directory with no `.git` above it is a plain directory even when git is
/// not installed. A directory that does have one must be readable by git:
/// falling back to a plain walk there would ignore the repository's ignore
/// rules.
pub(crate) fn is_work_tree(dir: &Path) -> Result<bool, CaptureError> {
    let args = ["rev-parse", "--show-toplevel"];
    match attempt(dir, &args, None) {
        Ok(done) if done.ok => Ok(true),
        Ok(done) if has_git_ancestor(dir) => Err(failed(dir, &args, &done)),
        Err(error) if has_git_ancestor(dir) => Err(unavailable(dir, error)),
        Ok(_) | Err(_) => Ok(false),
    }
}

/// A boolean configuration value, or `None` when it is unset.
pub(crate) fn config_bool(dir: &Path, key: &str) -> Result<Option<bool>, CaptureError> {
    let done = query(dir, &["config", "--bool", "--get", key])?;
    if !done.ok {
        return Ok(None);
    }
    Ok(match String::from_utf8_lossy(&done.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    })
}

/// Split NUL-terminated records. Names keep every other byte, including
/// spaces, quotes and newlines.
pub(crate) fn split_nul(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IndexEntry<'a> {
    pub path: &'a [u8],
    pub mode: u32,
    pub skip_worktree: bool,
}

/// Parse `git ls-files -z -s -t`: `<tag> <mode> <object> <stage>\t<path>`.
pub(crate) fn parse_index(output: &[u8]) -> Result<Vec<IndexEntry<'_>>, String> {
    let mut entries = Vec::new();
    for record in split_nul(output) {
        let bad = || format!("unexpected record `{}`", String::from_utf8_lossy(record));
        let tab = record.iter().position(|b| *b == b'\t').ok_or_else(bad)?;
        let fields: Vec<&[u8]> = record[..tab].split(|b| *b == b' ').collect();
        let (tag, mode) = match fields.as_slice() {
            [tag, mode, _object, _stage] => (*tag, *mode),
            [mode, _object, _stage] => (&b"H"[..], *mode),
            _ => return Err(bad()),
        };
        let mode = std::str::from_utf8(mode)
            .ok()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .ok_or_else(bad)?;
        entries.push(IndexEntry {
            path: &record[tab + 1..],
            mode,
            skip_worktree: tag.eq_ignore_ascii_case(b"S"),
        });
    }
    Ok(entries)
}

/// Whether bytes are a Git LFS pointer rather than the object itself.
pub(crate) fn is_lfs_pointer(bytes: &[u8]) -> bool {
    const CURRENT: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";
    const LEGACY: &[u8] = b"version https://hawser.github.com/spec/v1\n";
    const OID: &[u8] = b"\noid sha256:";
    bytes.len() <= LFS_POINTER_MAX_BYTES as usize
        && (bytes.starts_with(CURRENT) || bytes.starts_with(LEGACY))
        && bytes.windows(OID.len()).any(|w| w == OID)
}

/// Pointer files are small; anything larger is real content.
pub(crate) const LFS_POINTER_MAX_BYTES: u64 = 1024;

/// Whether the `filter` attribute of `path` (relative to `dir`) is `lfs`.
pub(crate) fn has_lfs_filter(dir: &Path, path: &str) -> Result<bool, CaptureError> {
    let args = ["check-attr", "filter", "-z", "--stdin"];
    let mut input = path.as_bytes().to_vec();
    input.push(0);
    let done = attempt(dir, &args, Some(&input)).map_err(|e| unavailable(dir, e))?;
    if !done.ok {
        return Err(failed(dir, &args, &done));
    }
    // One record: `<path>\0<attribute>\0<value>\0`.
    let value = done.stdout.split(|b| *b == 0).nth(2);
    Ok(value == Some(&b"lfs"[..]))
}

/// Remove credentials from a remote URL. Any user name or token is dropped
/// from `http(s)` URLs; other schemes keep the user name, which is part of
/// the address, and lose only the password.
pub(crate) fn strip_credentials(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let scheme = &url[..scheme_end];
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let (userinfo, host) = (&authority[..at], &authority[at + 1..]);
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    let user = if web {
        ""
    } else {
        userinfo.split(':').next().unwrap_or("")
    };
    if user.is_empty() {
        format!("{scheme}://{host}{tail}")
    } else {
        format!("{scheme}://{user}@{host}{tail}")
    }
}

fn text(done: Finished) -> Option<String> {
    let value = String::from_utf8_lossy(&done.stdout).trim().to_string();
    (done.ok && !value.is_empty()).then_some(value)
}

/// Where a working tree stood when it was captured. `dirty` covers the
/// directory being captured, not the rest of the repository.
pub(crate) fn provenance(dir: &Path, with_untracked: bool) -> Result<GitProvenance, CaptureError> {
    let head = text(query(dir, &["rev-parse", "--verify", "--quiet", "HEAD"])?);
    let branch = text(query(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?);
    let untracked = if with_untracked {
        "--untracked-files=normal"
    } else {
        "--untracked-files=no"
    };
    let status = run(dir, &["status", "--porcelain", "-z", untracked, "--", "."])?;
    let remote_url = text(query(dir, &["config", "--get", "remote.origin.url"])?)
        .map(|url| strip_credentials(&url));
    Ok(GitProvenance {
        head,
        branch,
        dirty: !status.is_empty(),
        remote_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a28: NUL-separated output keeps spaces, quotes, newlines and
    /// non-ASCII bytes of a name intact.
    #[test]
    fn a28_index_records_keep_unusual_names() {
        let output = b"H 100644 aaaa 0\tplain.txt\0\
            H 100755 bbbb 0\tdir with space/new\nline \"q\".sh\0\
            S 100644 cccc 0\t\xc3\xbcn\xc3\xaf.txt\0\
            H 160000 dddd 0\tvendor/lib\0\
            M 100644 eeee 1\tboth\tsides\0";
        let entries = parse_index(output).unwrap();
        let paths: Vec<&[u8]> = entries.iter().map(|e| e.path).collect();
        assert_eq!(
            paths,
            [
                &b"plain.txt"[..],
                b"dir with space/new\nline \"q\".sh",
                "ünï.txt".as_bytes(),
                b"vendor/lib",
                b"both\tsides",
            ]
        );
        assert_eq!(entries[1].mode, MODE_EXECUTABLE);
        assert!(entries[2].skip_worktree);
        assert!(!entries[0].skip_worktree);
        assert_eq!(entries[3].mode, MODE_GITLINK);
    }

    #[test]
    fn index_records_without_a_tag_are_accepted() {
        let entries = parse_index(b"120000 aaaa 0\tlink\0").unwrap();
        assert_eq!(entries[0].mode, MODE_SYMLINK);
        assert!(parse_index(b"garbage\0").is_err());
        assert!(parse_index(b"H notoctal aaaa 0\tx\0").is_err());
        assert!(parse_index(b"").unwrap().is_empty());
    }

    #[test]
    fn lfs_pointers_are_recognised() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\n\
            oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
            size 12345\n";
        assert!(is_lfs_pointer(pointer));
        assert!(!is_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\n"
        ));
        assert!(!is_lfs_pointer(b"just some text\n"));
        let mut large = pointer.to_vec();
        large.resize(2000, b' ');
        assert!(!is_lfs_pointer(&large));
    }

    #[test]
    fn credentials_are_stripped_from_remote_urls() {
        for (url, expected) in [
            (
                "https://user:token@example.com/org/repo.git",
                "https://example.com/org/repo.git",
            ),
            (
                "https://token@example.com/org/repo.git",
                "https://example.com/org/repo.git",
            ),
            (
                "HTTP://u:p@example.com:8080/x?y=1",
                "HTTP://example.com:8080/x?y=1",
            ),
            (
                "ssh://git:hunter2@example.com/org/repo.git",
                "ssh://git@example.com/org/repo.git",
            ),
            (
                "ssh://git@example.com/org/repo.git",
                "ssh://git@example.com/org/repo.git",
            ),
            (
                "https://example.com/a@b/repo.git",
                "https://example.com/a@b/repo.git",
            ),
            (
                "git@example.com:org/repo.git",
                "git@example.com:org/repo.git",
            ),
            ("../relative/path", "../relative/path"),
        ] {
            assert_eq!(strip_credentials(url), expected, "{url}");
        }
    }
}
