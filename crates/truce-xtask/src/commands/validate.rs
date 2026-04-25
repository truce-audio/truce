//! `cargo truce validate` — drive auval (AU v2/v3), pluginval (VST3), and
//! clap-validator (CLAP) against the project's installed bundles, with
//! shadow-install collision detection.

#[cfg(target_os = "macos")]
use crate::{deployment_target, project_root};
use crate::{dirs, load_config, tmp_dir, PluginDef, Res};
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::Command;

/// Read a single leaf value from a plist via `plutil -extract … raw`.
/// Returns `None` if the key path doesn't exist or the value isn't a scalar.
#[cfg(target_os = "macos")]
fn plist_extract(plist: &Path, key_path: &str) -> Option<String> {
    let out = Command::new("plutil")
        .args(["-extract", key_path, "raw", "-o", "-", plist.to_str()?])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Find every AU bundle on disk that declares the given component code.
/// Walks the standard AU install directories and reads each candidate's
/// Info.plist. Used by `cmd_validate` to surface stale-install collisions
/// (the underlying cause of cryptic auval errors like
/// `FATAL ERROR: Initialize: result: -10875` when an old `Truce Gain.app`
/// shadows the current `Truce Gain v3.app`).
#[cfg(target_os = "macos")]
fn find_au_collisions(au_type: &str, subtype: &str, manufacturer: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let home = dirs::home_dir().unwrap_or_default();

    // AU v3: Application bundles ship the appex inside Contents/PlugIns.
    // Walk both /Applications and ~/Applications (per-user install).
    for apps_dir in [PathBuf::from("/Applications"), home.join("Applications")] {
        let Ok(apps) = fs::read_dir(&apps_dir) else {
            continue;
        };
        for app in apps.flatten() {
            let plugins_dir = app.path().join("Contents/PlugIns");
            let Ok(appexes) = fs::read_dir(&plugins_dir) else {
                continue;
            };
            for appex in appexes.flatten() {
                let plist = appex.path().join("Contents/Info.plist");
                if matches_au_v3_descriptor(&plist, au_type, subtype, manufacturer) {
                    hits.push(appex.path());
                }
            }
        }
    }

    // AU v2: .component bundles in the standard system & user paths.
    for dir in [
        PathBuf::from("/Library/Audio/Plug-Ins/Components"),
        home.join("Library/Audio/Plug-Ins/Components"),
    ] {
        let Ok(comps) = fs::read_dir(&dir) else {
            continue;
        };
        for comp in comps.flatten() {
            let plist = comp.path().join("Contents/Info.plist");
            if matches_au_v2_descriptor(&plist, au_type, subtype, manufacturer) {
                hits.push(comp.path());
            }
        }
    }

    hits
}

#[cfg(target_os = "macos")]
fn matches_au_v3_descriptor(
    plist: &Path,
    au_type: &str,
    subtype: &str,
    manufacturer: &str,
) -> bool {
    if !plist.is_file() {
        return false;
    }
    let prefix = "NSExtension.NSExtensionAttributes.AudioComponents.0";
    plist_extract(plist, &format!("{prefix}.subtype")).as_deref() == Some(subtype)
        && plist_extract(plist, &format!("{prefix}.type")).as_deref() == Some(au_type)
        && plist_extract(plist, &format!("{prefix}.manufacturer")).as_deref() == Some(manufacturer)
}

#[cfg(target_os = "macos")]
fn matches_au_v2_descriptor(
    plist: &Path,
    au_type: &str,
    subtype: &str,
    manufacturer: &str,
) -> bool {
    if !plist.is_file() {
        return false;
    }
    plist_extract(plist, "AudioComponents.0.subtype").as_deref() == Some(subtype)
        && plist_extract(plist, "AudioComponents.0.type").as_deref() == Some(au_type)
        && plist_extract(plist, "AudioComponents.0.manufacturer").as_deref() == Some(manufacturer)
}

/// Print a warning if more than one bundle declares the AU component
/// `(au_type, subtype, manufacturer)`. macOS picks one at load time; the other
/// gets shadowed and produces opaque auval failures.
#[cfg(target_os = "macos")]
fn warn_on_au_collision(au_type: &str, subtype: &str, manufacturer: &str, expected: &Path) {
    let hits = find_au_collisions(au_type, subtype, manufacturer);
    if hits.len() <= 1 {
        return;
    }
    eprintln!(
        "    ⚠️  collision: {} other bundle(s) also claim {}/{}/{}",
        hits.len() - 1,
        au_type,
        subtype,
        manufacturer,
    );
    for h in &hits {
        let marker = if h.starts_with(expected) || expected.starts_with(h) {
            "← expected"
        } else {
            "← stale, remove this"
        };
        eprintln!("        • {} {}", h.display(), marker);
    }
    eprintln!("        macOS will pick one at load time; the rest are shadowed.");
}

pub(crate) fn cmd_validate(args: &[String]) -> Res {
    let config = load_config()?;

    let mut run_auval = false;
    let mut run_auval_v3 = false;
    let mut run_pluginval = false;
    let mut run_clap = false;
    let mut run_vst2 = false;
    let mut plugin_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--auval" => run_auval = true,
            "--auval3" => run_auval_v3 = true,
            "--pluginval" => run_pluginval = true,
            "--clap" => run_clap = true,
            "--vst2" => run_vst2 = true,
            "--all" => {
                run_auval = true;
                run_auval_v3 = true;
                run_pluginval = true;
                run_clap = true;
                run_vst2 = true;
            }
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
    if !run_auval && !run_auval_v3 && !run_pluginval && !run_clap && !run_vst2 {
        run_auval = true;
        run_auval_v3 = true;
        run_pluginval = true;
        run_clap = true;
        run_vst2 = true;
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

    let mut failures = 0;

    // --- auval (macOS only, AU v2) ---
    if run_auval {
        eprintln!("=== auval (AU v2) ===\n");
        if Command::new("auval").arg("-h").output().is_ok() {
            for p in &plugins {
                eprint!(
                    "  {} ({} {} {}) ... ",
                    p.name,
                    p.resolved_au_type(),
                    p.resolved_fourcc(),
                    config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args([
                        "-v",
                        p.resolved_au_type(),
                        p.resolved_fourcc(),
                        &config.vendor.au_manufacturer,
                    ])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    #[cfg(target_os = "macos")]
                    {
                        let expected = PathBuf::from(format!(
                            "/Library/Audio/Plug-Ins/Components/{}.component",
                            p.name
                        ));
                        warn_on_au_collision(
                            p.resolved_au_type(),
                            p.resolved_fourcc(),
                            &config.vendor.au_manufacturer,
                            &expected,
                        );
                    }
                    failures += 1;
                }
            }
        } else {
            eprintln!("  auval not found (macOS only)");
        }
    }

    // --- auval (AU v3 appex) ---
    if run_auval_v3 {
        eprintln!("\n=== auval (AU v3) ===\n");
        if Command::new("auval").arg("-h").output().is_ok() {
            for p in &plugins {
                let sub = p.au3_sub();
                eprint!(
                    "  {} ({} {} {}) ... ",
                    p.name,
                    p.resolved_au_type(),
                    sub,
                    config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args([
                        "-v",
                        p.resolved_au_type(),
                        sub,
                        &config.vendor.au_manufacturer,
                    ])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    #[cfg(target_os = "macos")]
                    {
                        let expected = PathBuf::from(format!(
                            "/Applications/{}.app/Contents/PlugIns/AUExt.appex",
                            p.au3_app_name()
                        ));
                        warn_on_au_collision(
                            p.resolved_au_type(),
                            sub,
                            &config.vendor.au_manufacturer,
                            &expected,
                        );
                    }
                    failures += 1;
                }
            }
        } else {
            eprintln!("  auval not found (macOS only)");
        }
    }

    // --- pluginval (VST3) ---
    if run_pluginval {
        eprintln!("\n=== pluginval (VST3) ===\n");
        let pluginval = find_pluginval();
        if let Some(pv) = pluginval {
            for p in &plugins {
                let vst3_path = format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name);
                if !Path::new(&vst3_path).exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }
                eprint!("  {} ... ", p.name);
                let output = Command::new(&pv)
                    .args(["--validate", &vst3_path, "--strictness-level", "5"])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("SUCCESS") || output.status.success() {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    failures += 1;
                }
            }
        } else {
            eprintln!("  pluginval not found. Install from https://github.com/Tracktion/pluginval");
        }
    }

    // --- clap-validator (CLAP) ---
    if run_clap {
        eprintln!("\n=== clap-validator (CLAP) ===\n");
        let clap_validator = find_clap_validator();
        if let Some(cv) = clap_validator {
            let clap_dir = dirs::home_dir()
                .map(|h| h.join("Library/Audio/Plug-Ins/CLAP"))
                .unwrap_or_default();
            // Project-local scratch. `cargo clean` sweeps it, and it
            // stays off the system `/tmp` so nothing outside the repo
            // gets touched.
            let scratch = tmp_dir().join("clap-validate");
            let _ = fs::create_dir_all(&scratch);

            for p in &plugins {
                let clap_name = format!("{}.clap", p.name);
                let installed = clap_dir.join(&clap_name);

                if !installed.exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }

                // clap-validator requires bundle format (Plugin.clap/Contents/MacOS/Plugin).
                // If the installed file is a bare dylib, create a temporary bundle.
                let validate_path = if installed.join("Contents/MacOS").is_dir() {
                    installed.clone()
                } else {
                    let bundle = scratch.join(&clap_name);
                    let macos = bundle.join("Contents/MacOS");
                    let _ = fs::create_dir_all(&macos);
                    let bin_name = clap_name.trim_end_matches(".clap");
                    let _ = fs::copy(&installed, macos.join(bin_name));
                    bundle
                };

                eprint!("  {} ... ", p.name);
                let output = Command::new(&cv)
                    .args(["validate", &validate_path.to_string_lossy()])
                    .output()?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{}{}", stdout, stderr);

                if output.status.success() && !combined.contains("FAILED") {
                    // Count passed/failed from output
                    let passed = combined.matches("passed").count();
                    eprintln!("PASS ({} tests)", passed);
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{}", stdout);
                    }
                    if !stderr.is_empty() {
                        eprintln!("{}", stderr);
                    }
                    failures += 1;
                }
            }

            let _ = fs::remove_dir_all(&scratch);
        } else {
            eprintln!("  clap-validator not found.");
            eprintln!(
                "  Install: cargo install --git https://github.com/free-audio/clap-validator"
            );
            eprintln!("  Or set CLAP_VALIDATOR=/path/to/clap-validator");
        }
    }

    // --- VST2 binary smoke (no industry validator; this is ours) ---
    if run_vst2 {
        eprintln!("=== VST2 binary smoke ===\n");
        #[cfg(target_os = "macos")]
        {
            failures += validate_vst2_macos(&plugins);
        }
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("  Skipping: VST2 binary smoke is currently macOS-only.");
            let _ = &plugins;
        }
    }

    eprintln!();
    if failures > 0 {
        Err(format!("{failures} validation(s) failed").into())
    } else {
        eprintln!("All validations passed.");
        Ok(())
    }
}

/// Build each plugin as a VST2 dylib, dlopen it via the C smoke binary
/// at `tests/test_vst2_binary.c`, and verify `VSTPluginMain` returns a
/// well-formed `AEffect`. macOS-only because the smoke binary uses
/// `dlfcn.h` and we hardcode `.dylib` here. Returns the failure count.
#[cfg(target_os = "macos")]
fn validate_vst2_macos(plugins: &[&PluginDef]) -> usize {
    let root = project_root();
    let test_src = root.join("tests/test_vst2_binary.c");
    if !test_src.exists() {
        eprintln!(
            "  Skipping: smoke source missing at {}.",
            test_src.display()
        );
        return 0;
    }

    let test_bin = root.join("target/test_vst2");
    let cc_status = match Command::new("cc")
        .args(["-o", test_bin.to_str().unwrap(), test_src.to_str().unwrap()])
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  Skipping: failed to invoke cc: {e}");
            return 0;
        }
    };
    if !cc_status.success() {
        eprintln!("  Skipping: cc failed to build the smoke binary.");
        return 0;
    }

    let mut failures = 0;
    for p in plugins {
        eprint!("  {} ... ", p.name);
        let build = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                &p.crate_name,
                "--no-default-features",
                "--features",
                "vst2",
            ])
            .env("MACOSX_DEPLOYMENT_TARGET", deployment_target())
            .output();
        let build = match build {
            Ok(o) => o,
            Err(e) => {
                eprintln!("BUILD ERROR ({e})");
                failures += 1;
                continue;
            }
        };
        if !build.status.success() {
            eprintln!("BUILD FAILED");
            eprint!("{}", String::from_utf8_lossy(&build.stderr));
            failures += 1;
            continue;
        }

        let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
        // AU type tag is the same code path that drives plugin-kind
        // detection elsewhere: `aumu` → synth, `aumi` → MIDI/note
        // effect, anything else → audio effect.
        let kind_flag: Option<&str> = match p.resolved_au_type() {
            "aumu" => Some("--synth"),
            "aumi" => Some("--midi-effect"),
            _ => None,
        };
        let mut cmd = Command::new(test_bin.to_str().unwrap());
        cmd.arg(dylib.to_str().unwrap());
        if let Some(flag) = kind_flag {
            cmd.arg(flag);
        }
        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if out.status.success() {
                    if let Some(line) = stdout.lines().last() {
                        eprintln!("{line}");
                    } else {
                        eprintln!("PASS");
                    }
                } else {
                    eprintln!("FAIL");
                    eprint!("{stdout}");
                    failures += 1;
                }
            }
            Err(e) => {
                eprintln!("INVOKE ERROR ({e})");
                failures += 1;
            }
        }
    }
    failures
}

fn find_pluginval() -> Option<String> {
    // Check common locations
    let candidates = [
        "/Applications/pluginval.app/Contents/MacOS/pluginval",
        "/usr/local/bin/pluginval",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    // Check PATH
    if Command::new("pluginval").arg("--help").output().is_ok() {
        return Some("pluginval".to_string());
    }
    None
}

fn find_clap_validator() -> Option<String> {
    // Check env var override
    if let Ok(path) = std::env::var("CLAP_VALIDATOR") {
        if Path::new(&path).exists() {
            return Some(path);
        }
    }
    // Check PATH
    if Command::new("clap-validator")
        .arg("--version")
        .output()
        .is_ok()
    {
        return Some("clap-validator".to_string());
    }
    // Check cargo install location
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo/bin/clap-validator");
        if cargo_bin.exists() {
            return Some(cargo_bin.to_string_lossy().into());
        }
    }
    None
}
