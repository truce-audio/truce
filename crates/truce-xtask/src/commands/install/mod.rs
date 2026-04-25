//! `cargo truce install` — build per-format dylibs and install into the
//! standard plug-in directories.

#![allow(unused_imports)]

use crate::util::fs_ctx;
use crate::{
    cargo_build, codesign_bundle, deployment_target, detect_default_features, dirs, load_config,
    project_root, release_lib, run_sudo, tmp_dir, Config, PluginDef, Res,
};
#[cfg(target_os = "windows")]
use crate::{common_program_files, program_files};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) mod aax;
pub(crate) mod au_v3;

use aax::{emit_aax_bundle, install_aax};
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
    let mut dev_mode = false;
    let mut plugin_filter: Option<String> = None;

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
            "--dev" => dev_mode = true,
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

    if !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax {
        // No format flags specified — enable all formats that the project supports.
        // Check which features are defined in the first plugin's Cargo.toml.
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
        vst2 = available.contains("vst2");
        lv2 = available.contains("lv2");
        #[cfg(target_os = "macos")]
        {
            au2 = available.contains("au");
            au3 = available.contains("au");
        }
        aax = available.contains("aax");
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

    let root = project_root();
    let dt = &deployment_target();

    let mut extra_features = Vec::new();
    if dev_mode {
        extra_features.push("dev");
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

        if aax {
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

        // In dev mode, also build the debug dylibs (the logic that
        // the hot-reload shells watch and load).
        if dev_mode {
            crate::vprintln!("Building debug dylibs (logic for hot-reload)...");
            let mut cmd = Command::new("cargo");
            cmd.arg("build").arg("--workspace");
            #[cfg(target_os = "macos")]
            cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
            let status = cmd.status()?;
            if !status.success() {
                return Err("debug workspace build failed".into());
            }
        }
    }

    // --- Install ---
    for p in &plugins {
        if clap {
            install_clap(&root, p, &config)?;
        }
        if vst3 {
            install_vst3(&root, p, &config)?;
        }
        if vst2 {
            install_vst2(&root, p, &config)?;
        }
        if lv2 {
            install_lv2(&root, p, &config)?;
        }
        if au2 {
            install_au(&root, p, &config)?;
        }
        if aax {
            install_aax(&root, p, &config)?;
        }
    }

    if au3 {
        build_and_install_au_v3(&root, &config, &plugins, no_build)?;
    }

    #[cfg(target_os = "macos")]
    if au2 {
        let cache = dirs::home_dir()
            .unwrap()
            .join("Library/Caches/AudioUnitCache");
        let _ = fs::remove_dir_all(&cache);
        crate::vprintln!("Cleared AU cache.");
    }

    eprintln!("\nDone. Restart your DAW to rescan.");
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn install_clap(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &format!("{}_clap", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
        let clap_dir = dirs::home_dir()
            .unwrap()
            .join("Library/Audio/Plug-Ins/CLAP");
        fs_ctx::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        codesign_bundle(
            dst.to_str().unwrap(),
            config.macos.application_identity(),
            false,
        )?;
        crate::vprintln!("CLAP: {}", dst.display());
    }

    #[cfg(target_os = "windows")]
    {
        let clap_dir = common_program_files().join("CLAP");
        fs_ctx::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("CLAP: {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        let clap_dir = dirs::home_dir().unwrap().join(".clap");
        fs_ctx::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("CLAP: {}", dst.display());
    }

    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst3(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &format!("{}_vst3", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
        let vst3_bundle = format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name);
        let contents = format!("{vst3_bundle}/Contents");

        run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
        run_sudo(
            "cp",
            &[
                dylib.to_str().unwrap(),
                &format!("{contents}/MacOS/{}", p.name),
            ],
        )?;

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
        run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
        codesign_bundle(&vst3_bundle, config.macos.application_identity(), true)?;
        crate::vprintln!("VST3: {vst3_bundle}");
    }

    #[cfg(target_os = "windows")]
    {
        // VST3 on Windows: %COMMONPROGRAMFILES%\VST3\{name}.vst3\Contents\x86_64-win\{name}.vst3
        let vst3_dir = common_program_files().join("VST3");
        let bundle = vst3_dir.join(format!("{}.vst3", p.name));
        let arch_dir = bundle.join("Contents").join("x86_64-win");
        fs_ctx::create_dir_all(&arch_dir)?;
        let dst = arch_dir.join(format!("{}.vst3", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("VST3: {}", bundle.display());
    }

    #[cfg(target_os = "linux")]
    {
        let vst3_dir = dirs::home_dir().unwrap().join(".vst3");
        let bundle = vst3_dir.join(format!("{}.vst3", p.name));
        let arch_dir = bundle.join("Contents").join("x86_64-linux");
        fs_ctx::create_dir_all(&arch_dir)?;
        let dst = arch_dir.join(format!("{}.so", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("VST3: {}", bundle.display());
    }

    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst2(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
        let vst_dir = dirs::home_dir().unwrap().join("Library/Audio/Plug-Ins/VST");
        let bundle = vst_dir.join(format!("{}.vst", p.name));

        let _ = fs::remove_dir_all(&bundle);
        let macos_dir = bundle.join("Contents/MacOS");
        fs_ctx::create_dir_all(&macos_dir)?;
        fs_ctx::copy(&dylib, macos_dir.join(&p.name))?;

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
        fs_ctx::write(bundle.join("Contents/Info.plist"), &plist)?;
        fs_ctx::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;

        codesign_bundle(
            bundle.to_str().unwrap(),
            config.macos.application_identity(),
            false,
        )?;
        crate::vprintln!("VST2: {}", bundle.display());
    }

    #[cfg(target_os = "windows")]
    {
        // VST2 on Windows: %PROGRAMFILES%\Steinberg\VstPlugins\{name}.dll
        // This is the Steinberg default path that Reaper and most hosts scan by default.
        let vst_dir = program_files().join("Steinberg").join("VstPlugins");
        fs_ctx::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.dll", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("VST2: {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        let vst_dir = dirs::home_dir().unwrap().join(".vst");
        fs_ctx::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.so", p.name));
        fs_ctx::copy(&dylib, &dst)?;
        crate::vprintln!("VST2: {}", dst.display());
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
fn install_lv2(root: &Path, p: &PluginDef, _config: &Config) -> Res {
    let lv2_dir = lv2_bundle_root()?;
    fs_ctx::create_dir_all(&lv2_dir)?;
    crate::commands::package::stage::stage_lv2(root, p, &lv2_dir)?;
    let slug = crate::commands::package::stage::lv2_slug(&p.name);
    crate::vprintln!("LV2: {}", lv2_dir.join(format!("{slug}.lv2")).display());
    Ok(())
}

/// User-level LV2 bundle root per platform convention.
fn lv2_bundle_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or("cannot locate home directory")?;
        Ok(home.join(".lv2"))
    }
    #[cfg(target_os = "macos")]
    {
        // LV2 SDK convention on macOS. Ardour, Carla and jalv all scan
        // this path by default; system-wide `/Library/Audio/Plug-Ins/LV2`
        // is also searched when present.
        let home = dirs::home_dir().ok_or("cannot locate home directory")?;
        Ok(home.join("Library/Audio/Plug-Ins/LV2"))
    }
    #[cfg(target_os = "windows")]
    {
        // Per the LV2 spec, the Windows per-user LV2 path is
        // `%APPDATA%\LV2`. `%COMMONPROGRAMFILES%\LV2` is the system-wide
        // search path; we target the user location so no admin rights
        // are needed.
        let appdata = std::env::var_os("APPDATA").ok_or("APPDATA env var not set")?;
        Ok(PathBuf::from(appdata).join("LV2"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("LV2 install is only supported on Linux, macOS, and Windows".into())
    }
}

fn install_au(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = root.join(format!("target/release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = format!("/Library/Audio/Plug-Ins/Components/{}.component", p.name);
    let contents = format!("{bundle}/Contents");

    let _ = run_sudo("rm", &["-rf", &bundle]);
    run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
    run_sudo(
        "cp",
        &[
            dylib.to_str().unwrap(),
            &format!("{contents}/MacOS/{}", p.name),
        ],
    )?;

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
    run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
    codesign_bundle(&bundle, config.macos.application_identity(), true)?;
    crate::vprintln!("AU:   {bundle}");
    Ok(())
}
