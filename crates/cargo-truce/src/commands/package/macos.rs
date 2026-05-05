//! macOS packaging pipeline: per-arch builds, lipo, stage, pkgbuild,
//! productbuild, optional notarization.

#![cfg(target_os = "macos")]

use super::PkgFormat;
use super::stage::{
    generate_distribution_xml, stage_aax, stage_au2, stage_au3, stage_clap, stage_vst2, stage_vst3,
    write_postinstall_script,
};
use crate::install_scope::{PkgScope, note_once};
use crate::{
    Config, MacArch, PluginDef, Res, cargo_build_for_arch, copy_dir_recursive, deployment_target,
    detect_default_features, lipo_into, load_config, project_root, read_workspace_version,
    release_lib_for_target,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn cmd_package_macos(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let parsed = parse_package_args(args)?;

    // Scope resolution: CLI > truce.toml [packaging] preferred_scope >
    // OS default (`--ask`). `cargo truce install` has no toml
    // override — the install scope is a per-invocation developer
    // choice, not a project-wide one.
    let scope = resolve_pkg_scope(parsed.cli_scope, &config)?;
    eprintln!("Package scope: {}", scope.label());

    // Universal by default: produce a fat Mach-O covering both Apple arches.
    // `--host-only` falls back to the host-only build for faster dev iteration.
    let archs: Vec<MacArch> = if parsed.host_only {
        vec![MacArch::host()]
    } else {
        vec![MacArch::X86_64, MacArch::Arm64]
    };
    let universal = archs.len() > 1;

    let formats = resolve_formats(parsed.format_str.as_deref(), &config)?;
    if formats.is_empty() {
        return Err("no formats to package".into());
    }
    let effective_scope = compute_effective_scope(scope, &formats);

    let plugins: Vec<&PluginDef> =
        crate::commands::pick_plugins(&config, parsed.plugin_filter.as_deref())?;

    eprintln!(
        "Packaging archs: {}",
        archs
            .iter()
            .map(|a| a.triple())
            .collect::<Vec<_>>()
            .join(", ")
    );

    build_all_formats(&root, &config, &plugins, &archs, dt, &formats, universal)?;

    let dist_dir = truce_build::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    let version = read_workspace_version(&root).unwrap_or_else(|| "0.0.0".to_string());

    let opts = PackageOpts {
        config: &config,
        formats: &formats,
        scope,
        effective_scope,
        version: &version,
        no_notarize: parsed.no_notarize,
        no_pace_sign: parsed.no_pace_sign,
        universal,
        has_au2: formats.contains(&PkgFormat::Au2),
    };
    for p in &plugins {
        package_one_plugin(&root, p, &dist_dir, &opts)?;
    }

    eprintln!("\nDone. Installers in {}", dist_dir.display());
    Ok(())
}

/// Parsed CLI flags for `cargo truce package` on macOS.
struct PackageArgs {
    plugin_filter: Option<String>,
    format_str: Option<String>,
    no_notarize: bool,
    host_only: bool,
    no_pace_sign: bool,
    cli_scope: Option<PkgScope>,
}

fn parse_package_args(args: &[String]) -> Result<PackageArgs, crate::BoxErr> {
    let mut plugin_filter: Option<String> = None;
    let mut format_str: Option<String> = None;
    let mut no_notarize = false;
    let mut host_only = false;
    let mut no_pace_sign = false;
    let mut cli_scope: Option<PkgScope> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--formats" => {
                format_str = Some(crate::util::arg_value(args, &mut i, "--formats")?.to_string());
            }
            "--no-notarize" => no_notarize = true,
            // `--no-sign` skips all signing including PACE. Apple codesign
            // on macOS is not actually skippable today (we always pass
            // through the configured identity, ad-hoc when none), so on
            // this platform `--no-sign` is treated as `--no-pace-sign`.
            "--no-pace-sign" | "--no-sign" => no_pace_sign = true,
            "--user" => set_cli_scope(&mut cli_scope, PkgScope::User)?,
            "--system" => set_cli_scope(&mut cli_scope, PkgScope::System)?,
            "--ask" => set_cli_scope(&mut cli_scope, PkgScope::Ask)?,
            // `--universal` is the default on macOS; `--no-installer` is a
            // Windows-only flag. Accept both as no-ops so cross-platform CI
            // scripts that also hit Windows keep working.
            "--universal" | "--no-installer" => {}
            "--host-only" => host_only = true,
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    Ok(PackageArgs {
        plugin_filter,
        format_str,
        no_notarize,
        host_only,
        no_pace_sign,
        cli_scope,
    })
}

/// Resolve the format list from CLI > toml > feature-detection.
fn resolve_formats(
    format_str: Option<&str>,
    config: &Config,
) -> Result<Vec<PkgFormat>, crate::BoxErr> {
    if let Some(s) = format_str {
        PkgFormat::parse_list(s)
    } else if !config.packaging.formats.is_empty() {
        PkgFormat::parse_list(&config.packaging.formats.join(","))
    } else {
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
        Ok(fmts)
    }
}

/// Widen `--user` scope to `System` when system-only formats (AAX,
/// AU v3) are in the bundle. macOS Installer.app's `<domains>` is
/// global to the installer, not per-payload — pure user-scope is only
/// possible when the format mix supports it. Emits a `note_once` per
/// system-only format so the developer sees why the widen happened.
fn compute_effective_scope(scope: PkgScope, formats: &[PkgFormat]) -> PkgScope {
    let has_system_only = formats
        .iter()
        .any(|f| matches!(f, PkgFormat::Aax | PkgFormat::Au3));
    match scope {
        PkgScope::User if has_system_only => {
            for f in formats {
                match f {
                    PkgFormat::Aax => note_once(
                        "AAX is system-only; --user package keeps AAX but installs every \
                         format to /Library/ (macOS Installer.app can't mix per-payload \
                         scopes). Drop AAX with --formats to keep a pure user-scope build.",
                    ),
                    PkgFormat::Au3 => note_once(
                        "AU v3 is system-only; --user package keeps AU v3 but installs every \
                         format to /Library/ (macOS Installer.app can't mix per-payload \
                         scopes). Drop AU v3 with --formats to keep a pure user-scope build.",
                    ),
                    _ => {}
                }
            }
            PkgScope::System
        }
        other => other,
    }
}

/// Drive Step 1 of the packaging pipeline: per-arch builds + lipo for
/// every requested format. Stage functions read from the canonical
/// `target/release/lib{stem}_{fmt}.dylib` paths populated here and
/// don't need to know whether the build was universal.
fn build_all_formats(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    archs: &[MacArch],
    dt: &str,
    formats: &[PkgFormat],
    universal: bool,
) -> Res {
    if formats.contains(&PkgFormat::Clap) {
        build_and_lipo_format(root, plugins, archs, dt, "clap", "CLAP")?;
    }
    if formats.contains(&PkgFormat::Vst3) {
        build_and_lipo_format(root, plugins, archs, dt, "vst3", "VST3")?;
    }
    if formats.contains(&PkgFormat::Vst2) {
        build_and_lipo_format(root, plugins, archs, dt, "vst2", "VST2")?;
    }
    if formats.contains(&PkgFormat::Au2) {
        // AU v2 is built per-plugin (distinct TRUCE_AU_PLUGIN_ID env var),
        // so the outer loop is plugins × archs rather than archs × plugins.
        for p in plugins {
            build_and_lipo_au2(root, p, archs, dt)?;
        }
    }
    if formats.contains(&PkgFormat::Aax) {
        build_and_lipo_format(root, plugins, archs, dt, "aax", "AAX")?;
        // Apple-sign + assemble the .aaxplugin bundle once we have the
        // universal Rust dylib. PACE wrap happens later in stage_aax
        // against the staging copy.
        for p in plugins {
            crate::commands::install::aax::emit_aax_bundle(root, p, config, universal)?;
        }
    }
    if formats.contains(&PkgFormat::Au3) {
        // Build per-arch Rust framework, lipo, xcodebuild, sign
        // inside-out → `target/bundles/{Plugin Name}.app/`. `stage_au3`
        // copies from there into the packaging staging tree.
        crate::commands::install::au_v3::emit_au_v3_bundle(root, config, plugins, archs)?;
    }
    Ok(())
}

/// AU v2 per-plugin build: each plugin needs its own
/// `TRUCE_AU_PLUGIN_ID` env var, so the multi-format helper doesn't
/// fit. Builds per-arch then lipos to `lib{stem}_au.dylib`.
fn build_and_lipo_au2(root: &Path, p: &PluginDef, archs: &[MacArch], dt: &str) -> Res {
    for &arch in archs {
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
        let src = release_lib_for_target(root, &p.dylib_stem(), Some(arch.triple()));
        let saved =
            release_lib_for_target(root, &format!("{}_au", p.dylib_stem()), Some(arch.triple()));
        fs::copy(&src, &saved)?;
    }
    let inputs: Vec<PathBuf> = archs
        .iter()
        .map(|a| release_lib_for_target(root, &format!("{}_au", p.dylib_stem()), Some(a.triple())))
        .collect();
    let output =
        truce_build::target_dir(root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));
    lipo_into(&inputs, &output)?;
    Ok(())
}

/// Captured driver state shared across the per-plugin packaging loop.
/// Carrying these as a struct keeps `package_one_plugin`'s signature
/// readable instead of fanning ten args out at every call.
// Sparse independent CLI flags — bitflags would just add ceremony.
#[allow(clippy::struct_excessive_bools)]
struct PackageOpts<'a> {
    config: &'a Config,
    formats: &'a [PkgFormat],
    scope: PkgScope,
    effective_scope: PkgScope,
    version: &'a str,
    no_notarize: bool,
    no_pace_sign: bool,
    universal: bool,
    has_au2: bool,
}

/// Stage signed bundles, run pkgbuild per format, then productbuild
/// the distribution. The function follows the original numbered steps
/// (2 through 7) — splitting them into separate helpers would inflate
/// the boilerplate without surfacing any reuse, since `cmd_package_macos`
/// is the only caller.
fn package_one_plugin(root: &Path, p: &PluginDef, dist_dir: &Path, o: &PackageOpts) -> Res {
    eprintln!("\n=== Packaging: {} ===", p.name);

    let staging = truce_build::target_dir(root)
        .join("package")
        .join(&p.bundle_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // Step 2: Stage signed bundles
    for fmt in o.formats {
        eprint!("  Staging {}... ", fmt.label());
        let result = match fmt {
            PkgFormat::Clap => stage_clap(root, p, &staging, o.config.macos.application_identity()),
            PkgFormat::Vst3 => stage_vst3(root, p, o.config, &staging),
            PkgFormat::Vst2 => stage_vst2(root, p, o.config, &staging).map(|_| ()),
            PkgFormat::Au2 => stage_au2(root, p, o.config, &staging),
            PkgFormat::Au3 => stage_au3(root, p, o.config, &staging),
            PkgFormat::Aax => stage_aax(root, p, o.config, &staging, o.universal, o.no_pace_sign),
        };
        match result {
            Ok(()) => eprintln!("ok"),
            Err(e) => {
                eprintln!("FAILED: {e}");
                return Err(e);
            }
        }
    }

    // Step 2.5: Notarization-readiness check.
    // Mirror Apple's notarization-server checks locally — every
    // Mach-O under the staged tree needs Developer ID +
    // timestamp + hardened runtime. Catches unsigned inner
    // Mach-Os (codesign --deep doesn't recurse into AAX
    // Resources/), missing --timestamp, missing --options
    // runtime, ad-hoc cert leakage. No-op when the signing
    // identity is ad-hoc.
    eprint!("  Verifying signing readiness... ");
    match crate::util::verify_signed_for_notarization(
        &staging,
        o.config.macos.application_identity(),
    ) {
        Ok(()) => eprintln!("ok"),
        Err(e) => {
            eprintln!("FAILED");
            return Err(e);
        }
    }

    // Step 3: Build component .pkg per format
    let components_dir = staging.join("components");
    fs::create_dir_all(&components_dir)?;

    // Prepare AU postinstall script
    let scripts_dir = staging.join("au_scripts");
    if o.has_au2 {
        write_postinstall_script(&scripts_dir)?;
    }

    for fmt in o.formats {
        run_pkgbuild_for_format(p, fmt, &staging, &components_dir, &scripts_dir, o)?;
    }

    // Step 4: Generate distribution.xml
    let dist_xml = generate_distribution_xml(
        &p.name,
        &o.config.vendor.id,
        &p.bundle_id,
        o.formats,
        o.version,
        Some(&o.config.packaging),
        o.effective_scope,
    );
    let dist_xml_path = staging.join("distribution.xml");
    fs::write(&dist_xml_path, &dist_xml)?;

    // Step 5: Prepare resources (optional welcome/license html)
    let resources_dir = staging.join("resources");
    fs::create_dir_all(&resources_dir)?;
    for (key, dst_name) in [
        (o.config.packaging.welcome_html.as_deref(), "welcome.html"),
        (o.config.packaging.license_html.as_deref(), "license.html"),
    ] {
        if let Some(html) = key {
            let src = root.join(html);
            if src.exists() {
                fs::copy(&src, resources_dir.join(dst_name))?;
            }
        }
    }

    let pkg_path = run_productbuild(
        p,
        dist_dir,
        &dist_xml_path,
        &components_dir,
        &resources_dir,
        o,
    )?;

    // Step 7: Notarize + staple
    if o.config.macos.packaging.notarize && !o.no_notarize {
        notarize_and_staple(&pkg_path, o.config)?;
    } else if !o.config.macos.packaging.notarize {
        eprintln!("  Skipped notarization (set notarize = true in [macos.packaging])");
    } else {
        eprintln!("  Skipped notarization (--no-notarize)");
    }

    eprintln!("  Package ready: {}", pkg_path.display());
    Ok(())
}

/// Step 6 of the per-plugin packaging pipeline: productbuild → signed
/// `.pkg`. The dist suffix uses the developer-requested `scope`, not
/// the effective one — a `--user` build that quietly widens to
/// system-domain because of AAX still gets the `-user` filename so the
/// developer's CI scripts find it.
fn run_productbuild(
    p: &PluginDef,
    dist_dir: &Path,
    dist_xml_path: &Path,
    components_dir: &Path,
    resources_dir: &Path,
    o: &PackageOpts,
) -> Result<PathBuf, crate::BoxErr> {
    let pkg_name = format!(
        "{}-{}-macos{}.pkg",
        p.name,
        o.version,
        o.scope.dist_suffix()
    );
    let pkg_path = dist_dir.join(&pkg_name);

    let mut pb_args = vec![
        "--distribution",
        dist_xml_path.to_str().unwrap(),
        "--package-path",
        components_dir.to_str().unwrap(),
        "--resources",
        resources_dir.to_str().unwrap(),
    ];

    if let Some(id) = o.config.macos.installer_identity() {
        pb_args.push("--sign");
        pb_args.push(id);
    }

    pb_args.push(pkg_path.to_str().unwrap());

    eprintln!("  productbuild...");
    let status = Command::new("productbuild").args(&pb_args).status()?;
    if !status.success() {
        return Err(format!("productbuild failed for {}", p.name).into());
    }
    Ok(pkg_path)
}

/// Run `pkgbuild` to wrap a single staged format into a component .pkg.
/// VST3 and AU2 are recognized macOS bundle types so `--component` works
/// directly; CLAP / VST2 / AAX need a temporary `--root` tree because
/// `pkgbuild` rejects them with `--component`.
fn run_pkgbuild_for_format(
    p: &PluginDef,
    fmt: &PkgFormat,
    staging: &Path,
    components_dir: &Path,
    scripts_dir: &Path,
    o: &PackageOpts,
) -> Res {
    let bundle_name = fmt.bundle_name(p);
    let component_path = staging.join(&bundle_name);
    let pkg_id = format!(
        "{}.{}.{}",
        o.config.vendor.id,
        p.bundle_id,
        fmt.pkg_id_suffix()
    );
    let component_pkg = components_dir.join(format!("{}-{}.pkg", p.name, fmt.label()));

    let mut pkgbuild_args = if fmt.is_native_bundle() {
        vec![
            "--component".to_string(),
            component_path.to_str().unwrap().to_string(),
            "--install-location".to_string(),
            fmt.install_location().to_string(),
        ]
    } else {
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
        o.version.to_string(),
    ]);

    if *fmt == PkgFormat::Au2 {
        pkgbuild_args.push("--scripts".to_string());
        pkgbuild_args.push(scripts_dir.to_str().unwrap().to_string());
    }

    pkgbuild_args.push(component_pkg.to_str().unwrap().to_string());

    let pkgbuild_refs: Vec<&str> = pkgbuild_args
        .iter()
        .map(std::string::String::as_str)
        .collect();
    eprintln!("  pkgbuild {}...", fmt.label());
    let status = Command::new("pkgbuild").args(&pkgbuild_refs).status()?;
    if !status.success() {
        return Err(format!("pkgbuild failed for {} {}", p.name, fmt.label()).into());
    }
    Ok(())
}

/// Build the workspace-wide `--no-default-features --features {feature}`
/// dylib for every requested arch, save each per-arch artifact under a
/// `_{feature}` suffix, then `lipo -create` the per-arch outputs into
/// the canonical `target/release/lib{stem}_{feature}.dylib` location
/// the stage helpers read from.
///
/// Used for CLAP / VST3 / VST2 / AAX, which all share the same
/// "build per arch, then lipo" shape. AU2 is per-plugin (distinct
/// `TRUCE_AU_PLUGIN_ID` env var) and AU3 has its own framework
/// pipeline, so neither routes through here.
fn build_and_lipo_format(
    root: &Path,
    plugins: &[&PluginDef],
    archs: &[MacArch],
    dt: &str,
    feature: &str,
    label: &str,
) -> Res {
    let suffix = format!("_{feature}");
    for &arch in archs {
        eprintln!("Building {label} ({})...", arch.triple());
        let mut base: Vec<&str> = Vec::new();
        for p in plugins {
            base.push("-p");
            base.push(&p.crate_name);
        }
        base.extend_from_slice(&["--no-default-features", "--features", feature]);
        cargo_build_for_arch(&[], &base, arch, dt)?;
        for p in plugins {
            let src = release_lib_for_target(root, &p.dylib_stem(), Some(arch.triple()));
            let saved = release_lib_for_target(
                root,
                &format!("{}{suffix}", p.dylib_stem()),
                Some(arch.triple()),
            );
            if src.exists() {
                fs::copy(&src, &saved)?;
            }
        }
    }
    for p in plugins {
        let inputs: Vec<PathBuf> = archs
            .iter()
            .map(|a| {
                release_lib_for_target(
                    root,
                    &format!("{}{suffix}", p.dylib_stem()),
                    Some(a.triple()),
                )
            })
            .collect();
        let output = truce_build::target_dir(root)
            .join(format!("release/lib{}{suffix}.dylib", p.dylib_stem()));
        lipo_into(&inputs, &output)?;
    }
    Ok(())
}

fn set_cli_scope(slot: &mut Option<PkgScope>, want: PkgScope) -> Res {
    if let Some(prev) = *slot
        && prev != want
    {
        return Err("--user, --system, and --ask are mutually exclusive".into());
    }
    *slot = Some(want);
    Ok(())
}

fn resolve_pkg_scope(cli: Option<PkgScope>, config: &Config) -> Result<PkgScope, crate::BoxErr> {
    if let Some(s) = cli {
        return Ok(s);
    }
    if let Some(ref raw) = config.packaging.preferred_scope {
        return raw.parse::<PkgScope>().map_err(Into::into);
    }
    Ok(PkgScope::os_default())
}

/// Notarize a .pkg and staple the ticket. (Phase 3)
#[allow(clippy::too_many_lines)]
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
            let password = std::env::var("APP_SPECIFIC_PASSWORD").map_err(
                |_| "notarization requires APP_SPECIFIC_PASSWORD env var or a keychain profile",
            )?;
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
        eprintln!("  Fetching notarization log for {id}...");
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
