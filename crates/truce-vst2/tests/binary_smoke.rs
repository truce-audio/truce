//! `truce-vst2` binary-surface smoke test.
//!
//! Builds `truce-example-block-gain` with the VST2 feature enabled,
//! compiles the C smoke harness at
//! `crates/truce-vst2/validate/binary_smoke.c`, and runs the harness
//! against the resulting `.dylib`. The harness exits 0 on success and
//! non-zero on any `AEffect`-surface assertion failure (magic, I/O
//! channel counts, flags, processReplacing output, state
//! save/load, ...).
//!
//! Previously this lived under `cargo truce validate --vst2`, but the
//! C source and the per-plugin loop are framework-internal: a
//! downstream plugin author has nothing to do with this test and the
//! validate command shouldn't even mention it. Moving here makes the
//! smoke a regular `cargo test -p truce-vst2` integration test - it
//! runs in this repo's CI alongside every other workspace test and
//! disappears from the plugin-author-facing CLI surface.
//!
//! `dlfcn.h` + the `.dylib` extension keep this macOS-only; that
//! matches the C harness's own assumptions.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the workspace root from `CARGO_MANIFEST_DIR` (this crate is
/// at `<root>/crates/truce-vst2`).
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR has at least two ancestors")
        .to_path_buf()
}

#[test]
fn block_gain_vst2_binary_smoke() {
    let root = workspace_root();
    let manifest = root.join("crates/truce-vst2/Cargo.toml");
    assert!(
        manifest.exists(),
        "truce-vst2/Cargo.toml not at {}",
        manifest.display()
    );

    // 1. Build the example plugin with VST2 enabled. Release profile
    // matches what `cargo truce install` ships, and pluginval-style
    // host code paths under release-mode optimisation tend to surface
    // bugs that debug-mode UB-checking hides.
    let cargo_status = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args([
            "build",
            "--release",
            "--no-default-features",
            "--features",
            "vst2",
            "-p",
            "truce-example-block-gain",
        ])
        .status()
        .expect("invoking cargo build for truce-example-block-gain");
    assert!(cargo_status.success(), "cargo build for block-gain failed");

    let target_dir = root.join("target/release");
    let dylib = target_dir.join("libtruce_example_block_gain.dylib");
    assert!(
        dylib.exists(),
        "VST2 dylib not produced at {}",
        dylib.display()
    );

    // 2. Compile the C smoke harness against the shim's header set.
    // The harness includes `../shim/vst2_types.h` (relative to its
    // own location), so leaving the source where it is keeps that
    // relative path valid.
    let smoke_src = root.join("crates/truce-vst2/validate/binary_smoke.c");
    let smoke_bin = target_dir.join("truce_vst2_binary_smoke");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let cc_status = Command::new(&cc)
        .args([
            "-o",
            smoke_bin.to_str().unwrap(),
            smoke_src.to_str().unwrap(),
            "-ldl",
        ])
        .status()
        .expect("invoking cc for binary_smoke.c");
    assert!(cc_status.success(), "compiling binary_smoke.c failed");

    // 3. Run the harness against the just-built dylib. `block-gain`
    // is a stereo effect, so no kind flag.
    let smoke = Command::new(&smoke_bin)
        .arg(&dylib)
        .output()
        .expect("invoking binary_smoke harness");
    let stdout = String::from_utf8_lossy(&smoke.stdout);
    let stderr = String::from_utf8_lossy(&smoke.stderr);
    assert!(
        smoke.status.success(),
        "binary_smoke failed for block-gain.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
