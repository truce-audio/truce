//! `cargo truce install` — build per-format dylibs and install into the
//! standard plug-in directories.

#![allow(unused_imports)]

use crate::install_scope::{Format, InstallScope, effective_scope, note_once};
use crate::util::fs_ctx;
use crate::{
    Config, PluginDef, Res, cargo_build, codesign_bundle, deployment_target,
    detect_default_features, dirs, load_config, project_root, release_lib, run_sudo, tmp_dir,
};
#[cfg(target_os = "windows")]
use crate::{common_program_files, program_files};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// AAX is macOS / Windows; AU is macOS only.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) mod aax;
#[cfg(target_os = "macos")]
pub(crate) mod au_v3;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use aax::{emit_aax_bundle, install_aax};
#[cfg(target_os = "macos")]
use au_v3::build_and_install_au_v3;

pub(crate) fn cmd_install(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
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
            "--no-build" => no_build = true,
            "--shell" => shell_mode = true,
            "--debug" => debug = true,
            "--user" => {
                if matches!(cli_scope, Some(InstallScope::System)) {
                    return Err("--user and --system are mutually exclusive".into());
                }
                cli_scope = Some(InstallScope::User);
            }
            "--system" => {
                if matches!(cli_scope, Some(InstallScope::User)) {
                    return Err("--user and --system are mutually exclusive".into());
                }
                cli_scope = Some(InstallScope::System);
            }
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo truce install` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err(
                        "-p requires a plugin crate name (e.g. -p truce-example-gain)".into(),
                    );
                }
                plugin_filter = Some(args[i].clone());
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

    if !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax {
        // No format flags specified — enable all formats that the project supports.
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
    // missing `[profile.shell]` (plugins scaffolded before 0.13.x).
    // Catching this here gives a one-line copy-paste fix instead of
    // cargo's terser "profile `shell` is not declared" downstream.
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

    // Logic profile (the dylib the shell dlopens at runtime). Baked
    // into the shell binary via `TRUCE_LOGIC_PROFILE` so the runtime
    // path lookup doesn't need to read env. Only meaningful when
    // shell_mode is on.
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
        // Set in the process env so every child cargo build inherits
        // it. truce-build's `emit_plugin_env()` reads the var and
        // re-emits it via `cargo:rustc-env=`, which bakes it into the
        // shell binary as a compile-time constant. The shell's runtime
        // dylib_path() lookup uses `option_env!("TRUCE_LOGIC_PROFILE")`
        // to know which target subdir holds the logic dylib.
        // 2024-edition: `set_var` is unsafe (process-wide env state).
        // Single-threaded install path; main thread is the only writer.
        unsafe {
            std::env::set_var("TRUCE_LOGIC_PROFILE", logic_profile);
        }
    }

    // --- Build ---
    //
    // One cargo invocation per (plugin, format) pair so that the
    // name-override env vars can be applied per-plugin. The shared
    // target cache means incremental rebuilds stay fast even though
    // we invoke cargo more times than strictly necessary.
    if !no_build {
        if clap {
            let mut format_features: Vec<&str> = vec!["clap"];
            for f in &extra_features {
                format_features.push(f);
            }
            let features_combined = format_features.join(",");
            if !extra_features.is_empty() {
                let label = extra_features.join(" + ");
                crate::vprintln!("Building CLAP ({label})...");
            } else {
                crate::vprintln!("Building CLAP...");
            }
            for p in &plugins {
                let mut env_pairs: Vec<(&str, &str)> = Vec::new();
                if let Some(n) = p.clap_name.as_deref() {
                    env_pairs.push(("TRUCE_CLAP_NAME_OVERRIDE", n));
                }
                cargo_build(
                    &env_pairs,
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        &features_combined,
                    ],
                    dt,
                )?;
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_clap", p.dylib_stem()));
                if src.exists() {
                    fs_ctx::copy(&src, &dst)?;
                }
            }
        }

        if vst3 {
            let mut format_features: Vec<&str> = vec!["vst3"];
            for f in &extra_features {
                format_features.push(f);
            }
            let features_combined = format_features.join(",");
            if !extra_features.is_empty() {
                let label = extra_features.join(" + ");
                crate::vprintln!("Building VST3 ({label})...");
            } else {
                crate::vprintln!("Building VST3...");
            }
            for p in &plugins {
                let mut env_pairs: Vec<(&str, &str)> = Vec::new();
                if let Some(n) = p.vst3_name.as_deref() {
                    env_pairs.push(("TRUCE_VST3_NAME_OVERRIDE", n));
                }
                cargo_build(
                    &env_pairs,
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        &features_combined,
                    ],
                    dt,
                )?;
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_vst3", p.dylib_stem()));
                if src.exists() {
                    fs_ctx::copy(&src, &dst)?;
                }
            }
        }

        if vst2 {
            crate::vprintln!("Building VST2...");
            for p in &plugins {
                let mut env_pairs: Vec<(&str, &str)> = Vec::new();
                if let Some(n) = p.vst2_name.as_deref() {
                    env_pairs.push(("TRUCE_VST2_NAME_OVERRIDE", n));
                }
                cargo_build(
                    &env_pairs,
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        "vst2",
                    ],
                    dt,
                )?;
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_vst2", p.dylib_stem()));
                fs_ctx::copy(&src, &dst)?;
            }
        }

        if lv2 {
            crate::vprintln!("Building LV2...");
            for p in &plugins {
                let mut env_pairs: Vec<(&str, &str)> = Vec::new();
                if let Some(n) = p.lv2_name.as_deref() {
                    env_pairs.push(("TRUCE_LV2_NAME_OVERRIDE", n));
                }
                cargo_build(
                    &env_pairs,
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        "lv2",
                    ],
                    dt,
                )?;
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_lv2", p.dylib_stem()));
                fs_ctx::copy(&src, &dst)?;
            }
        }

        if au2 {
            #[cfg(target_os = "macos")]
            {
                crate::vprintln!("Building AU v2...");
                for p in &plugins {
                    let mut env_pairs: Vec<(&str, &str)> = vec![
                        ("TRUCE_AU_VERSION", "2"),
                        ("TRUCE_AU_PLUGIN_ID", &p.bundle_id),
                    ];
                    if let Some(n) = p.au_name.as_deref() {
                        env_pairs.push(("TRUCE_AU_NAME_OVERRIDE", n));
                    }
                    cargo_build(
                        &env_pairs,
                        &[
                            "-p",
                            &p.crate_name,
                            "--no-default-features",
                            "--features",
                            "au",
                        ],
                        dt,
                    )?;
                    let src = release_lib(&root, &p.dylib_stem());
                    let dst = release_lib(&root, &format!("{}_au", p.dylib_stem()));
                    fs_ctx::copy(&src, &dst)?;
                }
            }
            #[cfg(not(target_os = "macos"))]
            crate::log_skip(
                "AU v2: not supported on this platform. Audio Unit is macOS-only.".to_string(),
            );
        }

        if aax {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // SDK-not-configured is project-wide, not per-plugin —
                // emit one skip line and bypass the build loop so we
                // don't redundantly cargo-build the `aax` feature only
                // to have `emit_aax_bundle` no-op each plugin.
                if crate::resolve_aax_sdk_path(&config).is_none() {
                    let hint = if cfg!(target_os = "windows") {
                        "[windows].aax_sdk_path"
                    } else {
                        "[macos].aax_sdk_path"
                    };
                    crate::log_skip(format!(
                        "AAX: SDK not configured. Set {hint} in truce.toml or the AAX_SDK_PATH env var."
                    ));
                } else {
                    crate::vprintln!("Building AAX...");
                    for p in &plugins {
                        let mut env_pairs: Vec<(&str, &str)> = Vec::new();
                        if let Some(n) = p.aax_name.as_deref() {
                            env_pairs.push(("TRUCE_AAX_NAME_OVERRIDE", n));
                        }
                        cargo_build(
                            &env_pairs,
                            &[
                                "-p",
                                &p.crate_name,
                                "--no-default-features",
                                "--features",
                                "aax",
                            ],
                            dt,
                        )?;
                        let src = release_lib(&root, &p.dylib_stem());
                        let dst = release_lib(&root, &format!("{}_aax", p.dylib_stem()));
                        fs_ctx::copy(&src, &dst)?;
                        emit_aax_bundle(&root, p, &config, false)?;
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            crate::log_skip(
                "AAX: not supported on this platform. Use macOS or Windows to build AAX."
                    .to_string(),
            );
        }

        // Shell mode: also build the logic dylibs (the dylibs each
        // installed shell will dlopen at runtime). Built in the
        // profile baked into the shell — release by default, debug
        // when `--debug` was passed. Scoped per-plugin (was
        // `--workspace` until 0.13.x; that rebuilt every framework
        // crate on a fresh checkout).
        if shell_mode {
            for p in &plugins {
                crate::vprintln!(
                    "Building {} logic dylib for {}...",
                    logic_profile,
                    p.crate_name
                );
                let mut cmd = Command::new("cargo");
                cmd.arg("build").arg("-p").arg(&p.crate_name);
                match logic_profile {
                    "debug" => {} // cargo default
                    "release" => {
                        cmd.arg("--release");
                    }
                    other => {
                        cmd.arg("--profile").arg(other);
                    }
                }
                #[cfg(target_os = "macos")]
                cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
                let status = cmd.status()?;
                if !status.success() {
                    return Err(format!("{logic_profile} build of {} failed", p.crate_name).into());
                }
            }
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
            // AU v3 is always system-scope on macOS — emit the note
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
    if au2 {
        let cache = dirs::home_dir()
            .unwrap()
            .join("Library/Caches/AudioUnitCache");
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
    let dst = clap_dir.join(format!("{}.clap", p.name));

    if scope.needs_sudo() {
        run_sudo("mkdir", &["-p", clap_dir.to_str().unwrap()])?;
        run_sudo("cp", &[dylib.to_str().unwrap(), dst.to_str().unwrap()])?;
    } else {
        fs_ctx::create_dir_all(&clap_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
    }

    #[cfg(target_os = "macos")]
    codesign_bundle(
        dst.to_str().unwrap(),
        config.macos.application_identity(),
        scope.needs_sudo(),
    )?;
    crate::log_output(format!("CLAP: {}", dst.display()));
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
        let plist_tmp = tmp_dir()
            .join(format!("{}_vst3.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            run_sudo("mkdir", &["-p", macos_dir.to_str().unwrap()])?;
            run_sudo(
                "cp",
                &[
                    dylib.to_str().unwrap(),
                    &format!("{}/{}", macos_dir.display(), p.name),
                ],
            )?;
            run_sudo(
                "cp",
                &[&plist_tmp, &format!("{}/Info.plist", contents.display())],
            )?;
        } else {
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            config.macos.application_identity(),
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
    <string>com.truce.{bundle_id}.vst2</string>
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
        );
        let plist_tmp = tmp_dir()
            .join(format!("{}_vst2.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            run_sudo("rm", &["-rf", bundle.to_str().unwrap()])?;
            run_sudo("mkdir", &["-p", macos_dir.to_str().unwrap()])?;
            run_sudo(
                "cp",
                &[
                    dylib.to_str().unwrap(),
                    &format!("{}/{}", macos_dir.display(), p.name),
                ],
            )?;
            run_sudo(
                "cp",
                &[
                    &plist_tmp,
                    &format!("{}/Contents/Info.plist", bundle.display()),
                ],
            )?;
            // PkgInfo is small enough that re-emitting via run_sudo
            // (rather than tee) keeps the helper surface minimal.
            let pkginfo_tmp = tmp_dir().join(format!("{}_vst2.pkginfo", p.bundle_id));
            fs_ctx::write(&pkginfo_tmp, "BNDL????")?;
            run_sudo(
                "cp",
                &[
                    pkginfo_tmp.to_str().unwrap(),
                    &format!("{}/Contents/PkgInfo", bundle.display()),
                ],
            )?;
        } else {
            let _ = fs::remove_dir_all(&bundle);
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;
            fs_ctx::write(bundle.join("Contents/Info.plist"), &plist)?;
            fs_ctx::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            config.macos.application_identity(),
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
/// Copies the built shared library into the bundle as `{slug}.so` on
/// Linux/macOS and `{slug}.dll` on Windows (the LV2 spec places no
/// constraint on the extension, but the Windows loader only accepts
/// `.dll`), then `LoadLibrary`/`dlopen`s it to call the plugin's
/// `__truce_lv2_emit_bundle` entry point, which writes `manifest.ttl`
/// and `plugin.ttl` describing ports, parameters, and the UI type
/// appropriate for the host platform.
///
/// Bundle and binary filenames are slugged to lowercase ASCII with hyphens
/// so that Turtle IRI references (`lv2:binary <...>`) don't need percent
/// encoding — some LV2 hosts reject bundles whose TTL has spaces or other
/// non-URI characters in filenames even when the on-disk files are valid.
fn install_lv2(root: &Path, p: &PluginDef, _config: &Config, scope: InstallScope) -> Res {
    let lv2_dir = scope.lv2_dir();
    if scope.needs_sudo() {
        run_sudo("mkdir", &["-p", lv2_dir.to_str().unwrap()])?;
    } else {
        fs_ctx::create_dir_all(&lv2_dir)?;
    }
    // `stage_lv2` writes into `lv2_dir/<slug>.lv2/`. The system-scope
    // path can be root-owned (e.g. /Library/Audio/Plug-Ins/LV2/),
    // which means each fs::write inside `stage_lv2` would EACCES.
    // Stage to a temp directory first, then move into place via
    // `run_sudo` for the system path.
    if scope.needs_sudo() {
        let staging = tmp_dir().join(format!("{}_lv2_stage", p.bundle_id));
        let _ = fs::remove_dir_all(&staging);
        fs_ctx::create_dir_all(&staging)?;
        crate::commands::package::stage::stage_lv2(root, p, &staging)?;
        let slug = crate::commands::package::stage::lv2_slug(&p.name);
        let staged_bundle = staging.join(format!("{slug}.lv2"));
        let dst_bundle = lv2_dir.join(format!("{slug}.lv2"));
        run_sudo("rm", &["-rf", dst_bundle.to_str().unwrap()])?;
        run_sudo(
            "cp",
            &[
                "-R",
                staged_bundle.to_str().unwrap(),
                dst_bundle.to_str().unwrap(),
            ],
        )?;
        crate::log_output(format!("LV2:  {}", dst_bundle.display()));
    } else {
        crate::commands::package::stage::stage_lv2(root, p, &lv2_dir)?;
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
    let dylib = crate::target_dir(root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope.au_v2_dir().join(format!("{}.component", p.name));
    let bundle_str = bundle.to_str().unwrap().to_string();
    let contents = bundle.join("Contents");
    let macos_dir = contents.join("MacOS");

    if scope.needs_sudo() {
        let _ = run_sudo("rm", &["-rf", &bundle_str]);
        run_sudo("mkdir", &["-p", macos_dir.to_str().unwrap()])?;
        run_sudo(
            "cp",
            &[
                dylib.to_str().unwrap(),
                &format!("{}/{}", macos_dir.display(), p.name),
            ],
        )?;
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
    let plist_tmp = tmp_dir()
        .join(format!("{}_au.plist", p.bundle_id))
        .to_string_lossy()
        .to_string();
    fs_ctx::write(&plist_tmp, &plist)?;
    let info_plist = contents.join("Info.plist");
    if scope.needs_sudo() {
        run_sudo("cp", &[&plist_tmp, info_plist.to_str().unwrap()])?;
    } else {
        fs_ctx::copy(&plist_tmp, &info_plist)?;
    }
    codesign_bundle(
        &bundle_str,
        config.macos.application_identity(),
        scope.needs_sudo(),
    )?;
    crate::log_output(format!("AU:   {}", bundle.display()));
    Ok(())
}
