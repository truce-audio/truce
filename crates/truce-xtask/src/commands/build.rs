//! `cargo truce build` — produce per-format bundles in `target/bundles/`
//! without installing.
//!
//! Every format flag (`--clap` / `--vst3` / `--vst2` / `--lv2` / `--au2`
//! / `--au3` / `--aax`) produces a self-contained, signed bundle in
//! `target/bundles/`; `cargo truce install` then copies those bundles
//! to system paths. See
//! `truce-docs/docs/internal/build-install-split.md`.

use crate::commands::package::stage::{
    lv2_slug, stage_au2, stage_clap, stage_lv2, stage_vst2, stage_vst3,
};
use crate::util::fs_ctx;
use crate::{
    cargo_build, deployment_target, detect_default_features, load_config, project_root,
    release_lib, PluginDef, Res,
};
use std::process::Command;

pub(crate) fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut lv2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
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
            "--dev" => dev_mode = true,
            "-p" => {
                i += 1;
                plugin_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin crate name")?,
                );
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    // No format flags → enable every format in the project's default
    // features, mirroring `install`'s discovery rule.
    if !clap && !vst3 && !vst2 && !lv2 && !au2 && !au3 && !aax {
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

    let plugins: Vec<&PluginDef> = if let Some(ref f) = plugin_filter {
        let matched: Vec<_> = config
            .plugin
            .iter()
            .filter(|p| p.crate_name == *f)
            .collect();
        if matched.is_empty() {
            return Err(format!(
                "No plugin with crate name '{f}'. Available: {}",
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
    // Each format gets its own cargo build with `--features {format}`.
    // Because every build overwrites `target/release/lib{stem}.dylib`,
    // we immediately copy the output to a format-suffixed path
    // (`_clap`, `_vst3`, `_vst2`, ...) that the stage/install steps
    // read from. Same pattern across all formats — keeps each path
    // self-contained with no implicit ordering.
    if clap {
        let mut feats: Vec<&str> = vec!["clap"];
        for f in &extra_features {
            feats.push(f);
        }
        let combined = feats.join(",");
        let label = if extra_features.is_empty() {
            "Building CLAP...".to_string()
        } else {
            format!("Building CLAP ({})...", extra_features.join(" + "))
        };
        crate::vprintln!("{label}");
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
                    &combined,
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
        let mut feats: Vec<&str> = vec!["vst3"];
        for f in &extra_features {
            feats.push(f);
        }
        let combined = feats.join(",");
        let label = if extra_features.is_empty() {
            "Building VST3...".to_string()
        } else {
            format!("Building VST3 ({})...", extra_features.join(" + "))
        };
        crate::vprintln!("{label}");
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
                    &combined,
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
            // Assemble the full .aaxplugin bundle (cmake template +
            // Rust dylib + Info.plist + codesign) into target/bundles/.
            // macOS/Windows-only; no-op on Linux. `cargo truce build`
            // is host-arch-only — `package --universal` is the only
            // caller that passes `true`.
            crate::commands::install::aax::emit_aax_bundle(&root, p, &config, false)?;
        }
    }

    // In dev mode, also build the debug dylibs (the logic that the
    // hot-reload shells watch and load).
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

    // --- Stage each format's bundle into target/bundles/ ---
    let identity = config.macos.application_identity();
    for p in &plugins {
        if clap {
            stage_clap(&root, p, &bundles_dir, identity)?;
            crate::log_output(format!(
                "CLAP: {}",
                bundles_dir.join(format!("{}.clap", p.name)).display()
            ));
        }
        if vst3 {
            stage_vst3(&root, p, &config, &bundles_dir)?;
            crate::log_output(format!(
                "VST3: {}",
                bundles_dir.join(format!("{}.vst3", p.name)).display()
            ));
        }
        if vst2 {
            stage_vst2(&root, p, &config, &bundles_dir)?;
            crate::log_output(format!(
                "VST2: {}",
                bundles_dir.join(format!("{}.vst", p.name)).display()
            ));
        }
        if lv2 {
            stage_lv2(&root, p, &bundles_dir)?;
            let slug = lv2_slug(&p.name);
            crate::log_output(format!(
                "LV2:  {}",
                bundles_dir.join(format!("{slug}.lv2")).display()
            ));
        }
        if au2 {
            stage_au2(&root, p, &config, &bundles_dir)?;
            crate::log_output(format!(
                "AU:   {}",
                bundles_dir.join(format!("{}.component", p.name)).display()
            ));
        }
    }

    // AU v3 has its own driver that builds Rust-framework + xcodebuild
    // + codesign inside-out and writes directly to target/bundles/.
    // Host arch only; universal builds are reserved for `package`.
    // macOS-only; the function returns a clear error on other platforms.
    if au3 {
        #[cfg(target_os = "macos")]
        {
            use crate::{extract_team_id, MacArch};
            // Same gate as install: ad-hoc / no-team-id makes AU v3
            // unbuildable. Soft-skip per plugin so the user gets the
            // same Built/Skipped surface as `cargo truce install`.
            let sign_id = config.macos.application_identity();
            if extract_team_id(sign_id).is_empty() {
                for p in &plugins {
                    crate::log_skip(format!(
                        "AU v3: skipped {} — needs a Developer ID with team ID. \
                         Set [macos.signing].application_identity in truce.toml \
                         (e.g., \"Developer ID Application: Your Name (TEAMID)\"); \
                         ad-hoc signing (\"-\") is not supported for AU v3 appex bundles.",
                        p.name
                    ));
                }
            } else {
                crate::commands::install::au_v3::emit_au_v3_bundle(
                    &root,
                    &config,
                    &plugins,
                    &[MacArch::host()],
                )?;
                for p in &plugins {
                    crate::log_output(format!(
                        "AU3:  {}",
                        bundles_dir
                            .join(format!("{}.app", p.au3_app_name()))
                            .display()
                    ));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            for p in &plugins {
                crate::log_skip(format!(
                    "AU v3: skipped {} — not supported on this platform. Use macOS to build AU v3.",
                    p.name
                ));
            }
        }
    }

    let outputs = crate::take_outputs();
    if !outputs.is_empty() {
        eprintln!("\nBuilt:");
        for line in outputs {
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
    eprintln!("\nBundles in {}", bundles_dir.display());
    Ok(())
}
