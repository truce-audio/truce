//! `cargo truce run` — build a plugin's `--features standalone` binary,
//! stage it into `target/bundles/`, and launch it from there.
//!
//! The staging step keeps every truce-produced artifact in one
//! directory: whatever `build` / `install` / `package` consume lives
//! alongside the standalone executable. `cargo clean` sweeps it.

use crate::util::fs_ctx;
use crate::{Res, cargo_build, deployment_target, load_config, project_root};
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn cmd_run(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut no_build = false;
    let mut debug = false;
    let mut extra_args: Vec<String> = Vec::new();
    let mut past_separator = false;
    let mut i = 0;
    while i < args.len() {
        if past_separator {
            extra_args.push(args[i].clone());
        } else {
            match args[i].as_str() {
                "-p" => {
                    plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
                }
                "--no-build" => no_build = true,
                "--debug" => debug = true,
                "--help" | "-h" => {
                    print_help();
                    return Ok(());
                }
                "--" => past_separator = true,
                other => return Err(format!("unknown flag: {other}").into()),
            }
        }
        i += 1;
    }

    crate::set_debug_profile(debug);

    let matched = super::pick_plugins(&config, plugin_filter.as_deref())?;
    let plugin = *matched.first().ok_or("no plugins in truce.toml")?;

    // Resolve the standalone `[[bin]]` name from the plugin's
    // `Cargo.toml` so hand-written manifests with non-conventional
    // bin names still work. Falls back to the scaffold convention
    // (`{crate_name}-standalone`) when the manifest can't be parsed.
    let bin_stem = crate::read_standalone_bin_name(&plugin.crate_name)
        .unwrap_or_else(|| format!("{}-standalone", plugin.crate_name));

    let bundles_dir = truce_build::target_dir(&root).join("bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;
    let staged = bundles_dir.join(standalone_bundle_name(&plugin.name));

    if !no_build {
        eprintln!("Building {} standalone...", plugin.name);
        cargo_build(
            &[],
            &["-p", &plugin.crate_name, "--features", "standalone"],
            dt,
        )?;

        let built = standalone_built_path(&root, &bin_stem);
        if !built.exists() {
            let bin_name = bin_filename(&bin_stem);
            return Err(format!(
                "standalone binary not found at {}. \
                 Does your plugin have a [[bin]] target named '{bin_name}'?",
                built.display()
            )
            .into());
        }

        // On macOS, wrap the binary in a `.app` bundle so the OS
        // treats the standalone as a proper application: shows in
        // the Dock with the plugin's name, attributes the menu bar
        // we install via `truce-standalone::menu_macos` to it, and
        // surfaces a plugin-specific `NSMicrophoneUsageDescription`
        // when the user enables mic capture for the first time.
        // Other platforms get the bare binary as before.
        #[cfg(target_os = "macos")]
        stage_macos_app_bundle(&built, &staged, plugin, &bin_stem, &config.vendor)?;
        #[cfg(not(target_os = "macos"))]
        {
            fs_ctx::copy(&built, &staged)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exec_path = exec_path_inside_stage(&staged, &bin_stem);
            let mut perms = std::fs::metadata(&exec_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&exec_path, perms)?;
        }
        // The standalone exe is parentless — without an embedded
        // application manifest declaring per-monitor v2 DPI awareness,
        // the editor renders blurry on non-100% Windows displays.
        #[cfg(target_os = "windows")]
        crate::windows_manifest::embed_dpi_manifest(&staged)?;
    }

    if !staged.exists() {
        return Err(format!(
            "standalone bundle missing at {}. Drop `--no-build` to build it.",
            staged.display()
        )
        .into());
    }

    let exec_path = exec_path_inside_stage(&staged, &bin_stem);
    eprintln!("Running {}...", exec_path.display());
    let status = Command::new(&exec_path).args(&extra_args).status()?;

    if !status.success() {
        return Err(format!("{} exited with {status}", exec_path.display()).into());
    }
    Ok(())
}

/// Build a `.app` bundle layout around the standalone binary.
///
/// ```text
/// <staged>.app/Contents/MacOS/<binary>
/// <staged>.app/Contents/Info.plist
/// ```
///
/// macOS treats the binary at `Contents/MacOS/<exe>` as the app's
/// principal executable when the parent `.app` directory and
/// `Info.plist` are present. The menu bar `truce-standalone`
/// installs via `NSApp.setMainMenu` then attributes correctly,
/// the Dock shows the plugin's name, and the system mic permission
/// dialog uses our `NSMicrophoneUsageDescription`.
#[cfg(target_os = "macos")]
fn stage_macos_app_bundle(
    built: &std::path::Path,
    staged: &std::path::Path,
    plugin: &crate::config::PluginDef,
    bin_stem: &str,
    vendor: &crate::config::VendorConfig,
) -> Res {
    // staged is `<bundles>/<Plugin>.app/`. Ensure a clean re-stage
    // on each run so stale bundle contents don't linger between
    // iterations.
    let _ = std::fs::remove_dir_all(staged);
    let macos = staged.join("Contents").join("MacOS");
    fs_ctx::create_dir_all(&macos)?;

    let exe_name = bin_filename(bin_stem);
    fs_ctx::copy(built, macos.join(&exe_name))?;

    crate::commands::package::stage::write_standalone_info_plist(staged, plugin, &exe_name, vendor)
}

/// On macOS the staged path is `<Plugin>.app/`; the real binary
/// lives at `Contents/MacOS/<exe>`. Other platforms, staged IS
/// the binary.
fn exec_path_inside_stage(
    staged: &std::path::Path,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] bin_stem: &str,
) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        staged
            .join("Contents")
            .join("MacOS")
            .join(bin_filename(bin_stem))
    }
    #[cfg(not(target_os = "macos"))]
    {
        staged.to_path_buf()
    }
}

/// Cargo's output path for the standalone binary. Tracks the active
/// build profile so `--debug` finds the bin under `target/debug/`.
///
/// `bin_stem` is the resolved `[[bin]] name` from the plugin's
/// `Cargo.toml` (see `read_standalone_bin_name`), with the
/// `{crate_name}-standalone` scaffold convention as the fallback.
fn standalone_built_path(root: &std::path::Path, bin_stem: &str) -> PathBuf {
    let profile = if crate::is_debug_profile() {
        "debug"
    } else {
        "release"
    };
    truce_build::target_dir(root)
        .join(profile)
        .join(bin_filename(bin_stem))
}

/// Append `.exe` on Windows so the cargo-output filename and the
/// staged-bundle filename round-trip through string equality.
fn bin_filename(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce run [-p <crate>] [--no-build] [--debug] [-- <args>]

Build and run a plugin standalone. Pass `--debug` for a faster-compile
dev-profile build (fine when iterating outside a DAW); release otherwise.

Anything after `--` is forwarded verbatim to the standalone binary
(e.g. `cargo truce run -- --headless --bpm 140`).

Options:
  -p <crate>       Build and run only the plugin with this cargo crate name.
  --no-build       Skip rebuild; run the existing staged binary.
  --debug          Cargo dev profile (faster compile).
  -h, --help       Show this message."
    );
}

/// Staged name inside `target/bundles/`. Kept in sync with the
/// install layout (`/Applications/{Plugin}.app` on macOS,
/// `<Vendor>\<Plugin>\<bin>.exe` on Windows) so dev iteration mirrors
/// what an end user gets. On macOS the `.app` suffix triggers Finder
/// and Launch Services to treat the directory as a proper application
/// (the historical `<Plugin>.standalone.app` name was confusing
/// Spotlight indexing).
fn standalone_bundle_name(plugin_name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("{plugin_name}.app")
    } else if cfg!(windows) {
        format!("{plugin_name}.standalone.exe")
    } else {
        format!("{plugin_name}.standalone")
    }
}
