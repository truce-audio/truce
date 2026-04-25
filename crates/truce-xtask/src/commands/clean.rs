//! `cargo truce clean` — clear AU/DAW caches and restart audio daemons.

use crate::{confirm_prompt, dirs, load_config, run_sudo_silent, tmp_dir, Res};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn cmd_clean(args: &[String]) -> Res {
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            other => return Err(format!("Unknown flag: {other}").into()),
        }
    }

    if !yes
        && !confirm_prompt(
            "Clear AU/DAW caches and restart audio daemons? This deletes cached plugin metadata, \
         resets pluginkit registrations, and wipes the AAX cache.",
        )
    {
        eprintln!("Cancelled.");
        return Ok(());
    }

    eprintln!("Clearing AU/DAW caches...");
    let home = dirs::home_dir().unwrap();

    // AU caches (system + sandboxed DAW containers)
    let cache_dirs = [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ];
    for dir in &cache_dirs {
        if dir.exists() {
            let _ = fs::remove_dir_all(dir);
            eprintln!("  Removed: {}", dir.display());
        }
    }

    // Audio preferences
    let prefs = [
        home.join("Library/Preferences/com.apple.audio.InfoHelper.plist"),
        home.join("Library/Preferences/com.apple.audio.SandboxHelper.plist"),
    ];
    for pref in &prefs {
        if pref.exists() {
            let _ = fs::remove_file(pref);
            eprintln!("  Removed: {}", pref.display());
        }
    }

    // Reaper AU cache
    let reaper_cache = home.join("Library/Application Support/REAPER/reaper-auplugins_arm64.ini");
    if reaper_cache.exists() {
        if let Ok(content) = fs::read_to_string(&reaper_cache) {
            if let Ok(config) = load_config() {
                let filtered: String = content
                    .lines()
                    .filter(|l| !l.contains(&config.vendor.name))
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = fs::write(&reaper_cache, filtered);
                eprintln!("  Cleaned Reaper AU cache");
            }
        }
    }

    // Flush pluginkit registrations (AU v3 appex cache)
    eprintln!("Flushing pluginkit registrations...");
    if let Ok(config) = load_config() {
        for p in &config.plugin {
            for pattern in [
                format!("com.{}.{}.v3.ext", config.vendor.id, p.bundle_id),
                format!("com.{}.{}.au", config.vendor.id, p.bundle_id),
            ] {
                let _ = Command::new("pluginkit")
                    .args(["-e", "ignore", "-i", &pattern])
                    .output();
                let _ = Command::new("pluginkit")
                    .args(["-e", "use", "-i", &pattern])
                    .output();
                eprintln!("  Reset pluginkit: {pattern}");
            }
        }

        // Force LaunchServices to re-scan v3 app bundles
        for p in &config.plugin {
            let app_path = format!("/Applications/{}.app", p.au3_app_name());
            if Path::new(&app_path).exists() {
                let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                    .args(["-f", "-R", &app_path])
                    .output();
                eprintln!("  Re-registered: {app_path}");
            }
        }
    }

    // AAX plugin cache (Pro Tools)
    let aax_cache = PathBuf::from("/Users/Shared/Pro Tools/AAXPlugInCache");
    if aax_cache.exists() {
        if let Ok(entries) = fs::read_dir(&aax_cache) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Ok(ref config) = load_config() {
                    if name.contains(&config.vendor.name) {
                        let _ = fs::remove_file(entry.path());
                        eprintln!("  Removed AAX cache: {}", name);
                    }
                }
            }
        }
    }

    // Clean AU v3 build temp dirs
    eprintln!("Cleaning AU v3 temp dirs...");
    let tmp = tmp_dir();
    if let Ok(entries) = fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("au_v3_build_") || name.starts_with("au_v3_fw_") {
                let _ = fs::remove_dir_all(entry.path());
                eprintln!("  Removed: {}", entry.path().display());
            }
        }
    }

    // Kill daemons to drop in-memory caches
    eprintln!("Restarting audio daemons...");
    run_sudo_silent("killall", &["-9", "AudioComponentRegistrar"]);
    run_sudo_silent("killall", &["-9", "pkd"]);

    eprintln!("Done. Restart your DAW to rescan.");
    Ok(())
}
