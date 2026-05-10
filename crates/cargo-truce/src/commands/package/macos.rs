//! macOS packaging pipeline: per-arch builds, lipo, stage, pkgbuild,
//! productbuild, optional notarization.

#![cfg(target_os = "macos")]

use super::PkgFormat;
use super::stage::{
    generate_distribution_xml, stage_aax, stage_au2, stage_au3, stage_clap, stage_standalone,
    stage_vst2, stage_vst3, write_postinstall_script,
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

pub(crate) fn cmd_package_macos(args: &[String], selection: &super::SuiteSelection) -> Res {
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
    // Per-plugin installers always run pkgbuild to produce the
    // component packages; whether we *also* run productbuild +
    // notarize for the per-plugin .pkg is the --no-per-plugin gate.
    // The component .pkgs are needed by the suite wrapper below.
    //
    // `-p <crate>` narrows to a single plugin, which can't satisfy a
    // multi-member suite. Skip suite installers in that mode so the
    // single-plugin run doesn't fail at the suite step looking for
    // unstaged siblings.
    let suites: Vec<crate::config::ResolvedSuite<'_>> = if parsed.plugin_filter.is_some() {
        if !config.suites.is_empty() {
            eprintln!("(-p set; skipping suite installers — they need every member plugin staged)");
        }
        Vec::new()
    } else {
        config
            .suites
            .iter()
            .filter(|s| selection.want_suite(&s.name))
            .map(|s| s.resolve(&config.plugin))
            .collect::<Result<_, _>>()?
    };
    let need_components_only = !selection.want_per_plugin() && !suites.is_empty();

    for p in &plugins {
        if selection.want_per_plugin() {
            package_one_plugin(&root, p, &dist_dir, &opts)?;
        } else if need_components_only {
            // Suite wrapping needs per-plugin components on disk.
            // Build them without running productbuild + notarize for
            // the per-plugin output.
            stage_components_only(&root, p, &opts)?;
        }
    }
    if !selection.want_per_plugin() {
        eprintln!("Skipping per-plugin .pkg installers (--no-per-plugin).");
    }

    if !suites.is_empty() {
        eprintln!("\nSuite installers");
        for suite in &suites {
            package_one_suite(&root, suite, &dist_dir, &opts)?;
        }
    }

    eprintln!("\nDone. Installers in {}", dist_dir.display());
    Ok(())
}

/// Run only the staging + per-format pkgbuild steps of the
/// per-plugin pipeline. Used by suite-wrapping when the user has
/// `--no-per-plugin` set: we still need the component .pkgs as
/// productbuild input, but skip the final per-plugin productbuild
/// + notarization round-trip.
///
/// The component .pkgs land at
/// `<staging>/components/<Plugin>-<format>.pkg`, which is the same
/// path the full per-plugin pipeline writes them to.
fn stage_components_only(root: &Path, p: &PluginDef, o: &PackageOpts) -> Res {
    eprintln!("\nComponents for: {}", p.name);

    let staging = truce_build::target_dir(root)
        .join("package/macos/plugin")
        .join(&p.bundle_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    for fmt in o.formats {
        eprint!("  Staging {}... ", fmt.label());
        // macOS package staging reads from `target/release/` after lipo
        // has produced a universal Mach-O at the canonical path; pass
        // None so `release_lib_for_target` resolves there.
        let result = match fmt {
            PkgFormat::Clap => stage_clap(
                root,
                p,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Vst3 => stage_vst3(root, p, o.config, &staging, None),
            PkgFormat::Vst2 => stage_vst2(root, p, o.config, &staging, None).map(|_| ()),
            PkgFormat::Au2 => stage_au2(root, p, o.config, &staging),
            PkgFormat::Au3 => stage_au3(root, p, o.config, &staging),
            PkgFormat::Aax => stage_aax(root, p, o.config, &staging, o.universal, o.no_pace_sign),
            PkgFormat::Standalone => stage_standalone(root, p, o.config, &staging),
        };
        match result {
            Ok(()) => eprintln!("ok"),
            Err(e) => {
                eprintln!("FAILED: {e}");
                return Err(e);
            }
        }
    }

    let components_dir = staging.join("components");
    fs::create_dir_all(&components_dir)?;
    let scripts_dir = staging.join("au_scripts");
    if o.has_au2 {
        write_postinstall_script(&scripts_dir)?;
    }
    for fmt in o.formats {
        run_pkgbuild_for_format(p, fmt, &staging, &components_dir, &scripts_dir, o)?;
    }
    Ok(())
}

/// Build one suite installer: wrap each member plugin's component
/// .pkgs in a single productbuild Distribution.xml that exposes
/// each plugin as a top-level choice.
fn package_one_suite(
    root: &Path,
    suite: &crate::config::ResolvedSuite<'_>,
    dist_dir: &Path,
    o: &PackageOpts,
) -> Res {
    let suite_name = &suite.def.name;
    eprintln!("\n  → {} ({} plugins)", suite_name, suite.plugins.len());

    // Collect every member plugin's component .pkgs into a single
    // directory that productbuild reads with `--package-path`.
    let suite_staging = truce_build::target_dir(root)
        .join("package/macos/suite")
        .join(&suite.def.bundle_id);
    let _ = fs::remove_dir_all(&suite_staging);
    fs::create_dir_all(&suite_staging)?;
    let components_dir = suite_staging.join("components");
    fs::create_dir_all(&components_dir)?;

    for plugin in &suite.plugins {
        let plugin_components = truce_build::target_dir(root)
            .join("package/macos/plugin")
            .join(&plugin.bundle_id)
            .join("components");
        if !plugin_components.exists() {
            return Err(format!(
                "suite '{}': missing component .pkgs for {} at {}. \
                 Run `cargo truce package` without --no-per-plugin first, \
                 or omit --no-per-plugin to let the suite flow build them.",
                suite_name,
                plugin.name,
                plugin_components.display(),
            )
            .into());
        }
        for entry in fs::read_dir(&plugin_components)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "pkg") {
                fs::copy(entry.path(), components_dir.join(entry.file_name()))?;
            }
        }
    }

    // Suite-level Distribution.xml: one outer choice per plugin,
    // one inner per-format choice underneath. Reuses the
    // per-format pkg-ref names (`{plugin_name}-{format}.pkg`) the
    // existing per-plugin pipeline emits.
    let suite_version = suite.def.version.as_deref().unwrap_or(o.version);
    let dist_xml = generate_suite_distribution_xml(
        suite,
        &o.config.vendor.id,
        o.formats,
        suite_version,
        Some(&o.config.packaging),
        o.effective_scope,
    );
    let dist_xml_path = suite_staging.join("distribution.xml");
    fs::write(&dist_xml_path, &dist_xml)?;

    let resources_dir = suite_staging.join("resources");
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

    // productbuild → final suite .pkg.
    let pkg_name = format!(
        "{}-{}-macos{}.pkg",
        suite.def.bundle_id,
        suite_version,
        o.scope.dist_suffix(),
    );
    let pkg_path = dist_dir.join(&pkg_name);
    let mut pb_args = vec![
        "--distribution".to_string(),
        dist_xml_path.to_string_lossy().into_owned(),
        "--package-path".to_string(),
        components_dir.to_string_lossy().into_owned(),
        "--resources".to_string(),
        resources_dir.to_string_lossy().into_owned(),
    ];
    if let Some(id) = crate::installer_identity() {
        pb_args.push("--sign".to_string());
        pb_args.push(id);
    }
    pb_args.push(pkg_path.to_string_lossy().into_owned());

    eprintln!("    productbuild...");
    let status = Command::new("productbuild").args(&pb_args).status()?;
    if !status.success() {
        return Err(format!("productbuild failed for suite '{suite_name}'").into());
    }

    // Verify the suite .pkg actually embeds every member plugin's
    // component .pkg. The exact bug this guards against shipped once
    // already: a malformed Distribution.xml (nested <choice> elements)
    // dropped every <pkg-ref> and productbuild emitted a 2 KB metadata-
    // only .pkg that opens in Installer.app and reports "can't find the
    // data needed for installation".
    let expected: Vec<String> = suite
        .plugins
        .iter()
        .flat_map(|plugin| {
            o.formats
                .iter()
                .map(move |fmt| format!("{}-{}.pkg", plugin.name, fmt.label()))
        })
        .collect();
    super::verify::assert_pkg_contains_components(&pkg_path, &expected)?;

    if o.config.macos.packaging.notarize && !o.no_notarize {
        notarize_and_staple(&pkg_path, o.config)?;
    }

    eprintln!("    Suite ready: {}", pkg_path.display());
    Ok(())
}

/// Distribution.xml for a suite installer: one outer `<choice>` per
/// plugin (a parent), with inner per-format `<choice>`s as children.
/// End-user UX in Apple Installer.app: the "Customize" button opens
/// a tree where each plugin is collapsible and lists its formats.
fn generate_suite_distribution_xml(
    suite: &crate::config::ResolvedSuite<'_>,
    vendor_id: &str,
    formats: &[PkgFormat],
    version: &str,
    resources: Option<&crate::config::PackagingConfig>,
    scope: crate::install_scope::PkgScope,
) -> String {
    use std::fmt::Write as _;

    let mut outline = String::new();
    let mut choices = String::new();
    let mut pkg_refs = String::new();

    // Apple's distribution.xml schema does NOT allow <choice> nesting.
    // The visual tree comes purely from <choices-outline>'s <line>
    // hierarchy; <choice> elements themselves must be flat siblings at
    // the top level. Nesting them silently drops the inner pkg-refs and
    // productbuild produces an empty (~2 KB) installer with no payload.
    for plugin in &suite.plugins {
        let outer_id = sanitize_id(&plugin.bundle_id);
        let _ = writeln!(outline, "        <line choice=\"{outer_id}\">");
        for fmt in formats {
            let inner_id = format!("{outer_id}-{}", fmt.pkg_id_suffix());
            let _ = writeln!(outline, "            <line choice=\"{inner_id}\"/>");
        }
        let _ = writeln!(outline, "        </line>");

        // Per-plugin parent: empty choice that only exists so the
        // outline can reference it as the visual group header. No
        // pkg-ref of its own.
        let _ = writeln!(
            choices,
            "    <choice id=\"{outer_id}\" title=\"{plugin_name}\" description=\"All formats for {plugin_name}.\"/>",
            plugin_name = plugin.name,
        );

        for fmt in formats {
            let inner_id = format!("{outer_id}-{}", fmt.pkg_id_suffix());
            let pkg_id = format!("{vendor_id}.{}.{}", plugin.bundle_id, fmt.pkg_id_suffix());
            let component_file = format!("{}-{}.pkg", plugin.name, fmt.label());
            let label = fmt.label();
            let desc = fmt.choice_description();
            let enabled_attr = if *fmt == PkgFormat::Aax {
                "\n            selected=\"false\""
            } else {
                ""
            };
            let _ = write!(
                choices,
                r#"    <choice id="{inner_id}" title="{label}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"/>
    </choice>
"#
            );
            let _ = writeln!(
                pkg_refs,
                "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\">{component_file}</pkg-ref>"
            );
        }
    }

    let welcome = resources
        .and_then(|r| r.welcome_html.as_deref())
        .map_or("", |_| "    <welcome file=\"welcome.html\"/>\n");
    let license = resources
        .and_then(|r| r.license_html.as_deref())
        .map_or("", |_| "    <license file=\"license.html\"/>\n");

    let domains = match scope {
        crate::install_scope::PkgScope::User => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" enable_localSystem=\"false\"/>\n"
        }
        crate::install_scope::PkgScope::System => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"false\" enable_localSystem=\"true\"/>\n"
        }
        crate::install_scope::PkgScope::Ask => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" enable_localSystem=\"true\"/>\n"
        }
    };

    let title = &suite.def.name;
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{title}</title>
{welcome}{license}{domains}    <options customize="always" require-scripts="false"/>

    <choices-outline>
{outline}    </choices-outline>
{choices}
{pkg_refs}</installer-gui-script>
"#,
    )
}

/// XML-attribute-safe identifier from a `bundle_id` that may contain
/// dots / dashes / etc. Distribution.xml choice ids must be
/// non-empty, ASCII, and unique across the file; collapsing
/// non-alphanumerics to `_` covers every realistic `bundle_id` we
/// produce.
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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
        if available.contains("standalone") {
            fmts.push(PkgFormat::Standalone);
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
        build_and_lipo_format(root, plugins, archs, dt, "au", "AU v2")?;
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
    if formats.contains(&PkgFormat::Standalone) {
        // Standalone is a `[[bin]]`, not a cdylib — the per-arch
        // outputs land at `target/<triple>/release/<bin>` rather than
        // `lib<stem>_<feature>.dylib`, so it doesn't fit the shared
        // `build_and_lipo_format` shape.
        build_and_lipo_standalone(root, plugins, archs, dt)?;
    }
    Ok(())
}

/// Build the standalone host binaries (`--features standalone`) for
/// every plugin in `plugins` and lipo each plugin's per-arch outputs
/// into the canonical `target/release/<bin>` path that
/// `stage_standalone` reads.
///
/// Cargo invocations are batched across plugins: one
/// `cargo build -p a -p b -p c … --features standalone` per arch
/// instead of one per (plugin, arch). The `[[bin]] required-features
/// = ["standalone"]` gate on each plugin's `Cargo.toml` keeps cargo
/// from producing bin output for plugins that don't enable the
/// feature, so a plugin without a standalone bin in the batch is
/// silently skipped at the cargo layer.
///
/// `bin_stem` resolves through `read_standalone_bin_name`, which
/// inspects each plugin's `Cargo.toml` so hand-edited `[[bin]] name`
/// values still work — falls back to the scaffold convention
/// (`{crate_name}-standalone`) when the manifest can't be parsed.
fn build_and_lipo_standalone(
    root: &Path,
    plugins: &[&PluginDef],
    archs: &[MacArch],
    dt: &str,
) -> Res {
    if plugins.is_empty() {
        return Ok(());
    }

    // Resolve every plugin's bin stem upfront so the per-arch staging
    // loop has them in hand.
    let bin_stems: Vec<String> = plugins
        .iter()
        .map(|p| {
            crate::read_standalone_bin_name(&p.crate_name)
                .unwrap_or_else(|| format!("{}-standalone", p.crate_name))
        })
        .collect();

    // One cargo build per arch covering every plugin in the batch.
    // Cargo pays the dep-graph-resolve / process-startup cost once per
    // arch and codegens the leaf example crates' bin targets in
    // parallel.
    for &arch in archs {
        eprintln!(
            "Building Standalone for {} ({} plugin{})...",
            arch.triple(),
            plugins.len(),
            if plugins.len() == 1 { "" } else { "s" },
        );
        let mut args: Vec<&str> = Vec::with_capacity(plugins.len() * 2 + 4);
        for p in plugins {
            args.push("-p");
            args.push(&p.crate_name);
        }
        args.push("--no-default-features");
        args.push("--features");
        args.push("standalone");
        cargo_build_for_arch(&[], &args, arch, dt)?;
    }

    // Per-plugin lipo: each plugin's per-arch bins land at
    // `target/<triple>/release/<bin_stem>` and need to be combined
    // into the universal `target/release/<bin_stem>` that
    // `stage_standalone` reads.
    for (p, bin_stem) in plugins.iter().zip(bin_stems.iter()) {
        let inputs: Vec<PathBuf> = archs
            .iter()
            .map(|a| {
                truce_build::target_dir(root)
                    .join(a.triple())
                    .join("release")
                    .join(bin_stem)
            })
            .collect();
        for src in &inputs {
            if !src.exists() {
                return Err(format!(
                    "Standalone build produced no binary for `{}` at {}. \
                     Make sure the plugin's Cargo.toml declares a [[bin]] target named '{bin_stem}'.",
                    p.name,
                    src.display()
                )
                .into());
            }
        }
        let output = truce_build::target_dir(root).join("release").join(bin_stem);
        if inputs.len() == 1 {
            // Single-arch (`--host-only`): nothing to lipo, just copy
            // into the canonical universal output path so
            // `stage_standalone` reads from one place regardless of
            // arch count.
            fs::copy(&inputs[0], &output)?;
        } else {
            lipo_into(&inputs, &output)?;
        }
    }
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
    eprintln!("\nPackaging: {}", p.name);

    let staging = truce_build::target_dir(root)
        .join("package/macos/plugin")
        .join(&p.bundle_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // Step 2: Stage signed bundles
    for fmt in o.formats {
        eprint!("  Staging {}... ", fmt.label());
        // macOS package staging reads from `target/release/` after lipo
        // has produced a universal Mach-O at the canonical path; pass
        // None so `release_lib_for_target` resolves there.
        let result = match fmt {
            PkgFormat::Clap => stage_clap(
                root,
                p,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Vst3 => stage_vst3(root, p, o.config, &staging, None),
            PkgFormat::Vst2 => stage_vst2(root, p, o.config, &staging, None).map(|_| ()),
            PkgFormat::Au2 => stage_au2(root, p, o.config, &staging),
            PkgFormat::Au3 => stage_au3(root, p, o.config, &staging),
            PkgFormat::Aax => stage_aax(root, p, o.config, &staging, o.universal, o.no_pace_sign),
            PkgFormat::Standalone => stage_standalone(root, p, o.config, &staging),
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
        &crate::application_identity(),
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

    // Step 6.5: Verify the produced .pkg actually embeds every
    // component we just fed productbuild. Catches the malformed-
    // distribution.xml class of bug where productbuild reports
    // success but drops the payload (e.g. nested <choice> elements
    // ignore their pkg-refs and produce a 2 KB metadata-only .pkg).
    let expected: Vec<String> = o
        .formats
        .iter()
        .map(|fmt| format!("{}-{}.pkg", p.name, fmt.label()))
        .collect();
    super::verify::assert_pkg_contains_components(&pkg_path, &expected)?;

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

    let installer_id = crate::installer_identity();
    let mut pb_args = vec![
        "--distribution",
        dist_xml_path.to_str().unwrap(),
        "--package-path",
        components_dir.to_str().unwrap(),
        "--resources",
        resources_dir.to_str().unwrap(),
    ];

    if let Some(id) = &installer_id {
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
/// Used for CLAP / VST3 / VST2 / AU2 / AAX, which all share the same
/// "build per arch, then lipo" shape. AU3 has its own framework
/// pipeline so it doesn't route through here.
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
fn notarize_and_staple(pkg_path: &Path, _config: &Config) -> Res {
    let pkg = pkg_path.to_str().unwrap();

    // Notarization credentials are per-developer — read from the
    // build env (`.cargo/config.toml [env]` or shell). The keychain
    // profile is preferred; explicit Apple ID + team ID are the
    // fallback path when no keychain profile is set up.
    let apple_id = crate::read_build_env("APPLE_ID").unwrap_or_default();
    let team_id = crate::read_build_env("TEAM_ID").unwrap_or_default();

    let keychain_profile =
        crate::read_build_env("TRUCE_NOTARY_PROFILE").unwrap_or_else(|| "TRUCE_NOTARY".to_string());

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
                    &apple_id,
                    "--team-id",
                    &team_id,
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
                 or set APPLE_ID + TEAM_ID + APP_SPECIFIC_PASSWORD in \
                 .cargo/config.toml [env]"
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
                eprintln!("\nNotarization log:");
                eprintln!("{log}");
            }
        }
    }
}
