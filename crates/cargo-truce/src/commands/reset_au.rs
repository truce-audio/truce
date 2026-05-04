//! `cargo truce reset-au` — flush macOS Audio Unit caches and restart
//! `pkd` / `AudioComponentRegistrar`.
//!
//! macOS-only. Clears `~/Library/Caches/AudioUnitCache`, the `GarageBand`
//! / Logic container caches, the Reaper AU plist, pluginkit
//! registrations, and the AU v3 build scratch under `target/tmp/au_v3_*`.
//! Does **not** touch Pro Tools AAX caches — see `cargo truce reset-aax`
//! for that. CLAP / VST3 / VST2 / LV2 are unaffected; those formats let
//! their host DAWs manage caches.

use crate::Res;

#[cfg(not(target_os = "macos"))]
pub(crate) fn cmd_reset_au(_args: &[String]) -> Res {
    Err(
        "`cargo truce reset-au` is macOS-only — it flushes Apple's AU \
         caches and restarts `pkd` / `AudioComponentRegistrar`, neither \
         of which exist on Linux or Windows. CLAP / VST3 / VST2 / LV2 \
         let their host DAWs manage caches; restart your DAW if a plugin \
         is stuck."
            .into(),
    )
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_reset_au(args: &[String]) -> Res {
    use crate::{confirm_prompt, dirs, load_config, run_silent, tmp_dir};
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
    }

    if !yes
        && !confirm_prompt(
            "Reset macOS Audio Unit caches and restart `pkd` / \
             `AudioComponentRegistrar`? This deletes cached plugin \
             metadata and resets pluginkit registrations.",
        )
    {
        eprintln!("Cancelled.");
        return Ok(());
    }

    eprintln!("Clearing AU/DAW caches...");
    let home = dirs::require_home_dir()?;

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
    if reaper_cache.exists()
        && let Ok(content) = fs::read_to_string(&reaper_cache)
        && let Ok(config) = load_config()
    {
        let filtered: String = content
            .lines()
            .filter(|l| !l.contains(&config.vendor.name))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(&reaper_cache, filtered);
        eprintln!("  Cleaned Reaper AU cache");
    }

    // Flush pluginkit registrations (AU v3 appex cache).
    // `vendor.id` is conventionally already in `com.<x>` reverse-DNS form,
    // so the prefix gets trimmed before re-prepending to match what
    // `install/au_v3.rs` and `commands/remove.rs` actually register.
    eprintln!("Flushing pluginkit registrations...");
    if let Ok(config) = load_config() {
        let vid = config.vendor.id.trim_start_matches("com.");
        for p in &config.plugin {
            for pattern in [
                format!("com.{}.{}.v3.ext", vid, p.bundle_id),
                format!("com.{}.{}.au", vid, p.bundle_id),
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
    run_silent("killall", &["-9", "AudioComponentRegistrar"]);
    run_silent("killall", &["-9", "pkd"]);

    eprintln!("Done. Restart your DAW to rescan.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn print_help() {
    eprintln!(
        "\
Usage: cargo truce reset-au [--yes]

macOS-only. Flush Audio Unit caches and restart `pkd` /
`AudioComponentRegistrar`. Use when AU bundles are stuck serving
stale binaries. CLAP / VST3 / VST2 / LV2 are unaffected.

Options:
  --yes, -y        Skip confirmation prompt.
  -h, --help       Show this message."
    );
}
