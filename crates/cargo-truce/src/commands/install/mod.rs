//! `cargo truce install` - build per-format dylibs and install into the
//! standard plug-in directories.

use crate::format::Format;
use crate::install_scope::{InstallScope, effective_scope, note_once, set_cli_install_scope};
use crate::util::{fs_ctx, parse_target_cpu_arg};
use crate::{
    Config, PluginDef, Res, deployment_target, detect_default_features, load_config, project_root,
};
// `run_sudo` shells out to `/usr/bin/sudo` and is therefore macOS-only.
// Windows admin elevation is per-process; the non-macOS install branches
// below use `fs_ctx` directly and surface the OS-level EACCES if the user
// isn't elevated for a system-scope write.
//
// `tmp_lv2` is only consumed by `install_lv2`'s macOS sudo-stage path
// (Windows/Linux write straight into the destination), so it joins the
// same cfg-gate to keep the non-macOS build clean.
#[cfg(target_os = "macos")]
use crate::{run_sudo, tmp_lv2};
// CLAP / VST3 / VST2 read the cdylib from `release_lib` on non-macOS
// targets only; on macOS those formats consume the bundle-bin produced
// by the `clang -bundle` link step, so `release_lib` is unused there.
#[cfg(not(target_os = "macos"))]
use crate::release_lib;
// Plist scratch (VST3 / VST2 / AU) only happens on macOS - gate the
// import so Windows / Linux builds don't see it as unused.
#[cfg(target_os = "macos")]
use crate::tmp_manifests;
#[cfg(target_os = "macos")]
use crate::{codesign_bundle, dirs};
// `OsStr` (run_sudo args) and `fs` (AU cache wipe, pre-install
// remove_dir) are only touched from macOS-gated branches below.
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;

// AAX is macOS / Windows; AU is macOS only.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) mod aax;
#[cfg(target_os = "macos")]
pub(crate) mod au_ios;
#[cfg(target_os = "macos")]
pub(crate) mod au_v3;
pub(crate) mod presets;

use presets::FactoryPresets;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use aax::install_aax;
#[cfg(target_os = "macos")]
use au_v3::build_and_install_au_v3;

/// Guarantee the param-manifest sidecar exists for every plugin that
/// ships presets, so install-time preset name resolution can't fail on a
/// missing one.
///
/// `truce::plugin!` / `#[derive(Params)]` write
/// `target/lv2-meta/<crate>/param_index.toml` as a compile-time side
/// effect, but cargo doesn't track it: delete it (or `cargo clean` only
/// that dir) while the crate stays cached and the next *incremental* build
/// won't re-run the macro, leaving preset install to abort with "no param
/// manifest". When the file is missing we force `cargo clean -p <crate>`
/// so the upcoming build recompiles the crate and regenerates it. A crate
/// that was never built has nothing to clean (the build writes the sidecar
/// naturally); only the deleted-but-cached case actually pays for it.
fn ensure_preset_sidecars(plugins: &[&PluginDef], root: &Path) -> Res {
    for p in plugins {
        let ships_presets = presets::authored_presets_dir(root, p).is_some_and(|d| d.is_dir());
        if !ships_presets {
            continue;
        }
        let sidecar = truce_build::target_dir(root)
            .join("lv2-meta")
            .join(&p.crate_name)
            .join("param_index.toml");
        if sidecar.exists() {
            continue;
        }
        crate::vprintln!(
            "  Param manifest for {} is missing; cleaning it so the build regenerates it.",
            p.crate_name
        );
        let status = std::process::Command::new("cargo")
            .arg("clean")
            .arg("-p")
            .arg(&p.crate_name)
            .status()
            .map_err(|e| format!("running `cargo clean -p {}`: {e}", p.crate_name))?;
        if !status.success() {
            return Err(format!(
                "`cargo clean -p {}` failed; needed to regenerate its preset param manifest",
                p.crate_name
            )
            .into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_install(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut ios = false;
    let mut ios_device = false;
    let mut no_build = false;
    let mut shell_mode = false;
    let mut debug = false;
    let mut target_cpu_arg: Option<String> = None;
    let mut plugin_filter: Option<String> = None;
    let mut cli_scope: Option<InstallScope> = None;
    let mut user_features: Vec<String> = Vec::new();

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
            "--ios" => ios = true,
            "--ios-device" => {
                ios = true;
                ios_device = true;
            }
            "--no-build" => no_build = true,
            "--shell" => shell_mode = true,
            "--debug" => debug = true,
            "--target-cpu" => {
                target_cpu_arg =
                    Some(crate::util::arg_value(args, &mut i, "--target-cpu")?.to_string());
            }
            "--user" => set_cli_install_scope(&mut cli_scope, InstallScope::User)?,
            "--system" => set_cli_install_scope(&mut cli_scope, InstallScope::System)?,
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo truce install` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--features" => {
                user_features.extend(crate::parse_extra_features(crate::util::arg_value(
                    args,
                    &mut i,
                    "--features",
                )?)?);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Extra Cargo features apply to every underlying build (desktop
    // formats, shell logic, iOS) via the global read by
    // `apply_extra_features`. Set before the iOS short-circuit below.
    crate::set_extra_features(user_features);

    // Scope is resolved per-format inside `scope_for` / `effective_scope`:
    // - explicit `--user` / `--system` wins (subject to hard upgrades);
    // - no flag falls back to the per-(format, OS) default (user
    //   everywhere except VST3 on Windows, which defaults to system).

    // In shell mode, `--debug` selects the *logic* profile. The
    // shell binary itself is always built into `target/shell/` via a
    // custom cargo profile; the second build (the logic dylib the
    // shell dlopens at runtime) defaults to release for better DSP
    // perf, with `--debug` flipping it to debug for fast iteration.

    // iOS short-circuit: AU v3 inside a container app is the only
    // viable iOS format. Drives the native Rust pipeline in
    // `au_ios::install_one`; `--ios` defaults to the simulator,
    // `--ios-device` switches to a tethered device install (real
    // signing identity + provisioning profile required).
    if ios {
        #[cfg(target_os = "macos")]
        {
            let target = if ios_device {
                au_ios::IosTarget::Device
            } else {
                au_ios::IosTarget::Simulator
            };
            return install_ios(plugin_filter.as_deref(), target);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = ios_device;
            return Err("iOS plugins build only on macOS (Xcode-only).".into());
        }
    }

    if !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax {
        // No format flags specified - enable all formats that the project supports.
        // Check which features are defined in the first plugin's Cargo.toml.
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
        vst2 = available.contains("vst2");
        lv2 = available.contains("lv2");
        // AU is macOS-only at runtime, but flip the flags on every platform
        // so the build/install paths can emit per-plugin skip lines for
        // Linux / Windows users with `"au"` in their `[features].default`.
        au2 = available.contains("au");
        au3 = available.contains("au");
        aax = available.contains("aax");
    }

    // Shell-mode preflight: bail early if the user's Cargo.toml is
    // missing `[profile.shell]`. Catching this here gives a one-line
    // copy-paste fix instead of cargo's terser "profile `shell` is not
    // declared" downstream.
    if shell_mode {
        crate::verify_shell_profile_declared()?;
    }

    // AU v3 + shell is unreliable: the appex's sandbox blocks
    // `dlopen` of arbitrary `target/` paths. Until the entitlement
    // workaround lands, warn and let the build proceed; the user
    // might still want the bundle for non-hot-reload smoke testing.
    if shell_mode && au3 && cfg!(target_os = "macos") {
        eprintln!(
            "note: AU v3 + --shell is unreliable. The appex sandbox blocks dlopen of \
             target/<profile>/lib<crate>.dylib, so hot-reload won't fire. Use --au2 \
             for hot-reload iteration; run `cargo truce install --au3` (no --shell) \
             for AU v3 smoke tests."
        );
    }

    // Filter plugins if -p specified
    let plugins: Vec<&PluginDef> = super::pick_plugins(&config, plugin_filter.as_deref())?;

    // Profile of the logic dylib that the shell dlopens at runtime.
    // Only meaningful when `shell_mode` is on.
    let logic_profile = if debug { "debug" } else { "release" };

    // For shell-mode builds the per-format cargo invocation uses the
    // custom `[profile.shell]` (defined in the user's Cargo.toml,
    // inherits from "release"). Output lands in `target/shell/`,
    // independent of `target/release/` and `target/debug/`. For the
    // non-shell case we honor `--debug` directly.
    if shell_mode {
        crate::set_build_profile("shell");
    } else {
        crate::set_debug_profile(debug);
    }
    let target_cpu = target_cpu_arg
        .as_deref()
        .map(parse_target_cpu_arg)
        .unwrap_or_default();
    crate::set_target_cpu(target_cpu);

    let root = project_root();
    let dt = &deployment_target();

    let mut extra_features = Vec::new();
    if shell_mode {
        extra_features.push("shell");
    }

    // Preset name resolution reads a param-manifest sidecar the plugin
    // macro writes at compile time; force a rebuild of any preset-shipping
    // plugin whose sidecar has gone missing so the upcoming build restores
    // it. Skipped under `--no-build` (nothing would rebuild it).
    if !no_build {
        ensure_preset_sidecars(&plugins, &root)?;
    }

    // --- Build ---
    //
    // One cargo invocation per (plugin, format) pair so the
    // name-override env vars can be applied per-plugin. Platform gates
    // (AU is macOS-only, AAX is macOS/Windows + SDK-configured) and
    // the format-suffix copy live inside `build_format_dylibs`; the
    // shared target cache absorbs the cost of one cargo invocation
    // per format.
    if !no_build {
        use super::build_dylibs::{BuildFormat, build_format_dylibs, build_logic_dylibs};
        let format_selection: &[(bool, BuildFormat)] = &[
            (clap, BuildFormat::Clap),
            (vst3, BuildFormat::Vst3),
            (vst2, BuildFormat::Vst2),
            (lv2, BuildFormat::Lv2),
            (au2, BuildFormat::Au2),
            (aax, BuildFormat::Aax),
        ];
        for &(selected, format) in format_selection {
            if selected {
                build_format_dylibs(format, &plugins, &extra_features, &config, &root, dt, None)?;
            }
        }

        // Shell mode: also build the logic dylibs the installed shells
        // dlopen at runtime. Profile follows `--debug` (release otherwise).
        if shell_mode {
            build_logic_dylibs(&plugins, logic_profile, dt)?;
        }
    }

    // --- Install ---
    //
    // Per-format scope is resolved through `effective_scope`, which
    // applies the per-(format, OS) default (when no CLI flag is set)
    // and silently downgrades AAX / AU v3 / Windows-VST2 to system
    // scope, emitting a one-line note (printed at most once per
    // message via `note_once`).
    // Only these formats re-envelope the loop's factory presets. AU v3
    // loads its own (after building its framework, so its sidecar exists),
    // and vst2 / aax have no preset support yet - so an au3-only (or vst2 /
    // aax-only) install must not read the param-manifest sidecar here,
    // which for `--au3` after a clean hasn't been built yet.
    let needs_loop_presets = clap || vst3 || lv2 || au2;

    for p in &plugins {
        // Parsed once per plugin; each format re-envelopes the same
        // canonical state blobs into its native preset files.
        let factory_presets = if needs_loop_presets {
            presets::load_factory_presets(&root, p, &config)?
        } else {
            None
        };
        let fp = factory_presets.as_ref();
        if clap {
            let s = scope_for(Format::Clap, cli_scope);
            install_clap(&root, p, &config, s, fp)?;
        }
        if vst3 {
            let s = scope_for(Format::Vst3, cli_scope);
            install_vst3(&root, p, &config, s, fp)?;
        }
        if vst2 {
            let s = scope_for(Format::Vst2, cli_scope);
            install_vst2(&root, p, &config, s)?;
        }
        if lv2 {
            let s = scope_for(Format::Lv2, cli_scope);
            install_lv2(&root, p, &config, s, fp)?;
        }
        if au2 {
            #[cfg(target_os = "macos")]
            {
                let s = scope_for(Format::Au2, cli_scope);
                install_au(&root, p, &config, s, fp)?;
            }
            // Non-macOS skip line was already pushed in the build phase.
        }
        if aax {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // AAX is always system-scope; the call to `scope_for`
                // exists for the side-effect (one-line note when the
                // user passed `--user`).
                let _ = scope_for(Format::Aax, cli_scope);
                install_aax(&root, p, &config)?;
            }
            // Non-macOS/Windows: build phase already pushed the single
            // platform-not-supported skip; nothing per-plugin to do.
        }
    }

    if au3 {
        #[cfg(target_os = "macos")]
        {
            // AU v3 is always system-scope on macOS - emit the note
            // once before delegating to the (system-only) installer.
            let _ = scope_for(Format::Au3, cli_scope);
            build_and_install_au_v3(&root, &config, &plugins, no_build)?;
        }
        #[cfg(not(target_os = "macos"))]
        crate::log_skip(
            "AU v3: not supported on this platform. Audio Unit is macOS-only.".to_string(),
        );
    }

    #[cfg(target_os = "macos")]
    if au2 && let Some(home) = dirs::home_dir() {
        let cache = home.join("Library/Caches/AudioUnitCache");
        let _ = fs::remove_dir_all(&cache);
        crate::vprintln!("Cleared AU cache.");
    }

    let installed = crate::take_outputs();
    if !installed.is_empty() {
        eprintln!("\nInstalled:");
        for line in installed {
            eprintln!("  {line}");
        }
    }
    let skipped = crate::take_skipped();
    if !skipped.is_empty() {
        eprintln!("\nSkipped:");
        for line in skipped {
            eprintln!("  {line}");
        }
    }
    eprintln!("\nDone. Restart your DAW to rescan.");
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce install [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax]
                           [--ios|--ios-device]
                           [--user|--system] [--shell] [--debug] [--no-build] [-p <crate>]
                           [--target-cpu <value>]

Build and install plugins into the host's plug-in directories. Defaults
to release. Defaults to whichever formats are in the plugin's Cargo.toml
default features (typically clap + vst3).

Per-format scope is per-user by default; pass --system for the shared
system directories. AAX and AU v3 are always system-scope. VST3 on
Windows defaults to system scope (the directory every commercial host
scans); pass --user for the per-user `%LOCALAPPDATA%\\Programs\\Common\\VST3`
location.

x86_64 builds default to `-C target-cpu=x86-64-v3` (AVX2 + FMA + BMI2)
so `wide`'s compile-time SIMD dispatch picks the wider path. aarch64
builds use NEON unconditionally and get no extra flag. Override with
`--target-cpu`.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --vst2           VST2 only (legacy format)
  --lv2            LV2 only
  --au2            AU v2 only (.component, macOS only)
  --au3            AU v3 only (.appex, macOS only)
  --aax            AAX only (requires pre-built template)
  --ios            AUv3 on the booted iOS Simulator (ad-hoc-signed)
  --ios-device     AUv3 on a tethered iOS device (needs team ID +
                   provisioning profile in .cargo/config.toml [env])
  --user           Install per-user (default; exception: VST3 on Windows
                   defaults to system - pass --user to override).
  --system         Install system-wide (sudo / admin required).
  --shell          Build dynamic shells + per-plugin logic dylibs.
  --debug          Cargo dev profile (faster compile, slower DSP).
  --no-build       Skip build, install existing artifacts.
  --features <list>
                   Extra Cargo features for the plugin crate, comma/space-
                   separated. Additive, applied to every underlying build.
                   Format features (clap/vst3/...) are reserved.
  -p <crate>       Install only the plugin with this cargo crate name.
  --target-cpu <value>
                   Override the x86_64 default. Accepted values:
                     baseline   no flag (rustc default = x86-64 / SSE2)
                     v2|v3|v4   x86-64-v<N> (v3 is the implicit default)
                     native     -C target-cpu=native (local-dev only;
                                won't run on machines without the
                                build host's exact feature set)
                     <literal>  passed verbatim to rustc (apple-m1, znver4)
  -h, --help       Show this message"
    );
}

/// Resolve the per-format effective scope and print the policy note
/// (once per `cargo truce` invocation) when the user-visible result
/// differs from a plain `--user`: a hard upgrade (AAX / AU v3 /
/// Windows-VST2) or a per-(format, OS) default (Windows VST3).
fn scope_for(format: Format, requested: Option<InstallScope>) -> InstallScope {
    let (effective, note) = effective_scope(format, requested);
    if let Some(msg) = note {
        note_once(msg);
    }
    effective
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn install_clap(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = release_lib(root, &format!("{}_clap", p.dylib_stem()));
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_clap");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let clap_dir = scope.clap_dir();
    let bundle = clap_dir.join(format!("{}.clap", p.file_stem()));

    #[cfg(target_os = "macos")]
    {
        // CLAP on macOS uses the loadable-bundle layout that hosts
        // (Bitwig, Studio One) require per Apple's bundle conventions.
        // Earlier truce versions wrote a flat dylib renamed `.clap`;
        // if that's still on disk at `bundle`, clear it before
        // building the directory.
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let exec_name = p.file_stem();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        let plist_tmp = tmp_manifests()
            .join(format!("{}_clap.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            if bundle.exists() && !bundle.is_dir() {
                run_sudo("rm", &[OsStr::new("-f"), bundle.as_os_str()])?;
            }
            run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
            let dst_dylib = macos_dir.join(&exec_name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            if bundle.exists() && !bundle.is_dir() {
                fs::remove_file(&bundle)?;
            }
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        // Presets are part of the bundle's sealed Resources - they
        // must land before codesign or the signature won't cover
        // them.
        if let Some(fp) = factory_presets {
            presets::emit_trucepreset_tree(
                fp,
                &contents.join("Resources/Presets"),
                scope.needs_sudo(),
                &format!("{}-clap", p.bundle_id),
            )?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Linux installs are always per-user; Windows system-scope
        // writes succeed when the cargo-truce process is elevated and
        // bubble an OS-level EACCES otherwise (Windows has no per-
        // command sudo - elevation is per-process via UAC).
        fs_ctx::create_dir_all(&clap_dir)?;
        fs_ctx::copy(&dylib, &bundle)?;
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(fp) = factory_presets {
        // The `.clap` is a single file here, so presets land in a
        // `<stem>.presets/` sibling directory; the wrapper's
        // discovery provider derives the same path from its own
        // dylib location at scan time.
        presets::emit_trucepreset_tree(
            fp,
            &clap_dir.join(format!("{}.presets", p.file_stem())),
            scope.needs_sudo(),
            &format!("{}-clap", p.bundle_id),
        )?;
    }

    crate::log_output(format!("CLAP: {}", bundle.display()));
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst3(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = release_lib(root, &format!("{}_vst3", p.dylib_stem()));
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_vst3");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope.vst3_dir().join(format!("{}.vst3", p.file_stem()));

    #[cfg(target_os = "macos")]
    {
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let exec_name = p.file_stem();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        let plist_tmp = tmp_manifests()
            .join(format!("{}_vst3.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
            let dst_dylib = macos_dir.join(&exec_name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    #[cfg(target_os = "windows")]
    {
        // VST3 on Windows: <vst3_dir>\{name}.vst3\Contents\x86_64-win\{name}.vst3
        let arch_dir = bundle.join("Contents").join("x86_64-win");
        let dst = arch_dir.join(format!("{}.vst3", p.file_stem()));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    #[cfg(target_os = "linux")]
    {
        let arch_dir = bundle.join("Contents").join("x86_64-linux");
        let dst = arch_dir.join(format!("{}.so", p.file_stem()));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    // The VST3 spec has no in-bundle preset location - hosts scan
    // the per-OS preset directories, so the files land there.
    if let Some(fp) = factory_presets {
        presets::emit_vst3_presets(fp, p, config, scope)?;
    }

    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst2(root: &Path, p: &PluginDef, config: &Config, scope: InstallScope) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_vst2");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let vst_dir = scope.vst2_dir();

    #[cfg(target_os = "macos")]
    {
        let bundle = vst_dir.join(format!("{}.vst", p.file_stem()));
        let macos_dir = bundle.join("Contents/MacOS");
        let exec_name = p.file_stem();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.vst2</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        let plist_tmp = tmp_manifests()
            .join(format!("{}_vst2.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            run_sudo("rm", &[OsStr::new("-rf"), bundle.as_os_str()])?;
            run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
            let dst_dylib = macos_dir.join(&exec_name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = bundle.join("Contents/Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
            // PkgInfo is small enough that re-emitting via run_sudo
            // (rather than tee) keeps the helper surface minimal.
            let pkginfo_tmp = tmp_manifests().join(format!("{}_vst2.pkginfo", p.bundle_id));
            fs_ctx::write(&pkginfo_tmp, "BNDL????")?;
            let dst_pkginfo = bundle.join("Contents/PkgInfo");
            run_sudo("cp", &[pkginfo_tmp.as_os_str(), dst_pkginfo.as_os_str()])?;
        } else {
            let _ = fs::remove_dir_all(&bundle);
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
            fs_ctx::write(bundle.join("Contents/Info.plist"), &plist)?;
            fs_ctx::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
        crate::log_output(format!("VST2: {}", bundle.display()));
    }

    #[cfg(target_os = "windows")]
    {
        // Windows VST2 is system-only (effective_scope guarantees the
        // fallback note); `vst_dir` resolves to %PROGRAMFILES%\Steinberg\VstPlugins.
        fs_ctx::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.dll", p.file_stem()));
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST2: {}", dst.display()));
    }

    #[cfg(target_os = "linux")]
    {
        fs_ctx::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.so", p.file_stem()));
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST2: {}", dst.display()));
    }

    Ok(())
}

/// Install an LV2 bundle.
///
/// Destination:
/// - **Linux**: `~/.lv2/{slug}.lv2/`
/// - **macOS**: `~/Library/Audio/Plug-Ins/LV2/{slug}.lv2/`
/// - **Windows**: `%APPDATA%\LV2\{slug}.lv2\`
///
/// Stages the bundle via `package::stage::stage_lv2`, which copies the
/// shared library (`{slug}.so` on Linux/macOS, `{slug}.dll` on Windows
/// because Windows's loader only accepts `.dll`) alongside the
/// `manifest.ttl` and `plugin.ttl` that `truce-derive`'s `export_lv2!`
/// proc-macro emitted at compile time as sidecar files.
///
/// Bundle and binary filenames are slugged to lowercase ASCII with hyphens
/// so that Turtle IRI references (`lv2:binary <...>`) don't need percent
/// encoding. Some LV2 hosts reject bundles whose TTL has spaces or other
/// non-URI characters in filenames even when the on-disk files are valid.
fn install_lv2(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    let lv2_dir = scope.lv2_dir();
    let lv2_plugin_uri =
        truce_build::lv2::plugin_uri(config.vendor.url.as_deref().unwrap_or(""), &p.bundle_id);

    // macOS system scope is the only path that needs the stage-then-
    // copy-via-sudo dance: `/Library/Audio/Plug-Ins/LV2/` is root-
    // owned, so the `stage_lv2`-internal `fs::write`s would EACCES if
    // they ran against the live destination. Windows admin elevation
    // is per-process (not per-command), and Linux installs are always
    // per-user, so both can stage straight into the destination.
    #[cfg(target_os = "macos")]
    if scope.needs_sudo() {
        run_sudo("mkdir", &[OsStr::new("-p"), lv2_dir.as_os_str()])?;
        let staging = tmp_lv2(&p.bundle_id);
        let _ = fs::remove_dir_all(&staging);
        fs_ctx::create_dir_all(&staging)?;
        crate::commands::package::stage::stage_lv2(
            root,
            p,
            &staging,
            &crate::application_identity(),
            None,
        )?;
        let slug = crate::commands::package::stage::lv2_slug(&p.name);
        let staged_bundle = staging.join(format!("{slug}.lv2"));
        // Presets join the bundle while it's still in user-writable
        // staging; the sudo copy below carries them along.
        if let Some(fp) = factory_presets {
            presets::emit_lv2_presets(fp, &staged_bundle, &lv2_plugin_uri)?;
        }
        let dst_bundle = lv2_dir.join(format!("{slug}.lv2"));
        run_sudo("rm", &[OsStr::new("-rf"), dst_bundle.as_os_str()])?;
        run_sudo(
            "cp",
            &[
                OsStr::new("-R"),
                staged_bundle.as_os_str(),
                dst_bundle.as_os_str(),
            ],
        )?;
        crate::log_output(format!("LV2:  {}", dst_bundle.display()));
        return Ok(());
    }

    // User scope on macOS, and every scope on Windows / Linux: write
    // straight into `lv2_dir`. Windows system-scope writes EACCES if
    // the cargo-truce process isn't elevated; the OS error message
    // surfaces unchanged.
    fs_ctx::create_dir_all(&lv2_dir)?;
    crate::commands::package::stage::stage_lv2(
        root,
        p,
        &lv2_dir,
        &crate::application_identity(),
        None,
    )?;
    let slug = crate::commands::package::stage::lv2_slug(&p.name);
    let dst_bundle = lv2_dir.join(format!("{slug}.lv2"));
    if let Some(fp) = factory_presets {
        presets::emit_lv2_presets(fp, &dst_bundle, &lv2_plugin_uri)?;
    }
    crate::log_output(format!("LV2:  {}", dst_bundle.display()));
    let _ = scope;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_au(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    // Profile-aware like every other format - hardcoding `release/`
    // here used to make `--debug` silently re-install a stale release
    // build.
    let dylib = crate::release_lib(root, &format!("{}_au", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope
        .au_v2_dir()
        .join(format!("{}.component", p.file_stem()));
    let bundle_str = bundle.to_str().unwrap().to_string();
    let contents = bundle.join("Contents");
    let macos_dir = contents.join("MacOS");
    let exec_name = p.file_stem();

    if scope.needs_sudo() {
        let _ = run_sudo("rm", &[OsStr::new("-rf"), bundle.as_os_str()]);
        run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
        let dst_dylib = macos_dir.join(&exec_name);
        run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
    } else {
        let _ = fs::remove_dir_all(&bundle);
        fs_ctx::create_dir_all(&macos_dir)?;
        fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.component</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>AudioComponents</key>
    <array>
        <dict>
            <key>type</key>
            <string>{au_type}</string>
            <key>subtype</key>
            <string>{au_subtype}</string>
            <key>manufacturer</key>
            <string>{au_mfr}</string>
            <key>name</key>
            <string>{vendor}: {display_name}</string>
            <key>description</key>
            <string>{display_name}</string>
            <key>version</key>
            <integer>65536</integer>
            <key>factoryFunction</key>
            <string>TruceAUFactory</string>
            <key>sandboxSafe</key>
            <true/>
            <key>tags</key>
            <array>
                <string>{au_tag}</string>
            </array>
        </dict>
    </array>
</dict>
</plist>"#,
        display_name = p.name,
        bundle_id = p.bundle_id,
        vendor_id = config.vendor.id,
        vendor = config.vendor.name,
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    let plist_tmp = tmp_manifests()
        .join(format!("{}_au.plist", p.bundle_id))
        .to_string_lossy()
        .to_string();
    fs_ctx::write(&plist_tmp, &plist)?;
    let info_plist = contents.join("Info.plist");
    if scope.needs_sudo() {
        run_sudo("cp", &[OsStr::new(&plist_tmp), info_plist.as_os_str()])?;
    } else {
        fs_ctx::copy(&plist_tmp, &info_plist)?;
    }

    // The shim's kAudioUnitProperty_FactoryPresets handler enumerates
    // these at runtime. Part of the sealed bundle - must precede
    // codesign.
    if let Some(fp) = factory_presets {
        presets::emit_trucepreset_tree(
            fp,
            &contents.join("Resources/Presets"),
            scope.needs_sudo(),
            &format!("{}-au-bundle", p.bundle_id),
        )?;
    }

    codesign_bundle(
        &bundle_str,
        &crate::application_identity(),
        scope.needs_sudo(),
    )?;
    crate::log_output(format!("AU:   {}", bundle.display()));

    // `.aupreset` files live outside the component bundle (the
    // `Library/Audio/Presets` walk hosts do), so they don't interact
    // with the codesign seal above.
    if let Some(fp) = factory_presets {
        presets::emit_au_presets(fp, p, config, scope)?;
    }
    Ok(())
}

/// Drive the iOS `AUv3` pipeline: build the Rust framework for the
/// chosen slice, compile the Swift `.appex` via `swiftc`, assemble
/// the container `.app`, sign, and install onto the simulator or a
/// tethered device.
#[cfg(target_os = "macos")]
fn install_ios(plugin_filter: Option<&str>, target: au_ios::IosTarget) -> Res {
    let root = crate::project_root();
    let config = crate::load_config()?;
    // Match the per-plugin loop the other formats use: `-p <crate>`
    // narrows to one entry; absence installs every `[[plugin]]` in
    // `truce.toml`. Each iOS install builds a fresh container `.app`
    // + embedded `.appex` so iterating is linear in plugin count,
    // but that's the same trade the other formats make.
    let plugins = super::pick_plugins(&config, plugin_filter)?;
    let total = plugins.len();
    for (i, p) in plugins.into_iter().enumerate() {
        // Outer-loop counter - distinguishes the per-plugin pass
        // from the `[N/5]` inner build stages `install_one` emits.
        // The iOS pipeline takes ~30-60 s per plugin, so a workspace
        // install (12 plugins) is several minutes of cargo + swiftc
        // chatter without this; the prefix is the only signal the
        // user has that the loop is making progress vs hung. Skip it
        // for single-plugin runs to avoid `[1/1]` noise.
        if total > 1 {
            eprintln!("==> [{}/{total}] {}", i + 1, p.crate_name);
        }
        au_ios::install_one(&root, p, target)?;
    }
    Ok(())
}
