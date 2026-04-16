//! Windows packaging: Authenticode signing + Inno Setup installer.
//!
//! Flow: build each format (release) → stage into `target\package\windows\{suffix}\`
//! → Authenticode-sign binaries → PACE-sign AAX if present → render `.iss`
//! → run `ISCC.exe` → Authenticode-sign the installer → output to `dist\`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    build_aax_template, cargo_build, common_program_files, copy_dir_recursive,
    detect_default_features, load_config, program_files, project_root,
    read_workspace_version, release_lib, resolve_aax_sdk_path, tmp_dir, Config,
    PkgFormat, PluginDef, Res, WindowsSigningConfig,
};

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

    // Warn about missing signing credentials unless --no-sign was passed.
    if !opts.no_sign && !config.windows.signing.is_configured() {
        eprintln!(
            "WARNING: [windows.signing] has no credentials configured. Binaries and \
             installer will be unsigned. Pass --no-sign to silence this warning, or \
             set azure_account / sha1 / pfx_path under [windows.signing] in truce.toml."
        );
    }

    build_all_formats(&plugins, &formats, &root)?;

    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir)?;

    for p in &plugins {
        eprintln!("\n=== Packaging: {} ===", p.name);

        let staging = root.join("target/package/windows").join(&p.suffix);
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        let staged = stage_plugin(&root, p, &config, &formats, &staging)?;

        if !opts.no_sign {
            // AAX: PACE first, Authenticode second. PACE wraps the binary; signing
            // the wrapped binary last is what Pro Tools actually verifies.
            if formats.iter().any(|f| matches!(f, PkgFormat::Aax)) {
                let aax_bundle = staging.join(format!("{}.aaxplugin", p.name));
                pace_sign_aax(&aax_bundle)?;
            }
            sign_files(&staged.signable, &config.windows.signing)?;
        }

        if opts.no_installer {
            eprintln!(
                "  Skipped installer build (--no-installer). Staging at {}",
                staging.display()
            );
            continue;
        }

        let iss = render_iss(&config, p, &formats, &staging, &version, &dist_dir);
        let iss_path = staging.join("installer.iss");
        fs::write(&iss_path, &iss)?;
        run_iscc(&iss_path)?;

        let installer = dist_dir.join(format!("{}-{}-windows-x64.exe", p.name, version));
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

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Opts {
    plugin_filter: Option<String>,
    format_str: Option<String>,
    no_sign: bool,
    no_installer: bool,
}

fn parse_args(args: &[String]) -> std::result::Result<Opts, crate::BoxErr> {
    let mut opts = Opts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                opts.plugin_filter = Some(
                    args.get(i).cloned().ok_or("-p requires a plugin suffix")?,
                );
            }
            "--formats" => {
                i += 1;
                opts.format_str = Some(
                    args.get(i).cloned().ok_or("--formats requires a value")?,
                );
            }
            "--no-sign" => opts.no_sign = true,
            "--no-installer" => opts.no_installer = true,
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
        return Err(
            "no Windows-eligible formats selected (AU is macOS-only)".into(),
        );
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

/// Run the cargo builds for each selected format. Mirrors cmd_package on
/// macOS: one `cargo build` per format with distinct `--features` so the
/// ObjC/format-specific code paths can't cross-contaminate.
fn build_all_formats(
    plugins: &[&PluginDef],
    formats: &[PkgFormat],
    root: &Path,
) -> Res {
    let dt = "";  // MACOSX_DEPLOYMENT_TARGET is ignored on Windows

    let has_clap = formats.iter().any(|f| matches!(f, PkgFormat::Clap));
    let has_vst3 = formats.iter().any(|f| matches!(f, PkgFormat::Vst3));
    let has_vst2 = formats.iter().any(|f| matches!(f, PkgFormat::Vst2));
    let has_aax = formats.iter().any(|f| matches!(f, PkgFormat::Aax));

    // CLAP+VST3 share the default feature set; build once, then save a copy
    // of the dylib so later format builds don't clobber it.
    if has_clap || has_vst3 {
        eprintln!("Building CLAP + VST3...");
        let mut build_args: Vec<&str> = Vec::new();
        for p in plugins {
            build_args.push("-p");
            build_args.push(&p.crate_name);
        }
        cargo_build(&[], &build_args, dt)?;
        for p in plugins {
            let src = release_lib(root, &p.dylib_stem());
            let saved = release_lib(root, &format!("{}_plugin", p.dylib_stem()));
            if src.exists() {
                fs::copy(&src, &saved)?;
            }
        }
    }

    if has_vst2 {
        eprintln!("Building VST2...");
        let mut build_args: Vec<&str> = Vec::new();
        for p in plugins {
            build_args.push("-p");
            build_args.push(&p.crate_name);
        }
        build_args.extend_from_slice(&["--no-default-features", "--features", "vst2"]);
        cargo_build(&[], &build_args, dt)?;
        for p in plugins {
            let src = release_lib(root, &p.dylib_stem());
            let dst = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
            fs::copy(&src, &dst)?;
        }
    }

    if has_aax {
        eprintln!("Building AAX...");
        let mut build_args: Vec<&str> = Vec::new();
        for p in plugins {
            build_args.push("-p");
            build_args.push(&p.crate_name);
        }
        build_args.extend_from_slice(&["--no-default-features", "--features", "aax"]);
        cargo_build(&[], &build_args, dt)?;
        for p in plugins {
            let src = release_lib(root, &p.dylib_stem());
            let dst = release_lib(root, &format!("{}_aax", p.dylib_stem()));
            fs::copy(&src, &dst)?;
        }
    }

    // Restore the CLAP/VST3 dylib at its canonical location since later
    // format builds overwrote it.
    if has_clap || has_vst3 {
        for p in plugins {
            let saved = release_lib(root, &format!("{}_plugin", p.dylib_stem()));
            let dst = release_lib(root, &p.dylib_stem());
            if saved.exists() {
                fs::copy(&saved, &dst)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

struct StagedPlugin {
    /// Files to feed signtool. Order matters for AAX: inner binaries first,
    /// then the outer bundle root (signtool doesn't sign directories but the
    /// ordering convention is preserved for future use).
    signable: Vec<PathBuf>,
}

fn stage_plugin(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    formats: &[PkgFormat],
    staging: &Path,
) -> std::result::Result<StagedPlugin, crate::BoxErr> {
    let mut signable = Vec::new();
    for fmt in formats {
        eprint!("  Staging {}... ", fmt.label());
        match fmt {
            PkgFormat::Clap => {
                signable.push(stage_clap(root, p, staging)?);
            }
            PkgFormat::Vst3 => {
                signable.push(stage_vst3(root, p, staging)?);
            }
            PkgFormat::Vst2 => {
                signable.push(stage_vst2(root, p, staging)?);
            }
            PkgFormat::Aax => {
                let (wrapper, dylib) = stage_aax(root, p, config, staging)?;
                // Sign the inner dylib first, then the wrapper. signtool doesn't
                // care about order but Pro Tools' verification wants the wrapper
                // signature to be the outermost.
                signable.push(dylib);
                signable.push(wrapper);
            }
            PkgFormat::Au2 | PkgFormat::Au3 => {
                // Filtered out upstream; keep the match exhaustive.
                return Err("AU is macOS-only; should have been filtered".into());
            }
        }
        eprintln!("ok");
    }
    Ok(StagedPlugin { signable })
}

fn stage_clap(root: &Path, p: &PluginDef, staging: &Path) -> std::result::Result<PathBuf, crate::BoxErr> {
    let dll = release_lib(root, &p.dylib_stem());
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let dst_dir = staging.join("clap");
    fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join(format!("{}.clap", p.name));
    fs::copy(&dll, &dst)?;
    Ok(dst)
}

fn stage_vst3(root: &Path, p: &PluginDef, staging: &Path) -> std::result::Result<PathBuf, crate::BoxErr> {
    // VST3 on Windows is a bundle directory:
    //   {name}.vst3/Contents/x86_64-win/{name}.vst3
    // The inner file is the DLL with a .vst3 extension. The DAW loads that
    // file directly, so signtool needs to sign the inner binary.
    let dll = release_lib(root, &p.dylib_stem());
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let bundle_root = staging.join("vst3");
    let bundle = bundle_root.join(format!("{}.vst3", p.name));
    let arch_dir = bundle.join("Contents").join("x86_64-win");
    fs::create_dir_all(&arch_dir)?;
    let inner = arch_dir.join(format!("{}.vst3", p.name));
    fs::copy(&dll, &inner)?;
    Ok(inner)
}

fn stage_vst2(root: &Path, p: &PluginDef, staging: &Path) -> std::result::Result<PathBuf, crate::BoxErr> {
    let dll = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
    if !dll.exists() {
        return Err(format!("Missing: {}", dll.display()).into());
    }
    let dst_dir = staging.join("vst2");
    fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join(format!("{}.dll", p.name));
    fs::copy(&dll, &dst)?;
    Ok(dst)
}

/// Build/stage the AAX bundle. Returns `(wrapper_binary, resources_dylib)` so
/// both get Authenticode-signed.
fn stage_aax(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
) -> std::result::Result<(PathBuf, PathBuf), crate::BoxErr> {
    // Build the template .aaxplugin wrapper if it isn't there yet.
    let template = tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin");
    if !template.exists() {
        if let Some(sdk_path) = resolve_aax_sdk_path(config) {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
            build_aax_template(root, &sdk_path)?;
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

    let dylib = release_lib(root, &format!("{}_aax", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    let bundle_root = staging.join("aax");
    let bundle = bundle_root.join(format!("{}.aaxplugin", p.name));
    let contents = bundle.join("Contents");
    let x64_dir = contents.join("x64");
    let resources_dir = contents.join("Resources");
    fs::create_dir_all(&x64_dir)?;
    fs::create_dir_all(&resources_dir)?;

    let wrapper = x64_dir.join(format!("{}.aaxplugin", p.name));
    let resource_dll = resources_dir.join(format!("{}_aax.dll", p.dylib_stem()));
    fs::copy(&template, &wrapper)?;
    fs::copy(&dylib, &resource_dll)?;

    Ok((wrapper, resource_dll))
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
    let signtool = locate_signtool().ok_or(
        "signtool.exe not found on PATH. Install the Windows 10 SDK or Windows 11 SDK.",
    )?;

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
        let dlib = config
            .azure_dlib
            .clone()
            .unwrap_or_else(default_azure_dlib);
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
    // Standard install path of the Azure.CodeSigning.Dlib.dll that ships with
    // the "Trusted Signing Client Tools" redistributable.
    r"C:\Program Files\Microsoft Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll"
        .to_string()
}

fn locate_signtool() -> Option<PathBuf> {
    // Prefer signtool on %PATH%. Fallback: probe known Windows SDK locations.
    if let Ok(p) = which("signtool.exe") {
        return Some(p);
    }
    let candidates = [
        r"C:\Program Files (x86)\Windows Kits\10\bin\x64\signtool.exe",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    // Check versioned SDK bins: bin\10.0.*\x64\signtool.exe
    let sdk_bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(&sdk_bin) {
        // Take the highest-versioned subdir that contains x64\signtool.exe.
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

/// Cross-platform equivalent of `where.exe`. Returns the first matching
/// entry on `%PATH%`.
fn which(name: &str) -> Result<PathBuf, std::io::Error> {
    let path = std::env::var_os("PATH").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "PATH not set")
    })?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, name.to_string()))
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
            eprintln!(
                "  PACE_SIGN_ID env var not set — skipping PACE signing."
            );
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

/// Render the `.iss` installer script from config + selected formats.
fn render_iss(
    config: &Config,
    p: &PluginDef,
    formats: &[PkgFormat],
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

    // Stable AppId — drives Inno Setup's "same product? upgrade in place"
    // behavior. Derived from vendor.id + suffix when the user hasn't
    // overridden it.
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

    let mut setup = String::new();
    setup.push_str("[Setup]\r\n");
    setup.push_str(&format!("AppId={{{{{}}}}}\r\n", iss_escape(&app_id)));
    setup.push_str(&format!("AppName={}\r\n", iss_escape(&p.name)));
    setup.push_str(&format!("AppVersion={}\r\n", iss_escape(version)));
    setup.push_str(&format!("AppPublisher={}\r\n", iss_escape(publisher)));
    if !publisher_url.is_empty() {
        setup.push_str(&format!("AppPublisherURL={}\r\n", iss_escape(&publisher_url)));
    }
    setup.push_str(&format!(
        "DefaultDirName={{commonpf}}\\{}\\{}\r\n",
        iss_escape(publisher),
        iss_escape(&p.name),
    ));
    setup.push_str("DisableDirPage=yes\r\n");
    setup.push_str(&format!("OutputDir={}\r\n", iss_escape_path(dist_dir)));
    setup.push_str(&format!(
        "OutputBaseFilename={}-{}-windows-x64\r\n",
        iss_escape(&p.name),
        iss_escape(version),
    ));
    setup.push_str("Compression=lzma2\r\n");
    setup.push_str("SolidCompression=yes\r\n");
    setup.push_str("ArchitecturesInstallIn64BitMode=x64compatible\r\n");
    setup.push_str("ArchitecturesAllowed=x64compatible\r\n");
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

    // [Components] — one per format, so end users can uncheck formats they
    // don't want. `default` is checked on first install; `Types: full`
    // includes the format in the Full install type.
    setup.push_str("[Types]\r\n");
    setup.push_str("Name: \"full\"; Description: \"Full installation\"\r\n");
    setup.push_str("Name: \"custom\"; Description: \"Custom installation\"; Flags: iscustom\r\n\r\n");

    setup.push_str("[Components]\r\n");
    for fmt in formats {
        let (name, desc, types) = iss_component_spec(fmt);
        setup.push_str(&format!(
            "Name: \"{}\"; Description: \"{}\"; Types: {}\r\n",
            name, desc, types
        ));
    }
    setup.push_str("\r\n");

    // [Files] — one block per format.
    setup.push_str("[Files]\r\n");
    for fmt in formats {
        let block = iss_files_block(fmt, p, staging);
        setup.push_str(&block);
    }
    setup.push_str("\r\n");

    // [UninstallDelete] — Inno removes every file it installed on uninstall.
    // For bundle directories (.vst3, .aaxplugin) that's the root dir itself;
    // use `filesandordirs` so any cache files the DAW wrote inside the
    // bundle also get cleaned up.
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

fn iss_files_block(fmt: &PkgFormat, p: &PluginDef, staging: &Path) -> String {
    match fmt {
        PkgFormat::Clap => {
            let src = staging.join("clap").join(format!("{}.clap", p.name));
            format!(
                "Source: \"{src}\"; DestDir: \"{{commoncf}}\\CLAP\"; \
                 Components: clap; Flags: ignoreversion overwritereadonly\r\n",
                src = iss_escape_path(&src),
            )
        }
        PkgFormat::Vst3 => {
            // Bundle directory: copy recursively into a destination dir named
            // after the plugin so the bundle's subdirectories land correctly.
            let src_dir = staging
                .join("vst3")
                .join(format!("{}.vst3", p.name));
            let src_glob = src_dir.join("*");
            format!(
                "Source: \"{src}\"; DestDir: \"{{commoncf}}\\VST3\\{name}.vst3\"; \
                 Components: vst3; Flags: ignoreversion overwritereadonly recursesubdirs createallsubdirs\r\n",
                src = iss_escape_path(&src_glob),
                name = iss_escape(&p.name),
            )
        }
        PkgFormat::Vst2 => {
            let src = staging.join("vst2").join(format!("{}.dll", p.name));
            format!(
                "Source: \"{src}\"; DestDir: \"{{pf}}\\Steinberg\\VstPlugins\"; \
                 Components: vst2; Flags: ignoreversion overwritereadonly\r\n",
                src = iss_escape_path(&src),
            )
        }
        PkgFormat::Aax => {
            let src_dir = staging
                .join("aax")
                .join(format!("{}.aaxplugin", p.name));
            let src_glob = src_dir.join("*");
            format!(
                "Source: \"{src}\"; DestDir: \"{{commoncf}}\\Avid\\Audio\\Plug-Ins\\{name}.aaxplugin\"; \
                 Components: aax; Flags: ignoreversion overwritereadonly recursesubdirs createallsubdirs\r\n",
                src = iss_escape_path(&src_glob),
                name = iss_escape(&p.name),
            )
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
        // CLAP/VST2 are single files; Inno's per-file uninstall handles them.
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

/// Escape a string value for Inno Setup. Inno Setup's .iss parser treats
/// `"` as the string delimiter — there's no backslash escape, instead you
/// double the quote. Most paths avoid quotes entirely but plugin names
/// could contain them, so we handle it.
fn iss_escape(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Escape a filesystem path for inclusion in an `.iss` string literal.
/// Inno Setup wants native backslashes, so we make sure paths are normalized
/// that way (helpful when paths came from cross-platform code that used `/`).
fn iss_escape_path(p: &Path) -> String {
    let s = p.display().to_string();
    let s = s.replace('/', "\\");
    iss_escape(&s)
}

// ---------------------------------------------------------------------------
// Doctor hook (called from cmd_doctor)
// ---------------------------------------------------------------------------

/// Pretty-print packaging-tool availability. Called from the Windows branch
/// of `cmd_doctor` so `cargo xtask doctor` shows a single unified status.
pub(crate) fn doctor() {
    match locate_iscc() {
        Some(p) => eprintln!("    ✅ Inno Setup 6 (ISCC.exe) at {}", p.display()),
        None => eprintln!(
            "    ⚠️  ISCC.exe not found — install Inno Setup 6 to produce installers"
        ),
    }
    match locate_signtool() {
        Some(p) => eprintln!("    ✅ signtool.exe at {}", p.display()),
        None => eprintln!(
            "    ⚠️  signtool.exe not found — install Windows 10/11 SDK for Authenticode"
        ),
    }
    match locate_wraptool() {
        Some(p) => eprintln!("    ✅ wraptool.exe (PACE) at {}", p.display()),
        None => eprintln!(
            "    ℹ️  wraptool.exe not found — only needed for signed AAX builds"
        ),
    }
}

// Suppress unused-import warnings when helpers aren't wired yet.
#[allow(dead_code)]
fn _unused_ref() {
    let _ = (common_program_files, program_files, copy_dir_recursive);
}
