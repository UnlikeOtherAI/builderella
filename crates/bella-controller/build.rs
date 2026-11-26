//! Embeds the built dashboard, when there is one.
//!
//! `dashboard/dist` is produced by `npm run build` and is not checked in.
//! This script lists its files into `$OUT_DIR/dashboard_assets.rs` as
//! `include_bytes!` entries. Without a built dashboard the list is empty and
//! the controller serves a placeholder page, so the crate builds on a
//! machine that has no Node.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=BELLA_DASHBOARD_DIST");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dashboard = manifest_dir.join("..").join("..").join("dashboard");
    let dist = env::var_os("BELLA_DASHBOARD_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| dashboard.join("dist"));

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        // Cargo scans the directory, so a rebuilt dashboard is embedded anew.
        println!("cargo::rerun-if-changed={}", dist.display());
        collect(&dist, &dist, &mut files)?;
        files.sort();
    } else if dashboard.is_dir() {
        // Watching a path that does not exist would rerun this script on
        // every build. The dashboard directory exists, and creating `dist`
        // inside it changes it.
        println!("cargo::rerun-if-changed={}", dashboard.display());
    }

    let mut source =
        String::from("/// Files of the built dashboard, by path relative to its root.\n");
    source.push_str("pub(crate) static ASSETS: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        let path = path.canonicalize()?;
        let _ = writeln!(
            source,
            "    ({name:?}, include_bytes!({:?}) as &[u8]),",
            path.to_string_lossy()
        );
    }
    source.push_str("];\n");

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    fs::write(out.join("dashboard_assets.rs"), source)
}
