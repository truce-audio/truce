//! macOS packaging pipeline: per-arch builds, lipo, stage, pkgbuild,
//! productbuild, optional notarization.

#![cfg(target_os = "macos")]

use super::PkgFormat;
use super::stage::{
    ExtraComponent, build_preset_component, generate_distribution_xml, stage_aax, stage_au2,
    stage_au3, stage_clap, stage_lv2_packaged, stage_standalone, stage_vst2, stage_vst3,
    write_format_scripts,
};
use crate::commands::build_dylibs::BuildFormat;
use crate::commands::install::presets;
use crate::install_scope::PkgScope;
use crate::preset_codec::xml_escape;
use crate::{
    CLAP_EXPORTS, Config, MacArch, PluginDef, Res, VST2_EXPORTS, VST3_EXPORTS,
    cargo_build_multi_arch, cargo_build_multi_arch_with_profile, copy_dir_recursive,
    deployment_target, detect_default_features, link_macos_bundle, lipo_into, load_config,
    project_root, read_workspace_version, release_bundle_bin, release_lib_for_target,
    release_static_for_target,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Run `task` over `items` on a bounded pool of OS threads, collecting
/// failures as `<label>: <error>` strings. Errors do NOT short-circuit -
/// every item runs so one failure can't strand the rest half-built. The
/// per-plugin and per-suite pipelines are independent (each owns its
/// staging dir and dist filename) and dominated by notarization's network
/// wait, so this turns `sum(waits)` into roughly `max(wait)`.
///
/// `task`'s `Res` is consumed inside the worker (formatted to a `String`),
/// so the error type never has to cross the thread boundary.
fn parallel_for<T, L, F>(items: &[T], workers: usize, label: L, task: F) -> Res
where
    T: Sync,
    L: Fn(&T) -> String + Sync,
    F: Fn(&T) -> Res + Sync,
{
    if items.is_empty() {
        return Ok(());
    }
    let workers = workers.clamp(1, items.len());
    let next = AtomicUsize::new(0);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    if let Err(e) = task(item) {
                        errors.lock().unwrap().push(format!("{}: {e}", label(item)));
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} packaging task(s) failed:\n  {}",
            errors.len(),
            errors.join("\n  ")
        )
        .into())
    }
}

/// Bounded worker count for the packaging pools: enough to overlap the
/// notarization waits without hammering Apple's notary / timestamp
/// services or contending on the login keychain during concurrent
/// `codesign`.
fn packaging_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(8)
}

pub(crate) fn cmd_package_macos(args: &[String], selection: &super::SuiteSelection) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let parsed = parse_package_args(args)?;

    let target_cpu = parsed
        .target_cpu_arg
        .as_deref()
        .map(crate::util::parse_target_cpu_arg)
        .unwrap_or_default();
    crate::set_target_cpu(target_cpu);

    // Scope resolution: CLI > truce.toml [packaging] preferred_scope >
    // OS default (`--ask`). `cargo truce install` has no toml
    // override - the install scope is a per-invocation developer
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
    // AAX / AU v3 / standalone are system-only on macOS; a `--user`
    // package that includes one still needs the localSystem domain to
    // install it, so widen the installer scope to System. The `.pkg`
    // filename keeps the requested scope's suffix (see `run_productbuild`)
    // so CI scripts still find it.
    let effective_scope =
        if scope == PkgScope::User && formats.iter().any(PkgFormat::is_system_only_on_macos) {
            PkgScope::System
        } else {
            scope
        };

    let all_plugins: Vec<&PluginDef> =
        crate::commands::pick_plugins(&config, parsed.plugin_filter.as_deref())?;

    eprintln!(
        "Packaging archs: {}",
        archs
            .iter()
            .map(|a| a.triple())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Resolve the suites to build up front so a `--suite` selection can
    // narrow the *build* to just those suites' members, not every plugin.
    //
    // Per-plugin installers always run pkgbuild to produce the component
    // packages; whether we *also* run productbuild + notarize for the
    // per-plugin .pkg is the --no-per-plugin gate. The component .pkgs are
    // needed by the suite wrapper below.
    //
    // `-p <crate>` narrows to a single plugin, which can't satisfy a
    // multi-member suite. Skip suite installers in that mode so the
    // single-plugin run doesn't fail at the suite step looking for
    // unstaged siblings.
    let suites: Vec<crate::config::ResolvedSuite<'_>> = if parsed.plugin_filter.is_some() {
        if !config.suites.is_empty() {
            eprintln!("(-p set; skipping suite installers - they need every member plugin staged)");
        }
        Vec::new()
    } else {
        config
            .suites
            .iter()
            .filter(|s| selection.want_suite(&s.name, &s.bundle_id))
            .map(|s| s.resolve(&config.plugin))
            .collect::<Result<_, _>>()?
    };

    // With `--suite <name>`, build only that suite's members (and their
    // per-plugin installers) instead of every plugin; without it, build
    // everything.
    let plugins: Vec<&PluginDef> = if selection.only_suites.is_empty() {
        all_plugins
    } else {
        let mut seen = std::collections::HashSet::new();
        let narrowed: Vec<&PluginDef> = suites
            .iter()
            .flat_map(|s| s.plugins.iter().copied())
            .filter(|p| seen.insert(p.bundle_id.clone()))
            .collect();
        eprintln!(
            "Limiting to {} plugin{} for the selected suite(s).",
            narrowed.len(),
            if narrowed.len() == 1 { "" } else { "s" }
        );
        narrowed
    };

    build_all_formats(&root, &config, &plugins, &archs, dt, &formats, universal)?;

    let dist_dir = truce_build::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    let version = read_workspace_version(&root).unwrap_or_else(|e| {
        eprintln!("WARNING: {e}; defaulting package version to 0.0.0");
        "0.0.0".to_string()
    });

    let opts = PackageOpts {
        config: &config,
        formats: &formats,
        scope,
        effective_scope,
        version: &version,
        no_notarize: parsed.no_notarize,
        no_pace_sign: parsed.no_pace_sign,
        universal,
    };
    let need_components_only = !selection.want_per_plugin() && !suites.is_empty();
    let workers = packaging_workers();

    // Per-plugin pipelines run concurrently; output from different plugins
    // interleaves. The pool joins before suite wrapping, which needs every
    // member's component .pkgs staged first.
    if selection.want_per_plugin() {
        parallel_for(
            &plugins,
            workers,
            |p| p.name.clone(),
            |p| package_one_plugin(&root, p, &dist_dir, &opts),
        )?;
    } else {
        if need_components_only {
            // Suite wrapping needs per-plugin components on disk. Build them
            // without running productbuild + notarize for the per-plugin
            // output.
            parallel_for(
                &plugins,
                workers,
                |p| p.name.clone(),
                |p| stage_components_only(&root, p, &opts),
            )?;
        }
        eprintln!("Skipping per-plugin .pkg installers (--no-per-plugin).");
    }

    if !suites.is_empty() {
        eprintln!("\nSuite installers");
        parallel_for(
            &suites,
            workers,
            |s| s.def.name.clone(),
            |s| package_one_suite(&root, s, &dist_dir, &opts),
        )?;
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

    let plugin_formats = formats_for_plugin(o.formats, p);

    for fmt in &plugin_formats {
        eprintln!("  Staging {}...", fmt.label());
        // macOS package staging reads from `target/release/` after lipo
        // has produced a universal Mach-O at the canonical path; pass
        // None so `release_lib_for_target` resolves there.
        let result = match fmt {
            PkgFormat::Clap => stage_clap(
                root,
                p,
                o.config,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Vst3 => stage_vst3(root, p, o.config, &staging, None),
            PkgFormat::Vst2 => stage_vst2(root, p, o.config, &staging, None).map(|_| ()),
            PkgFormat::Lv2 => stage_lv2_packaged(
                root,
                p,
                o.config,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Au2 => stage_au2(root, p, o.config, &staging),
            PkgFormat::Au3 => stage_au3(root, p, o.config, &staging),
            PkgFormat::Aax => stage_aax(root, p, o.config, &staging, o.universal, o.no_pace_sign),
            PkgFormat::Standalone => stage_standalone(root, p, o.config, &staging),
        };
        match result {
            Ok(()) => eprintln!("    ok"),
            Err(e) => {
                eprintln!("    FAILED: {e}");
                return Err(e);
            }
        }
    }

    let components_dir = staging.join("components");
    // Wipe the components dir before rebuilding so a previous run's
    // `--formats clap,vst3,au2,...` output doesn't leak into the
    // current `--formats clap,lv2` productbuild - productbuild reads
    // every `.pkg` it finds via `--package-path`, and a stale entry
    // typically fails to install (signature mismatch, entitlements
    // bound to an older identity, etc.).
    let _ = fs::remove_dir_all(&components_dir);
    fs::create_dir_all(&components_dir)?;
    for fmt in &plugin_formats {
        let appex_id = (*fmt == PkgFormat::Au3).then(|| au3_appex_id(o.config, p));
        let scripts_dir =
            write_format_scripts(&staging, fmt, &fmt.bundle_name(p), appex_id.as_deref())?;
        run_pkgbuild_for_format(p, fmt, &staging, &components_dir, &scripts_dir, o)?;
    }
    // VST3 presets ride as their own component; the suite distribution
    // picks it up via `vst3_preset_descriptor`.
    build_vst3_preset_component(root, p, o, &staging, &components_dir)?;
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
    // Per-member VST3 preset descriptors, aligned with `suite.plugins`.
    // The component .pkgs were built by `stage_components_only`.
    let extras_by_plugin: Vec<Vec<ExtraComponent>> = suite
        .plugins
        .iter()
        .map(|p| vst3_preset_descriptor(root, p, o).map(|d| d.into_iter().collect()))
        .collect::<Result<_, _>>()?;
    let dist_xml = generate_suite_distribution_xml(
        suite,
        &o.config.vendor.id,
        o.formats,
        &extras_by_plugin,
        suite_version,
        Some(&o.config.macos.packaging),
        o.effective_scope,
    );
    let dist_xml_path = suite_staging.join("distribution.xml");
    fs::write(&dist_xml_path, &dist_xml)?;

    let resources_dir = suite_staging.join("resources");
    fs::create_dir_all(&resources_dir)?;
    for (key, dst_name) in [
        (
            o.config.macos.packaging.welcome_html.as_deref(),
            "welcome.html",
        ),
        (
            o.config.macos.packaging.license_html.as_deref(),
            "license.html",
        ),
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
    let mut expected: Vec<String> = suite
        .plugins
        .iter()
        .flat_map(|plugin| {
            formats_for_plugin(o.formats, plugin)
                .into_iter()
                .map(move |fmt| format!("{}-{}.pkg", plugin.file_stem(), fmt.label()))
        })
        .collect();
    for (plugin, extras) in suite.plugins.iter().zip(&extras_by_plugin) {
        expected.extend(
            extras
                .iter()
                .map(|ec| format!("{}-{}.pkg", plugin.file_stem(), ec.label)),
        );
    }
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
    extras_by_plugin: &[Vec<ExtraComponent>],
    version: &str,
    resources: Option<&crate::config::MacosPackagingConfig>,
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
    for (plugin, extras) in suite.plugins.iter().zip(extras_by_plugin) {
        // A binless plugin has no Standalone component; keep it out of
        // this member's outline + pkg-refs so productbuild doesn't
        // reference a `.pkg` that was never built.
        let member_formats = formats_for_plugin(formats, plugin);
        // AU v3's app is the standalone host when this member ships a
        // standalone bin (the Standalone format is collapsed into it).
        let au3_is_standalone_host = crate::read_standalone_bin_name(&plugin.crate_name).is_some();
        let outer_id = sanitize_id(&plugin.bundle_id);
        let _ = writeln!(outline, "        <line choice=\"{outer_id}\">");
        for fmt in &member_formats {
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
            plugin_name = xml_escape(&plugin.name),
        );

        for fmt in &member_formats {
            let inner_id = format!("{outer_id}-{}", fmt.pkg_id_suffix());
            let pkg_id = format!("{vendor_id}.{}.{}", plugin.bundle_id, fmt.pkg_id_suffix());
            let component_file = format!("{}-{}.pkg", plugin.file_stem(), fmt.label());
            let (label, desc): (&str, &str) = if *fmt == PkgFormat::Au3 && au3_is_standalone_host {
                ("AU3 + Standalone", "Audio Unit v3 (appex) + standalone app")
            } else {
                (fmt.label(), fmt.choice_description())
            };
            // All formats checked by default - see matching note in
            // the per-plugin `generate_distribution_xml`.
            let enabled_attr = "";
            // Per-choice auth override - same scheme as the per-plugin
            // installer (see `generate_distribution_xml` in stage.rs).
            let pkg_ref_auth = match (scope, fmt.is_system_only_on_macos()) {
                (
                    crate::install_scope::PkgScope::User | crate::install_scope::PkgScope::Ask,
                    true,
                ) => " auth=\"Root\"",
                (crate::install_scope::PkgScope::User, false) => " auth=\"None\"",
                (
                    crate::install_scope::PkgScope::Ask | crate::install_scope::PkgScope::System,
                    _,
                ) => "",
            };
            // Out-of-bundle components (VST3 presets) ride inside the
            // matching format's choice, not their own - install with
            // VST3, same auth, separate pkg only because the install
            // location differs.
            let mut extra_refs = String::new();
            for ec in extras.iter().filter(|e| e.parent == *fmt) {
                let ex_id = format!("{vendor_id}.{}.{}", plugin.bundle_id, ec.suffix);
                let _ = writeln!(
                    extra_refs,
                    "        <pkg-ref id=\"{ex_id}\"{pkg_ref_auth}/>"
                );
            }
            let _ = write!(
                choices,
                r#"    <choice id="{inner_id}" title="{label}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"{pkg_ref_auth}/>
{extra_refs}    </choice>
"#
            );
            let _ = writeln!(
                pkg_refs,
                "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\">{component_file}</pkg-ref>"
            );
            for ec in extras.iter().filter(|e| e.parent == *fmt) {
                let ex_id = format!("{vendor_id}.{}.{}", plugin.bundle_id, ec.suffix);
                let ex_file = format!("{}-{}.pkg", plugin.file_stem(), ec.label);
                let _ = writeln!(
                    pkg_refs,
                    "    <pkg-ref id=\"{ex_id}\" version=\"{version}\">{ex_file}</pkg-ref>"
                );
            }
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

    let title = xml_escape(&suite.def.name);
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
    target_cpu_arg: Option<String>,
}

fn parse_package_args(args: &[String]) -> Result<PackageArgs, crate::CargoTruceError> {
    let mut plugin_filter: Option<String> = None;
    let mut format_str: Option<String> = None;
    let mut no_notarize = false;
    let mut host_only = false;
    let mut no_pace_sign = false;
    let mut cli_scope: Option<PkgScope> = None;
    let mut target_cpu_arg: Option<String> = None;

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
            "--target-cpu" => {
                target_cpu_arg =
                    Some(crate::util::arg_value(args, &mut i, "--target-cpu")?.to_string());
            }
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
        target_cpu_arg,
    })
}

/// Resolve the format list from CLI > toml > feature-detection.
fn resolve_formats(
    format_str: Option<&str>,
    config: &Config,
) -> Result<Vec<PkgFormat>, crate::CargoTruceError> {
    let fmts = if let Some(s) = format_str {
        PkgFormat::parse_list(s)?
    } else if !config.packaging.formats.is_empty() {
        PkgFormat::parse_list(&config.packaging.formats.join(","))?
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
        if available.contains("lv2") {
            fmts.push(PkgFormat::Lv2);
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
        fmts
    };

    Ok(drop_standalone_if_au3(fmts))
}

/// AU v3's bundle *is* the standalone `{name}.app` - the host with the
/// appex embedded, installed at the same path. When both are requested,
/// drop the separate Standalone format: a lean standalone app at that
/// path (no appex) clobbers the AU v3 one, and once `pkd` has scanned the
/// appex-less app first it won't register the appex the AU v3 payload
/// later adds. The AU v3 app is itself launchable as a standalone.
fn drop_standalone_if_au3(mut fmts: Vec<PkgFormat>) -> Vec<PkgFormat> {
    if fmts.contains(&PkgFormat::Au3) {
        fmts.retain(|f| *f != PkgFormat::Standalone);
    }
    fmts
}

/// The package formats that actually apply to `p`. Standalone is dropped
/// for plugins that declare no standalone `[[bin]]` (MIDI / utility
/// examples): they can't run as a desktop app, so they get no Standalone
/// component or installer choice. AU v3 still ships for them - as a stub
/// app - so it is never filtered here.
fn formats_for_plugin(formats: &[PkgFormat], p: &PluginDef) -> Vec<PkgFormat> {
    let has_standalone = crate::read_standalone_bin_name(&p.crate_name).is_some();
    formats
        .iter()
        .filter(|f| **f != PkgFormat::Standalone || has_standalone)
        .cloned()
        .collect()
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
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Clap)?;
    }
    if formats.contains(&PkgFormat::Vst3) {
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Vst3)?;
    }
    if formats.contains(&PkgFormat::Vst2) {
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Vst2)?;
    }
    if formats.contains(&PkgFormat::Lv2) {
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Lv2)?;
    }
    if formats.contains(&PkgFormat::Au2) {
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Au2)?;
    }
    if formats.contains(&PkgFormat::Aax) {
        build_and_lipo_format(root, plugins, archs, dt, BuildFormat::Aax)?;
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
        //
        // AU2 and AU3 share `--features au` byte-for-byte. When AU2 was
        // built first in this same run, the universal `lib<stem>_au.dylib`
        // is already on disk - let AU3 reuse it instead of re-running
        // cargo + lipo for an identical artifact.
        let reuse_au_artifacts = formats.contains(&PkgFormat::Au2);
        crate::commands::install::au_v3::emit_au_v3_bundle(
            root,
            config,
            plugins,
            archs,
            reuse_au_artifacts,
        )?;
    }
    if formats.contains(&PkgFormat::Standalone) {
        // Standalone is a `[[bin]]`, not a cdylib - the per-arch
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
/// values still work - falls back to the scaffold convention
/// (`{crate_name}-standalone`) when the manifest can't be parsed.
pub(crate) fn build_and_lipo_standalone(
    root: &Path,
    plugins: &[&PluginDef],
    archs: &[MacArch],
    dt: &str,
) -> Res {
    // Only plugins that declare a standalone `[[bin]]` produce a binary.
    // MIDI / utility examples enable the `standalone` feature (so the dep
    // is in their graph) but ship no bin, so building one for them yields
    // nothing - skip them here, as `formats_for_plugin` does in staging /
    // distribution.
    let plugins: Vec<&PluginDef> = plugins
        .iter()
        .copied()
        .filter(|p| crate::read_standalone_bin_name(&p.crate_name).is_some())
        .collect();
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
    // parallel. Per-arch invocations target distinct
    // `target/<triple>/release/` dirs and run concurrently when more
    // than one arch is requested.
    let mut args: Vec<&str> = Vec::with_capacity(plugins.len() * 2 + 3);
    for p in &plugins {
        args.push("-p");
        args.push(&p.crate_name);
    }
    args.push("--no-default-features");
    args.push("--features");
    args.push("standalone");

    if archs.len() == 1 {
        eprintln!(
            "Building Standalone for {} ({} plugin{})...",
            archs[0].triple(),
            plugins.len(),
            if plugins.len() == 1 { "" } else { "s" },
        );
    } else {
        eprintln!(
            "Building Standalone for {} archs ({} plugin{})...",
            archs.len(),
            plugins.len(),
            if plugins.len() == 1 { "" } else { "s" },
        );
    }
    // Always `release`, even under `install --shell` / `--debug`: the
    // standalone host is a distribution app, not the hot-reloaded plugin
    // binary, and the lipo paths below read from `release/`.
    cargo_build_multi_arch_with_profile(archs, &args, dt, "release")?;

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
// Sparse independent CLI flags - bitflags would just add ceremony.
#[allow(clippy::struct_excessive_bools)]
struct PackageOpts<'a> {
    config: &'a Config,
    formats: &'a [PkgFormat],
    /// The developer-requested scope. Drives the `.pkg` filename suffix
    /// so a build's output name stays stable regardless of widening.
    scope: PkgScope,
    /// The requested scope, widened to System when the package contains
    /// a system-only format (AAX / AU v3 / standalone) that a `--user`
    /// installer's domains couldn't reach. Drives the distribution.xml
    /// domains + per-component auth.
    effective_scope: PkgScope,
    version: &'a str,
    no_notarize: bool,
    no_pace_sign: bool,
    universal: bool,
}

/// Stage signed bundles, run pkgbuild per format, then productbuild
/// the distribution. The function follows the original numbered steps
/// (2 through 7) - splitting them into separate helpers would inflate
/// the boilerplate without surfacing any reuse, since `cmd_package_macos`
/// is the only caller.
/// AU v3 app-extension bundle id, matching what the build's pbxproj
/// stamps and `install_au_v3` registers - so the installer postinstall
/// registers the same identifier.
fn au3_appex_id(config: &Config, p: &PluginDef) -> String {
    // Vendor-rooted, matching the appex `PRODUCT_BUNDLE_IDENTIFIER` the
    // build's pbxproj stamps.
    format!("{}.{}.v3.ext", config.vendor.id, p.bundle_id)
}

/// A preset file tree as `(relative path -> bytes)`.
type PresetPayload = Vec<(PathBuf, Vec<u8>)>;

/// The VST3 preset payload for `p`, or `None` when VST3 isn't selected
/// or the plugin ships no presets.
fn vst3_preset_payload_for(
    root: &Path,
    p: &PluginDef,
    o: &PackageOpts,
) -> Result<Option<PresetPayload>, crate::CargoTruceError> {
    if !o.formats.contains(&PkgFormat::Vst3) {
        return Ok(None);
    }
    let Some(fp) = presets::load_factory_presets(root, p, o.config)? else {
        return Ok(None);
    };
    let payload = presets::vst3_preset_payload(&fp, p, o.config);
    Ok((!payload.is_empty()).then_some(payload))
}

/// The distribution descriptor for `p`'s VST3 preset component (the
/// pkg itself is built by [`build_vst3_preset_component`] /
/// `stage_components_only`). `None` when there's nothing to ship.
fn vst3_preset_descriptor(
    root: &Path,
    p: &PluginDef,
    o: &PackageOpts,
) -> Result<Option<ExtraComponent>, crate::CargoTruceError> {
    Ok(
        vst3_preset_payload_for(root, p, o)?.map(|_| ExtraComponent {
            suffix: "vst3presets".to_string(),
            label: "VST3-Presets".to_string(),
            parent: PkgFormat::Vst3,
        }),
    )
}

/// Build the VST3-presets pkg component into `components_dir` and
/// return its descriptor, or `None` when nothing to ship.
fn build_vst3_preset_component(
    root: &Path,
    p: &PluginDef,
    o: &PackageOpts,
    staging: &Path,
    components_dir: &Path,
) -> Result<Option<ExtraComponent>, crate::CargoTruceError> {
    let Some(payload) = vst3_preset_payload_for(root, p, o)? else {
        return Ok(None);
    };
    let Some(ec) = vst3_preset_descriptor(root, p, o)? else {
        return Ok(None);
    };
    let pkg_id = format!("{}.{}.{}", o.config.vendor.id, p.bundle_id, ec.suffix);
    build_preset_component(
        staging,
        components_dir,
        &p.file_stem(),
        &pkg_id,
        &ec.label,
        "/Library/Audio/Presets",
        o.version,
        &payload,
    )?;
    Ok(Some(ec))
}

fn package_one_plugin(root: &Path, p: &PluginDef, dist_dir: &Path, o: &PackageOpts) -> Res {
    eprintln!("\nPackaging: {}", p.name);

    let staging = truce_build::target_dir(root)
        .join("package/macos/plugin")
        .join(&p.bundle_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let plugin_formats = formats_for_plugin(o.formats, p);

    // Step 2: Stage signed bundles
    for fmt in &plugin_formats {
        eprintln!("  Staging {}...", fmt.label());
        // macOS package staging reads from `target/release/` after lipo
        // has produced a universal Mach-O at the canonical path; pass
        // None so `release_lib_for_target` resolves there.
        let result = match fmt {
            PkgFormat::Clap => stage_clap(
                root,
                p,
                o.config,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Vst3 => stage_vst3(root, p, o.config, &staging, None),
            PkgFormat::Vst2 => stage_vst2(root, p, o.config, &staging, None).map(|_| ()),
            PkgFormat::Lv2 => stage_lv2_packaged(
                root,
                p,
                o.config,
                &staging,
                &crate::application_identity(),
                None,
            ),
            PkgFormat::Au2 => stage_au2(root, p, o.config, &staging),
            PkgFormat::Au3 => stage_au3(root, p, o.config, &staging),
            PkgFormat::Aax => stage_aax(root, p, o.config, &staging, o.universal, o.no_pace_sign),
            PkgFormat::Standalone => stage_standalone(root, p, o.config, &staging),
        };
        match result {
            Ok(()) => eprintln!("    ok"),
            Err(e) => {
                eprintln!("    FAILED: {e}");
                return Err(e);
            }
        }
    }

    // Step 2.5: Notarization-readiness check.
    // Mirror Apple's notarization-server checks locally - every
    // Mach-O under the staged tree needs Developer ID +
    // timestamp + hardened runtime. Catches unsigned inner
    // Mach-Os (codesign --deep doesn't recurse into AAX
    // Resources/), missing --timestamp, missing --options
    // runtime, ad-hoc cert leakage. No-op when the signing
    // identity is ad-hoc.
    eprint!("  Verifying signing readiness... ");
    match crate::util::verify_signed_for_notarization(&staging, &crate::application_identity()) {
        Ok(()) => eprintln!("ok"),
        Err(e) => {
            eprintln!("FAILED");
            return Err(e);
        }
    }

    // Step 3: Build component .pkg per format
    let components_dir = staging.join("components");
    // Wipe stale components from prior runs before rebuilding - see
    // matching note in `stage_components_only` above.
    let _ = fs::remove_dir_all(&components_dir);
    fs::create_dir_all(&components_dir)?;

    for fmt in &plugin_formats {
        let appex_id = (*fmt == PkgFormat::Au3).then(|| au3_appex_id(o.config, p));
        let scripts_dir =
            write_format_scripts(&staging, fmt, &fmt.bundle_name(p), appex_id.as_deref())?;
        run_pkgbuild_for_format(p, fmt, &staging, &components_dir, &scripts_dir, o)?;
    }
    // Out-of-bundle VST3 presets ship as their own component.
    let extras: Vec<ExtraComponent> =
        build_vst3_preset_component(root, p, o, &staging, &components_dir)?
            .into_iter()
            .collect();

    // Step 4: Generate distribution.xml. Pass the sanitized
    // file stem (not `p.name`) so the `<pkg-ref>` URLs match the
    // pkgbuild outputs on disk - a display name like
    // `Truce Dry/Wet` would otherwise produce
    // `Truce Dry/Wet-LV2.pkg` here, while pkgbuild already wrote
    // `Truce Dry-Wet-LV2.pkg`. productbuild silently fails to
    // find any component and emits an empty ~15 KB installer.
    let dist_xml = generate_distribution_xml(
        &p.file_stem(),
        &o.config.vendor.id,
        &p.bundle_id,
        &plugin_formats,
        &extras,
        o.version,
        Some(&o.config.macos.packaging),
        o.effective_scope,
        crate::read_standalone_bin_name(&p.crate_name).is_some(),
    );
    let dist_xml_path = staging.join("distribution.xml");
    fs::write(&dist_xml_path, &dist_xml)?;

    // Step 5: Prepare resources (optional welcome/license html)
    let resources_dir = staging.join("resources");
    fs::create_dir_all(&resources_dir)?;
    for (key, dst_name) in [
        (
            o.config.macos.packaging.welcome_html.as_deref(),
            "welcome.html",
        ),
        (
            o.config.macos.packaging.license_html.as_deref(),
            "license.html",
        ),
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
    let mut expected: Vec<String> = plugin_formats
        .iter()
        .map(|fmt| format!("{}-{}.pkg", p.file_stem(), fmt.label()))
        .collect();
    expected.extend(
        extras
            .iter()
            .map(|ec| format!("{}-{}.pkg", p.file_stem(), ec.label)),
    );
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
/// `.pkg`. The dist suffix uses the developer-requested `scope`, not the
/// widened `effective_scope` - a `--user` build that widened to System
/// because it bundles AAX still gets the `-user` filename so the
/// developer's CI scripts find it.
fn run_productbuild(
    p: &PluginDef,
    dist_dir: &Path,
    dist_xml_path: &Path,
    components_dir: &Path,
    resources_dir: &Path,
    o: &PackageOpts,
) -> Result<PathBuf, crate::CargoTruceError> {
    // Filename uses `crate_name` for per-plugin installers so the
    // output is consistent across macOS / Windows / Linux and across
    // plugins vs suites (which already key off a bundle/crate-style
    // slug). Display names with spaces (`Truce Gain.pkg`) sort weirdly
    // in directory listings, embed funny in URLs / artifact-uploaders,
    // and don't match the Linux tarball's slug. Keep the user-facing
    // bundle name + Info.plist `CFBundleName` etc. on `p.name` - only
    // the dist artifact's filename changes.
    let pkg_name = format!(
        "{}-{}-macos{}.pkg",
        p.crate_name,
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
    let component_pkg = components_dir.join(format!("{}-{}.pkg", p.file_stem(), fmt.label()));

    // Stage every format through an isolated `_pkgroot_<fmt>/` so
    // pkgbuild sees exactly one payload per call. Historically we
    // used `--component <bundle>` for the "native bundle" formats
    // (VST3, AU v2, AU v3, standalone) and `--root` only for the
    // others, but `--component` auto-stamps `BundleIsRelocatable=true`
    // in the PackageInfo. That makes the installer "upgrade in place"
    // any prior copy of the bundle ID Launch Services knows about,
    // including stray `target/bundles/...` staging dirs from
    // `cargo truce run`, so the .pkg payload silently lands in the
    // developer's build tree instead of `/Applications/`. Using
    // `--root` + `--component-plist BundleIsRelocatable=false` pins
    // the install at the declared `install_location()`.
    let root_dir = staging.join(format!("_pkgroot_{}", fmt.label()));
    let _ = fs::remove_dir_all(&root_dir);
    fs::create_dir_all(&root_dir)?;
    let dst = root_dir.join(&bundle_name);
    if component_path.is_dir() {
        copy_dir_recursive(&component_path, &dst)?;
    } else {
        fs::copy(&component_path, &dst)?;
    }
    let mut pkgbuild_args = vec![
        "--root".to_string(),
        root_dir.to_str().unwrap().to_string(),
        "--install-location".to_string(),
        fmt.install_location().to_string(),
    ];
    if fmt.is_native_bundle() {
        let plist_path = staging.join(format!("_component_plist_{}.plist", fmt.label()));
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
    <dict>
        <key>RootRelativeBundlePath</key>
        <string>{bundle_name_escaped}</string>
        <key>BundleIsRelocatable</key>
        <false/>
        <key>BundleIsVersionChecked</key>
        <true/>
        <key>BundleHasStrictIdentifier</key>
        <true/>
        <key>BundleOverwriteAction</key>
        <string>upgrade</string>
    </dict>
</array>
</plist>
"#,
            bundle_name_escaped = bundle_name.replace('&', "&amp;").replace('<', "&lt;"),
        );
        fs::write(&plist_path, plist)?;
        pkgbuild_args.push("--component-plist".to_string());
        pkgbuild_args.push(plist_path.to_str().unwrap().to_string());
    }

    pkgbuild_args.extend_from_slice(&[
        "--identifier".to_string(),
        pkg_id,
        "--version".to_string(),
        o.version.to_string(),
        // `preserve` records the staged files' actual ownership
        // (mahae:staff for a developer build) in the BOM instead of
        // synthesising root:wheel. Shove then writes the payload as
        // the running user - no chown step, no `EACCES` when the
        // installer's auth level is `None` (per-user install).
        "--ownership".to_string(),
        "preserve".to_string(),
    ]);

    // Per-format scripts dir; always present now (preinstall sweeps
    // stale leftovers and AU v2's postinstall clears the AU cache).
    // `write_format_scripts` produced the dir; pkgbuild copies its
    // contents into the resulting `.pkg`'s `Scripts` payload.
    pkgbuild_args.push("--scripts".to_string());
    pkgbuild_args.push(scripts_dir.to_str().unwrap().to_string());

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
    format: BuildFormat,
) -> Res {
    let feature = format.feature();
    let label = format.label();
    let suffix = format.dylib_suffix();

    // AU v2 needs a unique cocoa-view class name per dylib so hosts
    // that look up classes via `[NSBundle classNamed:]` (REAPER) can
    // find the right one - see `truce-au`'s `build.rs`. That means
    // one cargo invocation per plugin with `TRUCE_AU_PLUGIN_ID` set,
    // instead of one batched build for all plugins.
    if format == BuildFormat::Au2 {
        if archs.len() == 1 {
            eprintln!("Building {label} ({})...", archs[0].triple());
        } else {
            eprintln!("Building {label} for {} archs...", archs.len());
        }
        for p in plugins {
            let env = [("TRUCE_AU_PLUGIN_ID", p.bundle_id.as_str())];
            let args: Vec<&str> = vec![
                "-p",
                &p.crate_name,
                "--no-default-features",
                "--features",
                feature,
            ];
            for &arch in archs {
                crate::cargo_build_for_arch(&env, &args, arch, dt)?;
            }
        }
    } else {
        let mut base: Vec<&str> = Vec::new();
        for p in plugins {
            base.push("-p");
            base.push(&p.crate_name);
        }
        base.extend_from_slice(&["--no-default-features", "--features", feature]);

        if archs.len() == 1 {
            eprintln!("Building {label} ({})...", archs[0].triple());
        } else {
            eprintln!("Building {label} for {} archs...", archs.len());
        }
        cargo_build_multi_arch(archs, &base, dt)?;
    }

    for &arch in archs {
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

    // VST3 / CLAP / VST2 also need an MH_BUNDLE binary (not the
    // MH_DYLIB that `rustc --crate-type cdylib` emits) so JUCE-hosted
    // VST3 + CFBundle loaders accept them. Link the per-arch Rust
    // `staticlib` (`lib<stem>.a`) through `clang -bundle` per arch,
    // then `lipo` the slices into a universal bundle-bin that the
    // stage / install pipelines copy into `Contents/MacOS/`. AU2 +
    // AAX keep the cdylib - their loaders are happy with MH_DYLIB.
    if matches!(
        format,
        BuildFormat::Clap | BuildFormat::Vst3 | BuildFormat::Vst2
    ) {
        let exports: &[&str] = match format {
            BuildFormat::Clap => CLAP_EXPORTS,
            BuildFormat::Vst3 => VST3_EXPORTS,
            BuildFormat::Vst2 => VST2_EXPORTS,
            _ => unreachable!(),
        };
        for p in plugins {
            let mut staticlibs: Vec<(MacArch, PathBuf)> = Vec::with_capacity(archs.len());
            for &arch in archs {
                let a = release_static_for_target(root, &p.dylib_stem(), Some(arch.triple()));
                if !a.exists() {
                    return Err(crate::missing_staticlib_error(&a).into());
                }
                staticlibs.push((arch, a));
            }
            let out = release_bundle_bin(root, &p.dylib_stem(), suffix);
            link_macos_bundle(&staticlibs, exports, dt, &out)?;
        }
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

fn resolve_pkg_scope(
    cli: Option<PkgScope>,
    config: &Config,
) -> Result<PkgScope, crate::CargoTruceError> {
    if let Some(s) = cli {
        return Ok(s);
    }
    if let Some(ref raw) = config.packaging.preferred_scope {
        return raw.parse::<PkgScope>().map_err(Into::into);
    }
    Ok(PkgScope::os_default())
}

/// Notarize a .pkg and staple the ticket.
#[allow(clippy::too_many_lines)]
fn notarize_and_staple(pkg_path: &Path, _config: &Config) -> Res {
    let pkg = pkg_path.to_str().unwrap();

    // Notarization credentials are per-developer - read from the
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

    // Submit with the keychain profile, retrying transient failures
    // (network blips, notary-service hiccups, `xcrun` failing to reach
    // Apple). A single transient failure must NOT fall straight through to
    // the explicit-credentials path - that's empty for most setups, so a
    // momentary glitch turned into a hard "requires APP_SPECIFIC_PASSWORD"
    // error. A `status: Invalid` / `Rejected` is a real verdict on the
    // bundle, not transient: stop and surface it (re-submitting with other
    // credentials yields the same verdict).
    let mut output_text = String::new();
    let mut succeeded = false;
    let mut rejected = false;
    let mut fatal = false;
    for (attempt, delay) in [0u64, 15, 45, 90].into_iter().enumerate() {
        if delay > 0 {
            eprintln!(
                "    notarytool submit didn't go through; retrying in {delay}s (attempt {})...",
                attempt + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
        // A failed `xcrun` spawn falls through to another retry.
        if let Ok(o) = Command::new("xcrun")
            .args([
                "notarytool",
                "submit",
                pkg,
                "--keychain-profile",
                &keychain_profile,
                "--wait",
            ])
            .output()
        {
            output_text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if output_text.contains("status: Invalid") || output_text.contains("status: Rejected") {
                rejected = true;
                break;
            }
            // notarytool returns 0 even on Invalid (caught above); a clean
            // exit now means Accepted.
            if o.status.success() {
                succeeded = true;
                break;
            }
            // Auth / agreement failures (HTTP 401/403) are account-wide and
            // not transient - retrying or swapping credentials can't help, so
            // stop and surface Apple's message verbatim.
            if output_text.contains("HTTP status code: 403")
                || output_text.contains("HTTP status code: 401")
                || output_text.contains("A required agreement")
            {
                fatal = true;
                break;
            }
            // Non-zero without a verdict = couldn't reach the service /
            // timed out: transient, retry.
        }
    }

    if rejected {
        fetch_notarization_log(&output_text, &keychain_profile);
        return Err("notarization failed (status: Invalid). See log above for details.".into());
    }

    if fatal {
        return Err(format!(
            "notarization blocked by Apple's notary service (not a build problem):\n\n{}\n\
             If this is a 403 / required-agreement error, sign in as the Account Holder at \
             https://developer.apple.com/account and App Store Connect > Agreements, Tax, and \
             Banking, accept any pending agreement, then re-run.",
            output_text.trim()
        )
        .into());
    }

    if !succeeded {
        // Transient retries exhausted. Fall back to explicit credentials
        // if the developer configured them; otherwise surface that this was
        // a connection / profile failure, not missing credentials.
        if !apple_id.is_empty() && !team_id.is_empty() {
            eprintln!("  Keychain profile submit kept failing; trying explicit credentials...");
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
                fetch_notarization_log(&text, &keychain_profile);
                return Err(
                    "notarization failed (status: Invalid). See log above for details.".into(),
                );
            }
        } else {
            return Err(format!(
                "notarization submit kept failing after retries - the notary service was \
                 unreachable or the keychain profile '{keychain_profile}' is misconfigured. \
                 Re-run the package step; if it persists, check the profile with \
                 `xcrun notarytool history --keychain-profile {keychain_profile}`, or set \
                 APPLE_ID + TEAM_ID + APP_SPECIFIC_PASSWORD as a fallback.\n\nLast output:\n{}",
                output_text.trim()
            )
            .into());
        }
    }

    // Staple. `notarytool --wait` returns as soon as Apple's notary
    // service has the ticket, but `stapler` reads the ticket from
    // CloudKit's edge - which can lag the notary service by a couple
    // of minutes on a fresh submission. Apple's docs explicitly say
    // to retry on the "CloudKit Record not found" failure path, so
    // do that with exponential backoff up to a few minutes total
    // before giving up.
    eprintln!("  Stapling...");
    let delays_secs = [15u64, 30, 60, 90, 120];
    let mut last_stderr = String::new();
    let mut stapled = false;
    for (i, delay) in std::iter::once(0u64).chain(delays_secs).enumerate() {
        if delay > 0 {
            eprintln!(
                "    CloudKit ticket not propagated yet; retrying in {delay}s (attempt {}/{})...",
                i + 1,
                delays_secs.len() + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
        let output = Command::new("xcrun")
            .args(["stapler", "staple", pkg])
            .output()?;
        if output.status.success() {
            stapled = true;
            break;
        }
        last_stderr = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Only retry on the documented propagation race. Other
        // stapler failures (corrupt pkg, signature mismatch, etc.)
        // won't fix themselves with more waiting.
        let is_propagation_race = last_stderr.contains("Record not found")
            || last_stderr.contains("Could not find base64 encoded ticket");
        if !is_propagation_race {
            eprintln!("{last_stderr}");
            return Err("stapler staple failed".into());
        }
    }
    if !stapled {
        eprintln!("{last_stderr}");
        return Err(
            "stapler staple kept hitting CloudKit propagation lag after retries - \
             re-run `xcrun stapler staple <pkg>` manually in a few minutes once the \
             ticket lands."
                .into(),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_dropped_when_au3_present() {
        let out =
            drop_standalone_if_au3(vec![PkgFormat::Clap, PkgFormat::Standalone, PkgFormat::Au3]);
        // The AU v3 app is the standalone host; the separate Standalone
        // format is dropped so they don't fight over `{name}.app`.
        assert_eq!(out, vec![PkgFormat::Clap, PkgFormat::Au3]);
    }

    #[test]
    fn standalone_kept_without_au3() {
        assert_eq!(
            drop_standalone_if_au3(vec![PkgFormat::Clap, PkgFormat::Standalone]),
            vec![PkgFormat::Clap, PkgFormat::Standalone]
        );
    }
}
