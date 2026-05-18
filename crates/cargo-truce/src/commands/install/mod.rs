//! `cargo truce install` - build per-format dylibs and install into the
//! standard plug-in directories.

use crate::format::Format;
use crate::install_scope::{InstallScope, effective_scope, note_once, set_cli_install_scope};
use crate::util::fs_ctx;
use crate::{
    Config, PluginDef, Res, deployment_target, detect_default_features, load_config, project_root,
    release_lib, run_sudo, tmp_lv2,
};
// Plist scratch (VST3 / VST2 / AU) only happens on macOS - gate the
// import so Windows / Linux builds don't see it as unused.
#[cfg(target_os = "macos")]
use crate::tmp_manifests;
#[cfg(target_os = "macos")]
use crate::{codesign_bundle, dirs};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

// AAX is macOS / Windows; AU is macOS only.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) mod aax;
#[cfg(target_os = "macos")]
pub(crate) mod au_ios;
#[cfg(target_os = "macos")]
pub(crate) mod au_v3;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use aax::install_aax;
#[cfg(target_os = "macos")]
use au_v3::build_and_install_au_v3;

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
    let mut plugin_filter: Option<String> = None;
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
            "--ios" => ios = true,
            "--ios-device" => {
                ios = true;
                ios_device = true;
            }
            "--no-build" => no_build = true,
            "--shell" => shell_mode = true,
            "--debug" => debug = true,
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
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Scope resolution: CLI flag wins, otherwise OS default (user
    // on every platform).
    let scope = cli_scope.unwrap_or_else(InstallScope::os_default);

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

    let root = project_root();
    let dt = &deployment_target();

    let mut extra_features = Vec::new();
    if shell_mode {
        extra_features.push("shell");
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
    // silently downgrades AAX / AU v3 / Windows-VST2 to system scope
    // and emits a one-line note (printed at most once per format).
    for p in &plugins {
        if clap {
            let s = scope_for(Format::Clap, scope);
            install_clap(&root, p, &config, s)?;
        }
        if vst3 {
            let s = scope_for(Format::Vst3, scope);
            install_vst3(&root, p, &config, s)?;
        }
        if vst2 {
            let s = scope_for(Format::Vst2, scope);
            install_vst2(&root, p, &config, s)?;
        }
        if lv2 {
            let s = scope_for(Format::Lv2, scope);
            install_lv2(&root, p, &config, s)?;
        }
        if au2 {
            #[cfg(target_os = "macos")]
            {
                let s = scope_for(Format::Au2, scope);
                install_au(&root, p, &config, s)?;
            }
            // Non-macOS skip line was already pushed in the build phase.
        }
        if aax {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // AAX is always system-scope; the call to `scope_for`
                // exists for the side-effect (one-line note when the
                // user passed `--user`).
                let _ = scope_for(Format::Aax, scope);
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
            let _ = scope_for(Format::Au3, scope);
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

Build and install plugins into the host's plug-in directories. Defaults
to release. Defaults to whichever formats are in the plugin's Cargo.toml
default features (typically clap + vst3).

Per-format scope is per-user by default; pass --system for the shared
system directories. AAX and AU v3 are always system-scope.

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
  --user           Install per-user (default).
  --system         Install system-wide (sudo / admin required).
  --shell          Build dynamic shells + per-plugin logic dylibs.
  --debug          Cargo dev profile (faster compile, slower DSP).
  --no-build       Skip build, install existing artifacts.
  -p <crate>       Install only the plugin with this cargo crate name.
  -h, --help       Show this message"
    );
}

/// Resolve the per-format effective scope and print the fallback note
/// (once per `cargo truce` invocation) when `--user` had to be ignored.
fn scope_for(format: Format, requested: InstallScope) -> InstallScope {
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
) -> Res {
    let dylib = release_lib(root, &format!("{}_clap", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let clap_dir = scope.clap_dir();
    let bundle = clap_dir.join(format!("{}.clap", p.name));

    #[cfg(target_os = "macos")]
    {
        // CLAP on macOS uses the loadable-bundle layout that hosts
        // (Bitwig, Studio One) require per Apple's bundle conventions.
        // Earlier truce versions wrote a flat dylib renamed `.clap`;
        // if that's still on disk at `bundle`, clear it before
        // building the directory.
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            name = p.name,
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
            let dst_dylib = macos_dir.join(&p.name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            if bundle.exists() && !bundle.is_dir() {
                fs::remove_file(&bundle)?;
            }
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        if scope.needs_sudo() {
            run_sudo("mkdir", &[OsStr::new("-p"), clap_dir.as_os_str()])?;
            run_sudo("cp", &[dylib.as_os_str(), bundle.as_os_str()])?;
        } else {
            fs_ctx::create_dir_all(&clap_dir)?;
            fs_ctx::copy(&dylib, &bundle)?;
        }
    }

    crate::log_output(format!("CLAP: {}", bundle.display()));
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst3(root: &Path, p: &PluginDef, config: &Config, scope: InstallScope) -> Res {
    let dylib = release_lib(root, &format!("{}_vst3", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope.vst3_dir().join(format!("{}.vst3", p.name));

    #[cfg(target_os = "macos")]
    {
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            name = p.name,
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
            let dst_dylib = macos_dir.join(&p.name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
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
        let dst = arch_dir.join(format!("{}.vst3", p.name));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    #[cfg(target_os = "linux")]
    {
        let arch_dir = bundle.join("Contents").join("x86_64-linux");
        let dst = arch_dir.join(format!("{}.so", p.name));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst2(root: &Path, p: &PluginDef, config: &Config, scope: InstallScope) -> Res {
    let dylib = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let vst_dir = scope.vst2_dir();

    #[cfg(target_os = "macos")]
    {
        let bundle = vst_dir.join(format!("{}.vst", p.name));
        let macos_dir = bundle.join("Contents/MacOS");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.vst2</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            name = p.name,
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
            let dst_dylib = macos_dir.join(&p.name);
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
            fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
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
        let dst = vst_dir.join(format!("{}.dll", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST2: {}", dst.display()));
    }

    #[cfg(target_os = "linux")]
    {
        fs_ctx::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.so", p.name));
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
/// - Windows's loader only accepts `.dll`) alongside the `manifest.ttl`
/// and `plugin.ttl` that `truce-derive`'s `export_lv2!` proc-macro
/// emitted at compile time as sidecar files.
///
/// Bundle and binary filenames are slugged to lowercase ASCII with hyphens
/// so that Turtle IRI references (`lv2:binary <...>`) don't need percent
/// encoding - some LV2 hosts reject bundles whose TTL has spaces or other
/// non-URI characters in filenames even when the on-disk files are valid.
fn install_lv2(root: &Path, p: &PluginDef, _config: &Config, scope: InstallScope) -> Res {
    let lv2_dir = scope.lv2_dir();
    if scope.needs_sudo() {
        run_sudo("mkdir", &[OsStr::new("-p"), lv2_dir.as_os_str()])?;
    } else {
        fs_ctx::create_dir_all(&lv2_dir)?;
    }
    // `stage_lv2` writes into `lv2_dir/<slug>.lv2/`. The system-scope
    // path can be root-owned (e.g. /Library/Audio/Plug-Ins/LV2/),
    // which means each fs::write inside `stage_lv2` would EACCES.
    // Stage to a temp directory first, then move into place via
    // `run_sudo` for the system path.
    if scope.needs_sudo() {
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
    } else {
        crate::commands::package::stage::stage_lv2(
            root,
            p,
            &lv2_dir,
            &crate::application_identity(),
            None,
        )?;
        let slug = crate::commands::package::stage::lv2_slug(&p.name);
        crate::log_output(format!(
            "LV2:  {}",
            lv2_dir.join(format!("{slug}.lv2")).display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_au(root: &Path, p: &PluginDef, config: &Config, scope: InstallScope) -> Res {
    let dylib =
        truce_build::target_dir(root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope.au_v2_dir().join(format!("{}.component", p.name));
    let bundle_str = bundle.to_str().unwrap().to_string();
    let contents = bundle.join("Contents");
    let macos_dir = contents.join("MacOS");

    if scope.needs_sudo() {
        let _ = run_sudo("rm", &[OsStr::new("-rf"), bundle.as_os_str()]);
        run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
        let dst_dylib = macos_dir.join(&p.name);
        run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
    } else {
        let _ = fs::remove_dir_all(&bundle);
        fs_ctx::create_dir_all(&macos_dir)?;
        fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.component</string>
    <key>CFBundleName</key>
    <string>{name}</string>
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
            <string>{vendor}: {name}</string>
            <key>description</key>
            <string>{name}</string>
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
        name = p.name,
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
    codesign_bundle(
        &bundle_str,
        &crate::application_identity(),
        scope.needs_sudo(),
    )?;
    crate::log_output(format!("AU:   {}", bundle.display()));
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
