//! Windows packaging: Authenticode signing + Inno Setup installer.
//!
//! Flow: build each format (release) → stage into `target\package\windows\{suffix}\`
//! → Authenticode-sign binaries → PACE-sign AAX if present → render `.iss`
//! → run `ISCC.exe` → Authenticode-sign the installer → output to `dist\`.
//!
//! Builds are **universal by default** — both `x86_64-pc-windows-msvc` and
//! `aarch64-pc-windows-msvc` slices are produced and stitched into a single
//! Inno Setup installer that runs on both architectures. Bundle formats
//! (VST3, AAX) carry both archs in architecture-scoped subdirectories inside
//! the bundle and let the host pick at load time; single-file formats (CLAP,
//! VST2) use Inno Setup `Check:` directives to install the matching DLL for
//! the installing machine. Pass `--host-only` to skip the cross-arch build
//! for faster dev iteration (or use `--universal` explicitly as a no-op).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install_scope::{note_once, PkgScope};
use crate::{
    build_aax_template, cargo_build, detect_default_features, load_config, project_root,
    read_workspace_version, release_lib_for_target, resolve_aax_sdk_path, rustup_has_target,
    tag_info, tag_ok, tag_warn, tmp_dir, Config, PkgFormat, PluginDef, Res, WindowsSigningConfig,
};

// ---------------------------------------------------------------------------
// Target architectures
// ---------------------------------------------------------------------------

/// Windows CPU architecture we can build for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetArch {
    X64,
    Arm64,
}

impl TargetArch {
    /// Architecture of the host running `cargo truce package`. Currently x64
    /// always — we don't have arm64 Windows as a supported host yet. Used to
    /// decide which archs can ship AAX (AAX template only builds for the host).
    fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            TargetArch::Arm64
        } else {
            TargetArch::X64
        }
    }

    /// Rust target triple passed to `cargo build --target`.
    fn triple(self) -> &'static str {
        match self {
            TargetArch::X64 => "x86_64-pc-windows-msvc",
            TargetArch::Arm64 => "aarch64-pc-windows-msvc",
        }
    }

    /// Short tag used in staging paths (`target/package/windows/{bundle_id}/clap/{tag}/…`).
    fn tag(self) -> &'static str {
        match self {
            TargetArch::X64 => "x64",
            TargetArch::Arm64 => "arm64",
        }
    }

    /// Arch sub-directory name inside a VST3 bundle (e.g. `Contents/x86_64-win/`).
    /// Steinberg defined `x86_64-win` and `arm64-win` for VST3 bundles on Windows.
    fn vst3_bundle_subdir(self) -> &'static str {
        match self {
            TargetArch::X64 => "x86_64-win",
            TargetArch::Arm64 => "arm64-win",
        }
    }

    /// Arch sub-directory name inside an AAX bundle (e.g. `Contents/x64/`).
    fn aax_bundle_subdir(self) -> &'static str {
        match self {
            TargetArch::X64 => "x64",
            TargetArch::Arm64 => "arm64",
        }
    }

    /// Inno Setup `Check:` predicate to guard this arch's `[Files]` entries.
    /// Returns the Pascal expression that should be true when the arch
    /// matches the machine running the installer.
    fn iss_check(self) -> &'static str {
        match self {
            TargetArch::X64 => "not IsArm64",
            TargetArch::Arm64 => "IsArm64",
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) fn cmd_package(args: &[String]) -> Res {
    let opts = parse_args(args)?;

    let config = load_config()?;
    let root = project_root();
    let version = read_workspace_version(&root).unwrap_or_else(|| "0.0.0".to_string());

    let formats = resolve_formats(&config, opts.format_str.as_deref())?;
    let plugins = resolve_plugins(&config, opts.plugin_filter.as_deref())?;
    let archs = opts.archs();
    let universal = archs.len() > 1;

    // Scope resolution: CLI > truce.toml [packaging] preferred_scope >
    // OS default (`--ask`).
    let scope = resolve_pkg_scope(opts.cli_scope, &config)?;
    eprintln!("Package scope: {}", scope.label());

    // System-only formats (AAX, VST2 on Windows) stay in the package
    // even under `--user`. The note tells the developer the end
    // user will see a UAC prompt for those. The `.iss` template
    // routes CLAP / VST3 to user paths and AAX / VST2 to system
    // paths in that mode (and bumps `PrivilegesRequired` to admin
    // so the installer can write to `{commoncf}` / `{commonpf}`).
    if matches!(scope, PkgScope::User) {
        for f in &formats {
            match f {
                PkgFormat::Aax => note_once(
                    "AAX is system-only; --user package keeps AAX but installs it to \
                     %COMMONPROGRAMFILES%\\Avid (end user will see one UAC prompt).",
                ),
                PkgFormat::Vst2 => note_once(
                    "VST2 on Windows is system-only; --user package keeps VST2 but installs \
                     it to %PROGRAMFILES%\\Steinberg\\VstPlugins (end user will see one UAC prompt).",
                ),
                _ => {}
            }
        }
    }

    if universal && formats.iter().any(|f| matches!(f, PkgFormat::Aax)) {
        eprintln!(
            "NOTE: AAX is host-arch-only ({}); the universal installer won't \
             carry an ARM64 AAX bundle. Avid's AAX SDK 2.9 ships x64 libs only, \
             and our template build (vcvars64 + MSVC) is x64-only. CLAP/VST2/VST3 \
             ship universally; AAX stays single-arch.",
            TargetArch::host().tag(),
        );
    }

    // Warn about missing signing credentials unless --no-sign was passed.
    if !opts.no_sign && !config.windows.signing.is_configured() {
        eprintln!(
            "WARNING: [windows.signing] has no credentials configured. Binaries and \
             installer will be unsigned. Pass --no-sign to silence this warning, or \
             set azure_account / sha1 / pfx_path under [windows.signing] in truce.toml."
        );
    }

    build_all_formats(&plugins, &formats, &archs, &root)?;

    let dist_dir = crate::target_dir(&root).join("dist");
    fs::create_dir_all(&dist_dir)?;

    for p in &plugins {
        eprintln!("\n=== Packaging: {} ({}) ===", p.name, archs_label(&archs));

        let staging = crate::target_dir(&root)
            .join("package/windows")
            .join(&p.bundle_id);
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        let mut all_signable: Vec<PathBuf> = Vec::new();
        for &arch in &archs {
            let staged = stage_plugin(&root, p, &config, &formats, &staging, arch)?;
            all_signable.extend(staged.signable);
        }

        if !opts.no_sign {
            // PACE-sign every AAX bundle (one per arch). PACE wraps the binary;
            // Authenticode signs the wrapped result — so PACE first.
            // `--no-pace-sign` (or `--no-sign`) skips the wraptool round-trip
            // while keeping Authenticode for smoke tests.
            if !opts.no_pace_sign && formats.iter().any(|f| matches!(f, PkgFormat::Aax)) {
                let aax_bundle = staging.join(format!("{}.aaxplugin", p.name));
                for &arch in &archs {
                    let inner_wrapper = aax_bundle
                        .join("Contents")
                        .join(arch.aax_bundle_subdir())
                        .join(format!("{}.aaxplugin", p.name));
                    if inner_wrapper.exists() {
                        pace_sign_aax(&inner_wrapper)?;
                    }
                }
            }
            sign_files(&all_signable, &config.windows.signing)?;
        }

        if opts.no_installer {
            eprintln!(
                "  Skipped installer build (--no-installer). Staging at {}",
                staging.display()
            );
            continue;
        }

        let iss = render_iss(
            &config, p, &formats, &archs, &staging, &version, &dist_dir, scope,
        );
        let iss_path = staging.join("installer.iss");
        fs::write(&iss_path, &iss)?;
        run_iscc(&iss_path)?;

        let installer = dist_dir.join(format!(
            "{}-{}-windows{}.exe",
            p.name,
            version,
            scope.dist_suffix()
        ));
        if !installer.exists() {
            return Err(format!(
                "ISCC reported success but installer is missing: {}",
                installer.display()
            )
            .into());
        }

        if !opts.no_sign {
            sign_files(std::slice::from_ref(&installer), &config.windows.signing)?;
        }
        eprintln!("  Installer: {}", installer.display());
    }

    eprintln!("\nDone. Installers in {}", dist_dir.display());
    Ok(())
}

fn archs_label(archs: &[TargetArch]) -> String {
    archs.iter().map(|a| a.tag()).collect::<Vec<_>>().join("+")
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Opts {
    plugin_filter: Option<String>,
    format_str: Option<String>,
    no_sign: bool,
    /// Skip just PACE — Authenticode still runs. Useful for dev iteration when
    /// the slow PACE round-trip isn't needed but we still want a signed
    /// installer for smoke testing. `--no-sign` implies this.
    no_pace_sign: bool,
    no_installer: bool,
    /// Build only the host arch. Default is universal (x64 + ARM64) so a
    /// single `cargo truce package` run produces the release artefact users
    /// expect; `--host-only` opts out for dev iteration speed.
    host_only: bool,
    /// Install scope the resulting installer targets. `--ask` (the
    /// default) lets the end user pick at install time via Inno
    /// Setup's "Choose installation mode" page; `--user` /
    /// `--system` hard-lock to one mode.
    cli_scope: Option<PkgScope>,
}

impl Opts {
    fn archs(&self) -> Vec<TargetArch> {
        if self.host_only {
            vec![TargetArch::host()]
        } else {
            vec![TargetArch::X64, TargetArch::Arm64]
        }
    }
}

fn set_cli_scope(slot: &mut Option<PkgScope>, want: PkgScope) -> Res {
    if let Some(prev) = *slot {
        if prev != want {
            return Err("--user, --system, and --ask are mutually exclusive".into());
        }
    }
    *slot = Some(want);
    Ok(())
}

fn resolve_pkg_scope(cli: Option<PkgScope>, config: &Config) -> Result<PkgScope, crate::BoxErr> {
    if let Some(s) = cli {
        return Ok(s);
    }
    if let Some(ref raw) = config.packaging.preferred_scope {
        return PkgScope::parse_toml_value(raw).map_err(|e| -> crate::BoxErr { e.into() });
    }
    Ok(PkgScope::os_default())
}

fn parse_args(args: &[String]) -> std::result::Result<Opts, crate::BoxErr> {
    let mut opts = Opts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                opts.plugin_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin crate name")?,
                );
            }
            "--formats" => {
                i += 1;
                opts.format_str = Some(args.get(i).cloned().ok_or("--formats requires a value")?);
            }
            "--no-sign" => opts.no_sign = true,
            "--no-pace-sign" => opts.no_pace_sign = true,
            "--no-installer" => opts.no_installer = true,
            "--user" => set_cli_scope(&mut opts.cli_scope, PkgScope::User)?,
            "--system" => set_cli_scope(&mut opts.cli_scope, PkgScope::System)?,
            "--ask" => set_cli_scope(&mut opts.cli_scope, PkgScope::Ask)?,
            // Universal is the default; accepted explicitly as a no-op so
            // existing CI scripts (and cross-platform invocations) keep working.
            "--universal" => {}
            "--host-only" => opts.host_only = true,
            // --no-notarize is a macOS concept; accept and ignore on Windows so
            // cross-platform CI scripts don't break.
            "--no-notarize" => {}
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }
    Ok(opts)
}

fn resolve_formats(
    config: &Config,
    format_str: Option<&str>,
) -> std::result::Result<Vec<PkgFormat>, crate::BoxErr> {
    let raw = if let Some(s) = format_str {
        PkgFormat::parse_list(s)?
    } else if !config.packaging.formats.is_empty() {
        PkgFormat::parse_list(&config.packaging.formats.join(","))?
    } else {
        let available: HashSet<String> = detect_default_features();
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
        if available.contains("aax") {
            fmts.push(PkgFormat::Aax);
        }
        fmts
    };

    // AU v2 / v3 are macOS-only. Drop silently: we don't want cross-platform
    // truce.toml files to error on the Windows runner just because they list
    // au2/au3 for macOS.
    let filtered: Vec<PkgFormat> = raw
        .into_iter()
        .filter(|f| !matches!(f, PkgFormat::Au2 | PkgFormat::Au3))
        .collect();

    if filtered.is_empty() {
        return Err("no Windows-eligible formats selected (AU is macOS-only)".into());
    }
    Ok(filtered)
}

fn resolve_plugins<'a>(
    config: &'a Config,
    filter: Option<&str>,
) -> std::result::Result<Vec<&'a PluginDef>, crate::BoxErr> {
    Ok(if let Some(filter) = filter {
        let matched: Vec<&PluginDef> = config
            .plugin
            .iter()
            .filter(|p| p.crate_name == filter)
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
    })
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Run the cargo builds for each selected format × arch. Mirrors cmd_package
/// on macOS: one `cargo build` per format with distinct `--features` so the
/// format-specific code paths can't cross-contaminate, plus an outer loop
/// over architectures.
///
/// Within a single arch the dylib at `target/{triple}/release/{stem}.dll` is
/// overwritten by successive format builds, so we save per-format copies
/// (`{stem}_clap`, `{stem}_vst3`, `{stem}_vst2`, `{stem}_aax`) after each
/// build. Archs have separate `target/{triple}/` directories so they don't
/// clash with each other.
fn build_all_formats(
    plugins: &[&PluginDef],
    formats: &[PkgFormat],
    archs: &[TargetArch],
    root: &Path,
) -> Res {
    let dt = ""; // MACOSX_DEPLOYMENT_TARGET is ignored on Windows

    let has_clap = formats.iter().any(|f| matches!(f, PkgFormat::Clap));
    let has_vst3 = formats.iter().any(|f| matches!(f, PkgFormat::Vst3));
    let has_vst2 = formats.iter().any(|f| matches!(f, PkgFormat::Vst2));
    let has_aax = formats.iter().any(|f| matches!(f, PkgFormat::Aax));

    for &arch in archs {
        eprintln!("--- Building for {} ---", arch.tag());
        let triple = arch.triple();

        if has_clap {
            eprintln!("Building CLAP ({})...", arch.tag());
            let mut build_args: Vec<String> = vec!["--target".into(), triple.into()];
            for p in plugins {
                build_args.push("-p".into());
                build_args.push(p.crate_name.clone());
            }
            build_args.extend_from_slice(&[
                "--no-default-features".into(),
                "--features".into(),
                "clap".into(),
            ]);
            let arg_refs: Vec<&str> = build_args.iter().map(|s| s.as_str()).collect();
            cargo_build(&[], &arg_refs, dt)?;
            for p in plugins {
                let src = release_lib_for_target(root, &p.dylib_stem(), Some(triple));
                let saved =
                    release_lib_for_target(root, &format!("{}_clap", p.dylib_stem()), Some(triple));
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }

        if has_vst3 {
            eprintln!("Building VST3 ({})...", arch.tag());
            let mut build_args: Vec<String> = vec!["--target".into(), triple.into()];
            for p in plugins {
                build_args.push("-p".into());
                build_args.push(p.crate_name.clone());
            }
            build_args.extend_from_slice(&[
                "--no-default-features".into(),
                "--features".into(),
                "vst3".into(),
            ]);
            let arg_refs: Vec<&str> = build_args.iter().map(|s| s.as_str()).collect();
            cargo_build(&[], &arg_refs, dt)?;
            for p in plugins {
                let src = release_lib_for_target(root, &p.dylib_stem(), Some(triple));
                let saved =
                    release_lib_for_target(root, &format!("{}_vst3", p.dylib_stem()), Some(triple));
                if src.exists() {
                    fs::copy(&src, &saved)?;
                }
            }
        }

        if has_vst2 {
            eprintln!("Building VST2 ({})...", arch.tag());
            let mut build_args: Vec<String> = vec!["--target".into(), triple.into()];
            for p in plugins {
                build_args.push("-p".into());
                build_args.push(p.crate_name.clone());
            }
            build_args.extend_from_slice(&[
                "--no-default-features".into(),
                "--features".into(),
                "vst2".into(),
            ]);
            let arg_refs: Vec<&str> = build_args.iter().map(|s| s.as_str()).collect();
            cargo_build(&[], &arg_refs, dt)?;
            for p in plugins {
                let src = release_lib_for_target(root, &p.dylib_stem(), Some(triple));
                let dst =
                    release_lib_for_target(root, &format!("{}_vst2", p.dylib_stem()), Some(triple));
                fs::copy(&src, &dst)?;
            }
        }

        // AAX staging is host-arch-only (see stage_aax), so only build the
        // AAX Rust cdylib for the host arch. The Rust code itself cross-
        // compiles fine — we're just avoiding orphan binaries that would
        // have nothing to pair with in the installer.
        if has_aax && arch == TargetArch::host() {
            eprintln!("Building AAX ({})...", arch.tag());
            let mut build_args: Vec<String> = vec!["--target".into(), triple.into()];
            for p in plugins {
                build_args.push("-p".into());
                build_args.push(p.crate_name.clone());
            }
            build_args.extend_from_slice(&[
                "--no-default-features".into(),
                "--features".into(),
                "aax".into(),
            ]);
            let arg_refs: Vec<&str> = build_args.iter().map(|s| s.as_str()).collect();
            cargo_build(&[], &arg_refs, dt)?;
            for p in plugins {
                let src = release_lib_for_target(root, &p.dylib_stem(), Some(triple));
                let dst =
                    release_lib_for_target(root, &format!("{}_aax", p.dylib_stem()), Some(triple));
                fs::copy(&src, &dst)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

struct StagedPlugin {
    /// Files to feed signtool for one arch's staging pass.
    signable: Vec<PathBuf>,
}

/// Stage a single plugin for one architecture. Multi-arch packaging calls
/// this once per arch; the bundle formats (VST3, AAX) accumulate arch-scoped
/// subdirectories in the same bundle root across calls.
fn stage_plugin(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    formats: &[PkgFormat],
    staging: &Path,
    arch: TargetArch,
) -> std::result::Result<StagedPlugin, crate::BoxErr> {
    let mut signable = Vec::new();
    for fmt in formats {
        eprint!("  Staging {} ({})... ", fmt.label(), arch.tag());
        match fmt {
            PkgFormat::Clap => {
                signable.push(stage_clap(root, p, staging, arch)?);
            }
            PkgFormat::Vst3 => {
                signable.push(stage_vst3(root, p, staging, arch)?);
            }
            PkgFormat::Vst2 => {
                signable.push(stage_vst2(root, p, staging, arch)?);
            }
            PkgFormat::Aax => match stage_aax(root, p, config, staging, arch)? {
                Some((wrapper, dylib)) => {
                    signable.push(dylib);
                    signable.push(wrapper);
                }
                None => {
                    eprintln!("skipped (AAX template is built for host arch only)");
                    continue;
                }
            },
            PkgFormat::Au2 | PkgFormat::Au3 => {
                return Err("AU is macOS-only; should have been filtered".into());
            }
        }
        eprintln!("ok");
    }
    Ok(StagedPlugin { signable })
}

fn stage_clap(
    root: &Path,
    p: &PluginDef,
    staging: &Path,
    arch: TargetArch,
) -> std::result::Result<PathBuf, crate::BoxErr> {
    let dll = release_lib_for_target(
        root,
        &format!("{}_clap", p.dylib_stem()),
        Some(arch.triple()),
    );
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let dst_dir = staging.join("clap").join(arch.tag());
    fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join(format!("{}.clap", p.name));
    fs::copy(&dll, &dst)?;
    Ok(dst)
}

fn stage_vst3(
    root: &Path,
    p: &PluginDef,
    staging: &Path,
    arch: TargetArch,
) -> std::result::Result<PathBuf, crate::BoxErr> {
    // VST3 on Windows is a bundle directory. Multi-arch bundles carry both
    // arch subdirs side-by-side — the host picks at load time.
    let dll = release_lib_for_target(
        root,
        &format!("{}_vst3", p.dylib_stem()),
        Some(arch.triple()),
    );
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let bundle_root = staging.join("vst3");
    let bundle = bundle_root.join(format!("{}.vst3", p.name));
    let arch_dir = bundle.join("Contents").join(arch.vst3_bundle_subdir());
    fs::create_dir_all(&arch_dir)?;
    let inner = arch_dir.join(format!("{}.vst3", p.name));
    fs::copy(&dll, &inner)?;
    Ok(inner)
}

fn stage_vst2(
    root: &Path,
    p: &PluginDef,
    staging: &Path,
    arch: TargetArch,
) -> std::result::Result<PathBuf, crate::BoxErr> {
    let dll = release_lib_for_target(
        root,
        &format!("{}_vst2", p.dylib_stem()),
        Some(arch.triple()),
    );
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let dst_dir = staging.join("vst2").join(arch.tag());
    fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join(format!("{}.dll", p.name));
    fs::copy(&dll, &dst)?;
    Ok(dst)
}

/// Build/stage the AAX bundle for one architecture. Returns
/// `Some((wrapper_binary, resources_dylib))` on success so both get
/// Authenticode-signed, or `None` when the arch can't be staged (today,
/// anything that isn't the host arch — see below).
///
/// For universal builds the host-arch pass writes under
/// `{Name}.aaxplugin/Contents/{x64,arm64}/` + `Contents/Resources/`.
///
/// ### Cross-arch AAX is intentionally skipped
///
/// The AAX template (`TruceAAXTemplate.aaxplugin`) is a C++ bundle that
/// links against Avid's AAX SDK libraries. Our `build_aax_template()` runs
/// cmake + MSVC via `vcvars64.bat`, which produces an x64 binary. To
/// produce an ARM64 template we'd need both:
///
/// 1. A cross-compile path via `vcvars_arm64.bat` / `vcvarsx86_arm64.bat`.
/// 2. ARM64 `AAX_SDK_Interface.lib` / `AAXLibrary.lib` from Avid. As of
///    AAX SDK 2.9 Avid ships x64 libs only — attempting to link arm64
///    objects against the x64 libs will fail at link time.
///
/// Rather than silently shipping an x64 template inside the arm64 bundle
/// subdir (which would fail to load at runtime), we skip AAX staging for
/// non-host archs and warn. CLAP/VST2/VST3 still ship universally; AAX
/// stays host-arch-only.
fn stage_aax(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    arch: TargetArch,
) -> std::result::Result<Option<(PathBuf, PathBuf)>, crate::BoxErr> {
    if arch != TargetArch::host() {
        return Ok(None);
    }

    // Build the template .aaxplugin wrapper if it isn't there yet.
    let template = tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin");
    if !template.exists() {
        if let Some(sdk_path) = resolve_aax_sdk_path(config) {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
            // On Windows, AAX stays host-arch regardless (SDK 2.9 ships x64
            // libs only — see stage_aax comments). `universal_mac` is a no-op.
            build_aax_template(root, &sdk_path, false)?;
        } else {
            return Err(
                "AAX SDK not configured. Set [windows].aax_sdk_path in truce.toml or \
                 AAX_SDK_PATH env var."
                    .into(),
            );
        }
    }
    if !template.exists() {
        return Err("AAX template build succeeded but binary not found".into());
    }

    let dylib = release_lib_for_target(
        root,
        &format!("{}_aax", p.dylib_stem()),
        Some(arch.triple()),
    );
    if !dylib.exists() {
        return Err(format!(
            "Missing AAX Rust cdylib for {}: {}",
            arch.tag(),
            dylib.display()
        )
        .into());
    }

    let bundle_root = staging.join("aax");
    let bundle = bundle_root.join(format!("{}.aaxplugin", p.name));
    let contents = bundle.join("Contents");
    let arch_dir = contents.join(arch.aax_bundle_subdir());
    let resources_dir = contents.join("Resources");
    fs::create_dir_all(&arch_dir)?;
    fs::create_dir_all(&resources_dir)?;

    let wrapper = arch_dir.join(format!("{}.aaxplugin", p.name));
    // Arch-tagged dylib so multi-arch bundles don't collide in Resources/.
    // The bridge C++ code scans Resources/*.dll via FindFirstFileA and loads
    // the first one whose arch matches the current process — arch tagging
    // in the filename is purely for storage; the binary's own arch header
    // determines what LoadLibrary accepts.
    let resource_dll = resources_dir.join(format!("{}_aax_{}.dll", p.dylib_stem(), arch.tag()));
    fs::copy(&template, &wrapper)?;
    fs::copy(&dylib, &resource_dll)?;

    Ok(Some((wrapper, resource_dll)))
}

// ---------------------------------------------------------------------------
// Authenticode signing (signtool.exe)
// ---------------------------------------------------------------------------

fn sign_files(files: &[PathBuf], config: &WindowsSigningConfig) -> Res {
    if files.is_empty() {
        return Ok(());
    }
    if !config.is_configured() {
        // No creds — emit a single notice and carry on. The warning at the top
        // of cmd_package already covered the "why."
        return Ok(());
    }
    let signtool = locate_signtool()
        .ok_or("signtool.exe not found on PATH. Install the Windows 10 SDK or Windows 11 SDK.")?;

    let mut args: Vec<String> = vec![
        "sign".into(),
        "/fd".into(),
        "SHA256".into(),
        "/tr".into(),
        config.resolved_timestamp_url().to_string(),
        "/td".into(),
        "SHA256".into(),
    ];

    // Credential source — Azure wins, then thumbprint, then pfx.
    if let (Some(account), Some(profile)) = (&config.azure_account, &config.azure_profile) {
        let dlib = config.azure_dlib.clone().unwrap_or_else(default_azure_dlib);
        let metadata_path = tmp_dir().join("truce_azure_signing_metadata.json");
        let metadata = format!(
            r#"{{
  "Endpoint": "https://eus.codesigning.azure.net/",
  "CodeSigningAccountName": "{account}",
  "CertificateProfileName": "{profile}"
}}"#,
            account = account,
            profile = profile,
        );
        fs::write(&metadata_path, metadata)?;
        args.extend_from_slice(&[
            "/dlib".into(),
            dlib,
            "/dmdf".into(),
            metadata_path.display().to_string(),
        ]);
    } else if let Some(sha1) = &config.sha1 {
        args.extend_from_slice(&["/sha1".into(), sha1.clone()]);
        if let Some(store) = &config.cert_store {
            args.extend_from_slice(&["/s".into(), store.clone()]);
        }
    } else if let Some(pfx) = &config.pfx_path {
        args.extend_from_slice(&["/f".into(), pfx.clone()]);
        if let Ok(pw) = std::env::var("TRUCE_PFX_PASSWORD") {
            args.extend_from_slice(&["/p".into(), pw]);
        }
    }

    for f in files {
        args.push(f.display().to_string());
    }

    eprintln!("  signtool: signing {} file(s)", files.len());
    let status = Command::new(&signtool).args(&args).status()?;
    if !status.success() {
        return Err("signtool failed".into());
    }
    Ok(())
}

fn default_azure_dlib() -> String {
    r"C:\Program Files\Microsoft Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll"
        .to_string()
}

fn locate_signtool() -> Option<PathBuf> {
    if let Ok(p) = which("signtool.exe") {
        return Some(p);
    }
    let candidates = [r"C:\Program Files (x86)\Windows Kits\10\bin\x64\signtool.exe"];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    let sdk_bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(&sdk_bin) {
        let mut best: Option<PathBuf> = None;
        for e in entries.flatten() {
            let candidate = e.path().join(r"x64\signtool.exe");
            if candidate.exists() {
                match &best {
                    None => best = Some(candidate),
                    Some(current) => {
                        if candidate > *current {
                            best = Some(candidate);
                        }
                    }
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

pub(crate) fn locate_iscc() -> Option<PathBuf> {
    if let Ok(p) = which("ISCC.exe") {
        return Some(p);
    }
    for c in [
        r"C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        r"C:\Program Files\Inno Setup 6\ISCC.exe",
    ] {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub(crate) fn locate_wraptool() -> Option<PathBuf> {
    if let Ok(p) = which("wraptool.exe") {
        return Some(p);
    }
    None
}

fn which(name: &str) -> Result<PathBuf, std::io::Error> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PATH not set"))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        name.to_string(),
    ))
}

// ---------------------------------------------------------------------------
// PACE / iLok signing (AAX)
// ---------------------------------------------------------------------------

fn pace_sign_aax(bundle: &Path) -> Res {
    let Some(wraptool) = locate_wraptool() else {
        eprintln!(
            "  wraptool.exe not found — AAX bundle is unsigned for PACE. \
             Pro Tools Developer will still load it; release builds need PACE."
        );
        return Ok(());
    };
    let pace_account = match std::env::var("PACE_ACCOUNT") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "  PACE_ACCOUNT env var not set — skipping PACE signing. \
                 Pro Tools Developer will still load the bundle."
            );
            return Ok(());
        }
    };
    let pace_signid = match std::env::var("PACE_SIGN_ID") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("  PACE_SIGN_ID env var not set — skipping PACE signing.");
            return Ok(());
        }
    };

    eprintln!("  wraptool: PACE-signing {}", bundle.display());
    let status = Command::new(&wraptool)
        .args([
            "sign",
            "--account",
            &pace_account,
            "--signid",
            &pace_signid,
            "--allowsigningservice",
            "--in",
            bundle.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
        ])
        .status()?;
    if !status.success() {
        return Err("wraptool failed".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inno Setup
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_iss(
    config: &Config,
    p: &PluginDef,
    formats: &[PkgFormat],
    archs: &[TargetArch],
    staging: &Path,
    version: &str,
    dist_dir: &Path,
    scope: PkgScope,
) -> String {
    let publisher = config
        .windows
        .packaging
        .publisher
        .as_deref()
        .unwrap_or(&config.vendor.name);
    let publisher_url = config
        .windows
        .packaging
        .publisher_url
        .clone()
        .or_else(|| config.vendor.url.clone())
        .unwrap_or_default();

    let app_id = config
        .windows
        .packaging
        .app_id
        .clone()
        .unwrap_or_else(|| format!("{}.{}", config.vendor.id, p.bundle_id));

    let root = project_root();

    let installer_icon = config
        .windows
        .packaging
        .installer_icon
        .as_ref()
        .map(|s| root.join(s))
        .filter(|p| p.exists());
    let welcome_bmp = config
        .windows
        .packaging
        .welcome_bmp
        .as_ref()
        .map(|s| root.join(s))
        .filter(|p| p.exists());
    let license_rtf = config
        .windows
        .packaging
        .license_rtf
        .as_ref()
        .map(|s| root.join(s))
        .filter(|p| p.exists());

    let universal = archs.len() > 1;

    let mut setup = String::new();
    setup.push_str("[Setup]\r\n");
    setup.push_str(&format!("AppId={{{{{}}}}}\r\n", iss_escape(&app_id)));
    setup.push_str(&format!("AppName={}\r\n", iss_escape(&p.name)));
    setup.push_str(&format!("AppVersion={}\r\n", iss_escape(version)));
    setup.push_str(&format!("AppPublisher={}\r\n", iss_escape(publisher)));
    if !publisher_url.is_empty() {
        setup.push_str(&format!(
            "AppPublisherURL={}\r\n",
            iss_escape(&publisher_url)
        ));
    }
    // `{autopf}` resolves to `{commonpf}` in admin install mode and
    // `{userpf}` (`%LOCALAPPDATA%\Programs`) in non-admin mode, so the
    // AppId-tracked install dir lands somewhere the installer can write
    // without elevation when the end user picks "for me only". `--system`
    // hard-locks to admin so `{commonpf}` is fine.
    let pf_const = match scope {
        PkgScope::System => "{commonpf}",
        PkgScope::User | PkgScope::Ask => "{autopf}",
    };
    setup.push_str(&format!(
        "DefaultDirName={}\\{}\\{}\r\n",
        pf_const,
        iss_escape(publisher),
        iss_escape(&p.name),
    ));
    setup.push_str("DisableDirPage=yes\r\n");
    setup.push_str(&format!("OutputDir={}\r\n", iss_escape_path(dist_dir)));
    setup.push_str(&format!(
        "OutputBaseFilename={}-{}-windows\r\n",
        iss_escape(&p.name),
        iss_escape(version),
    ));
    setup.push_str("Compression=lzma2\r\n");
    setup.push_str("SolidCompression=yes\r\n");
    if universal {
        // x64compatible includes both x64 and ARM64 hosts; that's what we want
        // for a universal installer. Inno Setup 6.3+ exposes IsArm64 so [Files]
        // entries can split on arch.
        setup.push_str("ArchitecturesInstallIn64BitMode=x64compatible\r\n");
        setup.push_str("ArchitecturesAllowed=x64compatible\r\n");
    } else {
        // Single-arch x64 installer. Explicitly rule out ARM64 so the installer
        // doesn't run on machines where none of its binaries would work.
        setup.push_str("ArchitecturesInstallIn64BitMode=x64compatible\r\n");
        setup.push_str("ArchitecturesAllowed=x64compatible and not arm64\r\n");
    }
    // PrivilegesRequired drives whether the installer relaunches
    // itself elevated before the wizard even appears.
    //   --user   → `lowest` if no system-only payloads (AAX, VST2);
    //              `admin` otherwise — AAX / VST2 still need to write
    //              under %COMMONPROGRAMFILES%/%PROGRAMFILES% so the
    //              whole installer escalates once for them while
    //              CLAP / VST3 still target user paths.
    //   --system → `admin`. UAC on launch, lands under system paths.
    //   --ask    → `admin` + `PrivilegesRequiredOverridesAllowed=...`
    //              shows the "Choose installation mode" page and
    //              relaunches elevated only if the user picks all-users.
    let has_system_only_format = formats
        .iter()
        .any(|f| matches!(f, PkgFormat::Aax | PkgFormat::Vst2));
    match scope {
        PkgScope::User if has_system_only_format => {
            setup.push_str("PrivilegesRequired=admin\r\n");
            // Mixing admin elevation with `{usercf}` (per-user CLAP/VST3
            // dest) is intentional here — the elevation exists to host
            // the AAX/VST2 payloads, not to upgrade CLAP/VST3 to common
            // areas. Suppress ISCC's UsedUserAreasWarning so the
            // intentional mix doesn't read as a script bug.
            setup.push_str("UsedUserAreasWarning=no\r\n");
        }
        PkgScope::User => {
            setup.push_str("PrivilegesRequired=lowest\r\n");
        }
        PkgScope::System => {
            setup.push_str("PrivilegesRequired=admin\r\n");
        }
        PkgScope::Ask => {
            setup.push_str("PrivilegesRequired=admin\r\n");
            setup.push_str("PrivilegesRequiredOverridesAllowed=commandline dialog\r\n");
            // `{auto*}` constants resolve per the runtime install mode,
            // but ISCC's static check still flags the admin-default +
            // user-area combination. The override is what makes this safe.
            setup.push_str("UsedUserAreasWarning=no\r\n");
        }
    }
    setup.push_str("WizardStyle=modern\r\n");
    setup.push_str("UninstallDisplayName=");
    setup.push_str(&iss_escape(&p.name));
    setup.push_str("\r\n");
    if let Some(icon) = &installer_icon {
        setup.push_str(&format!("SetupIconFile={}\r\n", iss_escape_path(icon)));
    }
    if let Some(bmp) = &welcome_bmp {
        setup.push_str(&format!("WizardImageFile={}\r\n", iss_escape_path(bmp)));
    }
    if let Some(rtf) = &license_rtf {
        setup.push_str(&format!("LicenseFile={}\r\n", iss_escape_path(rtf)));
    }
    setup.push_str("\r\n");

    // [Types] — full vs custom
    setup.push_str("[Types]\r\n");
    setup.push_str("Name: \"full\"; Description: \"Full installation\"\r\n");
    setup.push_str(
        "Name: \"custom\"; Description: \"Custom installation\"; Flags: iscustom\r\n\r\n",
    );

    // [Components] — one per format. ExtraDiskSpaceRequired drives the size
    // shown on the Components wizard page: Inno Setup excludes [Files] entries
    // with a `Check:` from that auto-sum, so without this only VST3 (the one
    // unconditional format) would display a size. We compute the install
    // footprint per component from staging and emit it explicitly.
    setup.push_str("[Components]\r\n");
    for fmt in formats {
        let (name, desc, types) = iss_component_spec(fmt);
        let size = component_install_size(fmt, p, staging, archs, universal, scope);
        setup.push_str(&format!(
            "Name: \"{}\"; Description: \"{}\"; Types: {}; ExtraDiskSpaceRequired: {}\r\n",
            name, desc, types, size
        ));
    }
    setup.push_str("\r\n");

    // [Files] — one block per format × arch.
    setup.push_str("[Files]\r\n");
    for fmt in formats {
        for &arch in archs {
            let block = iss_files_block(fmt, p, staging, arch, universal, scope);
            setup.push_str(&block);
        }
    }
    setup.push_str("\r\n");

    // [UninstallDelete] — per-format (bundle dirs get wholesale cleanup).
    setup.push_str("[UninstallDelete]\r\n");
    for fmt in formats {
        for line in iss_uninstall_lines(fmt, &p.name, scope) {
            setup.push_str(&line);
            setup.push_str("\r\n");
        }
    }

    setup
}

/// Bytes to add via `ExtraDiskSpaceRequired` so the wizard's per-component
/// size column reflects the install footprint.
///
/// Inno's `[Components]` page auto-sums `[Files]` entries — but excludes any
/// entry carrying a `Check:` directive (since those may not install).
/// `ExtraDiskSpaceRequired` is then *added on top* of that auto-sum. So we
/// only emit a non-zero hint when every `[Files]` entry for the component is
/// `Check:`-gated; otherwise the auto-sum already covers it and a hint would
/// double-count (e.g. universal VST3 was reading ~3× actual install size).
///
/// For multi-arch components we report one arch's worth (max-of-arches). The
/// host loads only the matching arch, so reporting the sum makes the entry
/// look 2× larger than its peers for no reason a user cares about. AAX is
/// host-arch-only so its bundle already reflects a single arch.
fn component_install_size(
    fmt: &PkgFormat,
    p: &PluginDef,
    staging: &Path,
    archs: &[TargetArch],
    universal: bool,
    scope: PkgScope,
) -> u64 {
    if !files_all_check_gated(fmt, universal, scope) {
        // Inno's `[Files]` auto-sum already counts this component correctly.
        return 0;
    }
    match fmt {
        PkgFormat::Clap => archs
            .iter()
            .filter_map(|a| {
                let f = staging
                    .join("clap")
                    .join(a.tag())
                    .join(format!("{}.clap", p.name));
                fs::metadata(&f).ok().map(|m| m.len())
            })
            .max()
            .unwrap_or(0),
        PkgFormat::Vst2 => archs
            .iter()
            .filter_map(|a| {
                let f = staging
                    .join("vst2")
                    .join(a.tag())
                    .join(format!("{}.dll", p.name));
                fs::metadata(&f).ok().map(|m| m.len())
            })
            .max()
            .unwrap_or(0),
        PkgFormat::Vst3 => {
            let contents = staging
                .join("vst3")
                .join(format!("{}.vst3", p.name))
                .join("Contents");
            archs
                .iter()
                .map(|a| dir_size_recursive(&contents.join(a.vst3_bundle_subdir())))
                .max()
                .unwrap_or(0)
        }
        PkgFormat::Aax => {
            dir_size_recursive(&staging.join("aax").join(format!("{}.aaxplugin", p.name)))
        }
        PkgFormat::Au2 | PkgFormat::Au3 => 0,
    }
}

/// Whether every `[Files]` entry this component will emit carries a `Check:`
/// directive. Inno excludes such entries from the per-component size auto-sum
/// (since they may not install), so we have to supply the size ourselves via
/// `ExtraDiskSpaceRequired:`. When this returns `false` the auto-sum is
/// already correct and we must not emit a hint, or it will double-count.
///
/// Mirror of the `iss_files_block` / `iss_admin_only` logic — keep these in
/// sync if the gating policy changes.
fn files_all_check_gated(fmt: &PkgFormat, universal: bool, scope: PkgScope) -> bool {
    match fmt {
        // Single-file: arch-gated (`Check: not IsArm64` etc.) only when universal.
        PkgFormat::Clap => universal,
        // Bundle, but only the matching arch's sub-dir installs — same arch
        // gating as CLAP/VST2 when universal.
        PkgFormat::Vst3 => universal,
        // System-rooted; gated on `IsAdminInstallMode` in `--ask`, plus
        // arch-gated when universal. In `--system`/`--user` the iss_admin_only
        // emitter drops the IsAdminInstallMode check, so single-arch is bare.
        PkgFormat::Vst2 => universal || matches!(scope, PkgScope::Ask),
        // Host-arch only (single arch dir staged), so no per-arch Check.
        // Only gated in `--ask` (on IsAdminInstallMode).
        PkgFormat::Aax => matches!(scope, PkgScope::Ask),
        PkgFormat::Au2 | PkgFormat::Au3 => false,
    }
}

fn dir_size_recursive(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let p = entry.path();
        match fs::metadata(&p) {
            Ok(md) if md.is_dir() => total += dir_size_recursive(&p),
            Ok(md) => total += md.len(),
            Err(_) => {}
        }
    }
    total
}

fn iss_component_spec(fmt: &PkgFormat) -> (&'static str, &'static str, &'static str) {
    match fmt {
        PkgFormat::Clap => ("clap", "CLAP", "full"),
        PkgFormat::Vst3 => ("vst3", "VST3", "full"),
        PkgFormat::Vst2 => ("vst2", "VST2 (legacy)", "custom"),
        PkgFormat::Aax => ("aax", "AAX", "full"),
        PkgFormat::Au2 | PkgFormat::Au3 => unreachable!("AU is filtered out on Windows"),
    }
}

/// Build the `[Files]` entries for one format × arch. For single-file formats
/// (CLAP, VST2) we gate with a `Check:` directive so only the matching arch's
/// DLL is installed on a given machine. Bundle formats (VST3, AAX) install
/// both archs side-by-side; the host picks at load time.
///
/// Scope-driven destinations:
/// - `--system` pins to `{commoncf}` (admin-only install mode).
/// - `--user` pins to `{usercf}` (`%LOCALAPPDATA%\Programs\Common`), even
///   when the installer is running elevated to host AAX/VST2 alongside.
/// - `--ask` uses `{autocf}` so Inno picks per the runtime install mode.
///   AAX/VST2 stay system-rooted with `Check: IsAdminInstallMode` — end
///   users who pick "for me only" simply don't get AAX (per the
///   install-scope doc).
fn iss_files_block(
    fmt: &PkgFormat,
    p: &PluginDef,
    staging: &Path,
    arch: TargetArch,
    universal: bool,
    scope: PkgScope,
) -> String {
    // For single-arch installers the Check: directive is unnecessary — drop it
    // so the output .iss stays simple.
    let arch_check = if universal {
        Some(arch.iss_check())
    } else {
        None
    };

    match fmt {
        PkgFormat::Clap => {
            let src = staging
                .join("clap")
                .join(arch.tag())
                .join(format!("{}.clap", p.name));
            let src_quoted = iss_escape_path(&src);
            let dest = format!("{}\\CLAP", scoped_cf(scope));
            iss_dual_dest(
                &src_quoted,
                &dest,
                "clap",
                arch_check,
                /* is_dir= */ false,
            )
        }
        PkgFormat::Vst3 => {
            // Bundle: install only the matching arch's sub-directory. The
            // staging tree carries both, but we arch-gate at install time so
            // an ARM64 machine doesn't waste ~9MB on an x86_64-win sub-bundle
            // it won't load (x64 hosts running on ARM64 via emulation are not
            // a use case we're optimizing for here).
            let src_dir = staging
                .join("vst3")
                .join(format!("{}.vst3", p.name))
                .join("Contents")
                .join(arch.vst3_bundle_subdir());
            let src_glob = src_dir.join("*");
            let src_quoted = iss_escape_path(&src_glob);
            let name = iss_escape(&p.name);
            let subdir = arch.vst3_bundle_subdir();
            let dest = format!(
                "{}\\VST3\\{name}.vst3\\Contents\\{subdir}",
                scoped_cf(scope)
            );
            iss_dual_dest(
                &src_quoted,
                &dest,
                "vst3",
                arch_check,
                /* is_dir = */ true,
            )
        }
        PkgFormat::Vst2 => {
            // Windows VST2 has no settled per-user path; in `--ask` mode
            // the doc keeps it system-only with `Check: IsAdminInstallMode`,
            // so end users in for-me-only mode simply don't receive VST2.
            // `--user` already filtered it out before render_iss is called.
            let src = staging
                .join("vst2")
                .join(arch.tag())
                .join(format!("{}.dll", p.name));
            let src_quoted = iss_escape_path(&src);
            iss_admin_only(
                scope,
                &src_quoted,
                "{commonpf}\\Steinberg\\VstPlugins",
                "vst2",
                arch_check,
                /* is_dir = */ false,
            )
        }
        PkgFormat::Aax => {
            // AAX bundle: arch subdir + arch-tagged resource DLL. Non-host
            // arches are skipped at stage time (see stage_aax); if the arch
            // subdir doesn't exist in staging, don't emit an .iss reference
            // to it — ISCC would fail on a missing Source otherwise.
            let src_arch_dir = staging
                .join("aax")
                .join(format!("{}.aaxplugin", p.name))
                .join("Contents")
                .join(arch.aax_bundle_subdir());
            if !src_arch_dir.exists() {
                return String::new();
            }
            let src_arch_glob = src_arch_dir.join("*");
            let resource_dll = staging
                .join("aax")
                .join(format!("{}.aaxplugin", p.name))
                .join("Contents")
                .join("Resources")
                .join(format!("{}_aax_{}.dll", p.dylib_stem(), arch.tag()));
            let name = iss_escape(&p.name);
            let subdir = arch.aax_bundle_subdir();
            let bundle_root = format!("{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{name}.aaxplugin");
            let mut out = String::new();
            out.push_str(&iss_admin_only(
                scope,
                &iss_escape_path(&src_arch_glob),
                &format!("{bundle_root}\\Contents\\{subdir}"),
                "aax",
                /* arch_check = */ None,
                /* is_dir = */ true,
            ));
            out.push_str(&iss_admin_only(
                scope,
                &iss_escape_path(&resource_dll),
                &format!("{bundle_root}\\Contents\\Resources"),
                "aax",
                /* arch_check = */ None,
                /* is_dir = */ false,
            ));
            out
        }
        PkgFormat::Au2 | PkgFormat::Au3 => unreachable!(),
    }
}

/// Inno Setup "common files" constant for the requested scope.
/// `{autocf}` does the right thing under `--ask` (resolves per the
/// runtime install mode picked by the user); `--system` and `--user`
/// pin to fixed constants so the destination doesn't depend on whether
/// the installer happened to escalate for a sibling system-only format.
fn scoped_cf(scope: PkgScope) -> &'static str {
    match scope {
        PkgScope::System => "{commoncf}",
        PkgScope::User => "{usercf}",
        PkgScope::Ask => "{autocf}",
    }
}

/// Emit one `[Files]` line for a destination computed via `scoped_cf`.
/// One line covers all three scopes — Inno's `{auto*}` constants handle
/// the `--ask` branching for us, so we no longer need a pair of
/// `IsAdminInstallMode`-gated entries.
fn iss_dual_dest(
    src_quoted: &str,
    dest: &str,
    component: &str,
    arch_check: Option<&str>,
    is_dir: bool,
) -> String {
    let dir_flags = if is_dir {
        " recursesubdirs createallsubdirs"
    } else {
        ""
    };
    let arch = arch_check
        .map(|c| format!(" Check: {c};"))
        .unwrap_or_default();
    format!(
        "Source: \"{src_quoted}\"; DestDir: \"{dest}\"; \
         Components: {component};{arch} \
         Flags: ignoreversion overwritereadonly{dir_flags}\r\n"
    )
}

/// Emit the `[Files]` line for a payload that is always system-rooted
/// (AAX, Windows VST2). `--system` and `--user` (which has already
/// bumped `PrivilegesRequired` to admin in the caller) both copy
/// unconditionally; `--ask` gates on `IsAdminInstallMode` so end users
/// who pick "for me only" see CLAP/VST3 land in user paths and AAX /
/// VST2 simply skip.
fn iss_admin_only(
    scope: PkgScope,
    src_quoted: &str,
    system_dest: &str,
    component: &str,
    arch_check: Option<&str>,
    is_dir: bool,
) -> String {
    let dir_flags = if is_dir {
        " recursesubdirs createallsubdirs"
    } else {
        ""
    };
    let arch_clause = arch_check.map(|c| format!(" and {c}")).unwrap_or_default();
    match scope {
        PkgScope::System | PkgScope::User => {
            let arch = arch_check
                .map(|c| format!(" Check: {c};"))
                .unwrap_or_default();
            format!(
                "Source: \"{src_quoted}\"; DestDir: \"{system_dest}\"; \
                 Components: {component};{arch} \
                 Flags: ignoreversion overwritereadonly{dir_flags}\r\n"
            )
        }
        PkgScope::Ask => format!(
            "Source: \"{src_quoted}\"; DestDir: \"{system_dest}\"; \
             Components: {component}; Check: IsAdminInstallMode{arch_clause}; \
             Flags: ignoreversion overwritereadonly{dir_flags}\r\n"
        ),
    }
}

fn iss_uninstall_lines(fmt: &PkgFormat, plugin_name: &str, scope: PkgScope) -> Vec<String> {
    let name = iss_escape(plugin_name);
    match fmt {
        PkgFormat::Vst3 => {
            // Mirror the install destination: `{autocf}` under `--ask`
            // matches whichever mode the user picked at install time, so
            // the uninstall hits the same path the bundle was written to.
            let path = format!("{}\\VST3\\{name}.vst3", scoped_cf(scope));
            vec![format!(
                "Type: filesandordirs; Name: \"{path}\"; Components: vst3"
            )]
        }
        PkgFormat::Aax => {
            // AAX is system-rooted regardless of scope (`iss_admin_only`
            // installs to `{commoncf}\Avid\…` for both `--system` and
            // `--user`, and gates on `IsAdminInstallMode` under `--ask`).
            // One line covers every case the file actually lands.
            vec![format!(
                "Type: filesandordirs; Name: \"{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{name}.aaxplugin\"; Components: aax"
            )]
        }
        _ => Vec::new(),
    }
}

fn run_iscc(iss_path: &Path) -> Res {
    let iscc = locate_iscc().ok_or(
        "ISCC.exe not found. Install Inno Setup 6 from https://jrsoftware.org/isinfo.php \
         or pass --no-installer to skip installer generation.",
    )?;
    eprintln!("  iscc: {}", iss_path.display());
    let status = Command::new(&iscc).arg(iss_path).status()?;
    if !status.success() {
        return Err("ISCC.exe failed".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// .iss value escaping
// ---------------------------------------------------------------------------

fn iss_escape(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn iss_escape_path(p: &Path) -> String {
    let s = p.display().to_string();
    let s = s.replace('/', "\\");
    iss_escape(&s)
}

// ---------------------------------------------------------------------------
// Doctor hook
// ---------------------------------------------------------------------------

pub(crate) fn doctor() {
    match locate_iscc() {
        Some(p) => eprintln!(
            "    {} Inno Setup 6 (ISCC.exe) at {}",
            tag_ok(),
            p.display()
        ),
        None => eprintln!(
            "    {} ISCC.exe not found — install Inno Setup 6 to produce installers",
            tag_warn()
        ),
    }
    match locate_signtool() {
        Some(p) => eprintln!("    {} signtool.exe at {}", tag_ok(), p.display()),
        None => eprintln!(
            "    {} signtool.exe not found — install Windows 10/11 SDK for Authenticode",
            tag_warn()
        ),
    }
    match locate_wraptool() {
        Some(p) => eprintln!("    {} wraptool.exe (PACE) at {}", tag_ok(), p.display()),
        None => eprintln!(
            "    {} wraptool.exe not found — only needed for signed AAX builds",
            tag_info()
        ),
    }

    // ARM64 readiness. Universal is the default, so missing ARM64 toolchain
    // downgrades to a warning (packages with `--host-only` still work).
    let has_rust_arm64 = rustup_has_target("aarch64-pc-windows-msvc");
    let has_msvc_arm64 = has_arm64_msvc_toolchain();
    match (has_rust_arm64, has_msvc_arm64) {
        (true, true) => eprintln!(
            "    {} ARM64 cross-compile available — `cargo truce package` will produce dual-arch installers by default",
            tag_ok()
        ),
        (true, false) => eprintln!(
            "    {} Rust has aarch64-pc-windows-msvc but VS is missing the ARM64 MSVC toolchain — C++ shims won't cross-compile. Install \"MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools\" via the VS Installer, or pass `--host-only` to skip ARM64.",
            tag_warn()
        ),
        (false, true) => eprintln!(
            "    {} VS has ARM64 MSVC but the Rust target isn't installed — run: rustup target add aarch64-pc-windows-msvc (or pass `--host-only` to skip)",
            tag_warn()
        ),
        (false, false) => eprintln!(
            "    {} ARM64 cross-compile not set up. `cargo truce package` defaults to universal and will fail without it — add the Rust target and the VS ARM64 toolchain, or pass `--host-only` to skip ARM64.",
            tag_warn()
        ),
    }
}

/// Look for an `arm64` lib directory under any VS MSVC toolchain version.
/// Presence of the lib dir is a reliable signal that the "ARM64 build tools"
/// component was installed. We don't require the cross-compiler binary to
/// live in a specific path — cc/build will locate it via vcvars_arm64.bat
/// when the Rust target triple requests it.
fn has_arm64_msvc_toolchain() -> bool {
    for vs_root in crate::vs_install_paths() {
        let msvc_root = vs_root.join(r"VC\Tools\MSVC");
        if let Ok(versions) = fs::read_dir(&msvc_root) {
            for v in versions.flatten() {
                if v.path().join(r"lib\arm64").is_dir() {
                    return true;
                }
            }
        }
    }
    false
}
