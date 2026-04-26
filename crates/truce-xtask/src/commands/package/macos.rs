//! macOS packaging pipeline: per-arch builds, lipo, stage, pkgbuild,
//! productbuild, optional notarization.

#![cfg(target_os = "macos")]

use super::stage::{
    generate_distribution_xml, stage_aax, stage_au2, stage_au3, stage_clap, stage_vst2, stage_vst3,
    write_postinstall_script,
};
use super::PkgFormat;
use crate::{
    cargo_build_for_arch, copy_dir_recursive, deployment_target, detect_default_features,
    lipo_into, load_config, project_root, read_workspace_version, release_lib_for_target, Config,
    MacArch, PluginDef, Res,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn cmd_package_macos(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut format_str: Option<String> = None;
    let mut no_notarize = false;
    let mut host_only = false;
    let mut no_pace_sign = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                plugin_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin crate name")?,
                );
            }
            "--formats" => {
                i += 1;
                format_str = Some(args.get(i).cloned().ok_or("--formats requires a value")?);
            }
            "--no-notarize" => no_notarize = true,
            "--no-pace-sign" => no_pace_sign = true,
            // Universal is the default on macOS — accept the flag explicitly
            // as a no-op so cross-platform CI scripts (that also hit Windows)
            // keep working.
            "--universal" => {}
            "--host-only" => host_only = true,
            // --no-sign implies skipping all signing, including PACE. Apple
            // codesign on macOS is not actually skippable today (we always
            // pass through the configured identity, ad-hoc when none), but
            // PACE is — accept the flag and treat it as `--no-pace-sign`.
            "--no-sign" => no_pace_sign = true,
            // --no-installer is a Windows-only flag; accept and ignore so
            // cross-platform CI scripts don't break.
            "--no-installer" => {}
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Universal by default: produce a fat Mach-O covering both Apple arches.
    // `--host-only` falls back to the host-only build for faster dev iteration.
    let archs: Vec<MacArch> = if host_only {
        vec![MacArch::host()]
    } else {
        vec![MacArch::X86_64, MacArch::Arm64]
    };
    let universal = archs.len() > 1;

    // Resolve formats
    let formats: Vec<PkgFormat> = if let Some(ref s) = format_str {
        PkgFormat::parse_list(s)?
    } else if !config.packaging.formats.is_empty() {
        PkgFormat::parse_list(&config.packaging.formats.join(","))?
    } else {
        // Default: auto-detect from project features
        let available = detect_default_features();
        let mut fmts = Vec::new();
        if available.contains("clap") {
            fmts.push(PkgFormat::Clap);
        }
        if available.contains("vst3") {
            fmts.push(PkgFormat::Vst3);
        }
        if available.contains("vst2") {
            fmts.push(PkgFormat::Vst2);
        }
        if available.contains("au") {
            fmts.push(PkgFormat::Au2);
            fmts.push(PkgFormat::Au3);
        }
        if available.contains("aax") {
            fmts.push(PkgFormat::Aax);
        }
        fmts
    };

    if formats.is_empty() {
        return Err("no formats to package".into());
    }

    // Resolve plugins
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

    let has_clap = formats.contains(&PkgFormat::Clap);
    let has_vst3 = formats.contains(&PkgFormat::Vst3);
    let has_vst2 = formats.contains(&PkgFormat::Vst2);
    let has_au2 = formats.contains(&PkgFormat::Au2);
    let has_au3 = formats.contains(&PkgFormat::Au3);
    let has_aax = formats.contains(&PkgFormat::Aax);

    // ---------------------------------------------------------------
    // Step 1: Build all requested formats (release mode).
    //
    // Per format, build once per arch (adding `--target <triple>`) then
    // `lipo -create` the per-arch outputs into the canonical
    // `target/release/lib{stem}_{fmt}.dylib` location. The stage functions
    // below read from that path and don't need to know whether the build
    // was universal.
    // ---------------------------------------------------------------

    eprintln!(
        "Packaging archs: {}",
        archs
            .iter()
            .map(|a| a.triple())
            .collect::<Vec<_>>()
            .join(", ")
    );

    if has_clap {
        for &arch in &archs {
            eprintln!("Building CLAP ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "clap"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(&root, &p.dylib_stem(), Some(arch.triple()));
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_clap", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_clap", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = crate::target_dir(&root).join(format!("release/lib{}_clap.dylib", p.dylib_stem()));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_vst3 {
        for &arch in &archs {
            eprintln!("Building VST3 ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "vst3"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(&root, &p.dylib_stem(), Some(arch.triple()));
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_vst3", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_vst3", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = crate::target_dir(&root).join(format!("release/lib{}_vst3.dylib", p.dylib_stem()));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_vst2 {
        for &arch in &archs {
            eprintln!("Building VST2 ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "vst2"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(&root, &p.dylib_stem(), Some(arch.triple()));
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_vst2", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_vst2", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = crate::target_dir(&root).join(format!("release/lib{}_vst2.dylib", p.dylib_stem()));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_au2 {
        // AU v2 is built per-plugin (distinct TRUCE_AU_PLUGIN_ID env var),
        // so the outer loop is plugins × archs rather than archs × plugins.
        for p in &plugins {
            for &arch in &archs {
                eprintln!("Building AU v2 ({}, {})...", p.name, arch.triple());
                cargo_build_for_arch(
                    &[
                        ("TRUCE_AU_VERSION", "2"),
                        ("TRUCE_AU_PLUGIN_ID", &p.bundle_id),
                    ],
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        "au",
                    ],
                    arch,
                    dt,
                )?;
                let src = release_lib_for_target(&root, &p.dylib_stem(), Some(arch.triple()));
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_au", p.dylib_stem()),
                    Some(arch.triple()),
                );
                fs::copy(&src, &saved)?;
            }
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_au", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = crate::target_dir(&root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_aax {
        for &arch in &archs {
            eprintln!("Building AAX ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "aax"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(&root, &p.dylib_stem(), Some(arch.triple()));
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_aax", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_aax", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = crate::target_dir(&root).join(format!("release/lib{}_aax.dylib", p.dylib_stem()));
            lipo_into(&inputs, &output)?;
        }
        // Apple-sign + assemble the .aaxplugin bundle once we have the
        // universal Rust dylib. PACE wrap happens later in stage_aax
        // against the staging copy.
        for p in &plugins {
            crate::commands::install::aax::emit_aax_bundle(&root, p, &config, universal)?;
        }
    }

    if has_au3 {
        // Build per-arch Rust framework, lipo, xcodebuild, sign
        // inside-out → `target/bundles/{Plugin Name}.app/`. `stage_au3`
        // copies from there into the packaging staging tree.
        crate::commands::install::au_v3::emit_au_v3_bundle(&root, &config, &plugins, &archs)?;
    }

    // ---------------------------------------------------------------
    // Step 2–7: Stage, sign, build .pkg per plugin
    // ---------------------------------------------------------------

    let dist_dir = crate::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    let version = read_workspace_version(&root).unwrap_or_else(|| "0.0.0".to_string());

    for p in &plugins {
        eprintln!("\n=== Packaging: {} ===", p.name);

        let staging = crate::target_dir(&root).join("package").join(&p.bundle_id);
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        // Step 2: Stage signed bundles
        for fmt in &formats {
            eprint!("  Staging {}... ", fmt.label());
            let result = match fmt {
                PkgFormat::Clap => {
                    stage_clap(&root, p, &staging, config.macos.application_identity())
                }
                PkgFormat::Vst3 => stage_vst3(&root, p, &config, &staging),
                PkgFormat::Vst2 => stage_vst2(&root, p, &config, &staging).map(|_| ()),
                PkgFormat::Au2 => stage_au2(&root, p, &config, &staging),
                PkgFormat::Au3 => stage_au3(&root, p, &config, &staging),
                PkgFormat::Aax => stage_aax(&root, p, &config, &staging, universal, no_pace_sign),
            };
            match result {
                Ok(()) => eprintln!("ok"),
                Err(e) => {
                    eprintln!("FAILED: {e}");
                    return Err(e);
                }
            }
        }

        // Step 3: Build component .pkg per format
        let components_dir = staging.join("components");
        fs::create_dir_all(&components_dir)?;

        // Prepare AU postinstall script
        let scripts_dir = staging.join("au_scripts");
        if has_au2 {
            write_postinstall_script(&scripts_dir)?;
        }

        for fmt in &formats {
            let bundle_name = fmt.bundle_name(p);
            let component_path = staging.join(&bundle_name);
            let pkg_id = format!(
                "{}.{}.{}",
                config.vendor.id,
                p.bundle_id,
                fmt.pkg_id_suffix()
            );
            let component_pkg = components_dir.join(format!("{}-{}.pkg", p.name, fmt.label()));

            let mut pkgbuild_args = if fmt.is_native_bundle() {
                // VST3, AU2: recognized macOS bundle types
                vec![
                    "--component".to_string(),
                    component_path.to_str().unwrap().to_string(),
                    "--install-location".to_string(),
                    fmt.install_location().to_string(),
                ]
            } else {
                // CLAP, VST2, AAX: not recognized by pkgbuild --component.
                // Use --root with a temp directory containing just this bundle,
                // and set --install-location to the parent directory.
                let root_dir = staging.join(format!("_pkgroot_{}", fmt.label()));
                let _ = fs::remove_dir_all(&root_dir);
                fs::create_dir_all(&root_dir)?;
                let dst = root_dir.join(&bundle_name);
                if component_path.is_dir() {
                    copy_dir_recursive(&component_path, &dst)?;
                } else {
                    fs::copy(&component_path, &dst)?;
                }
                vec![
                    "--root".to_string(),
                    root_dir.to_str().unwrap().to_string(),
                    "--install-location".to_string(),
                    fmt.install_location().to_string(),
                ]
            };

            pkgbuild_args.extend_from_slice(&[
                "--identifier".to_string(),
                pkg_id,
                "--version".to_string(),
                version.to_string(),
            ]);

            // AU2 gets a postinstall script to clear caches
            if *fmt == PkgFormat::Au2 {
                pkgbuild_args.push("--scripts".to_string());
                pkgbuild_args.push(scripts_dir.to_str().unwrap().to_string());
            }

            pkgbuild_args.push(component_pkg.to_str().unwrap().to_string());

            let pkgbuild_refs: Vec<&str> = pkgbuild_args.iter().map(|s| s.as_str()).collect();
            eprintln!("  pkgbuild {}...", fmt.label());
            let status = Command::new("pkgbuild").args(&pkgbuild_refs).status()?;
            if !status.success() {
                return Err(format!("pkgbuild failed for {} {}", p.name, fmt.label()).into());
            }
        }

        // Step 4: Generate distribution.xml
        let dist_xml = generate_distribution_xml(
            &p.name,
            &config.vendor.id,
            &p.bundle_id,
            &formats,
            &version,
            Some(&config.packaging),
        );
        let dist_xml_path = staging.join("distribution.xml");
        fs::write(&dist_xml_path, &dist_xml)?;

        // Step 5: Prepare resources (optional welcome/license html)
        let resources_dir = staging.join("resources");
        fs::create_dir_all(&resources_dir)?;
        if let Some(ref html) = config.packaging.welcome_html {
            let src = root.join(html);
            if src.exists() {
                fs::copy(&src, resources_dir.join("welcome.html"))?;
            }
        }
        if let Some(ref html) = config.packaging.license_html {
            let src = root.join(html);
            if src.exists() {
                fs::copy(&src, resources_dir.join("license.html"))?;
            }
        }

        // Step 6: productbuild → signed .pkg
        let pkg_name = format!("{}-{}-macos.pkg", p.name, version);
        let pkg_path = dist_dir.join(&pkg_name);

        let mut pb_args = vec![
            "--distribution",
            dist_xml_path.to_str().unwrap(),
            "--package-path",
            components_dir.to_str().unwrap(),
            "--resources",
            resources_dir.to_str().unwrap(),
        ];

        let installer_id = config.macos.installer_identity();
        if let Some(id) = installer_id {
            pb_args.push("--sign");
            pb_args.push(id);
        }

        pb_args.push(pkg_path.to_str().unwrap());

        eprintln!("  productbuild...");
        let status = Command::new("productbuild").args(&pb_args).status()?;
        if !status.success() {
            return Err(format!("productbuild failed for {}", p.name).into());
        }

        // Step 7: Notarize + staple
        if config.macos.packaging.notarize && !no_notarize {
            notarize_and_staple(&pkg_path, &config)?;
        } else if !config.macos.packaging.notarize {
            eprintln!("  Skipped notarization (set notarize = true in [macos.packaging])");
        } else {
            eprintln!("  Skipped notarization (--no-notarize)");
        }

        eprintln!("  Package ready: {}", pkg_path.display());
    }

    eprintln!("\nDone. Installers in {}", dist_dir.display());
    Ok(())
}

/// Notarize a .pkg and staple the ticket. (Phase 3)
fn notarize_and_staple(pkg_path: &Path, config: &Config) -> Res {
    let pkg = pkg_path.to_str().unwrap();

    // Determine credential source: env vars or truce.toml
    let apple_id_env = std::env::var("APPLE_ID").unwrap_or_default();
    let team_id_env = std::env::var("TEAM_ID").unwrap_or_default();
    let apple_id = config
        .macos
        .packaging
        .apple_id
        .as_deref()
        .unwrap_or(&apple_id_env);
    let team_id = config
        .macos
        .packaging
        .team_id
        .as_deref()
        .unwrap_or(&team_id_env);

    // First try keychain profile, then fall back to explicit credentials
    let keychain_profile =
        std::env::var("TRUCE_NOTARY_PROFILE").unwrap_or_else(|_| "TRUCE_NOTARY".to_string());

    eprintln!(
        "  Notarizing {}...",
        pkg_path.file_name().unwrap().to_str().unwrap()
    );

    // Submit and capture output to check status + extract submission ID
    let output = Command::new("xcrun")
        .args([
            "notarytool",
            "submit",
            pkg,
            "--keychain-profile",
            &keychain_profile,
            "--wait",
        ])
        .output();

    let (succeeded, output_text) = match output {
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            // notarytool returns 0 even on Invalid — check the status string
            let ok = o.status.success()
                && !text.contains("status: Invalid")
                && !text.contains("status: Rejected");
            (ok, text)
        }
        Err(_) => (false, String::new()),
    };

    if !succeeded {
        // Try explicit credentials as fallback
        if !apple_id.is_empty() && !team_id.is_empty() {
            eprintln!("  Keychain profile failed, trying explicit credentials...");
            let password = std::env::var("APP_SPECIFIC_PASSWORD").map_err(|_| {
                "notarization requires APP_SPECIFIC_PASSWORD env var or a keychain profile"
            })?;
            let output = Command::new("xcrun")
                .args([
                    "notarytool",
                    "submit",
                    pkg,
                    "--apple-id",
                    apple_id,
                    "--team-id",
                    team_id,
                    "--password",
                    &password,
                    "--wait",
                ])
                .output()?;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if !output.status.success()
                || text.contains("status: Invalid")
                || text.contains("status: Rejected")
            {
                // Extract submission ID and fetch the log
                fetch_notarization_log(&text, &keychain_profile);
                return Err(
                    "notarization failed (status: Invalid). See log above for details.".into(),
                );
            }
        } else {
            // Extract submission ID and fetch the log
            fetch_notarization_log(&output_text, &keychain_profile);
            if output_text.contains("status: Invalid") || output_text.contains("status: Rejected") {
                return Err(
                    "notarization failed (status: Invalid). See log above for details.".into(),
                );
            }
            return Err("notarization failed. Set up credentials via:\n  \
                 xcrun notarytool store-credentials TRUCE_NOTARY\n  \
                 or set apple_id/team_id in [macos.packaging] + APP_SPECIFIC_PASSWORD env var"
                .into());
        }
    }

    // Staple
    eprintln!("  Stapling...");
    let status = Command::new("xcrun")
        .args(["stapler", "staple", pkg])
        .status()?;
    if !status.success() {
        return Err("stapler staple failed".into());
    }

    eprintln!("  Notarized and stapled.");
    Ok(())
}

/// Extract submission ID from notarytool output and fetch the detailed log.
fn fetch_notarization_log(output: &str, keychain_profile: &str) {
    // Look for "id: <uuid>" in the output
    let id = output
        .lines()
        .find(|l| l.trim().starts_with("id:"))
        .and_then(|l| l.trim().strip_prefix("id:"))
        .map(|s| s.trim().to_string());

    if let Some(id) = id {
        eprintln!("  Fetching notarization log for {}...", id);
        let log_output = Command::new("xcrun")
            .args([
                "notarytool",
                "log",
                &id,
                "--keychain-profile",
                keychain_profile,
            ])
            .output();
        if let Ok(o) = log_output {
            let log = String::from_utf8_lossy(&o.stdout);
            if !log.is_empty() {
                eprintln!("\n--- Notarization Log ---");
                eprintln!("{log}");
                eprintln!("--- End Log ---\n");
            }
        }
    }
}
