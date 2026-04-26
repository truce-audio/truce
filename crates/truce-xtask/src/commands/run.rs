//! `cargo truce run` — build a plugin's `--features standalone` binary,
//! stage it into `target/bundles/`, and launch it from there.
//!
//! The staging step keeps every truce-produced artifact in one
//! directory: whatever `build` / `install` / `package` consume lives
//! alongside the standalone executable. `cargo clean` sweeps it.

use crate::util::fs_ctx;
use crate::{cargo_build, deployment_target, load_config, project_root, Res};
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn cmd_run(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut no_build = false;
    let mut extra_args: Vec<String> = Vec::new();
    let mut past_separator = false;
    let mut i = 0;
    while i < args.len() {
        if past_separator {
            extra_args.push(args[i].clone());
        } else {
            match args[i].as_str() {
                "-p" => {
                    i += 1;
                    plugin_filter = Some(
                        args.get(i)
                            .cloned()
                            .ok_or("-p requires a plugin crate name")?,
                    );
                }
                "--no-build" => no_build = true,
                "--" => past_separator = true,
                other => return Err(format!("unknown flag: {other}").into()),
            }
        }
        i += 1;
    }

    let plugin = if let Some(ref f) = plugin_filter {
        config
            .plugin
            .iter()
            .find(|p| p.crate_name == *f)
            .ok_or_else(|| {
                format!(
                    "No plugin with crate name '{f}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| p.crate_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
    } else {
        config.plugin.first().ok_or("no plugins in truce.toml")?
    };

    let bundles_dir = crate::target_dir(&root).join("bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;
    let staged = bundles_dir.join(standalone_bundle_name(&plugin.name));

    if !no_build {
        eprintln!("Building {} standalone...", plugin.name);
        cargo_build(
            &[],
            &["-p", &plugin.crate_name, "--features", "standalone"],
            dt,
        )?;

        let built = standalone_built_path(&root, &plugin.bundle_id);
        if !built.exists() {
            let bin_name = standalone_bin_name(&plugin.bundle_id);
            return Err(format!(
                "standalone binary not found at {}. \
                 Does your plugin have a [[bin]] target named '{bin_name}'?",
                built.display()
            )
            .into());
        }
        fs_ctx::copy(&built, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&staged)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&staged, perms)?;
        }
    }

    if !staged.exists() {
        return Err(format!(
            "standalone bundle missing at {}. Drop `--no-build` to build it.",
            staged.display()
        )
        .into());
    }

    eprintln!("Running {}...", staged.display());
    let status = Command::new(&staged).args(&extra_args).status()?;

    if !status.success() {
        return Err(format!("{} exited with {status}", staged.display()).into());
    }
    Ok(())
}

/// Cargo's output path for the standalone binary inside `target/release/`.
fn standalone_built_path(root: &std::path::Path, bundle_id: &str) -> PathBuf {
    crate::target_dir(&root).join("release")
        .join(standalone_bin_name(bundle_id))
}

fn standalone_bin_name(bundle_id: &str) -> String {
    if cfg!(windows) {
        format!("{bundle_id}-standalone.exe")
    } else {
        format!("{bundle_id}-standalone")
    }
}

/// Staged name inside `target/bundles/` — `.standalone` (or
/// `.standalone.exe` on Windows) suffix keeps it distinct from
/// plugin bundles like `{Plugin Name}.clap` or `.vst3`.
fn standalone_bundle_name(plugin_name: &str) -> String {
    if cfg!(windows) {
        format!("{plugin_name}.standalone.exe")
    } else {
        format!("{plugin_name}.standalone")
    }
}
