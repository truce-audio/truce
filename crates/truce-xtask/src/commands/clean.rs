//! `cargo truce clean` — clean cargo build artifacts.
//!
//! Thin wrapper over `cargo clean`. Wipes the entire `target/` tree,
//! which covers shim builds, AAX template, AU v3 staging, and the
//! `target/dist/` installer output from `cargo truce package`.
//!
//! Does not touch installed bundles in system plugin paths (see
//! `cargo truce remove`) and does not flush AU / AAX host caches
//! (see `cargo truce reset-au-aax`). Both side-effects are loud
//! enough that they belong behind explicit commands.

use std::process::Command;

use crate::{project_root, Res};

pub(crate) fn cmd_clean(args: &[String]) -> Res {
    if let Some(other) = args.first() {
        return Err(format!("Unknown flag: {other}").into());
    }

    let root = project_root();
    let status = Command::new("cargo")
        .arg("clean")
        .current_dir(&root)
        .status()?;
    if !status.success() {
        return Err(format!("cargo clean exited with {status}").into());
    }
    Ok(())
}
