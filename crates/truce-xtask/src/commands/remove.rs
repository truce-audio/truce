//! `cargo truce remove` — uninstall plugin bundles for the current project,
//! or with `--stale` evict vendor-matching bundles no longer in `truce.toml`.

use crate::{confirm_prompt, dirs, load_config, run_sudo, Config, PluginDef, Res};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct RemoveTarget {
    format: &'static str,
    path: PathBuf,
    needs_sudo: bool,
}

fn unregister_au3(config: &Config, plugin: &PluginDef, app_path: &Path) {
    let vid = config.vendor.id.trim_start_matches("com.");
    for pattern in [
        format!("com.{}.{}.v3.ext", vid, plugin.suffix),
        format!("com.{}.{}.au", vid, plugin.suffix),
    ] {
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &pattern])
            .output();
        let _ = Command::new("pluginkit")
            .args(["-r", "-i", &pattern])
            .output();
    }
    let _ = Command::new(
        "/System/Library/Frameworks/CoreServices.framework/\
         Frameworks/LaunchServices.framework/Support/lsregister",
    )
    .args(["-u", app_path.to_str().unwrap_or("")])
    .output();
}

fn clear_au_caches() {
    let home = dirs::home_dir().unwrap();
    for dir in [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ] {
        let _ = fs::remove_dir_all(&dir);
    }
    let _ = Command::new("killall")
        .args(["-9", "AudioComponentRegistrar"])
        .output();
}

pub(crate) fn cmd_remove(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut dry_run = false;
    let mut yes = false;
    let mut stale = false;
    let mut crate_filter: Option<String> = None;
    let mut name_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--stale" => stale = true,
            "-p" => {
                i += 1;
                crate_filter =
                    Some(args.get(i).cloned().ok_or("-p requires a plugin crate name")?);
            }
            "-n" => {
                i += 1;
                name_filter = Some(args.get(i).cloned().ok_or("-n requires a plugin name")?);
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Default: all formats if none specified
    if !clap && !vst3 && !vst2 && !au2 && !au3 && !aax {
        clap = true;
        vst3 = true;
        vst2 = true;
        au2 = true;
        au3 = true;
        aax = true;
    }

    let home = dirs::home_dir().unwrap();
    let vendor = &config.vendor.name;
    let known_names: Vec<&str> = config.plugin.iter().map(|p| p.name.as_str()).collect();

    let mut targets: Vec<RemoveTarget> = Vec::new();

    if stale {
        // --stale: find vendor-matching bundles NOT in the current project
        let scan = |dir: &Path,
                    ext: &str,
                    format: &'static str,
                    needs_sudo: bool,
                    targets: &mut Vec<RemoveTarget>| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.contains(vendor) {
                        continue;
                    }
                    // Strip extension to get the display name
                    let display = name.trim_end_matches(&format!(".{ext}"));
                    if known_names.iter().any(|k| *k == display) {
                        continue;
                    }
                    targets.push(RemoveTarget {
                        format,
                        path: entry.path(),
                        needs_sudo,
                    });
                }
            }
        };

        if clap {
            scan(
                &home.join("Library/Audio/Plug-Ins/CLAP"),
                "clap",
                "CLAP",
                false,
                &mut targets,
            );
            scan(
                Path::new("/Library/Audio/Plug-Ins/CLAP"),
                "clap",
                "CLAP",
                true,
                &mut targets,
            );
        }
        if vst3 {
            scan(
                Path::new("/Library/Audio/Plug-Ins/VST3"),
                "vst3",
                "VST3",
                true,
                &mut targets,
            );
            scan(
                &home.join("Library/Audio/Plug-Ins/VST3"),
                "vst3",
                "VST3",
                false,
                &mut targets,
            );
        }
        if vst2 {
            scan(
                &home.join("Library/Audio/Plug-Ins/VST"),
                "vst",
                "VST2",
                false,
                &mut targets,
            );
            scan(
                Path::new("/Library/Audio/Plug-Ins/VST"),
                "vst",
                "VST2",
                true,
                &mut targets,
            );
        }
        if au2 {
            scan(
                Path::new("/Library/Audio/Plug-Ins/Components"),
                "component",
                "AU v2",
                true,
                &mut targets,
            );
            scan(
                &home.join("Library/Audio/Plug-Ins/Components"),
                "component",
                "AU v2",
                false,
                &mut targets,
            );
        }
        if au3 {
            // Scan /Applications for vendor-matching v3 apps not in project.
            // Recognize truce AU v3 containers by bundle-name convention:
            // legacy "<name> v3.app" or the new default "<name> (AUv3).app".
            // A custom `au3_name` override may produce neither pattern — those
            // orphans can only be detected when the current config still
            // produces a recognizable name, so we compare against the current
            // bundle names as well.
            let known_au3_bundles: Vec<String> = config
                .plugin
                .iter()
                .map(|p| format!("{}.app", p.au3_app_name()))
                .collect();
            if let Ok(entries) = fs::read_dir("/Applications") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if !name_str.contains(vendor) || !name_str.ends_with(".app") {
                        continue;
                    }
                    let looks_like_au3 =
                        name_str.ends_with(" v3.app") || name_str.ends_with("(AUv3).app");
                    if !looks_like_au3 {
                        continue;
                    }
                    if known_au3_bundles
                        .iter()
                        .any(|k| k.as_str() == name_str.as_ref())
                    {
                        continue;
                    }
                    targets.push(RemoveTarget {
                        format: "AU v3",
                        path: entry.path(),
                        needs_sudo: true,
                    });
                }
            }
        }
        if aax {
            scan(
                Path::new("/Library/Application Support/Avid/Audio/Plug-Ins"),
                "aaxplugin",
                "AAX",
                true,
                &mut targets,
            );
        }

        // Apply -p (substring match on filename) or -n (exact display name match)
        if let Some(ref filter) = crate_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                t.path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_lowercase().contains(&filter_lower))
                    .unwrap_or(false)
            });
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                let fname = t
                    .path
                    .file_stem()
                    .map(|f| f.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                // Strip AU v3 suffixes: legacy " v3" and the new " (auv3)".
                let display = fname.trim_end_matches(" v3").trim_end_matches(" (auv3)");
                display == filter_lower
            });
        }
    } else {
        // Normal mode: remove bundles for plugins in the project

        // Filter plugins by crate name (-p) or display name (-n)
        let plugins: Vec<&PluginDef> = if let Some(ref filter) = crate_filter {
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
                        .map(|p| format!("{} (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.name.to_lowercase() == filter_lower)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("\"{}\" (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else {
            config.plugin.iter().collect()
        };

        for p in &plugins {
            if clap {
                let path = home.join(format!("Library/Audio/Plug-Ins/CLAP/{}.clap", p.name));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "CLAP",
                        path,
                        needs_sudo: false,
                    });
                }
            }
            if vst3 {
                let path = PathBuf::from(format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "VST3",
                        path,
                        needs_sudo: true,
                    });
                }
            }
            if vst2 {
                let path = home.join(format!("Library/Audio/Plug-Ins/VST/{}.vst", p.name));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "VST2",
                        path,
                        needs_sudo: false,
                    });
                }
            }
            if au2 {
                let path = PathBuf::from(format!(
                    "/Library/Audio/Plug-Ins/Components/{}.component",
                    p.name
                ));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "AU v2",
                        path,
                        needs_sudo: true,
                    });
                }
            }
            if au3 {
                let path = PathBuf::from(format!("/Applications/{}.app", p.au3_app_name()));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "AU v3",
                        path,
                        needs_sudo: true,
                    });
                }
            }
            if aax {
                let path = PathBuf::from(format!(
                    "/Library/Application Support/Avid/Audio/Plug-Ins/{}.aaxplugin",
                    p.name
                ));
                if path.exists() {
                    targets.push(RemoveTarget {
                        format: "AAX",
                        path,
                        needs_sudo: true,
                    });
                }
            }
        }
    }

    if targets.is_empty() {
        eprintln!("No installed plugins found to remove.");
        return Ok(());
    }

    // Show summary
    eprintln!("The following plugins will be removed:\n");
    for t in &targets {
        eprintln!("  {:<5} {}", t.format, t.path.display());
    }
    eprintln!();

    if dry_run {
        eprintln!("Dry run — nothing was removed.");
        return Ok(());
    }

    if !yes && !confirm_prompt(&format!("Remove {} bundle(s)?", targets.len())) {
        eprintln!("Cancelled.");
        return Ok(());
    }

    // Remove bundles
    let mut removed_au = false;
    let mut errors = 0u32;

    for t in &targets {
        // AU v3 special handling: unregister before deleting
        if t.format == "AU v3" {
            // Try to find a matching plugin def for precise unregistration
            let matched_plugin = config
                .plugin
                .iter()
                .find(|p| t.path == Path::new(&format!("/Applications/{}.app", p.au3_app_name())));
            if let Some(p) = matched_plugin {
                unregister_au3(&config, p, &t.path);
            } else {
                // Stale AU v3 — unregister by path only (lsregister)
                let _ = Command::new(
                    "/System/Library/Frameworks/CoreServices.framework/\
                     Frameworks/LaunchServices.framework/Support/lsregister",
                )
                .args(["-u", t.path.to_str().unwrap_or("")])
                .output();
            }
            removed_au = true;
        }
        if t.format == "AU v2" {
            removed_au = true;
        }

        let result = if t.needs_sudo {
            run_sudo("rm", &["-rf", t.path.to_str().unwrap()])
        } else {
            fs::remove_dir_all(&t.path)
                .or_else(|_| fs::remove_file(&t.path))
                .map_err(|e| e.into())
        };

        let name = t.path.file_name().unwrap_or_default().to_string_lossy();
        match result {
            Ok(()) => eprintln!("  \u{2713} {:<5} {}", t.format, name),
            Err(e) => {
                eprintln!("  \u{2717} {:<5} {} ({})", t.format, name, e);
                errors += 1;
            }
        }
    }

    // Clear AU caches if any AU bundles were removed
    if removed_au {
        clear_au_caches();
        eprintln!("\nCleared AU caches.");
    }

    if errors > 0 {
        eprintln!("\n{errors} error(s). Check permissions or run with sudo.");
    } else {
        eprintln!("\nDone. Restart your DAW to rescan.");
    }
    Ok(())
}
