//! `cargo truce build` — produce per-format bundles in `target/bundles/`
//! without installing.
//!
//! Mirrors `install`'s build phase + `package`'s staging helpers, so the
//! same flag set (`--clap` / `--vst3` / `--vst2` / `--lv2` / `--au2`)
//! works the same way. AU v3 (`.app`) and AAX are install-time-only —
//! they require xcodebuild / a cmake C++ template / system install
//! locations and don't make sense for a "build into a folder" flow; use
//! `cargo truce install --au3` / `--aax` instead.

use crate::commands::package::stage::{
    lv2_slug, stage_au2, stage_clap, stage_lv2, stage_vst2, stage_vst3,
};
use crate::util::fs_ctx;
use crate::{
    cargo_build, deployment_target, detect_default_features, load_config,
    project_root, release_lib, PluginDef, Res,
};
use std::process::Command;

pub(crate) fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
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
            // Reject install-only formats explicitly so the failure mode
            // is "use install for these" rather than a silent no-op.
            "--au3" => return Err(
                "--au3 is install-only (xcodebuild + /Applications layout). \
                 Use `cargo truce install --au3` instead.".into()
            ),
            "--aax" => return Err(
                "--aax is install-only (cmake AAX template + system path). \
                 Use `cargo truce install --aax` instead.".into()
            ),
            "--dev" => dev_mode = true,
            "-p" => {
                i += 1;
                plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a suffix")?);
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    // No format flags → enable every format in the project's default
    // features, mirroring `install`'s discovery rule.
    if !clap && !vst3 && !vst2 && !lv2 && !au2 {
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
        vst2 = available.contains("vst2");
        lv2 = available.contains("lv2");
        #[cfg(target_os = "macos")]
        {
            au2 = available.contains("au");
        }
    }

    let plugins: Vec<&PluginDef> = if let Some(ref f) = plugin_filter {
        config.plugin.iter().filter(|p| p.suffix == *f).collect()
    } else {
        config.plugin.iter().collect()
    };
    if plugins.is_empty() {
        return Err("no matching plugins".into());
    }

    let root = project_root();
    let dt = &deployment_target();
    let bundles_dir = root.join("target/bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;

    let extra_features: Vec<&str> = if dev_mode { vec!["dev"] } else { vec![] };

    // --- Build dylibs per format ---
    //
    // CLAP and VST3 share a dylib (built once with `--features clap,vst3`).
    // The other formats each get their own with `--features {format}`.
    // Each per-format build overwrites `target/release/lib{stem}.dylib`,
    // so we shuffle the CLAP/VST3 build into a `_plugin` backup and
    // restore it at the end before staging.
    if clap || vst3 {
        let mut feats: Vec<&str> = Vec::new();
        if clap { feats.push("clap"); }
        if vst3 { feats.push("vst3"); }
        for f in &extra_features { feats.push(f); }
        let combined = feats.join(",");
        let label = if extra_features.is_empty() {
            "Building CLAP + VST3...".to_string()
        } else {
            format!("Building CLAP + VST3 ({})...", extra_features.join(" + "))
        };
        eprintln!("{label}");
        for p in &plugins {
            let mut env_pairs: Vec<(&str, &str)> = Vec::new();
            if clap {
                if let Some(n) = p.clap_name.as_deref() {
                    env_pairs.push(("TRUCE_CLAP_NAME_OVERRIDE", n));
                }
            }
            if vst3 {
                if let Some(n) = p.vst3_name.as_deref() {
                    env_pairs.push(("TRUCE_VST3_NAME_OVERRIDE", n));
                }
            }
            cargo_build(
                &env_pairs,
                &["-p", &p.crate_name, "--no-default-features", "--features", &combined],
                dt,
            )?;
            let src = release_lib(&root, &p.dylib_stem());
            let dst = release_lib(&root, &format!("{}_plugin", p.dylib_stem()));
            if src.exists() {
                fs_ctx::copy(&src, &dst)?;
            }
        }
    }

    if vst2 {
        eprintln!("Building VST2...");
        for p in &plugins {
            let mut env_pairs: Vec<(&str, &str)> = Vec::new();
            if let Some(n) = p.vst2_name.as_deref() {
                env_pairs.push(("TRUCE_VST2_NAME_OVERRIDE", n));
            }
            cargo_build(
                &env_pairs,
                &["-p", &p.crate_name, "--no-default-features", "--features", "vst2"],
                dt,
            )?;
            let src = release_lib(&root, &p.dylib_stem());
            let dst = release_lib(&root, &format!("{}_vst2", p.dylib_stem()));
            fs_ctx::copy(&src, &dst)?;
        }
    }

    if lv2 {
        eprintln!("Building LV2...");
        for p in &plugins {
            let mut env_pairs: Vec<(&str, &str)> = Vec::new();
            if let Some(n) = p.lv2_name.as_deref() {
                env_pairs.push(("TRUCE_LV2_NAME_OVERRIDE", n));
            }
            cargo_build(
                &env_pairs,
                &["-p", &p.crate_name, "--no-default-features", "--features", "lv2"],
                dt,
            )?;
            let src = release_lib(&root, &p.dylib_stem());
            let dst = release_lib(&root, &format!("{}_lv2", p.dylib_stem()));
            fs_ctx::copy(&src, &dst)?;
        }
    }

    if au2 {
        eprintln!("Building AU v2...");
        for p in &plugins {
            let mut env_pairs: Vec<(&str, &str)> = vec![
                ("TRUCE_AU_VERSION", "2"),
                ("TRUCE_AU_PLUGIN_ID", &p.suffix),
            ];
            if let Some(n) = p.au_name.as_deref() {
                env_pairs.push(("TRUCE_AU_NAME_OVERRIDE", n));
            }
            cargo_build(
                &env_pairs,
                &["-p", &p.crate_name, "--no-default-features", "--features", "au"],
                dt,
            )?;
            let src = release_lib(&root, &p.dylib_stem());
            let dst = release_lib(&root, &format!("{}_au", p.dylib_stem()));
            fs_ctx::copy(&src, &dst)?;
        }
    }

    // Restore the CLAP/VST3 dylib at the canonical location now that the
    // per-format builds have all run (each clobbered it with its own
    // feature set).
    if clap || vst3 {
        for p in &plugins {
            let saved = release_lib(&root, &format!("{}_plugin", p.dylib_stem()));
            let dst = release_lib(&root, &p.dylib_stem());
            if saved.exists() {
                fs_ctx::copy(&saved, &dst)?;
            }
        }
    }

    // In dev mode, also build the debug dylibs (the logic that the
    // hot-reload shells watch and load).
    if dev_mode {
        eprintln!("Building debug dylibs (logic for hot-reload)...");
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--workspace");
        #[cfg(target_os = "macos")]
        cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
        let status = cmd.status()?;
        if !status.success() {
            return Err("debug workspace build failed".into());
        }
    }

    // --- Stage each format's bundle into target/bundles/ ---
    let identity = config.macos.application_identity();
    for p in &plugins {
        if clap {
            stage_clap(&root, p, &bundles_dir, identity)?;
            eprintln!("  CLAP: {}", bundles_dir.join(format!("{}.clap", p.name)).display());
        }
        if vst3 {
            stage_vst3(&root, p, &config, &bundles_dir)?;
            eprintln!("  VST3: {}", bundles_dir.join(format!("{}.vst3", p.name)).display());
        }
        if vst2 {
            stage_vst2(&root, p, &config, &bundles_dir)?;
            eprintln!("  VST2: {}", bundles_dir.join(format!("{}.vst", p.name)).display());
        }
        if lv2 {
            stage_lv2(&root, p, &bundles_dir)?;
            let slug = lv2_slug(&p.name);
            eprintln!("  LV2:  {}", bundles_dir.join(format!("{slug}.lv2")).display());
        }
        if au2 {
            stage_au2(&root, p, &config, &bundles_dir)?;
            eprintln!("  AU:   {}", bundles_dir.join(format!("{}.component", p.name)).display());
        }
    }

    eprintln!("Bundles in {}", bundles_dir.display());
    Ok(())
}
