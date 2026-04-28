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
                    i += 1;
                    plugin_filter = Some(
                        args.get(i)
                            .cloned()
                            .ok_or("-p requires a plugin crate name")?,
                    );
                }
                "--no-build" => no_build = true,
                "--debug" => debug = true,
                "--" => past_separator = true,
                other => return Err(format!("unknown flag: {other}").into()),
            }
        }
        i += 1;
    }

    crate::set_debug_profile(debug);

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

        // On macOS, wrap the binary in a `.app` bundle so the OS
        // treats the standalone as a proper application: shows in
        // the Dock with the plugin's name, attributes the menu bar
        // we install via `truce-standalone::menu_macos` to it, and
        // surfaces a plugin-specific `NSMicrophoneUsageDescription`
        // when the user enables mic capture for the first time.
        // Other platforms get the bare binary as before.
        #[cfg(target_os = "macos")]
        stage_macos_app_bundle(&built, &staged, plugin, &config.vendor)?;
        #[cfg(not(target_os = "macos"))]
        {
            fs_ctx::copy(&built, &staged)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exec_path = exec_path_inside_stage(&staged, plugin);
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

    let exec_path = exec_path_inside_stage(&staged, plugin);
    eprintln!("Running {}...", exec_path.display());
    let status = Command::new(&exec_path).args(&extra_args).status()?;

    if !status.success() {
        return Err(format!("{} exited with {status}", exec_path.display()).into());
    }
    Ok(())
}

/// Build a `.app` bundle layout around the standalone binary.
///
///     <staged>.app/Contents/MacOS/<binary>
///     <staged>.app/Contents/Info.plist
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
    vendor: &crate::config::VendorConfig,
) -> Res {
    // staged is `<bundles>/<Plugin>.standalone.app/`. Ensure a
    // clean re-stage on each run so stale bundle contents don't
    // linger between iterations.
    let _ = std::fs::remove_dir_all(staged);
    let contents = staged.join("Contents");
    let macos = contents.join("MacOS");
    fs_ctx::create_dir_all(&macos)?;

    let exe_name = standalone_bin_name(&plugin.bundle_id);
    fs_ctx::copy(built, macos.join(&exe_name))?;

    // Microphone usage description is plugin-specific so the
    // permission dialog reads "<Plugin> wants to use the
    // microphone" instead of a generic system message.
    let mic_usage = format!(
        "{} would like to use the microphone for plugin audio input.",
        plugin.name
    );

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundleDisplayName</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.standalone</string>
    <key>CFBundleExecutable</key>
    <string>{exe}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>{mic_usage}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.music</string>
</dict>
</plist>
"#,
        name = plugin.name,
        vendor_id = vendor.id,
        bundle_id = plugin.bundle_id,
        exe = exe_name,
        mic_usage = mic_usage,
    );
    fs_ctx::write(contents.join("Info.plist"), plist)?;

    Ok(())
}

/// On macOS the staged path is `<Plugin>.standalone.app/`; the
/// real binary lives at `Contents/MacOS/<exe>`. Other platforms,
/// staged IS the binary.
fn exec_path_inside_stage(
    staged: &std::path::Path,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    plugin: &crate::config::PluginDef,
) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        staged
            .join("Contents")
            .join("MacOS")
            .join(standalone_bin_name(&plugin.bundle_id))
    }
    #[cfg(not(target_os = "macos"))]
    {
        staged.to_path_buf()
    }
}

/// Cargo's output path for the standalone binary. Tracks the active
/// build profile so `--debug` finds the bin under `target/debug/`.
fn standalone_built_path(root: &std::path::Path, bundle_id: &str) -> PathBuf {
    let profile = if crate::is_debug_profile() {
        "debug"
    } else {
        "release"
    };
    crate::target_dir(root)
        .join(profile)
        .join(standalone_bin_name(bundle_id))
}

fn standalone_bin_name(bundle_id: &str) -> String {
    if cfg!(windows) {
        format!("{bundle_id}-standalone.exe")
    } else {
        format!("{bundle_id}-standalone")
    }
}

/// Staged name inside `target/bundles/` — keeps the standalone
/// distinct from plugin bundles like `{Plugin Name}.clap` /
/// `.vst3`. On macOS the `.app` suffix triggers Finder + the OS
/// to recognize the directory as a proper application bundle.
fn standalone_bundle_name(plugin_name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("{plugin_name}.standalone.app")
    } else if cfg!(windows) {
        format!("{plugin_name}.standalone.exe")
    } else {
        format!("{plugin_name}.standalone")
    }
}
