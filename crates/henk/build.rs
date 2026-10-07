//! Embeds the dashboard app (#199): every file of `dashboard/dist`, as
//! `npm run build` leaves it, becomes a `(path, bytes)` entry of the table
//! `dashboard::app` serves. Without a `dist` the table is empty and Henk
//! says the app was not built, so `cargo build` and `cargo test` never need
//! Node.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn main() -> io::Result<()> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = manifest.join("../../dashboard/dist");
    println!("cargo:rerun-if-changed=build.rs");
    // A watched directory counts with everything in it. A missing path
    // counts as changed on every build, so without a dist the watch is on
    // dashboard/ itself, which is in the repository: a first `npm run
    // build` is noticed, and a build without Node does not rebuild henk
    // every time.
    let watched = if dist.is_dir() {
        dist.clone()
    } else {
        manifest.join("../../dashboard")
    };
    println!("cargo:rerun-if-changed={}", watched.display());

    let mut files = Vec::new();
    if dist.is_dir() {
        collect(&dist, &dist, &mut files)?;
    }
    files.sort();

    let mut table = String::from("&[\n");
    for (name, path) in &files {
        let _ = writeln!(table, "    ({name:?}, include_bytes!({path:?})),");
    }
    table.push(']');
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    fs::write(out.join("dashboard_assets.rs"), table)
}

/// Every file under `dir`, as its path relative to `root` with `/`
/// between the parts, and its absolute path.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            let name = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let absolute = fs::canonicalize(&path)?;
            files.push((name, absolute.to_string_lossy().into_owned()));
        }
    }
    Ok(())
}
