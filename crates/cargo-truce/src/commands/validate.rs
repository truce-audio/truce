//! `cargo truce validate` - drive auval (AU v2/v3), pluginval (VST3), and
//! clap-validator (CLAP) against the project's installed bundles, with
//! shadow-install collision detection.

use crate::format::Format;
use crate::install_scope::InstallScope;
#[cfg(target_os = "macos")]
use crate::tmp_verify;
use crate::{PluginDef, Res, dirs, load_config, tag_warn};
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "windows"))]
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

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_validate(args: &[String]) -> Res {
    let config = load_config()?;

    let mut run_auval = false;
    let mut run_auval_v3 = false;
    let mut run_pluginval = false;
    let mut run_clap = false;
    let mut run_aax = false;
    // Track explicit per-format flags so a missing validator counts as a
    // failure for CI (`--clap`, `--pluginval`, …) but stays a warning for
    // a casual `cargo truce validate` run on a host that's missing some
    // tools. `--all` keeps the casual semantics.
    let mut auval_explicit = false;
    let mut auval_v3_explicit = false;
    let mut pluginval_explicit = false;
    let mut clap_explicit = false;
    let mut aax_explicit = false;
    let mut plugin_filter: Option<String> = None;
    // Forwarded to pluginval. Lets CI skip the editor-instantiation
    // probe on hosts where the GL stack can't satisfy a real plugin
    // editor's `glXChooseFBConfig` call (headless Linux runners with
    // no GPU + software-only Xorg/Xvfb don't advertise FBConfigs).
    let mut skip_gui_tests = false;

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
            "--aax" => {
                run_aax = true;
                aax_explicit = true;
            }
            "--all" => {
                run_auval = true;
                run_auval_v3 = true;
                run_pluginval = true;
                run_clap = true;
                run_aax = true;
            }
            "--skip-gui-tests" => {
                skip_gui_tests = true;
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
    if !run_auval && !run_auval_v3 && !run_pluginval && !run_clap && !run_aax {
        run_auval = true;
        run_auval_v3 = true;
        run_pluginval = true;
        run_clap = true;
        run_aax = true;
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
                        .join(format!("{}.component", p.file_stem()));
                    let system_path = InstallScope::System
                        .au_v2_dir()
                        .join(format!("{}.component", p.file_stem()));
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
                            p.file_stem()
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
                    .join(format!("{}.vst3", p.file_stem()));
                let system_path = InstallScope::System
                    .vst3_dir()
                    .join(format!("{}.vst3", p.file_stem()));
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
                let mut cmd = Command::new(&pv);
                // `cargo run -- validate` injects DYLD_FALLBACK_LIBRARY_PATH
                // (target/debug deps) into our env; inherited by pluginval
                // it breaks the bundle's dylib resolution and the scan
                // reports zero types. Scrub the DYLD vars for the child.
                cmd.env_remove("DYLD_FALLBACK_LIBRARY_PATH");
                cmd.env_remove("DYLD_LIBRARY_PATH");
                cmd.args([
                    "--validate",
                    validate_path.to_str().unwrap(),
                    "--strictness-level",
                    "10",
                ]);
                if skip_gui_tests {
                    cmd.arg("--skip-gui-tests");
                }
                let output = cmd.output()?;
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
                let clap_name = format!("{}.clap", p.file_stem());
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
                let mut cmd = Command::new(&cv);
                cmd.args(["validate", &validate_path.to_string_lossy()]);
                // clap-validator requires location paths to start
                // with '/', but the CLAP header defines FILE
                // locations as OS paths ('\' separators work on
                // Windows) - so spec-compliant Windows paths can
                // never pass its preset-discovery tests (Surge XT
                // fails them identically). Skip those tests here
                // until the validator accepts Windows paths.
                #[cfg(target_os = "windows")]
                cmd.args(["--test-filter", "preset-discovery", "--invert-filter"]);
                let output = cmd.output()?;

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

    // pluginrunner (AAX). Ships with Pro Tools Developer's CommandLineTools.
    // Catches ABI / load-time failures that Pro Tools' own scanner
    // reports only as cryptic "OOP cache generation timed out"
    // subprocess kills - by then the actual stderr message from the
    // template (e.g. `[truce-aax] ABI version mismatch`) has been
    // swallowed.
    if run_aax {
        eprintln!("\npluginrunner (AAX)\n");
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            failures += validate_aax(&plugins, aax_explicit);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            eprintln!("  Skipping: AAX is macOS / Windows only.");
            let _ = aax_explicit;
        }
    }

    // VST2's binary-surface smoke (dlopen + AEffect probe) used to
    // live here under `--vst2`. It moved to a regular cargo
    // integration test (`crates/truce-vst2/tests/binary_smoke.rs`) -
    // the C harness is a framework-internal asset and plugin authors
    // running `cargo truce validate` against their own crate should
    // never see references to it.

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
Usage: cargo truce validate [--auval] [--auval3] [--pluginval] [--clap]
                            [--aax] [--all] [--skip-gui-tests]
                            [-p <crate>]

Run validation tools on installed plugins. With no flag, runs every
available validator.

Options:
  --auval          AU v2 validation via auval (macOS).
  --auval3         AU v3 validation via auval (macOS).
  --pluginval      VST3 validation via pluginval.
  --clap           CLAP validation via clap-validator.
  --aax            AAX validation via pluginrunner (Pro Tools
                   Developer's CommandLineTools, macOS / Windows).
  --all            Run every available validator (default).
  --skip-gui-tests Forwarded to pluginval as `--skip-gui-tests`. Use
                   on headless Linux CI without a GPU: the editor
                   probe needs FBConfigs the software-only GL stack
                   doesn't advertise.
  -p <crate>       Validate only the plugin with this cargo crate name.
  -h, --help       Show this message"
    );
}

/// Run Avid's `pluginrunner` against each installed AAX bundle. AAX
/// is always system-scope, so we read from the canonical system path.
/// `pluginrunner` exits non-zero on bridge load failures (ABI
/// mismatch, dlopen errors, codesign breakage) without needing a Pro
/// Tools rescan, so this catches ABI drift before the user notices
/// plugins missing in the host.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_aax(plugins: &[&PluginDef], aax_explicit: bool) -> usize {
    let Some(runner) = find_pluginrunner() else {
        eprintln!(
            "  pluginrunner not found at /Applications/Pro Tools Developer.app/\
             Contents/CommandLineTools/pluginrunner. Install Pro Tools \
             Developer to enable AAX validation, or set PLUGINRUNNER=/path/to/pluginrunner."
        );
        return usize::from(aax_explicit);
    };

    let mut failures = 0;
    for p in plugins {
        let bundle = aax_install_dir().join(format!("{}.aaxplugin", p.file_stem()));
        if !bundle.exists() {
            eprintln!("  {} ... SKIP (not installed)", p.name);
            continue;
        }
        eprint!("  {} ... ", p.name);
        let output = Command::new(&runner).arg(&bundle).output();
        let output = match output {
            Ok(o) => o,
            Err(e) => {
                eprintln!("INVOKE ERROR ({e})");
                failures += 1;
                continue;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // pluginrunner exits 0 even when the truce template refuses
        // to load (the bridge prints to stderr and returns control
        // cleanly), so the stderr banner is the reliable signal.
        let bridge_failed = stderr.contains("ABI version mismatch")
            || stderr.contains("Failed to load Rust plugin");
        if output.status.success() && !bridge_failed {
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
    }
    failures
}

#[cfg(target_os = "macos")]
fn aax_install_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/Avid/Audio/Plug-Ins")
}

#[cfg(target_os = "windows")]
fn aax_install_dir() -> PathBuf {
    PathBuf::from("C:\\Program Files\\Common Files\\Avid\\Audio\\Plug-Ins")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn find_pluginrunner() -> Option<String> {
    if let Ok(path) = std::env::var("PLUGINRUNNER")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    #[cfg(target_os = "macos")]
    let candidates = [
        "/Applications/Pro Tools Developer.app/Contents/CommandLineTools/pluginrunner",
        "/Applications/Pro Tools.app/Contents/CommandLineTools/pluginrunner",
    ];
    #[cfg(target_os = "windows")]
    let candidates = [
        "C:\\Program Files\\Avid\\Pro Tools Developer\\pluginrunner.exe",
        "C:\\Program Files\\Avid\\Pro Tools\\pluginrunner.exe",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    None
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
