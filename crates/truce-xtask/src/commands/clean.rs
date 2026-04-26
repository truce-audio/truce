//! `cargo truce clean` — clean cargo build artifacts, preserving
//! `target/dist/` (signed/notarized installers) by default.
//!
//! Notarized `.pkg` files take 5–10 minutes to produce, so the
//! default behavior is to stash `target/dist/` aside, run
//! `cargo clean`, and restore it. Pass `--all` to wipe `target/dist/`
//! as well — equivalent to a bare `cargo clean`.
//!
//! Does not touch installed bundles in system plugin paths (see
//! `cargo truce remove`) and does not flush AU / AAX host caches
//! (see `cargo truce reset-au` and `cargo truce reset-aax`). Both
//! side-effects are loud enough that they belong behind explicit
//! commands.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::{project_root, Res};

pub(crate) fn cmd_clean(args: &[String]) -> Res {
    let mut clean_installers = false;
    for a in args {
        match a.as_str() {
            "--all" => clean_installers = true,
            other => return Err(format!("Unknown flag: {other}").into()),
        }
    }

    let root = project_root();
    let dist = crate::target_dir(&root).join("dist");
    let stash = root.join(".truce-dist-stash");

    let stashed = !clean_installers && dist.exists();
    if stashed {
        if stash.exists() {
            return Err(format!(
                "stash dir already exists at {} — likely from an earlier \
                 interrupted `cargo truce clean`. Move its contents back \
                 to `target/dist/` (or delete it) and retry.",
                stash.display()
            )
            .into());
        }
        fs::rename(&dist, &stash)?;
    }

    let cargo_status = Command::new("cargo")
        .arg("clean")
        .current_dir(&root)
        .status();

    if stashed {
        let restored = restore_dist(&root, &stash, &dist);
        // Surface the restore error only if cargo clean itself succeeded;
        // otherwise the cargo error is more useful.
        if let Err(e) = restored {
            if matches!(&cargo_status, Ok(s) if s.success()) {
                return Err(e);
            }
        }
    }

    let status = cargo_status?;
    if !status.success() {
        return Err(format!("cargo clean exited with {status}").into());
    }
    Ok(())
}

fn restore_dist(root: &Path, stash: &Path, dist: &Path) -> Res {
    fs::create_dir_all(root.join("target"))?;
    fs::rename(stash, dist)?;
    Ok(())
}
