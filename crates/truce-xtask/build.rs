//! Invalidate the crate's incremental cache whenever any embedded
//! template file changes.
//!
//! `src/templates.rs` pulls every file under `templates/` into the
//! binary via `include_str!`. Cargo's default rebuild tracking only
//! watches Rust sources, so a pure template edit (a `.swift` / `.h` /
//! `.cpp` / `.plist` touch) doesn't re-trigger `include_str!` — the
//! `cargo-truce install --au3` / `--aax` paths then ship stale bytes
//! to xcodebuild / cmake and any recent template fix silently falls
//! on the floor.
//!
//! Watching the template directories (recursive) fixes that. We also
//! watch this build script itself so editing the watch list triggers
//! a rebuild.
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    watch_dir(Path::new("templates"));
}

fn watch_dir(dir: &Path) {
    // `rerun-if-changed` on a directory tells cargo to watch the
    // directory itself (entries added/removed). We additionally
    // recurse so every file under the tree is watched for content
    // changes.
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            watch_dir(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
