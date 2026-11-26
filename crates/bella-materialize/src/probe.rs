//! Probing what a volume does with names.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether names that differ only in letter case are different files in
/// `dir`.
///
/// The answer comes from the volume itself: a uniquely named empty file is
/// created in `dir`, looked up under the other letter case and removed
/// again. `dir` must exist and be writable. Case sensitivity can be a
/// per-directory setting, so probe the directory the tree will live in.
pub fn is_case_sensitive(dir: &Path) -> io::Result<bool> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let unique = format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let lower = dir.join(format!(".bella-case-probe-{unique}"));
    let upper = dir.join(format!(".BELLA-CASE-PROBE-{unique}"));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lower)?;
    let folded = match fs::symlink_metadata(&upper) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    };
    let removed = fs::remove_file(&lower);
    let folded = folded?;
    removed?;
    Ok(!folded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a28_probe_matches_the_volume_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let probed = is_case_sensitive(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        fs::write(dir.path().join("name"), b"x").unwrap();
        let other_case_exists = dir.path().join("NAME").exists();
        assert_eq!(probed, !other_case_exists);
        // Asking again gives the same answer.
        assert_eq!(is_case_sensitive(dir.path()).unwrap(), probed);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_case_sensitive(&dir.path().join("absent")).is_err());
    }
}
