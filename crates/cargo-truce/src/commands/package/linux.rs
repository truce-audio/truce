//! `cargo truce package` on Linux — phase 1: tarball + install.sh.
//!
//! Per linux-package.md (internal design doc): produces a tarball
//! per declared `[[suite]]` (and per plugin, unless `--no-per-plugin`).
//! Each tarball contains the staged plugin bundles + a generated
//! `install.sh` that picks user vs system paths and per-plugin
//! components.
//!
//! The actual cross-compile to `x86_64-unknown-linux-gnu` is the
//! plugin author's responsibility — this code organises whatever's
//! sitting under `target/<profile>/` after a build into a tarball
//! shaped for end-user install. Running `cargo truce package` on a
//! macOS host produces a Linux tarball whose bundles are the host's
//! Mach-O binaries; that's a developer error to catch in CI, not
//! something the packager corrects automatically.
//!
//! Phase 1 deliberately stops at `.tar.gz` + `install.sh`. `.deb`,
//! `.rpm`, `AppImage`, and AUR are deferred (see linux-package.md).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::SuiteSelection;
use crate::config::{Config, PluginDef, ResolvedSuite};
use crate::{BoxErr, Res, load_config, project_root, read_workspace_version};

const INSTALL_SH_TEMPLATE: &str = include_str!("install.sh.tmpl");

pub(crate) fn cmd_package_linux(args: &[String], selection: &SuiteSelection) -> Res {
    if let Some(unknown) = args.iter().find(|a| !a.is_empty()) {
        return Err(format!(
            "unknown flag: {unknown}\n\
             Linux `cargo truce package` accepts only the suite-selection \
             flags (--suite, --no-suite, --no-per-plugin) in phase 1. \
             Format selection (--formats) and signing flags will land in \
             a later phase."
        )
        .into());
    }

    let config = load_config()?;
    let root = project_root();
    let version = read_workspace_version(&root).unwrap_or_else(|| "0.0.0".to_string());

    if config.plugin.is_empty() {
        return Err("no [[plugin]] entries in truce.toml".into());
    }

    let dist_dir = truce_build::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    if selection.want_per_plugin() {
        eprintln!("Per-plugin tarballs");
        for plugin in &config.plugin {
            build_per_plugin_tarball(&root, &config, plugin, &dist_dir, &version)?;
        }
    } else {
        eprintln!("Skipping per-plugin tarballs (--no-per-plugin).");
    }

    let suites: Vec<ResolvedSuite<'_>> = config
        .suites
        .iter()
        .filter(|s| selection.want_suite(&s.name))
        .map(|s| s.resolve(&config.plugin))
        .collect::<Result<_, _>>()?;

    if !suites.is_empty() {
        eprintln!("\nSuite tarballs");
        for suite in &suites {
            build_suite_tarball(&root, &config, suite, &dist_dir, &version)?;
        }
    }

    eprintln!("\nDone. Tarballs in {}", dist_dir.display());
    Ok(())
}

/// One tarball per plugin: `{bundle_id}-{version}-linux-{arch}.tar.gz`.
fn build_per_plugin_tarball(
    root: &Path,
    config: &Config,
    plugin: &PluginDef,
    dist_dir: &Path,
    version: &str,
) -> Res {
    let arch = host_linux_arch();
    let stem = format!("{}-{}-linux-{}", plugin.bundle_id, version, arch);
    let staging = stage_dir(root, &stem)?;

    let plugin_summary = stage_plugin_payload(root, plugin, &staging)?;
    write_install_sh(&staging, config, &[plugin_summary], None)?;
    write_readme(&staging, config, version, &[plugin], None)?;

    let out = dist_dir.join(format!("{stem}.tar.gz"));
    create_tarball(&staging, &out, &stem)?;
    eprintln!("  {} → {}", plugin.name, out.display());
    Ok(())
}

/// One tarball per suite: `{suite.bundle_id}-{version}-linux-{arch}.tar.gz`.
fn build_suite_tarball(
    root: &Path,
    config: &Config,
    suite: &ResolvedSuite<'_>,
    dist_dir: &Path,
    version: &str,
) -> Res {
    let arch = host_linux_arch();
    let suite_version = suite.def.version.as_deref().unwrap_or(version);
    let stem = format!("{}-{}-linux-{}", suite.def.bundle_id, suite_version, arch);
    let staging = stage_dir(root, &stem)?;

    let mut summaries = Vec::with_capacity(suite.plugins.len());
    for plugin in &suite.plugins {
        summaries.push(stage_plugin_payload(root, plugin, &staging)?);
    }
    write_install_sh(&staging, config, &summaries, Some(suite))?;
    write_readme(&staging, config, suite_version, &suite.plugins, Some(suite))?;

    let out = dist_dir.join(format!("{stem}.tar.gz"));
    create_tarball(&staging, &out, &stem)?;
    eprintln!("  {} → {}", suite.def.name, out.display());
    Ok(())
}

/// Per-plugin payload — what the install.sh sees. Captures per-plugin
/// metadata the script needs for the user-vs-system + per-plugin
/// selection logic.
struct PluginSummary {
    bundle_id: String,
    /// Display name from `[[plugin]].name`.
    display_name: String,
    /// Bundle directory names that the script needs to know about, e.g.
    /// `["My Gain.clap", "My Gain.vst3"]`. The strings are relative to
    /// the tarball root's `plugins/` subdirectory.
    bundles: Vec<BundleEntry>,
    /// Standalone executable name relative to `standalone/`, if any.
    standalone: Option<String>,
}

struct BundleEntry {
    /// Format slug for install path: "clap" / "vst3" / "lv2" / "vst".
    format: &'static str,
    /// Filename within the tarball's `plugins/<bundle_id>/` directory.
    name: String,
}

/// Stage one plugin's bundles + standalone into `<staging>/` and
/// return a summary for install.sh generation.
///
/// Looks for already-built bundles under `target/bundles/` (the
/// output of `cargo truce build`) and copies them into the tarball
/// staging tree. Missing bundles get a warning, not an error — a
/// suite that includes plugin A's CLAP and plugin B's VST3 only is
/// a legitimate shape, not a build error.
fn stage_plugin_payload(
    root: &Path,
    plugin: &PluginDef,
    staging: &Path,
) -> Result<PluginSummary, BoxErr> {
    let bundles_dir = truce_build::target_dir(root).join("bundles");
    let plugin_staging = staging.join("plugins").join(&plugin.bundle_id);
    fs::create_dir_all(&plugin_staging)?;

    let mut bundles = Vec::new();

    // Each pair: (bundle filename pattern in `target/bundles/`,
    // format slug for install path). Matches the per-format bundle
    // names produced by the macOS / Windows / Linux build paths.
    let candidates: &[(String, &str)] = &[
        (format!("{}.clap", plugin.name), "clap"),
        (format!("{}.vst3", plugin.name), "vst3"),
        (format!("{}.lv2", plugin.name), "lv2"),
        // VST2 on Linux is a single .so, not a bundle directory.
        (format!("{}.so", plugin.name), "vst"),
    ];

    for (name, format) in candidates {
        let src = bundles_dir.join(name);
        if !src.exists() {
            continue;
        }
        let dst = plugin_staging.join(name);
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
        bundles.push(BundleEntry {
            format,
            name: name.clone(),
        });
    }

    if bundles.is_empty() {
        eprintln!(
            "  warning: no bundles for {} under {}. Run `cargo truce build` first.",
            plugin.name,
            bundles_dir.display(),
        );
    }

    // Standalone — when the plugin has the `standalone` feature in
    // its default features, `cargo truce run` stages a binary or
    // .app under `target/bundles/<Plugin>.standalone[.app]`. On
    // Linux it's a bare ELF; pick that up if present.
    let standalone = stage_standalone_payload(root, plugin, &plugin_staging)?;

    Ok(PluginSummary {
        bundle_id: plugin.bundle_id.clone(),
        display_name: plugin.name.clone(),
        bundles,
        standalone,
    })
}

fn stage_standalone_payload(
    root: &Path,
    plugin: &PluginDef,
    plugin_staging: &Path,
) -> Result<Option<String>, BoxErr> {
    let bundles_dir = truce_build::target_dir(root).join("bundles");
    // On Linux, `cargo truce run` stages a bare binary at
    // `target/bundles/<Plugin>.standalone`. macOS uses `.app`,
    // Windows uses `.exe`. We're producing a Linux tarball, so the
    // ELF is the canonical case.
    let candidate = bundles_dir.join(format!("{}.standalone", plugin.name));
    if !candidate.exists() {
        return Ok(None);
    }
    let bin_name = standalone_binary_name(plugin);
    let dst_dir = plugin_staging.join("standalone");
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

    let rendered = INSTALL_SH_TEMPLATE
        .replace("{{PROJECT}}", &project_label)
        .replace("{{VENDOR}}", &config.vendor.name)
        .replace("{{PLUGIN_NAMES}}", &plugin_names)
        .replace("{{PLUGIN_CASES}}", &plugin_cases);

    let path = staging.join("install.sh");
    fs::write(&path, rendered)?;
    set_executable(&path)?;
    Ok(())
}

/// One case-block per plugin in the install.sh's main loop.
/// Format-to-path mapping lives in the template's `dest_dir()` helper.
fn format_plugin_case(p: &PluginSummary) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "    {})", p.bundle_id);
    let _ = writeln!(s, "        echo \"  Installing {} ...\"", p.display_name);
    for b in &p.bundles {
        let _ = writeln!(
            s,
            "        install_bundle \"{format}\" \"plugins/{bundle_id}/{name}\"",
            format = b.format,
            bundle_id = p.bundle_id,
            name = b.name,
        );
    }
    if let Some(bin) = &p.standalone {
        let _ = writeln!(
            s,
            "        install_standalone \"plugins/{bundle_id}/standalone/{bin}\" \"{bin}\"",
            bundle_id = p.bundle_id,
        );
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

fn host_linux_arch() -> &'static str {
    // For now: report the host arch. Cross-compile awareness lands
    // when --target is added. `cargo truce package` running on
    // macOS produces a tarball labelled with the host arch; that's
    // a developer error to catch via CI, not silent correction.
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn stage_dir(root: &Path, stem: &str) -> Result<PathBuf, BoxErr> {
    let staging = truce_build::target_dir(root).join("package").join(stem);
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
        .ok_or_else(|| BoxErr::from("staging dir has no parent"))?;
    let basename = staging
        .file_name()
        .ok_or_else(|| BoxErr::from("staging dir has no name"))?
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
