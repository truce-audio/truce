//! `cargo truce validate` - drive auval (AU v2/v3), pluginval (VST3), and
//! clap-validator (CLAP) against the project's installed bundles, with
//! shadow-install collision detection.

use crate::format::Format;
use crate::install_scope::InstallScope;
use crate::{PluginDef, Res, dirs, load_config, tag_warn};
#[cfg(target_os = "macos")]
use crate::{deployment_target, project_root, tmp_verify};
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::Command;

/// Print a one-line warning when the same plugin is installed under
/// both user and system scope. Both copies are valid bundles; the
/// host picks one at scan time and shadows the other, which is a
/// frequent cause of "DAW loads my old build" support questions.
fn warn_on_scope_collision(format: Format, user_path: &Path, system_path: &Path) {
    // On platforms with no distinct system-scope plug-in dir (Linux,
    // Windows for some formats), `InstallScope::User` and `::System`
    // resolve to the same path - a single install can't shadow itself.
    if user_path == system_path {
        return;
    }
    if user_path.exists() && system_path.exists() {
        eprintln!(
            "    {} {} installed in both scopes:",
            tag_warn(),
            format.label(),
        );
        eprintln!("        • user:   {}", user_path.display());
        eprintln!("        • system: {}", system_path.display());
        eprintln!(
            "        Hosts pick one at scan time; remove the stale copy with \
             `cargo truce uninstall --user` or `--system`."
        );
    }
}

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
    if s.is_empty() { None } else { Some(s) }
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
        "    {} collision: {} other bundle(s) also claim {}/{}/{}",
        tag_warn(),
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

// `vst2_explicit` is only consulted on non-macOS (the smoke validator
// always runs on macOS), so the assignments are dead on the macOS build.
#[cfg_attr(target_os = "macos", allow(unused_variables, unused_assignments))]
#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_validate(args: &[String]) -> Res {
    let config = load_config()?;

    let mut run_auval = false;
    let mut run_auval_v3 = false;
    let mut run_pluginval = false;
    let mut run_clap = false;
    let mut run_vst2 = false;
    // Track explicit per-format flags so a missing validator counts as a
    // failure for CI (`--clap`, `--pluginval`, …) but stays a warning for
    // a casual `cargo truce validate` run on a host that's missing some
    // tools. `--all` keeps the casual semantics.
    let mut auval_explicit = false;
    let mut auval_v3_explicit = false;
    let mut pluginval_explicit = false;
    let mut clap_explicit = false;
    let mut vst2_explicit = false;
    let mut plugin_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--auval" => {
                run_auval = true;
                auval_explicit = true;
            }
            "--auval3" => {
                run_auval_v3 = true;
                auval_v3_explicit = true;
            }
            "--pluginval" => {
                run_pluginval = true;
                pluginval_explicit = true;
            }
            "--clap" => {
                run_clap = true;
                clap_explicit = true;
            }
            "--vst2" => {
                run_vst2 = true;
                vst2_explicit = true;
            }
            "--all" => {
                run_auval = true;
                run_auval_v3 = true;
                run_pluginval = true;
                run_clap = true;
                run_vst2 = true;
            }
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
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

    let plugins: Vec<&PluginDef> = super::pick_plugins(&config, plugin_filter.as_deref())?;

    let mut failures = 0;

    // auval (macOS only, AU v2)
    if run_auval {
        eprintln!("auval (AU v2)\n");
        if Command::new("auval").arg("-h").output().is_ok() {
            for p in &plugins {
                #[cfg(target_os = "macos")]
                {
                    let user_path = InstallScope::User
                        .au_v2_dir()
                        .join(format!("{}.component", p.name));
                    let system_path = InstallScope::System
                        .au_v2_dir()
                        .join(format!("{}.component", p.name));
                    warn_on_scope_collision(Format::Au2, &user_path, &system_path);
                }
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
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
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
            if auval_explicit {
                failures += 1;
            }
        }
    }

    // auval (AU v3 appex)
    if run_auval_v3 {
        eprintln!("\nauval (AU v3)\n");
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
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
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
            if auval_v3_explicit {
                failures += 1;
            }
        }
    }

    // pluginval (VST3)
    if run_pluginval {
        eprintln!("\npluginval (VST3)\n");
        let pluginval = find_pluginval();
        if let Some(pv) = pluginval {
            for p in &plugins {
                let user_path = InstallScope::User
                    .vst3_dir()
                    .join(format!("{}.vst3", p.name));
                let system_path = InstallScope::System
                    .vst3_dir()
                    .join(format!("{}.vst3", p.name));
                // Validate the system bundle when it's there (the
                // historical default), else fall through to user.
                let validate_path = if system_path.exists() {
                    system_path.clone()
                } else if user_path.exists() {
                    user_path.clone()
                } else {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                };
                eprint!("  {} ... ", p.name);
                let output = Command::new(&pv)
                    .args([
                        "--validate",
                        validate_path.to_str().unwrap(),
                        "--strictness-level",
                        "10",
                    ])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stdout.contains("SUCCESS") || output.status.success() {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
                    failures += 1;
                }
                warn_on_scope_collision(Format::Vst3, &user_path, &system_path);
            }
        } else {
            eprintln!("  pluginval not found. Install from https://github.com/Tracktion/pluginval");
            if pluginval_explicit {
                failures += 1;
            }
        }
    }

    // clap-validator (CLAP)
    if run_clap {
        eprintln!("\nclap-validator (CLAP)\n");
        let clap_validator = find_clap_validator();
        if let Some(cv) = clap_validator {
            // Project-local scratch for the macOS bundle-wrap fallback.
            // `cargo clean` sweeps it, and it stays off the system
            // `/tmp` so nothing outside the repo gets touched. On
            // Linux/Windows we hand clap-validator the installed file
            // directly, so the scratch dir is never created there.
            #[cfg(target_os = "macos")]
            let scratch = {
                let s = tmp_verify().join("clap-validate");
                let _ = fs::create_dir_all(&s);
                s
            };

            for p in &plugins {
                let clap_name = format!("{}.clap", p.name);
                let user_path = InstallScope::User.clap_dir().join(&clap_name);
                let system_path = InstallScope::System.clap_dir().join(&clap_name);
                // Prefer the user-scope bundle (the default install
                // location); fall through to system-scope if the
                // user installed there instead.
                let installed = if user_path.exists() {
                    user_path.clone()
                } else {
                    system_path.clone()
                };

                if !installed.exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }

                // CLAP plugin shape is per-platform:
                //   macOS: a `.clap` *bundle* directory with a binary
                //          at `Contents/MacOS/<name>`. The scratch-
                //          bundle branch below is a fallback for
                //          flat-file `.clap` installs that some
                //          third-party tools still produce; truce's
                //          own installer writes the bundle layout.
                //   Linux:   a `.so` renamed `.clap`. dlopen-loadable
                //          directly - no bundle.
                //   Windows: a `.dll` renamed `.clap`. LoadLibrary-
                //          loadable directly - no bundle.
                #[cfg(target_os = "macos")]
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
                #[cfg(not(target_os = "macos"))]
                let validate_path = installed.clone();

                eprint!("  {} ... ", p.name);
                let output = Command::new(&cv)
                    .args(["validate", &validate_path.to_string_lossy()])
                    .output()?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{stdout}{stderr}");

                if output.status.success() && !combined.contains("FAILED") {
                    eprintln!("PASS{}", parse_clap_summary(&combined));
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
                    failures += 1;
                }
                warn_on_scope_collision(Format::Clap, &user_path, &system_path);
            }

            #[cfg(target_os = "macos")]
            let _ = fs::remove_dir_all(&scratch);
        } else {
            eprintln!("  clap-validator not found.");
            eprintln!(
                "  Install: cargo install --git https://github.com/free-audio/clap-validator"
            );
            eprintln!("  Or set CLAP_VALIDATOR=/path/to/clap-validator");
            if clap_explicit {
                failures += 1;
            }
        }
    }

    // VST2 binary smoke (no industry validator; this is ours)
    if run_vst2 {
        eprintln!("VST2 binary smoke\n");
        #[cfg(target_os = "macos")]
        {
            failures += validate_vst2_macos(&plugins);
        }
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("  Skipping: VST2 binary smoke is currently macOS-only.");
            let _ = &plugins;
            if vst2_explicit {
                failures += 1;
            }
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

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce validate [--auval] [--auval3] [--pluginval] [--clap] [--vst2]
                            [--all] [-p <crate>]

Run validation tools on installed plugins. With no flag, runs every
available validator.

Options:
  --auval          AU v2 validation via auval (macOS).
  --auval3         AU v3 validation via auval (macOS).
  --pluginval      VST3 validation via pluginval.
  --clap           CLAP validation via clap-validator.
  --vst2           VST2 dlopen + AEffect smoke (macOS).
  --all            Run every available validator (default).
  -p <crate>       Validate only the plugin with this cargo crate name.
  -h, --help       Show this message"
    );
}

/// Build each plugin as a VST2 dylib, dlopen it via the C smoke binary
/// at `crates/truce-vst2/validate/binary_smoke.c`, and verify
/// `VSTPluginMain` returns a well-formed `AEffect`. macOS-only because
/// the smoke binary uses `dlfcn.h` and we hardcode `.dylib` here.
/// Returns the failure count.
#[cfg(target_os = "macos")]
fn validate_vst2_macos(plugins: &[&PluginDef]) -> usize {
    let root = project_root();
    let test_src = root.join("crates/truce-vst2/validate/binary_smoke.c");
    if !test_src.exists() {
        eprintln!(
            "  Skipping: smoke source missing at {}.",
            test_src.display()
        );
        return 0;
    }

    let test_bin = truce_build::target_dir(&root).join("vst2_binary_smoke");
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
        // Cross-scope collision check first - visible regardless of
        // whether the smoke binary builds. Two installed `.vst` bundles
        // are valid on disk but only one will be loaded by any given
        // host scan.
        let user_path = InstallScope::User
            .vst2_dir()
            .join(format!("{}.vst", p.name));
        let system_path = InstallScope::System
            .vst2_dir()
            .join(format!("{}.vst", p.name));
        warn_on_scope_collision(Format::Vst2, &user_path, &system_path);

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

        let dylib =
            truce_build::target_dir(&root).join(format!("release/lib{}.dylib", p.dylib_stem()));
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

/// Pull the test counts out of clap-validator's summary line, e.g.
/// `"20 tests run, 16 passed, 0 failed, 4 skipped, 1 warnings"`. Returns
/// `" (16/20, 4 skipped)"` or an empty string if the summary isn't found.
fn parse_clap_summary(output: &str) -> String {
    let Some(summary) = output.lines().find(|l| l.contains("tests run")) else {
        return String::new();
    };
    let pick = |key: &str| -> Option<u32> {
        let idx = summary.find(key)?;
        summary[..idx]
            .split(|c: char| !c.is_ascii_digit())
            .rfind(|s| !s.is_empty())?
            .parse()
            .ok()
    };
    match (pick("tests run"), pick("passed"), pick("skipped")) {
        (Some(total), Some(passed), Some(skipped)) if skipped > 0 => {
            format!(" ({passed}/{total}, {skipped} skipped)")
        }
        (Some(total), Some(passed), _) => format!(" ({passed}/{total})"),
        _ => String::new(),
    }
}

fn find_pluginval() -> Option<String> {
    // Env-var override takes precedence - CI uses it to point at a
    // cached download outside the standard locations.
    if let Ok(path) = std::env::var("PLUGINVAL")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    // Common locations.
    let candidates = [
        "/Applications/pluginval.app/Contents/MacOS/pluginval",
        "/usr/local/bin/pluginval",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    // PATH lookup.
    if Command::new("pluginval").arg("--help").output().is_ok() {
        return Some("pluginval".to_string());
    }
    None
}

fn find_clap_validator() -> Option<String> {
    // Check env var override
    if let Ok(path) = std::env::var("CLAP_VALIDATOR")
        && Path::new(&path).exists()
    {
        return Some(path);
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
