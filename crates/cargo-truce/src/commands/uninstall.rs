//! `cargo truce uninstall` - remove plugin bundles for the current project,
//! or with `--stale` evict vendor-matching bundles no longer in `truce.toml`.

#[cfg(target_os = "macos")]
use crate::Config;
use crate::install_scope::{InstallScope, note_once, set_cli_install_scope};
use crate::{PluginDef, Res, confirm_prompt, dirs, load_config, run_sudo};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use truce_utils::shell_sidecar::sidecar_path;

struct RemoveTarget {
    format: &'static str,
    path: PathBuf,
    needs_sudo: bool,
}

#[cfg(target_os = "macos")]
fn unregister_au3(config: &Config, plugin: &PluginDef, app_path: &Path) {
    let vid = config.vendor.id.trim_start_matches("com.");
    for pattern in [
        format!("com.{}.{}.v3.ext", vid, plugin.bundle_id),
        format!("com.{}.{}.au", vid, plugin.bundle_id),
    ] {
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &pattern])
            .output();
        let _ = Command::new("pluginkit")
            .args(["-r", "-i", &pattern])
            .output();
    }
    // `lsregister -u ""` interprets the empty string as the current
    // directory and unregisters whatever app-bundle the CWD happens to
    // be - alarming if the user invoked `cargo truce uninstall` from inside
    // some other `.app`. Skip the call instead.
    if let Some(app_path_str) = app_path.to_str() {
        let _ = Command::new(
            "/System/Library/Frameworks/CoreServices.framework/\
             Frameworks/LaunchServices.framework/Support/lsregister",
        )
        .args(["-u", app_path_str])
        .output();
    }
}

fn clear_au_caches() {
    // No HOME (or USERPROFILE on Windows) → skip the per-user cache
    // sweep silently. The system-wide `killall AudioComponentRegistrar`
    // below still runs; AU caches in $HOME just don't exist for a user
    // whose env we can't resolve.
    if let Some(home) = dirs::home_dir() {
        for dir in [
            home.join("Library/Caches/AudioUnitCache"),
            home.join(
                "Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache",
            ),
            home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
            home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
        ] {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    let _ = Command::new("killall")
        .args(["-9", "AudioComponentRegistrar"])
        .output();
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_uninstall(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut standalone = false;
    let mut dry_run = false;
    let mut yes = false;
    let mut stale = false;
    let mut crate_filter: Option<String> = None;
    let mut name_filter: Option<String> = None;
    let mut cli_scope: Option<InstallScope> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--lv2" => lv2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--standalone" => standalone = true,
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--stale" => stale = true,
            "--user" => set_cli_install_scope(&mut cli_scope, InstallScope::User)?,
            "--system" => set_cli_install_scope(&mut cli_scope, InstallScope::System)?,
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo truce uninstall` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                crate_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "-n" => {
                name_filter = Some(crate::util::arg_value(args, &mut i, "-n")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Without an explicit scope flag, scan both user and system -
    // a dev who switched scopes mid-iteration may have stale copies
    // in the other half of the disk.
    let scopes_to_scan: Vec<InstallScope> = match cli_scope {
        Some(InstallScope::User) => vec![InstallScope::User],
        Some(InstallScope::System) => vec![InstallScope::System],
        None => vec![InstallScope::User, InstallScope::System],
    };
    // AAX, AU v3, and (on Windows) VST2 are always system-scope -
    // surface the same one-line note as `install` when `--user` was
    // explicitly requested for one of them.
    let user_explicit = matches!(cli_scope, Some(InstallScope::User));
    if user_explicit {
        if aax {
            note_once("AAX is system-only; ignoring --user");
        }
        if au3 && cfg!(target_os = "macos") {
            note_once("AU v3 is system-only; ignoring --user");
        }
        if vst2 && cfg!(target_os = "windows") {
            note_once("VST2 on Windows is system-only; ignoring --user");
        }
    }

    // Captured before the default-fill below so the post-loop sidecar
    // cleanup can tell "user passed no format flag → uninstall
    // everything for these plugins" apart from "user picked specific
    // formats → leave shell sidecars alone for the others".
    let all_formats_default =
        !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax && !standalone;

    // Default: all formats if none specified.
    // `au3 = true` lands in a flag that's read only inside macOS-gated
    // blocks; the assignment-never-read warning on Linux/Windows is
    // intentional - keeping the flag uniform across platforms is more
    // readable than a per-platform `if`.
    #[allow(unused_assignments)]
    if all_formats_default {
        clap = true;
        vst3 = true;
        vst2 = true;
        lv2 = true;
        au2 = true;
        au3 = true;
        aax = true;
        standalone = true;
    }

    let vendor = &config.vendor.name;
    let known_names: Vec<&str> = config.plugin.iter().map(|p| p.name.as_str()).collect();

    let mut targets: Vec<RemoveTarget> = Vec::new();
    // Collected in the non-stale branch below; used after the
    // bundle-removal loop to clean up `~/.truce/shell/<crate>.path`
    // sidecars when the user is uninstalling all formats for a
    // plugin. Empty for `--stale` (we only have display names there,
    // not crate names - sidecars stay).
    let mut crate_names_for_sidecar_cleanup: Vec<String> = Vec::new();

    if stale {
        // --stale: find vendor-matching bundles NOT in the current project
        let scan = |dir: &Path,
                    ext: &str,
                    format: &'static str,
                    match_token: &str,
                    known_stems: &[&str],
                    needs_sudo: bool,
                    targets: &mut Vec<RemoveTarget>| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.contains(match_token) {
                        continue;
                    }
                    // Strip extension to get the stem
                    let stem = name.trim_end_matches(&format!(".{ext}"));
                    if known_stems.contains(&stem) {
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

        // LV2 bundles are slug-cased (`Truce Analyzer` ->
        // `truce-analyzer.lv2`), so the vendor display name won't
        // substring-match and the trimmed stem won't equal any raw
        // display name in `known_names`. Pre-compute slug forms so
        // LV2's stale scan actually catches orphans.
        let vendor_lv2_slug = truce_utils::slugify(vendor);
        let known_lv2_slugs: Vec<String> = config
            .plugin
            .iter()
            .map(|p| crate::commands::package::stage::lv2_slug(&p.name))
            .collect();
        let known_lv2_slug_refs: Vec<&str> = known_lv2_slugs.iter().map(String::as_str).collect();

        let scan_system = scopes_to_scan.contains(&InstallScope::System);
        if clap {
            for s in &scopes_to_scan {
                scan(
                    &s.clap_dir(),
                    "clap",
                    "CLAP",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        if vst3 {
            for s in &scopes_to_scan {
                scan(
                    &s.vst3_dir(),
                    "vst3",
                    "VST3",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        if vst2 && !cfg!(target_os = "windows") {
            for s in &scopes_to_scan {
                scan(
                    &s.vst2_dir(),
                    "vst",
                    "VST2",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        } else if vst2 && scan_system {
            // Windows VST2 is always system-only - `vst2_dir()` returns
            // the same path for both scopes.
            scan(
                &InstallScope::System.vst2_dir(),
                "dll",
                "VST2",
                vendor,
                &known_names,
                InstallScope::System.needs_sudo(),
                &mut targets,
            );
        }
        if lv2 {
            #[cfg(target_os = "linux")]
            {
                // Linux LV2 lives at `~/.lv2/`; scope is irrelevant
                // (`lv2_dir` returns the same path for User and System).
                let s = InstallScope::User;
                scan(
                    &s.lv2_dir(),
                    "lv2",
                    "LV2",
                    &vendor_lv2_slug,
                    &known_lv2_slug_refs,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
            #[cfg(not(target_os = "linux"))]
            {
                for s in &scopes_to_scan {
                    scan(
                        &s.lv2_dir(),
                        "lv2",
                        "LV2",
                        &vendor_lv2_slug,
                        &known_lv2_slug_refs,
                        s.needs_sudo(),
                        &mut targets,
                    );
                }
            }
        }
        #[cfg(target_os = "macos")]
        if au2 {
            for s in &scopes_to_scan {
                scan(
                    &s.au_v2_dir(),
                    "component",
                    "AU v2",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        // AU v3 lives in `/Applications/...` only on macOS, so the
        // `--au3` removal scan is macOS-only. The flag is still parsed
        // on every platform so cross-platform CI scripts don't break;
        // it just no-ops on Linux / Windows.
        //
        // `--user` skips this scan: AU v3 has no user-scope install
        // path (the install-side note already explained that to the
        // user), so there's nothing for `--user` to clean up.
        #[cfg(target_os = "macos")]
        if au3 && scan_system {
            // Scan /Applications for vendor-matching v3 apps not in project.
            // Recognize truce AU v3 containers by bundle-name convention:
            // legacy "<name> v3.app" or the new default "<name> (AUv3).app".
            // A custom `au3_name` override may produce neither pattern - those
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
        if aax && scan_system {
            scan(
                Path::new("/Library/Application Support/Avid/Audio/Plug-Ins"),
                "aaxplugin",
                "AAX",
                vendor,
                &known_names,
                true,
                &mut targets,
            );
        }
        // `--stale --standalone` cleans up legacy `<Name>.standalone.app`
        // bundles only - the historical convention `cargo truce package`
        // used before the rename. The current `<Plugin>.app` layout
        // collides with arbitrary unrelated apps the user installed
        // from anywhere; vendor-string substring matching isn't enough
        // to confidently delete a `.app` from `/Applications`, so we
        // skip those here. Run with `--stale` + `-p` / `-n` for a
        // targeted sweep instead.
        #[cfg(target_os = "macos")]
        if standalone {
            let scan_legacy_standalone =
                |dir: &Path, needs_sudo: bool, targets: &mut Vec<RemoveTarget>| {
                    if let Ok(entries) = fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            let name_str = name.to_string_lossy();
                            if !name_str.ends_with(".standalone.app") {
                                continue;
                            }
                            if !name_str.contains(vendor) {
                                continue;
                            }
                            let display = name_str.trim_end_matches(".standalone.app");
                            if known_names.contains(&display) {
                                continue;
                            }
                            targets.push(RemoveTarget {
                                format: "Standalone",
                                path: entry.path(),
                                needs_sudo,
                            });
                        }
                    }
                };
            for s in &scopes_to_scan {
                scan_legacy_standalone(&s.standalone_dir(), s.needs_sudo(), &mut targets);
            }
        }

        // Apply -p (substring match on filename) or -n (exact display name match)
        if let Some(ref filter) = crate_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                t.path
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy().to_lowercase().contains(&filter_lower))
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

        if all_formats_default {
            crate_names_for_sidecar_cleanup.extend(plugins.iter().map(|p| p.crate_name.clone()));
        }

        let scan_system = scopes_to_scan.contains(&InstallScope::System);
        let push_if_exists =
            |format: &'static str, path: PathBuf, needs_sudo: bool, targets: &mut Vec<_>| {
                if path.exists() && !targets.iter().any(|t: &RemoveTarget| t.path == path) {
                    targets.push(RemoveTarget {
                        format,
                        path,
                        needs_sudo,
                    });
                }
            };
        for p in &plugins {
            if clap {
                for s in &scopes_to_scan {
                    let path = s.clap_dir().join(format!("{}.clap", p.file_stem()));
                    push_if_exists("CLAP", path, s.needs_sudo(), &mut targets);
                }
            }
            if vst3 {
                for s in &scopes_to_scan {
                    let path = s.vst3_dir().join(format!("{}.vst3", p.file_stem()));
                    push_if_exists("VST3", path, s.needs_sudo(), &mut targets);
                }
            }
            if vst2 {
                #[cfg(target_os = "macos")]
                {
                    for s in &scopes_to_scan {
                        let path = s.vst2_dir().join(format!("{}.vst", p.file_stem()));
                        push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                    }
                }
                #[cfg(target_os = "windows")]
                if scan_system {
                    let s = InstallScope::System;
                    let path = s.vst2_dir().join(format!("{}.dll", p.file_stem()));
                    push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                }
                #[cfg(target_os = "linux")]
                {
                    // Linux VST2 is `~/.vst/<name>.so` for both scopes.
                    let s = InstallScope::User;
                    let path = s.vst2_dir().join(format!("{}.so", p.file_stem()));
                    push_if_exists("VST2", path, s.needs_sudo(), &mut targets);
                }
            }
            if lv2 {
                // LV2 bundle name uses the slugged display name to
                // match what `install_lv2` writes via
                // `package::stage::lv2_slug(&p.name)`.
                let slug = crate::commands::package::stage::lv2_slug(&p.name);
                #[cfg(target_os = "linux")]
                {
                    // Linux LV2 lives at `~/.lv2/`; scope is irrelevant.
                    let s = InstallScope::User;
                    let path = s.lv2_dir().join(format!("{slug}.lv2"));
                    push_if_exists("LV2", path, s.needs_sudo(), &mut targets);
                }
                #[cfg(not(target_os = "linux"))]
                {
                    for s in &scopes_to_scan {
                        let path = s.lv2_dir().join(format!("{slug}.lv2"));
                        push_if_exists("LV2", path, s.needs_sudo(), &mut targets);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if au2 {
                for s in &scopes_to_scan {
                    let path = s.au_v2_dir().join(format!("{}.component", p.file_stem()));
                    push_if_exists("AU v2", path, s.needs_sudo(), &mut targets);
                }
            }
            #[cfg(target_os = "macos")]
            if au3 && scan_system {
                let path = PathBuf::from(format!("/Applications/{}.app", p.au3_app_name()));
                push_if_exists("AU v3", path, true, &mut targets);
            }
            if aax && scan_system {
                let path = PathBuf::from(format!(
                    "/Library/Application Support/Avid/Audio/Plug-Ins/{}.aaxplugin",
                    p.file_stem()
                ));
                push_if_exists("AAX", path, true, &mut targets);
            }
            if standalone {
                #[cfg(target_os = "macos")]
                {
                    for s in &scopes_to_scan {
                        // Current convention: plain `<Plugin>.app` so
                        // Spotlight / Launch Services index it as a
                        // regular application. The historical
                        // `<Plugin>.standalone.app` name is checked too
                        // for users upgrading from older installers.
                        let dir = s.standalone_dir();
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.standalone.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                    }
                }
                // Linux / Windows standalone paths are handled by the
                // platform installer (a bare ELF under `~/.local/bin`
                // on Linux, `%PROGRAMFILES%\<Vendor>\<Plugin>\...exe`
                // on Windows). Uninstall there is the OS package
                // manager's responsibility - `cargo truce uninstall`
                // never put the file there in the first place.
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = p;
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
        eprintln!("Dry run - nothing was removed.");
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
        // AU v3 special handling: unregister before deleting (macOS-only).
        #[cfg(target_os = "macos")]
        if t.format == "AU v3" {
            // Try to find a matching plugin def for precise unregistration
            let matched_plugin = config
                .plugin
                .iter()
                .find(|p| t.path == Path::new(&format!("/Applications/{}.app", p.au3_app_name())));
            if let Some(p) = matched_plugin {
                unregister_au3(&config, p, &t.path);
            } else if let Some(path_str) = t.path.to_str() {
                // Stale AU v3 - unregister by path only (lsregister).
                // Skip the call when the path can't be UTF-8'd: `lsregister
                // -u ""` would unregister whatever app the CWD happens to be.
                let _ = Command::new(
                    "/System/Library/Frameworks/CoreServices.framework/\
                     Frameworks/LaunchServices.framework/Support/lsregister",
                )
                .args(["-u", path_str])
                .output();
            }
            removed_au = true;
        }
        if t.format == "AU v2" {
            removed_au = true;
        }

        let result = if t.needs_sudo {
            run_sudo("rm", &[OsStr::new("-rf"), t.path.as_os_str()])
        } else {
            fs::remove_dir_all(&t.path)
                .or_else(|_| fs::remove_file(&t.path))
                .map_err(std::convert::Into::into)
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

    // Clean up `~/.truce/shell/<crate>.path` sidecars for plugins
    // whose entire format set is being uninstalled. Skipped on
    // partial uninstalls (`--clap`, `-p` etc. without all formats)
    // since the sidecar is shared across format wrappers and
    // removing it would break shell-mode for any still-installed
    // formats. Skipped on `--stale` because we don't have crate
    // names there.
    for crate_name in &crate_names_for_sidecar_cleanup {
        if let Some(path) = sidecar_path(crate_name)
            && path.exists()
        {
            match fs::remove_file(&path) {
                Ok(()) => eprintln!("  \u{2713} sidecar {}", path.display()),
                Err(e) => eprintln!("  \u{2717} sidecar {} ({})", path.display(), e),
            }
        }
    }

    if errors > 0 {
        eprintln!("\n{errors} error(s). Check permissions or run with sudo.");
    } else {
        eprintln!("\nDone. Restart your DAW to rescan.");
    }
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce uninstall [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax]
                             [--standalone] [--user|--system] [-p <crate>] [-n <name>]
                             [--stale] [--dry-run] [--yes]

Uninstall plugin bundles for this project. Default: all formats,
all plugins, both user + system scopes. Asks for confirmation. AAX and
AU v3 are always system-scope - `--user` skips them.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --vst2           VST2 only
  --lv2            LV2 only
  --au2            AU v2 only (.component, macOS only)
  --au3            AU v3 only (.app, macOS only)
  --aax            AAX only
  --standalone     Standalone host app only (.app, macOS only)
  --user           Only uninstall from per-user directories.
  --system         Only uninstall from system directories.
  -p <crate>       Filter by cargo crate name.
  -n <name>        Filter by display name.
  --stale          Uninstall vendor bundles NOT in the current project.
  --dry-run        Show what would be uninstalled without deleting.
  --yes, -y        Skip confirmation prompt.
  -h, --help       Show this message"
    );
}
