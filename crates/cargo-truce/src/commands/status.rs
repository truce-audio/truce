//! `cargo truce status` - scan installed plugin bundles for this
//! workspace's plugins.
//!
//! macOS-only: every path it scans (`/Library/Audio/Plug-Ins/...`,
//! `~/Library/Audio/Plug-Ins/...`) is Apple-specific. Linux / Windows
//! are handled with a clean "not supported" message instead of an
//! empty banner that suggests nothing was found.
//!
//! Filesystem-only: AU registration state is not probed via `auval`
//! because that walks the `AudioComponentRegistrar` (slow, can hang
//! on broken third-party components). Use `cargo truce validate
//! --auval` when an actual registry check is wanted.

use crate::Res;

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce status

Scan installed plugin bundles for this workspace's plugins (matched
exactly by the on-disk name the installer writes, per format).
macOS-only - every path scanned (/Library/Audio/Plug-Ins/...,
~/Library/Audio/Plug-Ins/...) is Apple-specific. Filesystem-only;
for an AU registry check use `cargo truce validate --auval`.

Options:
  -h, --help       Show this message."
    );
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn cmd_status(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    Err(
        "`cargo truce status` is macOS-only - every directory it scans \
         (`/Library/Audio/Plug-Ins/...`) is Apple-specific. \
         For Linux / Windows, list bundles directly under your DAW's \
         configured plug-in path."
            .into(),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn cmd_status(args: &[String]) -> Res {
    use crate::commands::package::stage::lv2_slug;
    use crate::{dirs, load_config};
    use std::collections::HashSet;
    use std::path::PathBuf;

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if let Some(unknown) = args.iter().find(|a| !a.is_empty()) {
        return Err(format!("unknown flag: {unknown}").into());
    }

    let config = load_config()?;
    let home = dirs::require_home_dir()?;

    // Per-format expected bundle names, derived from `truce.toml`.
    // Matching is exact (the installer writes the same names), so an
    // unrelated plugin from another workspace with the same vendor
    // can't surface here.
    let expect_with_ext = |ext: &str| -> HashSet<String> {
        config
            .plugin
            .iter()
            .map(|p| format!("{}.{ext}", p.file_stem()))
            .collect()
    };
    let clap_names = expect_with_ext("clap");
    let vst3_names = expect_with_ext("vst3");
    let vst2_names = expect_with_ext("vst");
    let au2_names = expect_with_ext("component");
    let lv2_names: HashSet<String> = config
        .plugin
        .iter()
        .map(|p| format!("{}.lv2", lv2_slug(&p.name)))
        .collect();
    let au3_app_names: HashSet<String> = config
        .plugin
        .iter()
        .map(|p| format!("{}.app", p.au3_app_name()))
        .collect();

    // Each format can land in either user or system scope; both are
    // scanned so a per-user install isn't invisible to status.
    let sections: &[(&str, [PathBuf; 2], &HashSet<String>)] = &[
        (
            "AU v2 Components",
            [
                home.join("Library/Audio/Plug-Ins/Components"),
                PathBuf::from("/Library/Audio/Plug-Ins/Components"),
            ],
            &au2_names,
        ),
        (
            "CLAP",
            [
                home.join("Library/Audio/Plug-Ins/CLAP"),
                PathBuf::from("/Library/Audio/Plug-Ins/CLAP"),
            ],
            &clap_names,
        ),
        (
            "VST2",
            [
                home.join("Library/Audio/Plug-Ins/VST"),
                PathBuf::from("/Library/Audio/Plug-Ins/VST"),
            ],
            &vst2_names,
        ),
        (
            "VST3",
            [
                home.join("Library/Audio/Plug-Ins/VST3"),
                PathBuf::from("/Library/Audio/Plug-Ins/VST3"),
            ],
            &vst3_names,
        ),
        (
            "LV2",
            [
                home.join("Library/Audio/Plug-Ins/LV2"),
                PathBuf::from("/Library/Audio/Plug-Ins/LV2"),
            ],
            &lv2_names,
        ),
    ];

    for (i, (label, paths, expected)) in sections.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }
        eprintln!("{label}");
        for path in paths {
            scan_expected_entries(path, expected)?;
        }
    }

    // AU v3 ships as a `.appex` inside a `.app` bundle that the
    // packager drops in `/Applications` (or `~/Applications` for
    // user-scope packages).
    eprintln!("\nAU v3");
    let app_dirs = [home.join("Applications"), PathBuf::from("/Applications")];
    for app_dir in &app_dirs {
        scan_au_v3_apps(app_dir, &au3_app_names)?;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn scan_expected_entries(
    dir: &std::path::Path,
    expected: &std::collections::HashSet<String>,
) -> Res {
    use std::fs;
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if expected.contains(name.as_ref()) {
            eprintln!("  {name}");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn scan_au_v3_apps(app_dir: &std::path::Path, expected: &std::collections::HashSet<String>) -> Res {
    use std::fs;
    if !app_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(app_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !expected.contains(name.as_ref()) {
            continue;
        }
        // Only flag `.app`s that actually carry an AUv3 `.appex` so a
        // same-named non-AU app (rare but possible) doesn't show up.
        let plugins_dir = entry.path().join("Contents/PlugIns");
        let Ok(plugins) = fs::read_dir(&plugins_dir) else {
            continue;
        };
        let has_appex = plugins.into_iter().any(|p| {
            p.ok()
                .is_some_and(|p| p.file_name().to_string_lossy().ends_with(".appex"))
        });
        if has_appex {
            eprintln!("  {name}");
        }
    }
    Ok(())
}
