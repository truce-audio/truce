//! `cargo truce validate` - drive auval (AU v2/v3), pluginval (VST3), and
//! clap-validator (CLAP) against the project's installed bundles, with
//! shadow-install collision detection.

use crate::format::Format;
use crate::install_scope::InstallScope;
#[cfg(target_os = "macos")]
use crate::tmp_verify;
use crate::{PluginDef, Res, dirs, load_config, tag_warn};
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;

/// A `Command` for a validator that loads the plugin bundle, with
/// cargo-injected dynamic-linker vars scrubbed. `cargo run -- validate`
/// injects `DYLD_FALLBACK_LIBRARY_PATH` (macOS) / `LD_LIBRARY_PATH`
/// (Linux) pointing at target/debug deps; inherited by a child that
/// `dlopen`s the bundle, they break its dylib resolution (pluginval
/// scanned zero types; clap-validator, auval, and the AAX runner load
/// the same way). The wider `DYLD_*` family is scrubbed for the same
/// reason - any of them can redirect the bundle's resolution.
fn validator_command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    for var in [
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_INSERT_LIBRARIES",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
    ] {
        cmd.env_remove(var);
    }
    cmd
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::PathBuf;
use std::process::Command;

/// Which AAX check `--aax` runs, from lightest to heaviest. `load` uses
/// `pluginrunner` (a load / ABI check); the rest are test sets of Avid's
/// AAX Plug-In Validator (the `DigiShell` `aaxval` dish, via
/// `AAX_VALIDATOR`).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum AaxTestSet {
    /// `pluginrunner` load / ABI check - no full validator needed. Selected
    /// explicitly (otherwise it's only the fallback when `AAX_VALIDATOR`
    /// isn't set).
    Load,
    /// Curated fast checks (seconds): describe validation, data model,
    /// parameters, fast parameter traversal, load/unload. Skips the
    /// up-to-5-minute linear traversal and the page-table tests (truce
    /// ships no AAX page tables, so those always fail).
    #[default]
    Fast,
    /// The validator's complete `col_tests` suite (minutes per effect;
    /// includes the linear traversal and page-table tests).
    Full,
    /// Avid's marketplace `col_required` subset (describe + cycle counts).
    Required,
    /// Information-gathering tests only (`col_info`): product IDs, host and
    /// feature support. Always passes; prints the gathered data.
    Info,
}

impl AaxTestSet {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "load" | "pluginrunner" => Some(Self::Load),
            "fast" => Some(Self::Fast),
            "full" | "all" => Some(Self::Full),
            "required" => Some(Self::Required),
            "info" => Some(Self::Info),
            _ => None,
        }
    }

    // Only the AAX validation path (macOS / Windows) formats the label.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn label(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Fast => "fast",
            Self::Full => "full",
            Self::Required => "required",
            Self::Info => "info",
        }
    }
}

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
    let mut aax_tests = AaxTestSet::default();
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
            "--aax-tests" => {
                let v = crate::util::arg_value(args, &mut i, "--aax-tests")?;
                aax_tests = AaxTestSet::parse(v).ok_or_else(|| {
                    format!("unknown --aax-tests value '{v}' (fast|full|required|info)")
                })?;
                // Selecting a set implies running AAX validation.
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
                let output = validator_command("auval")
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
                let output = validator_command("auval")
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
                let mut cmd = validator_command(&pv);
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
                let mut cmd = validator_command(&cv);
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

    // AAX. Prefer Avid's full AAX Plug-In Validator (the DigiShell
    // `aaxval` dish, via `AAX_VALIDATOR`) - the real spec test suite, peer
    // to pluginval / clap-validator / auval. Fall back to `pluginrunner`
    // (a load / ABI check from Pro Tools Developer's CommandLineTools)
    // when only that is present; it catches ABI / load-time failures Pro
    // Tools otherwise reports only as cryptic "OOP cache generation timed
    // out" subprocess kills - by then the template's stderr message (e.g.
    // `[truce-aax] ABI version mismatch`) has been swallowed.
    if run_aax {
        eprintln!("\nAAX\n");
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            failures += validate_aax(&plugins, aax_explicit, aax_tests);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            eprintln!("  Skipping: AAX is macOS / Windows only.");
            let _ = (aax_explicit, aax_tests);
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
                            [--aax] [--aax-tests <set>] [--all]
                            [--skip-gui-tests] [-p <crate>]

Run validation tools on installed plugins. With no flag, runs every
available validator.

Options:
  --auval          AU v2 validation via auval (macOS).
  --auval3         AU v3 validation via auval (macOS).
  --pluginval      VST3 validation via pluginval.
  --clap           CLAP validation via clap-validator.
  --aax            AAX validation (macOS / Windows). Uses Avid's AAX
                   Plug-In Validator when AAX_VALIDATOR points at its
                   `dsh`; otherwise falls back to pluginrunner (a load
                   check).
  --aax-tests <set> Which AAX check to run (implies --aax), lightest to
                   heaviest. One of:
                     load     - pluginrunner load / ABI check (no full
                                validator needed)
                     fast     - curated quick checks (default; needs
                                AAX_VALIDATOR)
                     full     - complete suite (minutes; incl. the
                                5-min linear traversal + page tables)
                     required - Avid's marketplace-required subset
                     info     - information-gathering tests only
                   `fast`/`full`/`required`/`info` use Avid's AAX
                   Plug-In Validator (AAX_VALIDATOR); without it they
                   fall back to `load`.
  --all            Run every available validator (default).
  --skip-gui-tests Forwarded to pluginval as `--skip-gui-tests`. Use
                   on headless Linux CI without a GPU: the editor
                   probe needs FBConfigs the software-only GL stack
                   doesn't advertise.
  -p <crate>       Validate only the plugin with this cargo crate name.
  -h, --help       Show this message"
    );
}

/// Validate installed AAX bundles. Prefers Avid's full AAX Plug-In
/// Validator (`AAX_VALIDATOR` -> the `DigiShell` `aaxval` dish) and runs the
/// selected [`AaxTestSet`]; falls back to `pluginrunner` (a load / ABI
/// check) when the full validator isn't configured. AAX is always
/// system-scope, so we read from the canonical system path.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_aax(plugins: &[&PluginDef], aax_explicit: bool, tests: AaxTestSet) -> usize {
    // `--aax-tests load` explicitly wants the pluginrunner load check.
    if tests == AaxTestSet::Load {
        let Some(runner) = find_pluginrunner() else {
            eprintln!(
                "  pluginrunner not found (Pro Tools Developer / set \
                 PLUGINRUNNER=/path/to/pluginrunner)."
            );
            return usize::from(aax_explicit);
        };
        return validate_aax_pluginrunner(&runner, plugins);
    }
    // A test set: prefer the full validator, fall back to pluginrunner.
    if let Some(dsh) = find_aax_validator() {
        return validate_aax_dsh(&dsh, plugins, tests);
    }
    if let Some(runner) = find_pluginrunner() {
        eprintln!(
            "  note: AAX_VALIDATOR not set - falling back to pluginrunner (a load check). \
             Set AAX_VALIDATOR=/path/to/dsh for `--aax-tests {}`, or pass `--aax-tests load` \
             to select the load check explicitly.",
            tests.label()
        );
        return validate_aax_pluginrunner(&runner, plugins);
    }
    eprintln!(
        "  No AAX validator found. Set AAX_VALIDATOR=/path/to/dsh (Avid AAX Plug-In \
         Validator) for full validation, or install Pro Tools Developer / set \
         PLUGINRUNNER=/path/to/pluginrunner for a basic load check."
    );
    usize::from(aax_explicit)
}

/// Per-plugin tally from a `dsh` session: total result rows, passes, and
/// the distinct `id (status)` labels of the non-passing ones.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Default)]
struct AaxTally {
    total: usize,
    passed: usize,
    failed: Vec<String>,
}

/// Run Avid's full AAX Plug-In Validator (the `DigiShell` `aaxval` dish)
/// against every installed bundle in a single `dsh` session. One session
/// is both the tool's intended usage and load-bearing for correctness:
/// the audio-engine host cold-starts on the session's first test event and
/// aborts it (`E_ABORTED`), so a per-plugin process would fail each
/// plugin's first test. A throwaway warm-up test absorbs that abort; every
/// real test then runs on a warm engine. `dsh` reads commands from stdin
/// and writes yaml `cmd_result` blocks to stdout, each carrying a
/// `result_status` (`E_COMPLETED_PASS` on success).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_aax_dsh(dsh: &str, plugins: &[&PluginDef], tests: AaxTestSet) -> usize {
    use std::io::Write;
    use std::process::Stdio;

    eprintln!("  Avid AAX Plug-In Validator ({} tests)", tests.label());

    // Partition into installed / missing; the missing get a SKIP line.
    let mut installed: Vec<(&&PluginDef, String)> = Vec::new();
    let mut failures = 0;
    for p in plugins {
        let bundle = aax_install_dir().join(format!("{}.aaxplugin", p.file_stem()));
        if bundle.exists() {
            installed.push((p, bundle.to_string_lossy().into_owned()));
        } else {
            eprintln!("  {} ... SKIP (not installed)", p.name);
        }
    }
    if installed.is_empty() {
        return failures;
    }
    // The whole run is one `dsh` session (see below), so per-plugin results
    // only print once it finishes - warn up front for a multi-plugin run so
    // the wait doesn't look like a hang.
    if installed.len() > 1 {
        eprintln!(
            "  Validating {} plugins in one session; results print when it finishes...",
            installed.len()
        );
    }

    let paths: Vec<&str> = installed.iter().map(|(_, path)| path.as_str()).collect();
    let script = dsh_session_script(&paths, tests);
    let mut child = match validator_command(dsh)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  INVOKE ERROR ({e})");
            return failures + installed.len();
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
        // Dropping `stdin` closes it, so `dsh` sees EOF and exits after the
        // trailing `exit` command.
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("  INVOKE ERROR ({e})");
            return failures + installed.len();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let by_bundle = parse_dsh_session(&stdout);

    if by_bundle.is_empty() {
        // Nothing ran - the dish failed to load or the validator errored
        // (often PACE / iLok not running). Surface stdout/stderr once.
        eprintln!("  ERROR (no test results - is iLok/PACE running?)");
        let stderr = String::from_utf8_lossy(&output.stderr);
        for text in [stdout.trim(), stderr.trim()] {
            if !text.is_empty() {
                eprintln!("{text}");
            }
        }
        return failures + installed.len();
    }

    for (p, path) in &installed {
        eprint!("  {} ... ", p.name);
        match by_bundle.get(path) {
            Some(t) if t.total > 0 && t.failed.is_empty() => {
                eprintln!("PASS ({}/{})", t.passed, t.total);
            }
            Some(t) if t.total > 0 => {
                eprintln!("FAIL ({}/{})", t.passed, t.total);
                for label in &t.failed {
                    eprintln!("      - {label}");
                }
                failures += 1;
            }
            _ => {
                eprintln!("ERROR (no results)");
                failures += 1;
            }
        }
    }
    failures
}

/// Build the `dsh` script for a whole session: load the dish, a throwaway
/// warm-up test (absorbs the cold-start `E_ABORTED`), then the chosen test
/// set for every bundle, then exit. `bundles` is non-empty.
///
/// The `fast` set runs a curated list of quick tests per bundle (the
/// built-in `col_tests` collection includes an up-to-5-minute linear
/// traversal and page-table tests truce doesn't implement); the others map
/// onto the validator's own collections via `runtests`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn dsh_session_script(bundles: &[&str], tests: AaxTestSet) -> String {
    // Curated fast checks, skipping the linear traversal and page tables.
    const FAST_TESTS: &[&str] = &[
        "test.describe_validation",
        "test.data_model",
        "test.parameters",
        "test.parameter_traversal.random.fast",
        "test.load_unload",
    ];
    use std::fmt::Write as _;
    let mut script = String::from("load_dish aaxval\n");
    // Warm-up: its result block is dropped by `parse_dsh_session`.
    let _ = writeln!(
        script,
        "runtest {{test: test.describe_validation, path: \"{}\", stringformat: yaml}}",
        bundles[0]
    );
    for bundle in bundles {
        match tests {
            // `Load` is handled by the dispatcher (pluginrunner) and never
            // reaches here; treat it as the fast set defensively.
            AaxTestSet::Fast | AaxTestSet::Load => {
                for t in FAST_TESTS {
                    let _ = writeln!(
                        script,
                        "runtest {{test: {t}, path: \"{bundle}\", stringformat: yaml}}"
                    );
                }
            }
            AaxTestSet::Full => coll_runtests(&mut script, "col_tests", bundle),
            AaxTestSet::Required => coll_runtests(&mut script, "col_required", bundle),
            AaxTestSet::Info => coll_runtests(&mut script, "col_info", bundle),
        }
    }
    script.push_str("exit\n");
    script
}

/// Append a `runtests {coll: ..., path: ...}` line for a collection.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn coll_runtests(script: &mut String, collection: &str, bundle_path: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(
        script,
        "runtests {{coll: {collection}, path: \"{bundle_path}\", stringformat: yaml}}"
    );
}

/// Parse a whole-session `dsh` transcript into a per-bundle [`AaxTally`].
/// Each result block prints `bundle_path:`, then `id: <test>`, then one or
/// more `result_status:` lines (one per Effect). The first result block is
/// the warm-up and is dropped. A status other than `E_COMPLETED_PASS`
/// (including the cold-start `E_ABORTED`) is a failure, labeled
/// `id (status)` and deduplicated per bundle.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn parse_dsh_session(stdout: &str) -> std::collections::HashMap<String, AaxTally> {
    let mut by_bundle: std::collections::HashMap<String, AaxTally> =
        std::collections::HashMap::new();
    let mut cur_bundle = String::new();
    let mut cur_id = String::new();
    // `id:` lines delimit test blocks; block 1 is the warm-up (dropped).
    let mut block_index = 0usize;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(b) = trimmed.strip_prefix("bundle_path: ") {
            cur_bundle = b.to_string();
        } else if let Some(id) = trimmed.strip_prefix("id: ") {
            cur_id = id.to_string();
            block_index += 1;
        } else if let Some(status) = trimmed.strip_prefix("result_status: ") {
            if block_index <= 1 {
                continue; // warm-up block
            }
            let tally = by_bundle.entry(cur_bundle.clone()).or_default();
            tally.total += 1;
            if status == "E_COMPLETED_PASS" {
                tally.passed += 1;
            } else {
                let label = format!("{cur_id} ({status})");
                if !tally.failed.contains(&label) {
                    tally.failed.push(label);
                }
            }
        }
    }
    by_bundle
}

/// Path to the Avid AAX Plug-In Validator's `dsh` binary, from
/// `AAX_VALIDATOR`. Read via the build-env resolver (shell env or
/// `.cargo/config.toml [env]`, same as `AAX_SDK_PATH`), since there's no
/// standard install location - it's a downloaded developer package. A
/// stale value warns and resolves to `None`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn find_aax_validator() -> Option<String> {
    let raw = crate::read_build_env("AAX_VALIDATOR")?;
    if Path::new(&raw).exists() {
        return Some(raw);
    }
    eprintln!(
        "warning: AAX_VALIDATOR={raw} (from .cargo/config.toml [env] or shell env) but \
         file does not exist"
    );
    None
}

/// Run Avid's `pluginrunner` against each installed AAX bundle.
/// `pluginrunner` exits non-zero on bridge load failures (ABI mismatch,
/// dlopen errors, codesign breakage) without needing a Pro Tools rescan,
/// so this catches ABI drift before the user notices plugins missing in
/// the host.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_aax_pluginrunner(runner: &str, plugins: &[&PluginDef]) -> usize {
    eprintln!("  pluginrunner (load check)");
    let mut failures = 0;
    for p in plugins {
        let bundle = aax_install_dir().join(format!("{}.aaxplugin", p.file_stem()));
        if !bundle.exists() {
            eprintln!("  {} ... SKIP (not installed)", p.name);
            continue;
        }
        eprint!("  {} ... ", p.name);
        let output = validator_command(runner).arg(&bundle).output();
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
pub(crate) fn find_pluginrunner() -> Option<String> {
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

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn dsh_session_drops_warmup_and_tallies_per_bundle() {
        // A session transcript: the load-dish block (no result), the
        // warm-up block (dropped), then two plugins. Plugin A passes;
        // plugin B fails one test. The warm-up shares plugin A's path but
        // must not inflate its tally.
        let out = "\
dsh> load_dish aaxval
---
cmd_result:
  loaded_dishes_count: 1
...
---
cmd_result:
  bundle_path: /A.aaxplugin
  id: test.describe_validation
  results:
    - connection_id: 0.0
      result_status: E_ABORTED
...
---
cmd_result:
  bundle_path: /A.aaxplugin
  id: test.describe_validation
  results:
    - connection_id: 1.0
      result_status: E_COMPLETED_PASS
...
---
cmd_result:
  bundle_path: /A.aaxplugin
  id: test.data_model
  results:
    - connection_id: 2.0
      result_status: E_COMPLETED_PASS
...
---
cmd_result:
  bundle_path: /B.aaxplugin
  id: test.describe_validation
  results:
    - connection_id: 3.0
      result_status: E_COMPLETED_PASS
...
---
cmd_result:
  bundle_path: /B.aaxplugin
  id: test.page_table.load
  results:
    - connection_id: 4.0
      result_status: E_COMPLETED_FAIL
...
";
        let map = parse_dsh_session(out);
        let a = map.get("/A.aaxplugin").expect("plugin A tallied");
        assert_eq!((a.total, a.passed), (2, 2));
        assert!(a.failed.is_empty());
        let b = map.get("/B.aaxplugin").expect("plugin B tallied");
        assert_eq!((b.total, b.passed), (2, 1));
        assert_eq!(
            b.failed,
            vec!["test.page_table.load (E_COMPLETED_FAIL)".to_string()]
        );
    }

    #[test]
    fn dsh_session_empty_when_no_tests() {
        // A dish-load failure (no result blocks) yields no bundles, so the
        // caller reports an error rather than a false pass.
        assert!(parse_dsh_session("dsh> load_dish aaxval\nerror\n").is_empty());
    }

    #[test]
    fn dsh_session_fast_skips_linear_and_page_table() {
        let s = dsh_session_script(&["/X.aaxplugin"], AaxTestSet::Fast);
        // First runtest is the warm-up (describe), then the real fast set.
        assert!(s.contains("test.parameter_traversal.random.fast"));
        assert!(!s.contains("parameter_traversal.linear"));
        assert!(!s.contains("page_table"));
        assert!(s.starts_with("load_dish aaxval\n") && s.ends_with("exit\n"));
    }

    #[test]
    fn dsh_session_full_uses_col_tests_per_bundle() {
        let s = dsh_session_script(&["/A.aaxplugin", "/B.aaxplugin"], AaxTestSet::Full);
        assert!(s.contains("runtests {coll: col_tests, path: \"/A.aaxplugin\""));
        assert!(s.contains("runtests {coll: col_tests, path: \"/B.aaxplugin\""));
    }
}
