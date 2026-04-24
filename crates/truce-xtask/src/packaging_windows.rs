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

use crate::{
    build_aax_template, cargo_build, detect_default_features, load_config, project_root,
    read_workspace_version, release_lib_for_target, resolve_aax_sdk_path, rustup_has_target,
    tmp_dir, Config, PkgFormat, PluginDef, Res, WindowsSigningConfig,
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

    /// Short tag used in staging paths (`target/package/windows/{suffix}/clap/{tag}/…`).
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

    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir)?;

    for p in &plugins {
        eprintln!("\n=== Packaging: {} ({}) ===", p.name, archs_label(&archs));

        let staging = root.join("target/package/windows").join(&p.suffix);
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

        let iss = render_iss(&config, p, &formats, &archs, &staging, &version, &dist_dir);
        let iss_path = staging.join("installer.iss");
        fs::write(&iss_path, &iss)?;
        run_iscc(&iss_path)?;

        let installer = dist_dir.join(format!("{}-{}-windows.exe", p.name, version));
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

fn parse_args(args: &[String]) -> std::result::Result<Opts, crate::BoxErr> {
    let mut opts = Opts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                opts.plugin_filter =
                    Some(args.get(i).cloned().ok_or("-p requires a plugin suffix")?);
            }
            "--formats" => {
                i += 1;
                opts.format_str = Some(args.get(i).cloned().ok_or("--formats requires a value")?);
            }
            "--no-sign" => opts.no_sign = true,
            "--no-pace-sign" => opts.no_pace_sign = true,
            "--no-installer" => opts.no_installer = true,
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
            .filter(|p| p.suffix == filter)
            .collect();
        if matched.is_empty() {
            return Err(format!(
                "No plugin with suffix '{filter}'. Available: {}",
                config
                    .plugin
                    .iter()
                    .map(|p| p.suffix.as_str())
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

fn render_iss(
    config: &Config,
    p: &PluginDef,
    formats: &[PkgFormat],
    archs: &[TargetArch],
    staging: &Path,
    version: &str,
    dist_dir: &Path,
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
        .unwrap_or_else(|| format!("{}.{}", config.vendor.id, p.suffix));

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
    setup.push_str(&format!(
        "DefaultDirName={{commonpf}}\\{}\\{}\r\n",
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
    setup.push_str("PrivilegesRequired=admin\r\n");
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

    // [Components] — one per format.
    setup.push_str("[Components]\r\n");
    for fmt in formats {
        let (name, desc, types) = iss_component_spec(fmt);
        setup.push_str(&format!(
            "Name: \"{}\"; Description: \"{}\"; Types: {}\r\n",
            name, desc, types
        ));
    }
    setup.push_str("\r\n");

    // [Files] — one block per format × arch.
    setup.push_str("[Files]\r\n");
    for fmt in formats {
        for &arch in archs {
            let block = iss_files_block(fmt, p, staging, arch, universal);
            setup.push_str(&block);
        }
    }
    setup.push_str("\r\n");

    // [UninstallDelete] — per-format (bundle dirs get wholesale cleanup).
    setup.push_str("[UninstallDelete]\r\n");
    for fmt in formats {
        if let Some(line) = iss_uninstall_line(fmt, &p.name) {
            setup.push_str(&line);
            setup.push_str("\r\n");
        }
    }

    setup
}

fn iss_component_spec(fmt: &PkgFormat) -> (&'static str, &'static str, &'static str) {
    match fmt {
        PkgFormat::Clap => ("clap", "CLAP (Reaper, Bitwig)", "full"),
        PkgFormat::Vst3 => ("vst3", "VST3 (most DAWs)", "full"),
        PkgFormat::Vst2 => ("vst2", "VST2 (legacy — Reaper, older hosts)", "custom"),
        PkgFormat::Aax => ("aax", "AAX (Pro Tools)", "full"),
        PkgFormat::Au2 | PkgFormat::Au3 => unreachable!("AU is filtered out on Windows"),
    }
}

/// Build the `[Files]` entries for one format × arch. For single-file formats
/// (CLAP, VST2) we gate with a `Check:` directive so only the matching arch's
/// DLL is installed on a given machine. Bundle formats (VST3, AAX) install
/// both archs side-by-side; the host picks at load time.
fn iss_files_block(
    fmt: &PkgFormat,
    p: &PluginDef,
    staging: &Path,
    arch: TargetArch,
    universal: bool,
) -> String {
    // For single-arch installers the Check: directive is unnecessary — drop it
    // so the output .iss stays simple.
    let check_clause = if universal {
        format!(" Check: {};", arch.iss_check())
    } else {
        String::new()
    };

    match fmt {
        PkgFormat::Clap => {
            let src = staging
                .join("clap")
                .join(arch.tag())
                .join(format!("{}.clap", p.name));
            format!(
                "Source: \"{src}\"; DestDir: \"{{commoncf}}\\CLAP\"; \
                 Components: clap;{check_clause} Flags: ignoreversion overwritereadonly\r\n",
                src = iss_escape_path(&src),
                check_clause = check_clause,
            )
        }
        PkgFormat::Vst3 => {
            // Bundle: copy just this arch's sub-directory. No Check: — hosts
            // of either arch can coexist on ARM64 machines (x64 hosts run via
            // emulation), so both sub-dirs should always be present.
            let src_dir = staging
                .join("vst3")
                .join(format!("{}.vst3", p.name))
                .join("Contents")
                .join(arch.vst3_bundle_subdir());
            let src_glob = src_dir.join("*");
            format!(
                "Source: \"{src}\"; \
                 DestDir: \"{{commoncf}}\\VST3\\{name}.vst3\\Contents\\{subdir}\"; \
                 Components: vst3; Flags: ignoreversion overwritereadonly recursesubdirs createallsubdirs\r\n",
                src = iss_escape_path(&src_glob),
                name = iss_escape(&p.name),
                subdir = arch.vst3_bundle_subdir(),
            )
        }
        PkgFormat::Vst2 => {
            let src = staging
                .join("vst2")
                .join(arch.tag())
                .join(format!("{}.dll", p.name));
            format!(
                "Source: \"{src}\"; DestDir: \"{{pf}}\\Steinberg\\VstPlugins\"; \
                 Components: vst2;{check_clause} Flags: ignoreversion overwritereadonly\r\n",
                src = iss_escape_path(&src),
                check_clause = check_clause,
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
            let mut out = String::new();
            out.push_str(&format!(
                "Source: \"{src}\"; \
                 DestDir: \"{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{name}.aaxplugin\\Contents\\{subdir}\"; \
                 Components: aax; Flags: ignoreversion overwritereadonly recursesubdirs createallsubdirs\r\n",
                src = iss_escape_path(&src_arch_glob),
                name = iss_escape(&p.name),
                subdir = arch.aax_bundle_subdir(),
            ));
            out.push_str(&format!(
                "Source: \"{src}\"; \
                 DestDir: \"{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{name}.aaxplugin\\Contents\\Resources\"; \
                 Components: aax; Flags: ignoreversion overwritereadonly\r\n",
                src = iss_escape_path(&resource_dll),
                name = iss_escape(&p.name),
            ));
            out
        }
        PkgFormat::Au2 | PkgFormat::Au3 => unreachable!(),
    }
}

fn iss_uninstall_line(fmt: &PkgFormat, plugin_name: &str) -> Option<String> {
    match fmt {
        PkgFormat::Vst3 => Some(format!(
            "Type: filesandordirs; Name: \"{{commoncf}}\\VST3\\{}.vst3\"; Components: vst3",
            iss_escape(plugin_name)
        )),
        PkgFormat::Aax => Some(format!(
            "Type: filesandordirs; Name: \"{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{}.aaxplugin\"; Components: aax",
            iss_escape(plugin_name)
        )),
        _ => None,
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
        Some(p) => eprintln!("    ✅ Inno Setup 6 (ISCC.exe) at {}", p.display()),
        None => {
            eprintln!("    ⚠️  ISCC.exe not found — install Inno Setup 6 to produce installers")
        }
    }
    match locate_signtool() {
        Some(p) => eprintln!("    ✅ signtool.exe at {}", p.display()),
        None => {
            eprintln!("    ⚠️  signtool.exe not found — install Windows 10/11 SDK for Authenticode")
        }
    }
    match locate_wraptool() {
        Some(p) => eprintln!("    ✅ wraptool.exe (PACE) at {}", p.display()),
        None => eprintln!("    ℹ️  wraptool.exe not found — only needed for signed AAX builds"),
    }

    // ARM64 readiness. Universal is the default, so missing ARM64 toolchain
    // downgrades to a warning (packages with `--host-only` still work).
    let has_rust_arm64 = rustup_has_target("aarch64-pc-windows-msvc");
    let has_msvc_arm64 = has_arm64_msvc_toolchain();
    match (has_rust_arm64, has_msvc_arm64) {
        (true, true) => eprintln!(
            "    ✅ ARM64 cross-compile available — `cargo truce package` will produce dual-arch installers by default"
        ),
        (true, false) => eprintln!(
            "    ⚠️  Rust has aarch64-pc-windows-msvc but VS is missing the ARM64 MSVC toolchain — C++ shims won't cross-compile. Install \"MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools\" via the VS Installer, or pass `--host-only` to skip ARM64."
        ),
        (false, true) => eprintln!(
            "    ⚠️  VS has ARM64 MSVC but the Rust target isn't installed — run: rustup target add aarch64-pc-windows-msvc (or pass `--host-only` to skip)"
        ),
        (false, false) => eprintln!(
            "    ⚠️  ARM64 cross-compile not set up. `cargo truce package` defaults to universal and will fail without it — add the Rust target and the VS ARM64 toolchain, or pass `--host-only` to skip ARM64."
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
