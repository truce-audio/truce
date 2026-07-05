//! `cargo truce package` on Linux: tarball + `install.sh`.
//!
//! Produces a tarball per declared `[[suite]]` (and per plugin,
//! unless `--no-per-plugin`). Each tarball contains the staged
//! plugin bundles + a generated `install.sh` that picks user vs
//! system paths and per-plugin components. Native package formats
//! (`.deb`, `.rpm`, `AppImage`, AUR) are out of scope here.
//!
//! The actual cross-compile to `x86_64-unknown-linux-gnu` is the
//! plugin author's responsibility - this code organises whatever's
//! sitting under `target/<profile>/` after a build into a tarball
//! shaped for end-user install. Running `cargo truce package` on a
//! macOS host produces a Linux tarball whose bundles are the host's
//! Mach-O binaries; that's a developer error to catch in CI, not
//! something the packager corrects automatically.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PkgFormat, SuiteSelection};
use crate::commands::install::presets;
use crate::config::{Config, PluginDef, ResolvedSuite};
use crate::{CargoTruceError, Res, load_config, project_root, read_workspace_version};
use truce_build::BundleManifest;

const INSTALL_SH_TEMPLATE: &str = include_str!("install.sh.tmpl");

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_package_linux(args: &[String], selection: &SuiteSelection) -> Res {
    let mut no_build = false;
    let mut targets: Vec<String> = Vec::new();
    let mut formats: Option<Vec<PkgFormat>> = None;
    let mut plugin_filter: Option<String> = None;
    let mut target_cpu_arg: Option<String> = None;
    let mut i = 0;
    let mut leftover: Vec<&str> = Vec::new();
    while i < args.len() {
        match args[i].as_str() {
            // Empty string: arg-split residue. `--no-sign` /
            // `--no-pace-sign` / `--no-notarize`: macOS-only flags
            // accepted silently here so cross-platform CI matrices
            // can pass the same `--no-sign --no-notarize` to every
            // OS without per-platform branches.
            "" | "--no-sign" | "--no-pace-sign" | "--no-notarize" => {}
            "--no-build" => no_build = true,
            "--target" => {
                i += 1;
                let v = args.get(i).ok_or("--target requires a value")?;
                targets.push(v.clone());
            }
            "--formats" => {
                i += 1;
                let v = args.get(i).ok_or("--formats requires a value")?;
                formats = Some(PkgFormat::parse_list(v)?);
            }
            "-p" => {
                i += 1;
                let v = args.get(i).ok_or("-p requires a plugin crate name")?;
                plugin_filter = Some(v.clone());
            }
            "--target-cpu" => {
                i += 1;
                let v = args.get(i).ok_or("--target-cpu requires a value")?;
                target_cpu_arg = Some(v.clone());
            }
            other => leftover.push(other),
        }
        i += 1;
    }
    if let Some(unknown) = leftover.first() {
        return Err(format!(
            "unknown flag: {unknown}\n\
             Linux `cargo truce package` accepts -p <crate>, the \
             suite-selection flags (--suite, --no-suite, --no-per-plugin), \
             --target <triple> (repeatable), --formats <list>, --target-cpu \
             <value>, and --no-build."
        )
        .into());
    }

    // `--formats` drives the build step (Linux doesn't have a
    // post-build filter the way macOS / Windows pkgbuild + iscc do -
    // the tarball stages whatever's in the manifest). Combining
    // `--formats` with `--no-build` is therefore a no-op at best and
    // misleading at worst: the manifest already reflects whatever the
    // prior build produced.
    if no_build && formats.is_some() {
        return Err("--formats cannot be combined with --no-build on Linux: \
                    `--formats` drives the implicit `cargo truce build`, so \
                    with `--no-build` the existing manifest is consumed as-is."
            .into());
    }

    let config = load_config()?;
    let root = project_root();
    let version = read_workspace_version(&root).unwrap_or_else(|e| {
        eprintln!("WARNING: {e}; defaulting tarball version to 0.0.0");
        "0.0.0".to_string()
    });

    if config.plugin.is_empty() {
        return Err("no [[plugin]] entries in truce.toml".into());
    }

    // Run `cargo truce build` first unless the caller opted out via
    // `--no-build`. Cargo's incremental build makes a fresh `package`
    // after a recent `build` essentially free. With `--target` flags,
    // we pass them through so each requested target gets built /
    // staged before we consume its manifest.
    if !no_build {
        eprintln!("Building bundles...");
        let mut build_args: Vec<String> = Vec::new();
        for t in &targets {
            build_args.push("--target".into());
            build_args.push(t.clone());
        }
        if let Some(ref fmts) = formats {
            for f in fmts {
                if let Some(flag) = build_flag_for_format(f) {
                    build_args.push(flag.into());
                }
            }
        }
        if let Some(ref p) = plugin_filter {
            build_args.push("-p".into());
            build_args.push(p.clone());
        }
        if let Some(ref v) = target_cpu_arg {
            build_args.push("--target-cpu".into());
            build_args.push(v.clone());
        }
        super::super::build::cmd_build(&build_args)?;
        eprintln!();
    }

    let dist_dir = truce_build::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    // Build a list of (target_triple, bundles_dir, manifest) tuples to
    // package. With no `--target`, we read the flat
    // `target/bundles/manifest.toml` (matches the historical layout);
    // with one or more `--target`, we read each
    // `target/bundles/<triple>/manifest.toml`.
    let bundles_root = truce_build::target_dir(&root).join("bundles");
    let plans: Vec<(String, PathBuf, BundleManifest)> = if targets.is_empty() {
        let manifest = BundleManifest::load(&bundles_root).map_err(CargoTruceError::from)?;
        validate_manifest_triple(&manifest, &bundles_root, truce_build::host_triple())?;
        vec![(
            manifest.target_triple.clone(),
            bundles_root.clone(),
            manifest,
        )]
    } else {
        let mut out = Vec::new();
        for t in &targets {
            let dir = bundles_root.join(t);
            let manifest = BundleManifest::load(&dir).map_err(CargoTruceError::from)?;
            validate_manifest_triple(&manifest, &dir, t)?;
            out.push((t.clone(), dir, manifest));
        }
        out
    };

    // `-p <crate>` narrows packaging to a single plugin. Same rule as
    // the macOS / Windows packagers: a single-plugin run can't satisfy
    // a multi-member suite, so suite tarballs are skipped in that mode.
    let selected_plugins = crate::commands::pick_plugins(&config, plugin_filter.as_deref())?;
    let suites: Vec<ResolvedSuite<'_>> = if plugin_filter.is_some() {
        if !config.suites.is_empty() {
            eprintln!("(-p set; skipping suite tarballs - they need every member plugin staged)");
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

    // The standalone host is a `[[bin]]`, not a cdylib, so the
    // `cargo truce build` in the build phase above never produces it
    // (`build_flag_for_format` returns `None` for it). macOS/Windows
    // `package` build the standalone inline; Linux historically relied on
    // a prior `cargo truce run` having staged it. Build it here too so a
    // clean `package` (CI, with no prior `run`) still ships it - but only
    // when `standalone` is *explicitly* requested, so a bare `cargo truce
    // package` over a suite of effect-only plugins doesn't error trying
    // to build a standalone bin they don't have. Without `--formats` we
    // keep the old behaviour: stage a `run`-staged binary if present.
    let build_standalone = formats
        .as_ref()
        .is_some_and(|f| f.contains(&PkgFormat::Standalone));

    for (triple, bundles_dir, manifest) in &plans {
        let arch = arch_from_triple(triple);
        if plans.len() > 1 {
            eprintln!("Target: {triple}");
        }

        if !no_build && build_standalone {
            let cross = (triple.as_str() != truce_build::host_triple()).then_some(triple.as_str());
            for plugin in &selected_plugins {
                build_standalone_binary(&root, plugin, cross, bundles_dir)?;
            }
        }

        let ctx = TarballCtx {
            root: &root,
            config: &config,
            dist_dir: &dist_dir,
            version: &version,
            bundles_dir,
            manifest,
            arch,
        };

        if selection.want_per_plugin() {
            eprintln!("Per-plugin tarballs");
            for plugin in &selected_plugins {
                build_per_plugin_tarball(&ctx, plugin)?;
            }
        } else {
            eprintln!("Skipping per-plugin tarballs (--no-per-plugin).");
        }

        if !suites.is_empty() {
            eprintln!("\nSuite tarballs");
            for suite in &suites {
                build_suite_tarball(&ctx, suite)?;
            }
        }
        if plans.len() > 1 {
            eprintln!();
        }
    }

    eprintln!("\nDone. Tarballs in {}", dist_dir.display());
    Ok(())
}

/// Validate that a loaded manifest's `target_triple` matches what we
/// expected from its containing directory (or the host fallback for
/// the flat layout). Catches the case where someone manually edits
/// the manifest or copies a target/ tree across hosts.
fn validate_manifest_triple(manifest: &BundleManifest, bundles_dir: &Path, expected: &str) -> Res {
    if manifest.target_triple != expected {
        return Err(format!(
            "build manifest at {} is for target {} but expected {}. \
             Re-run `cargo truce build` for the matching target.",
            BundleManifest::manifest_path(bundles_dir).display(),
            manifest.target_triple,
            expected,
        )
        .into());
    }
    Ok(())
}

/// Map a cargo target triple to the short arch label embedded in the
/// tarball stem (`aarch64`, `x86_64`). Falls back to the first dash-
/// separated segment for triples we don't recognise - same shape as
/// `uname -m` on Linux.
fn arch_from_triple(triple: &str) -> &str {
    triple.split('-').next().unwrap_or("unknown")
}

/// Context shared by every tarball build: per-target directories plus
/// the loaded manifest. Bundled into one struct so the per-plugin and
/// per-suite builders stay under the workspace argument-count cap.
struct TarballCtx<'a> {
    root: &'a Path,
    config: &'a Config,
    dist_dir: &'a Path,
    version: &'a str,
    bundles_dir: &'a Path,
    manifest: &'a BundleManifest,
    arch: &'a str,
}

/// One tarball per plugin: `{crate_name}-{version}-linux-{arch}.tar.gz`.
/// Uses `crate_name` (not `bundle_id`) so the dist filename matches
/// the macOS `.pkg` / Windows `.exe` produced for the same plugin.
fn build_per_plugin_tarball(ctx: &TarballCtx<'_>, plugin: &PluginDef) -> Res {
    let stem = format!("{}-{}-linux-{}", plugin.crate_name, ctx.version, ctx.arch);
    let staging = plugin_stage_dir(ctx.root, &plugin.bundle_id, ctx.arch)?;

    let plugin_summary = stage_plugin_payload(
        ctx.root,
        ctx.config,
        plugin,
        &staging,
        ctx.bundles_dir,
        ctx.manifest,
    )?;
    let install_paths = expected_tarball_paths(&stem, &[&plugin_summary]);
    write_install_sh(&staging, ctx.config, &[plugin_summary], None)?;
    write_readme(&staging, ctx.config, ctx.version, &[plugin], None)?;

    let out = ctx.dist_dir.join(format!("{stem}.tar.gz"));
    create_tarball(&staging, &out, &stem)?;

    // Sanity-check the tarball: must contain install.sh and every
    // bundle filename listed by the manifest under its format dir.
    // Catches the case where `tar` reports success but staging
    // silently produced no files.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let expected: Vec<&str> = install_paths.iter().map(String::as_str).collect();
        super::verify::assert_tarball_contains(&out, &expected)?;
    }
    let _ = install_paths; // unused on macOS/Windows verify-skip path

    eprintln!("  {} → {}", plugin.name, out.display());
    Ok(())
}

/// One tarball per suite: `{suite.bundle_id}-{version}-linux-{arch}.tar.gz`.
fn build_suite_tarball(ctx: &TarballCtx<'_>, suite: &ResolvedSuite<'_>) -> Res {
    let suite_version = suite.def.version.as_deref().unwrap_or(ctx.version);
    let stem = format!(
        "{}-{}-linux-{}",
        suite.def.bundle_id, suite_version, ctx.arch
    );
    let staging = suite_stage_dir(ctx.root, &suite.def.bundle_id, ctx.arch)?;

    let mut summaries = Vec::with_capacity(suite.plugins.len());
    for plugin in &suite.plugins {
        summaries.push(stage_plugin_payload(
            ctx.root,
            ctx.config,
            plugin,
            &staging,
            ctx.bundles_dir,
            ctx.manifest,
        )?);
    }
    let summary_refs: Vec<&PluginSummary> = summaries.iter().collect();
    let install_paths = expected_tarball_paths(&stem, &summary_refs);
    write_install_sh(&staging, ctx.config, &summaries, Some(suite))?;
    write_readme(
        &staging,
        ctx.config,
        suite_version,
        &suite.plugins,
        Some(suite),
    )?;

    let out = ctx.dist_dir.join(format!("{stem}.tar.gz"));
    create_tarball(&staging, &out, &stem)?;

    // Suite tarball must contain install.sh + every bundle the manifest
    // listed for any member plugin, under its format-grouped path.
    // The check catches a staging silent-skip that would otherwise
    // ship a partial suite archive.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let expected: Vec<&str> = install_paths.iter().map(String::as_str).collect();
        super::verify::assert_tarball_contains(&out, &expected)?;
    }
    let _ = install_paths;

    eprintln!("  {} → {}", suite.def.name, out.display());
    Ok(())
}

/// Compute the substrings the verify step expects to find in the
/// produced `.tar.gz`: `install.sh` plus every staged bundle path
/// under its format-grouped directory. Standalone binaries appear
/// under the top-level `standalone/` dir.
fn expected_tarball_paths(stem: &str, summaries: &[&PluginSummary]) -> Vec<String> {
    let mut paths = vec![format!("{stem}/install.sh")];
    for s in summaries {
        for b in &s.bundles {
            paths.push(format!("{stem}/{}/{}", b.format, b.name));
        }
        if let Some(bin) = &s.standalone {
            paths.push(format!("{stem}/standalone/{bin}"));
        }
    }
    paths
}

/// Per-plugin payload - what the install.sh sees. Captures per-plugin
/// metadata the script needs for the user-vs-system + per-plugin
/// selection logic.
struct PluginSummary {
    bundle_id: String,
    /// Display name from `[[plugin]].name`.
    display_name: String,
    /// Bundle directory/file names that the script needs to know about,
    /// e.g. `["My Gain.clap", "My Gain.vst3"]`. Each entry's `format`
    /// names the format-grouped directory in the tarball
    /// (`clap/`, `vst3/`, `lv2/`, `vst/`).
    bundles: Vec<BundleEntry>,
    /// Standalone executable name relative to the tarball's
    /// `standalone/` directory, if any.
    standalone: Option<String>,
    /// Tarball-relative path of the CLAP `<stem>.presets/` sibling
    /// directory, when the plugin ships factory presets and CLAP is
    /// staged. install.sh copies it next to the `.clap`.
    clap_presets: Option<String>,
    /// Tarball-relative path of this plugin's VST3 preset tree
    /// (`vst3-presets/<Vendor>/<Plugin>`), when staged. install.sh
    /// *merges* it into the shared `~/.vst3/presets` folder.
    vst3_presets: Option<String>,
    /// Tarball-relative path of the standalone `<bin>.presets/` sibling
    /// (`standalone/<bin>.presets`), when the plugin ships factory
    /// presets and a standalone is staged. install.sh copies it next to
    /// the installed binary - where the standalone resolves factory
    /// presets at runtime.
    standalone_presets: Option<String>,
}

struct BundleEntry {
    /// Format slug for install path AND for the format-grouped
    /// directory in the tarball: `"clap"`, `"vst3"`, `"lv2"`, `"vst"`.
    format: &'static str,
    /// Filename inside the tarball's `<format>/` directory.
    name: String,
}

/// Stage one plugin's bundles + standalone into `<staging>/` and
/// return a summary for install.sh generation.
///
/// Reads the build manifest written by `cargo truce build` to find
/// what bundles to copy; if the manifest lists nothing for this
/// plugin and the plugin has no standalone either, that's a hard
/// error rather than a silent skip - empty plugin payloads ship
/// broken tarballs.
fn stage_plugin_payload(
    root: &Path,
    config: &Config,
    plugin: &PluginDef,
    staging: &Path,
    bundles_dir: &Path,
    manifest: &BundleManifest,
) -> Result<PluginSummary, CargoTruceError> {
    let mut bundles = Vec::new();

    // Tarball layout mirrors the Linux install destinations: bundles
    // get grouped by format (`clap/`, `vst3/`, `lv2/`, `vst/`) at the
    // tarball root rather than nested under `plugins/<bundle_id>/`.
    // This makes `tar xf … -C ~/.config/...` viable as a manual
    // alternative to `install.sh` and keeps install.sh's per-plugin
    // case bodies short (one path component to copy from).
    for entry in manifest.bundles_for_plugin(&plugin.crate_name) {
        let Some(slug) = linux_install_slug(&entry.format) else {
            // AU2/AU3/AAX would never appear in a host-Linux manifest
            // because they're macOS-only; if one slips in via a copied
            // target/ from another host, the host_triple check above
            // already rejected it. Skip defensively here.
            continue;
        };
        let src = bundles_dir.join(&entry.filename);
        if !src.exists() {
            return Err(format!(
                "build manifest lists {} for {} but {} is missing on disk. \
                 Re-run `cargo truce build`.",
                entry.filename,
                plugin.name,
                src.display(),
            )
            .into());
        }
        let format_dir = staging.join(slug);
        fs::create_dir_all(&format_dir)?;
        let dst = format_dir.join(&entry.filename);
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
        bundles.push(BundleEntry {
            format: slug,
            name: entry.filename.clone(),
        });
    }

    // Factory presets, emitted at stage time (the build manifest only
    // tracks the bundles themselves). CLAP gets a `<stem>.presets/`
    // sibling, LV2 presets go inside the staged bundle, and VST3
    // presets stage as a loose tree install.sh merges into the shared
    // `~/.vst3/presets` folder.
    let mut clap_presets = None;
    let mut vst3_presets = None;
    if let Some(fp) = presets::load_factory_presets(root, plugin, config)? {
        for b in &bundles {
            match b.format {
                "clap" => {
                    let dir = format!("{}.presets", plugin.file_stem());
                    presets::emit_trucepreset_tree(
                        &fp,
                        &staging.join("clap").join(&dir),
                        false,
                        &format!("{}-clap", plugin.bundle_id),
                    )?;
                    clap_presets = Some(format!("clap/{dir}"));
                }
                "lv2" => {
                    let uri = truce_build::lv2::plugin_uri(
                        config.vendor.url.as_deref().unwrap_or(""),
                        &plugin.bundle_id,
                    );
                    presets::emit_lv2_presets(&fp, &staging.join("lv2").join(&b.name), &uri)?;
                }
                "vst3" => {
                    let payload = presets::vst3_preset_payload(&fp, plugin, config);
                    if let Some((first, _)) = payload.first() {
                        // All entries share the `<Vendor>/<Plugin>` prefix.
                        let plugin_dir: PathBuf = first.iter().take(2).collect();
                        for (rel, bytes) in &payload {
                            let dst = staging.join("vst3-presets").join(rel);
                            if let Some(parent) = dst.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::write(&dst, bytes)?;
                        }
                        vst3_presets = Some(format!("vst3-presets/{}", plugin_dir.display()));
                    }
                }
                _ => {}
            }
        }
    }

    // Standalone - when the plugin has the `standalone` feature in
    // its default features, `cargo truce run` stages a binary or
    // .app under `target/bundles/<Plugin>.standalone[.app]`. On
    // Linux it's a bare ELF; pick that up if present.
    let standalone = stage_standalone_payload(plugin, staging, bundles_dir)?;

    // Factory presets for the standalone ride as a sibling
    // `<bin>.presets/` next to the staged binary - the same path the
    // installed standalone resolves at runtime (`standalone_factory_root`,
    // shared with macOS and `cargo truce run`). `emit_standalone_factory`
    // is a no-op when the plugin ships no library.
    let standalone_presets = if let Some(bin) = &standalone {
        let exe = staging.join("standalone").join(bin);
        presets::emit_standalone_factory(root, plugin, config, &exe)?;
        let dir = staging.join("standalone").join(format!("{bin}.presets"));
        dir.is_dir().then(|| format!("standalone/{bin}.presets"))
    } else {
        None
    };

    if bundles.is_empty() && standalone.is_none() {
        return Err(format!(
            "no bundles or standalone for {} in {}. \
             The build manifest doesn't list this plugin's formats - \
             re-run `cargo truce build` (optionally with `-p {}`).",
            plugin.name,
            bundles_dir.display(),
            plugin.crate_name,
        )
        .into());
    }

    Ok(PluginSummary {
        bundle_id: plugin.bundle_id.clone(),
        display_name: plugin.name.clone(),
        bundles,
        standalone,
        clap_presets,
        vst3_presets,
        standalone_presets,
    })
}

/// Map a `PkgFormat` to its `cargo truce build` flag, or `None` for
/// formats that aren't a build target (the standalone host binary
/// is staged by `cargo truce run`, not `build`). `Au2`/`Au3`/`Aax`
/// map to their build flags even though those formats are no-ops on
/// Linux; `cargo truce build` already emits a single skip line and
/// returns cleanly, so passing them is harmless.
fn build_flag_for_format(f: &PkgFormat) -> Option<&'static str> {
    match f {
        PkgFormat::Clap => Some("--clap"),
        PkgFormat::Vst3 => Some("--vst3"),
        PkgFormat::Vst2 => Some("--vst2"),
        PkgFormat::Lv2 => Some("--lv2"),
        PkgFormat::Au2 => Some("--au2"),
        PkgFormat::Au3 => Some("--au3"),
        PkgFormat::Aax => Some("--aax"),
        PkgFormat::Standalone => None,
    }
}

/// Map a manifest format slug (`"vst2"`, `"clap"`, …) to the install
/// path slug used by `install.sh`'s `dest_dir()` (`"vst"`, `"clap"`,
/// …). Returns `None` for formats that don't have a Linux install
/// path (AU, AAX).
fn linux_install_slug(format: &str) -> Option<&'static str> {
    match format {
        "clap" => Some("clap"),
        "vst3" => Some("vst3"),
        "vst2" => Some("vst"),
        "lv2" => Some("lv2"),
        _ => None,
    }
}

fn stage_standalone_payload(
    plugin: &PluginDef,
    staging: &Path,
    bundles_dir: &Path,
) -> Result<Option<String>, CargoTruceError> {
    // On Linux, `cargo truce run` stages a bare binary at
    // `target/bundles/<Plugin>.standalone`. macOS uses `.app`,
    // Windows uses `.exe`. We're producing a Linux tarball, so the
    // ELF is the canonical case.
    let candidate = bundles_dir.join(format!("{}.standalone", plugin.file_stem()));
    if !candidate.exists() {
        return Ok(None);
    }
    let bin_name = standalone_binary_name(plugin);
    // All standalones in a suite tarball share the top-level
    // `standalone/` directory. Crate-derived names keep filenames
    // unique across plugins; an explicit `[[plugin]].standalone_bin`
    // override is the user's responsibility to keep distinct.
    let dst_dir = staging.join("standalone");
    fs::create_dir_all(&dst_dir)?;
    fs::copy(&candidate, dst_dir.join(&bin_name))?;
    // Mark executable. Tar will preserve the mode through the
    // archive; the install.sh's `cp -p` keeps it on the user's host.
    set_executable(&dst_dir.join(&bin_name))?;
    Ok(Some(bin_name))
}

fn standalone_binary_name(plugin: &PluginDef) -> String {
    crate::read_standalone_bin_name(&plugin.crate_name)
        .unwrap_or_else(|| format!("{}-standalone", plugin.crate_name))
}

/// Build `plugin`'s standalone `[[bin]]` (`--features standalone`) and
/// drop the release ELF where [`stage_standalone_payload`] looks for it
/// (`<bundles_dir>/<file_stem>.standalone`). Mirrors what `cargo truce
/// run` builds, but without launching anything - so `package` ships the
/// standalone on a clean checkout the same way macOS/Windows do.
/// `cross` is the target triple for a `--target` build, or `None` for a
/// host build (the CI / common case).
fn build_standalone_binary(
    root: &Path,
    plugin: &PluginDef,
    cross: Option<&str>,
    bundles_dir: &Path,
) -> Res {
    let bin_stem = standalone_binary_name(plugin);
    eprintln!("Building {} standalone...", plugin.name);

    let mut args: Vec<String> = vec![
        "-p".into(),
        plugin.crate_name.clone(),
        "--features".into(),
        "standalone".into(),
    ];
    if let Some(triple) = cross {
        args.push("--target".into());
        args.push(triple.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // `package` always builds release (it never flips the debug flag).
    crate::cargo_build(&[], &arg_refs, "")?;

    // Cargo writes the bin under `target/release/` (host) or
    // `target/<triple>/release/` (cross).
    let release = match cross {
        Some(triple) => truce_build::target_dir(root).join(triple).join("release"),
        None => truce_build::target_dir(root).join("release"),
    };
    let built = release.join(&bin_stem);
    if !built.exists() {
        return Err(format!(
            "standalone binary not found at {} after build. Does {} declare \
             a [[bin]] named '{bin_stem}' gated on the `standalone` feature?",
            built.display(),
            plugin.crate_name,
        )
        .into());
    }

    fs::create_dir_all(bundles_dir)?;
    let dst = bundles_dir.join(format!("{}.standalone", plugin.file_stem()));
    fs::copy(&built, &dst)?;
    set_executable(&dst)?;
    Ok(())
}

/// Render `install.sh` from the embedded template.
///
/// Template variables are simple `{{key}}` substitutions; we don't
/// pull in `tinytemplate` or `handlebars` for ten lines of bash.
fn write_install_sh(
    staging: &Path,
    config: &Config,
    summaries: &[PluginSummary],
    suite: Option<&ResolvedSuite<'_>>,
) -> Res {
    let project_label = match suite {
        Some(s) => s.def.name.clone(),
        None if summaries.len() == 1 => summaries[0].display_name.clone(),
        None => config.vendor.name.clone(),
    };

    let plugin_cases = summaries
        .iter()
        .map(format_plugin_case)
        .collect::<Vec<_>>()
        .join("\n\n");

    let plugin_names = summaries
        .iter()
        .map(|s| s.bundle_id.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    // `{{PROJECT}}` lands in an unquoted `cat <<EOF` heredoc (and
    // comments); `{{VENDOR}}` in comments. Both still expand `$` /
    // backticks there, so escape. `{{PLUGIN_NAMES}}` is a space-joined
    // list of validated bundle ids (no shell metachars); `{{PLUGIN_CASES}}`
    // is already shell-quoted by `format_plugin_case`.
    let rendered = INSTALL_SH_TEMPLATE
        .replace("{{PROJECT}}", &heredoc_escape(&project_label))
        .replace("{{VENDOR}}", &heredoc_escape(&config.vendor.name))
        .replace("{{PLUGIN_NAMES}}", &plugin_names)
        .replace("{{PLUGIN_CASES}}", &plugin_cases);

    let path = staging.join("install.sh");
    fs::write(&path, rendered)?;
    set_executable(&path)?;
    Ok(())
}

/// Single-quote a value for safe interpolation into the generated
/// `install.sh`. Inside single quotes the shell treats everything
/// literally; an embedded `'` is closed, escaped, and reopened
/// (`'\''`). Neutralizes `$(...)`, backticks, `$var`, and quotes in a
/// display name / bundle filename that would otherwise run on the end
/// user's machine at `./install.sh` time.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a value for an *unquoted* `cat <<EOF` heredoc, where the
/// shell still expands `$`, backticks, and `\`. Backslash first so the
/// escapes it emits aren't themselves re-escaped.
fn heredoc_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// One case-block per plugin in the install.sh's main loop.
/// Source paths reference the format-grouped tarball layout
/// (`clap/<filename>` etc.); destination paths come from the
/// template's `dest_dir()` helper. `bundle_id` is validated
/// (lowercase `a-z0-9` + `-_.`), so the `case` label is safe raw; the
/// free-text display name and filenames are single-quoted.
fn format_plugin_case(p: &PluginSummary) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "    {})", p.bundle_id);
    let _ = writeln!(
        s,
        "        echo \"  Installing \"{}\" ...\"",
        sh_quote(&p.display_name)
    );
    for b in &p.bundles {
        let _ = writeln!(
            s,
            "        install_bundle \"{format}\" \"{format}/\"{name}",
            format = b.format,
            name = sh_quote(&b.name),
        );
    }
    if let Some(src) = &p.clap_presets {
        let _ = writeln!(s, "        install_clap_presets {}", sh_quote(src));
    }
    if let Some(src) = &p.vst3_presets {
        let _ = writeln!(s, "        install_vst3_presets {}", sh_quote(src));
    }
    if let Some(bin) = &p.standalone {
        let q = sh_quote(bin);
        let _ = writeln!(s, "        install_standalone \"standalone/\"{q} {q}");
        if let Some(src) = &p.standalone_presets {
            let _ = writeln!(
                s,
                "        install_standalone_presets {} {q}",
                sh_quote(src)
            );
        }
    }
    s.push_str("        ;;");
    s
}

fn write_readme(
    staging: &Path,
    config: &Config,
    version: &str,
    plugins: &[&PluginDef],
    suite: Option<&ResolvedSuite<'_>>,
) -> Res {
    let title = match suite {
        Some(s) => format!("{} {}", s.def.name, version),
        None if plugins.len() == 1 => format!("{} {}", plugins[0].name, version),
        None => format!("{} {}", config.vendor.name, version),
    };
    let mut readme = String::new();
    readme.push_str(&title);
    readme.push('\n');
    readme.push_str(&"=".repeat(title.len()));
    readme.push_str("\n\n");
    if let Some(s) = suite
        && let Some(d) = &s.def.description
    {
        readme.push_str(d);
        readme.push_str("\n\n");
    }
    readme.push_str("Contents:\n");
    for p in plugins {
        let _ = writeln!(readme, "  - {} ({})", p.name, p.category);
    }
    readme.push_str(
        "\nInstall:\n  ./install.sh                   # interactive\n  \
         ./install.sh --plugin <name>     # one plugin (repeatable)\n  \
         ./install.sh --all               # everything, user scope\n  \
         ./install.sh --system            # system-wide install\n  \
         ./install.sh --help              # full options\n",
    );
    fs::write(staging.join("README.txt"), readme)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Per-plugin Linux staging dir, namespaced by arch so dual-target
/// packaging in one invocation doesn't have the second target wipe the
/// first. The in-archive layout uses the version-tagged stem via
/// `create_tarball`'s `--transform`, so the on-disk path stays
/// internal-only.
fn plugin_stage_dir(root: &Path, bundle_id: &str, arch: &str) -> Result<PathBuf, CargoTruceError> {
    stage_dir(root, "plugin", bundle_id, arch)
}

/// Per-suite Linux staging dir, also arch-namespaced.
fn suite_stage_dir(
    root: &Path,
    suite_bundle_id: &str,
    arch: &str,
) -> Result<PathBuf, CargoTruceError> {
    stage_dir(root, "suite", suite_bundle_id, arch)
}

fn stage_dir(
    root: &Path,
    kind: &str,
    bundle_id: &str,
    arch: &str,
) -> Result<PathBuf, CargoTruceError> {
    let staging = truce_build::target_dir(root)
        .join("package/linux")
        .join(arch)
        .join(kind)
        .join(bundle_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    Ok(staging)
}

fn create_tarball(staging: &Path, out: &Path, stem: &str) -> Res {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(out);
    // Use the system `tar`. Every Unix and modern Windows ships one;
    // Rust-side `tar` crates add a heavyweight dep for ten lines of
    // shell-out. The `--transform` swap renames the staging dir's
    // basename to `stem` so the archive's top-level directory name
    // is stable regardless of where we staged.
    let parent = staging
        .parent()
        .ok_or_else(|| CargoTruceError::from("staging dir has no parent"))?;
    let basename = staging
        .file_name()
        .ok_or_else(|| CargoTruceError::from("staging dir has no name"))?
        .to_string_lossy()
        .into_owned();
    let status = Command::new("tar")
        .arg("-czf")
        .arg(out)
        .arg("-C")
        .arg(parent)
        .arg("--transform")
        .arg(format!("s,^{basename},{stem},"))
        .arg(&basename)
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            return Err(format!("failed to spawn tar: {e}").into());
        }
    };
    if !status.success() {
        // BSD tar (default on macOS) doesn't support GNU `--transform`.
        // Fall back: rename the staging dir to `stem` first, then tar.
        let renamed = parent.join(stem);
        let _ = fs::remove_dir_all(&renamed);
        fs::rename(staging, &renamed)?;
        let status2 = Command::new("tar")
            .arg("-czf")
            .arg(out)
            .arg("-C")
            .arg(parent)
            .arg(stem)
            .status()?;
        if !status2.success() {
            return Err(format!("tar failed for {}", out.display()).into());
        }
        // Restore the staging dir name so the next build doesn't
        // collide on the renamed path.
        fs::rename(&renamed, staging)?;
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    // Windows: no chmod. The tarball preserves the mode from the
    // archive metadata (set explicitly in the archive entry by
    // `tar`'s default behaviour on Unix); on Windows there's nothing
    // meaningful to do. Result-typed so the cfg(unix) caller path
    // doesn't need a parallel branch.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{heredoc_escape, sh_quote};

    #[test]
    fn sh_quote_wraps_and_neutralizes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        // Inside single quotes only `'` is special; it closes, escapes,
        // and reopens. `$`, `(`, backticks stay literal.
        assert_eq!(sh_quote("$(x)`y`"), "'$(x)`y`'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn heredoc_escape_neutralizes_expansion() {
        // Backslash first so the escapes it emits aren't re-escaped.
        assert_eq!(heredoc_escape("$"), "\\$");
        assert_eq!(heredoc_escape("`"), "\\`");
        assert_eq!(heredoc_escape("\\"), "\\\\");
        assert_eq!(heredoc_escape("a$(x)`y`b"), "a\\$(x)\\`y\\`b");
    }
}
