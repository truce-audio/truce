//! `cargo truce nuke` — nuclear reset for stale AU v3 appex cache.

use crate::{dirs, load_config, run_sudo, run_sudo_silent, tmp_dir, PluginDef, Res};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn cmd_nuke(args: &[String]) -> Res {
    let config = load_config()?;

    let mut plugin_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err("-p requires a plugin crate name".into());
                }
                plugin_filter = Some(args[i].clone());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    let plugins: Vec<&PluginDef> = if let Some(ref filter) = plugin_filter {
        let matched: Vec<_> = config
            .plugin
            .iter()
            .filter(|p| p.crate_name == *filter)
            .collect();
        if matched.is_empty() {
            return Err(format!(
                "No plugin with crate name '{filter}'. Available: {}",
                config
                    .plugin
                    .iter()
                    .map(|p| p.crate_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
        matched
    } else {
        config.plugin.iter().collect()
    };

    // 1. Unregister from LaunchServices + pluginkit
    eprintln!("Unregistering AU v3 plugins...");
    for p in &plugins {
        let app_dir = format!("/Applications/{}.app", p.au3_app_name());
        // Unregister from LaunchServices
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .args(["-u", &app_dir])
            .output();
        // Full remove from pluginkit (not just disable)
        let vid = config.vendor.id.trim_start_matches("com.");
        for pattern in [
            format!("com.{}.{}.v3.ext", vid, p.bundle_id),
            format!("com.{}.{}.au", vid, p.bundle_id),
        ] {
            let _ = Command::new("pluginkit")
                .args(["-r", "-i", &pattern])
                .output();
        }
        // Remove the app bundle
        if Path::new(&app_dir).exists() {
            let _ = run_sudo("rm", &["-rf", &app_dir]);
            eprintln!("  Removed: {app_dir}");
        }
    }

    // 2. Kill daemons
    eprintln!("Killing audio daemons...");
    run_sudo_silent("killall", &["-9", "pkd"]);
    run_sudo_silent("killall", &["-9", "AudioComponentRegistrar"]);

    // 4. Clear all caches
    eprintln!("Clearing all caches...");
    let home = dirs::home_dir().unwrap();
    for dir in [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ] {
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
            eprintln!("  Removed: {}", dir.display());
        }
    }

    // 5. Clean AU v3 temp dirs
    if let Ok(entries) = fs::read_dir(tmp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("au_v3_build_") || name.starts_with("au_v3_fw_") {
                let _ = fs::remove_dir_all(entry.path());
                eprintln!("  Removed: {}", entry.path().display());
            }
        }
    }

    // 6. Cargo clean
    eprintln!("Running cargo clean...");
    let status = Command::new("cargo").arg("clean").status()?;
    if !status.success() {
        eprintln!("  cargo clean failed");
    }

    eprintln!("\nNuke complete. Wait a few seconds, then run:");
    eprintln!("  cargo xtask install --au3");
    Ok(())
}
