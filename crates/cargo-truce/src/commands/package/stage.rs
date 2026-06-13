//! Format-specific staging: copy the built dylib into a bundle layout,
//! write the per-format Info.plist, and codesign.

#[cfg(target_os = "macos")]
use super::PkgFormat;
use crate::commands::install::presets;
#[cfg(target_os = "macos")]
use crate::install_scope::PkgScope;
#[cfg(target_os = "macos")]
use crate::pace_sign_aax_macos;
use crate::{Config, PluginDef, Res, codesign_bundle};
#[cfg(target_os = "macos")]
use crate::{MacosPackagingConfig, copy_dir_recursive};
#[cfg(target_os = "macos")]
use std::fmt::Write;
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

/// Slug a plugin's display name into a lowercase, hyphenated, ASCII-safe
/// identifier suitable for LV2 bundle / file / IRI use. Thin re-export
/// of [`truce_utils::slugify`]; kept under the `package::stage` module
/// path because every cargo-truce caller already imports from here.
pub(crate) fn lv2_slug(name: &str) -> String {
    truce_utils::slugify(name)
}

/// Stage an LV2 bundle into `staging/{slug}.lv2/`. Copies the built
/// shared library plus the proc-macro-emitted `manifest.ttl` /
/// `plugin.ttl` sidecars (written by truce-derive's `derive(Params)`
/// during the cdylib's compile). No dlopen - the binary doesn't have
/// to load on this host, so cross-arch builds Just Work.
///
/// On macOS, the inner `.so` is a Mach-O and gets signed with the
/// caller's identity via `codesign_bundle` - same Developer-ID +
/// hardened-runtime + secure-timestamp treatment as CLAP / VST3.
/// Without that step, Apple's notarization-readiness check flags the
/// LV2 binary as ad-hoc-signed and refuses to submit.
///
/// `target` selects which `target/<triple>/release/` directory to
/// read the built dylib from. `None` reads from the default
/// `target/release/` (host build). On non-macOS hosts `identity` is
/// unused (`codesign_bundle` is a no-op there).
pub(crate) fn stage_lv2(
    root: &Path,
    p: &PluginDef,
    staging: &Path,
    identity: &str,
    target: Option<&str>,
) -> Res {
    let built = crate::release_lib_for_target(root, &format!("{}_lv2", p.dylib_stem()), target);
    if !built.exists() {
        return Err(format!("Missing: {}", built.display()).into());
    }

    let target_dir = truce_build::target_dir(root);
    let sidecar_dir = target_dir.join("lv2-meta").join(&p.crate_name);
    let manifest_ttl = sidecar_dir.join("manifest.ttl");
    let plugin_ttl = sidecar_dir.join("plugin.ttl");
    if !manifest_ttl.exists() || !plugin_ttl.exists() {
        return Err(format!(
            "no LV2 metadata sidecar at {} for {}. \
             `derive(Params)` writes this during the cdylib's compile; \
             missing it means either the params struct uses `#[nested]` \
             (unsupported for the compile-time TTL path) or the plugin \
             crate isn't listed under `[[plugin]]` in truce.toml.",
            sidecar_dir.display(),
            p.name,
        )
        .into());
    }

    let slug = lv2_slug(&p.name);
    let bundle = staging.join(format!("{slug}.lv2"));
    let _ = fs::remove_dir_all(&bundle);
    fs::create_dir_all(&bundle)?;

    let bin_ext = if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let bin_name = format!("{slug}.{bin_ext}");
    let bin_path = bundle.join(&bin_name);
    fs::copy(&built, &bin_path)?;
    fs::copy(&manifest_ttl, bundle.join("manifest.ttl"))?;
    fs::copy(&plugin_ttl, bundle.join("plugin.ttl"))?;

    // Sign the inner Mach-O directly rather than passing the bundle
    // dir. An LV2 "bundle" is just a directory of files, not a real
    // macOS bundle (no `Contents/Info.plist`) - codesign refuses to
    // seal directories it doesn't recognise, but the Mach-O itself
    // signs fine and that's the only file Apple's notary actually
    // inspects.
    codesign_bundle(&bin_path.to_string_lossy(), identity, false)?;
    Ok(())
}

/// Package wrapper: stage the LV2 bundle, then append its factory
/// preset TTLs + `manifest.ttl` entries. `stage_lv2` itself stays
/// preset-free because `install` emits LV2 presets onto its own copy
/// (a double-emit would duplicate manifest entries); only packaging
/// layers them in here. The TTLs sit outside the signed inner Mach-O,
/// so emitting after staging is safe.
#[cfg(target_os = "macos")]
pub(crate) fn stage_lv2_packaged(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    identity: &str,
    target: Option<&str>,
) -> Res {
    stage_lv2(root, p, staging, identity, target)?;
    if let Some(fp) = presets::load_factory_presets(root, p, config)? {
        let bundle = staging.join(format!("{}.lv2", lv2_slug(&p.name)));
        let uri =
            truce_build::lv2::plugin_uri(config.vendor.url.as_deref().unwrap_or(""), &p.bundle_id);
        presets::emit_lv2_presets(&fp, &bundle, &uri)?;
    }
    Ok(())
}

/// Build a standalone pkg component for an out-of-bundle preset
/// payload: stage `payload` (relative path -> bytes) under a private
/// pkgroot, then `pkgbuild` it with `install_location` as the target.
/// Used for VST3 presets, which install to the OS preset folder rather
/// than the plugin bundle. Produces `<file_stem>-<label>.pkg` in
/// `components_dir`. No `--scripts`: the preset dir is shared with
/// user / host presets, so the component only adds its own files (BOM
/// removal on upgrade leaves user presets untouched) and never wipes.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_preset_component(
    staging: &Path,
    components_dir: &Path,
    file_stem: &str,
    pkg_id: &str,
    label: &str,
    install_location: &str,
    version: &str,
    payload: &[(PathBuf, Vec<u8>)],
) -> Res {
    let root = staging.join(format!("_presetroot_{label}"));
    let _ = fs::remove_dir_all(&root);
    for (rel, bytes) in payload {
        let dst = root.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, bytes)?;
    }
    let component_pkg = components_dir.join(format!("{file_stem}-{label}.pkg"));
    let status = Command::new("pkgbuild")
        .args([
            "--root",
            root.to_str().unwrap(),
            "--install-location",
            install_location,
            "--identifier",
            pkg_id,
            "--version",
            version,
            "--ownership",
            "preserve",
            component_pkg.to_str().unwrap(),
        ])
        .status()?;
    if !status.success() {
        return Err(format!("pkgbuild failed for {file_stem} {label}").into());
    }
    Ok(())
}

/// Stage a CLAP bundle into the staging directory. `target` selects
/// which `target/<triple>/release/` to read from (`None` = host's
/// `target/release/`).
///
/// macOS uses the loadable-bundle layout that hosts like Bitwig expect
/// (`{name}.clap/Contents/MacOS/<name>` + `Info.plist`). Linux and
/// Windows keep the flat `.so` / `.dll` renamed `.clap`.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn stage_clap(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    identity: &str,
    target: Option<&str>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = crate::release_lib_for_target(root, &format!("{}_clap", p.dylib_stem()), target);
    #[cfg(target_os = "macos")]
    let dylib = {
        let _ = target;
        crate::release_bundle_bin(root, &p.dylib_stem(), "_clap")
    };
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.clap", p.file_stem()));

    #[cfg(target_os = "macos")]
    {
        let macos_dir = bundle.join("Contents/MacOS");
        fs::create_dir_all(&macos_dir)?;
        let exec_name = p.file_stem();
        fs::copy(&dylib, macos_dir.join(&exec_name))?;

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        fs::write(bundle.join("Contents/Info.plist"), &plist)?;
        // Presets are part of the bundle's sealed Resources - emit
        // before codesign so the signature covers them.
        if let Some(fp) = presets::load_factory_presets(root, p, config)? {
            presets::emit_trucepreset_tree(
                &fp,
                &bundle.join("Contents/Resources/Presets"),
                false,
                &format!("{}-clap", p.bundle_id),
            )?;
        }
        codesign_bundle(bundle.to_str().unwrap(), identity, false)?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        fs::copy(&dylib, &bundle)?;
        // Single-file `.clap`; presets land in a `<stem>.presets/`
        // sibling, where the discovery provider looks.
        if let Some(fp) = presets::load_factory_presets(root, p, config)? {
            presets::emit_trucepreset_tree(
                &fp,
                &staging.join(format!("{}.presets", p.file_stem())),
                false,
                &format!("{}-clap", p.bundle_id),
            )?;
        }
        codesign_bundle(bundle.to_str().unwrap(), identity, false)?;
    }

    Ok(())
}

/// Stage a VST3 bundle into the staging directory. `target` selects
/// which `target/<triple>/release/` to read from (`None` = host's
/// `target/release/`) and also drives the VST3 inner-arch subdir
/// (`Contents/x86_64-linux/`, `Contents/aarch64-linux/`, etc.).
pub(crate) fn stage_vst3(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    target: Option<&str>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = crate::release_lib_for_target(root, &format!("{}_vst3", p.dylib_stem()), target);
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_vst3");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.vst3", p.file_stem()));

    // VST3 bundle layout is platform-specific (Steinberg "Bundle Locations"
    // section of the SDK docs):
    //   macOS:   Contents/MacOS/<name>             (Mach-O, no extension)
    //   Linux:   Contents/<arch>-linux/<name>.so   (ELF, .so)
    //   Windows: Contents/<arch>-win/<name>.vst3   (PE, .vst3)
    // The earlier "always Contents/MacOS/<name>" layout produced bundles
    // that hosts on Linux refused to load - VST3 hosts pick the inner
    // binary from the arch-specific subdir and fall back to nothing.
    #[cfg(target_os = "macos")]
    {
        let macos_dir = bundle.join("Contents/MacOS");
        fs::create_dir_all(&macos_dir)?;
        let exec_name = p.file_stem();
        fs::copy(&dylib, macos_dir.join(&exec_name))?;

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        fs::write(bundle.join("Contents/Info.plist"), &plist)?;
        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            false,
        )?;
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let _ = config; // unused on these platforms
        let triple: &str = match target {
            Some(t) => t,
            None => truce_build::host_triple(),
        };
        let arch_dir = bundle.join("Contents").join(vst3_arch_subdir(triple));
        fs::create_dir_all(&arch_dir)?;
        let inner_filename = format!("{}.{}", p.file_stem(), vst3_inner_extension(triple));
        fs::copy(&dylib, arch_dir.join(inner_filename))?;
    }
    #[cfg(target_os = "macos")]
    let _ = target; // macOS uses Contents/MacOS regardless of arch (lipo'd later).
    Ok(())
}

/// VST3 bundle inner-directory name per the VST3 SDK "Bundle Locations"
/// spec. Maps a cargo target triple to the bundle's `Contents/<dir>/`.
/// macOS callers don't reach this - they use the special `MacOS` dir.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn vst3_arch_subdir(triple: &str) -> &'static str {
    match triple {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => "x86_64-linux",
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => "aarch64-linux",
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => "x86_64-win",
        "aarch64-pc-windows-msvc" => "aarch64-win",
        // Linux/Windows on a non-mainstream arch - VST3 hosts on those
        // arches wouldn't load it anyway. Emit something deterministic
        // so the bundle structure stays parseable.
        _ => "unknown",
    }
}

/// VST3 inner-binary extension per the VST3 SDK spec. Linux uses
/// `.so`; Windows uses `.vst3`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn vst3_inner_extension(triple: &str) -> &'static str {
    if triple.contains("linux") {
        "so"
    } else if triple.contains("windows") {
        "vst3"
    } else {
        "so"
    }
}

/// Stage a VST2 build artifact into the staging directory and return
/// the staged path. macOS produces a `.vst` directory bundle (with
/// `Contents/MacOS/X` + Info.plist + codesign); Linux / Windows just
/// copy the bare `.so` / `.dll` since neither platform uses a bundle
/// layout for VST2.
///
/// `target` selects which `target/<triple>/release/` to read the
/// dylib from; `None` reads from `target/release/` (host build).
pub(crate) fn stage_vst2(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    target: Option<&str>,
) -> Result<std::path::PathBuf, crate::CargoTruceError> {
    let _ = config; // only used on macOS
    #[cfg(not(target_os = "macos"))]
    let dylib = crate::release_lib_for_target(root, &format!("{}_vst2", p.dylib_stem()), target);
    #[cfg(target_os = "macos")]
    let dylib = {
        let _ = target;
        crate::release_bundle_bin(root, &p.dylib_stem(), "_vst2")
    };
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "linux")]
    {
        let dst = staging.join(format!("{}.so", p.file_stem()));
        fs::copy(&dylib, &dst)?;
        Ok(dst)
    }

    #[cfg(target_os = "windows")]
    {
        let dst = staging.join(format!("{}.dll", p.file_stem()));
        fs::copy(&dylib, &dst)?;
        Ok(dst)
    }

    #[cfg(target_os = "macos")]
    {
        let bundle = staging.join(format!("{}.vst", p.file_stem()));
        let macos_dir = bundle.join("Contents/MacOS");
        fs::create_dir_all(&macos_dir)?;
        let exec_name = p.file_stem();
        fs::copy(&dylib, macos_dir.join(&exec_name))?;

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.truce.{bundle_id}.vst2</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
        );
        fs::write(bundle.join("Contents/Info.plist"), &plist)?;
        fs::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;
        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            false,
        )?;
        Ok(bundle)
    }
}

/// Stage an AU v2 bundle (`.component` directory) into the staging
/// directory. Audio Unit is macOS-only.
#[cfg(target_os = "macos")]
pub(crate) fn stage_au2(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib =
        truce_build::target_dir(root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.component", p.file_stem()));
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    let exec_name = p.file_stem();
    fs::copy(&dylib, macos_dir.join(&exec_name))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.component</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>AudioComponents</key>
    <array>
        <dict>
            <key>type</key>
            <string>{au_type}</string>
            <key>subtype</key>
            <string>{au_subtype}</string>
            <key>manufacturer</key>
            <string>{au_mfr}</string>
            <key>name</key>
            <string>{vendor}: {display_name}</string>
            <key>description</key>
            <string>{display_name}</string>
            <key>version</key>
            <integer>65536</integer>
            <key>factoryFunction</key>
            <string>TruceAUFactory</string>
            <key>sandboxSafe</key>
            <true/>
            <key>tags</key>
            <array>
                <string>{au_tag}</string>
            </array>
        </dict>
    </array>
</dict>
</plist>"#,
        display_name = p.name,
        bundle_id = p.bundle_id,
        vendor_id = config.vendor.id,
        vendor = config.vendor.name,
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    // The shim's kAudioUnitProperty_FactoryPresets handler enumerates
    // these from the sealed bundle - emit before codesign.
    if let Some(fp) = presets::load_factory_presets(root, p, config)? {
        presets::emit_trucepreset_tree(
            &fp,
            &bundle.join("Contents/Resources/Presets"),
            false,
            &format!("{}-au-bundle", p.bundle_id),
        )?;
    }
    codesign_bundle(
        bundle.to_str().unwrap(),
        &crate::application_identity(),
        false,
    )?;
    Ok(())
}

/// Stage an AAX bundle into the staging directory.
///
/// `universal_mac` controls whether the AAX C++ template (the wrapper binary
/// Pro Tools launches) is built fat - the Rust cdylib in Resources/ is
/// already lipo'd universal when the caller passes `universal_mac = true`.
/// Stage an AAX bundle into the packaging staging tree.
///
/// Reads from `target/bundles/{Plugin}.aaxplugin/` - the fully-
/// assembled + Apple-signed output of
/// [`emit_aax_bundle`](crate::commands::install::aax::emit_aax_bundle).
/// PACE-signs the staged copy in place (PACE wraps the Apple
/// signature, and pkgbuild reads the staging tree directly so the
/// order is safe).
#[cfg(target_os = "macos")]
pub(crate) fn stage_aax(
    root: &Path,
    p: &PluginDef,
    _config: &Config,
    staging: &Path,
    _universal_mac: bool,
    no_pace_sign: bool,
) -> Res {
    let bundle_name = format!("{}.aaxplugin", p.file_stem());
    let built = truce_build::target_dir(root)
        .join("bundles")
        .join(&bundle_name);
    if !built.exists() {
        return Err(format!(
            "AAX bundle missing at {}. Call `emit_aax_bundle` from the package driver before staging.",
            built.display()
        )
        .into());
    }

    let dst = staging.join(&bundle_name);
    let _ = fs::remove_dir_all(&dst);
    crate::util::copy_dir_recursive(&built, &dst)?;

    if !no_pace_sign {
        pace_sign_aax_macos(&dst)?;
    }
    Ok(())
}

/// Stage an AU v3 `.app` bundle into the staging directory for pkgbuild.
///
/// Reads from `target/bundles/{Plugin}.app/` - the fully-signed output
/// of `emit_au_v3_bundle` - and copies it into the staging tree.
/// The bundle is already signed + has its embedded framework, so this
/// is a pure copy.
#[cfg(target_os = "macos")]
pub(crate) fn stage_au3(root: &Path, p: &PluginDef, _config: &Config, staging: &Path) -> Res {
    let app_name = format!("{}.app", p.au3_app_name());
    let built_app = truce_build::target_dir(root)
        .join("bundles")
        .join(&app_name);
    if !built_app.exists() {
        return Err(format!(
            "AU v3 bundle missing at {}. Run `cargo truce build --au3 -p {}` first.",
            built_app.display(),
            p.bundle_id,
        )
        .into());
    }

    let dst = staging.join(&app_name);
    // May be root-owned from a previous install-based run. Best-effort
    // `rm -rf` covers that case; surface a pointed error if it still
    // fails so the user knows exactly which command to run by hand.
    if dst.exists() && fs::remove_dir_all(&dst).is_err() {
        let status = Command::new("rm")
            .args(["-rf", dst.to_str().unwrap()])
            .status();
        if dst.exists() {
            return Err(format!(
                "could not remove stale staging dir {} \
                 (rm exit: {status:?}). \
                 This is usually root-owned leftovers from an earlier \
                 `cargo truce install`. Run:\n    \
                 sudo rm -rf {}",
                dst.display(),
                dst.display(),
            )
            .into());
        }
    }
    copy_dir_recursive(&built_app, &dst)?;
    Ok(())
}

/// Stage the standalone host as a `.app` bundle inside the packaging
/// staging tree. Reads the per-arch standalone binaries built by
/// `build_and_lipo_standalone`, lipo-merges (or copies, single-arch)
/// into `<staging>/<Plugin>.app/Contents/MacOS/<bin>`, writes the
/// Info.plist, and codesigns. The pkgbuild step downstream installs
/// the resulting `.app` to `/Applications/`.
#[cfg(target_os = "macos")]
pub(crate) fn stage_standalone(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    use std::os::unix::fs::PermissionsExt;

    let bin_stem = crate::read_standalone_bin_name(&p.crate_name)
        .unwrap_or_else(|| format!("{}-standalone", p.crate_name));

    // Universal output written by `build_and_lipo_standalone` to
    // `target/release/<bin_stem>` (single-arch falls through to the
    // same path via `cp`).
    let built = truce_build::target_dir(root)
        .join("release")
        .join(&bin_stem);
    if !built.exists() {
        return Err(format!(
            "Standalone binary missing at {}. \
             The build step should have produced it - make sure the \
             plugin's Cargo.toml declares a [[bin]] target named '{}'.",
            built.display(),
            bin_stem,
        )
        .into());
    }

    let staged_app = staging.join(format!("{}.app", p.file_stem()));
    let _ = fs::remove_dir_all(&staged_app);
    let macos_dir = staged_app.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    let exe_dst = macos_dir.join(&bin_stem);
    fs::copy(&built, &exe_dst)?;

    // Mark the binary executable. `pkgbuild` preserves the staged
    // mode bits; without this the installed app refuses to launch
    // ("permission denied") on the end user's machine.
    let mut perms = fs::metadata(&exe_dst)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&exe_dst, perms)?;

    // Optional per-plugin app icon. Drop the `.icns` into
    // `Contents/Resources/icon.icns` and let `write_standalone_info_plist`
    // emit the matching `CFBundleIconFile` key. Absent = no icon
    // (system default folder-with-cog).
    let icon_present = if let Some(icon_rel) = &p.macos_icon {
        let icon_src = crate::project_root().join(icon_rel);
        if !icon_src.exists() {
            return Err(format!(
                "macos_icon for `{}` points to {} but no file is there.",
                p.name,
                icon_src.display()
            )
            .into());
        }
        let resources_dir = staged_app.join("Contents/Resources");
        fs::create_dir_all(&resources_dir)?;
        fs::copy(&icon_src, resources_dir.join("icon.icns"))?;
        true
    } else {
        false
    };

    write_standalone_info_plist(&staged_app, p, &bin_stem, &config.vendor, icon_present)?;

    // Factory presets into Contents/Resources/Presets, before
    // codesign so the seal covers them. The installed app resolves
    // them through its own `installed_factory_root`.
    crate::commands::install::presets::emit_standalone_factory(root, p, config, &exe_dst)?;

    codesign_bundle(
        staged_app.to_str().unwrap(),
        &crate::application_identity(),
        false,
    )?;

    Ok(())
}

/// Write a `.app/Contents/Info.plist` for a standalone host bundle.
/// Shared between `commands::run` (dev iteration) and the packaging
/// pipeline so the live-run app and the installed app present
/// identically to the OS - same Dock name, same mic-permission prompt,
/// same hi-DPI flag.
#[cfg(target_os = "macos")]
pub(crate) fn write_standalone_info_plist(
    bundle_root: &Path,
    plugin: &PluginDef,
    bin_stem: &str,
    vendor: &crate::config::VendorConfig,
    icon_present: bool,
) -> Res {
    let mic_usage = format!(
        "{} would like to use the microphone for plugin audio input.",
        plugin.name
    );
    // Emit `CFBundleIconFile` only when the caller staged an
    // `icon.icns` next to Info.plist. macOS will otherwise scribble a
    // missing-resource error in the system log on first launch.
    let icon_key = if icon_present {
        "    <key>CFBundleIconFile</key>\n    <string>icon</string>\n"
    } else {
        ""
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundleDisplayName</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.standalone</string>
    <key>CFBundleExecutable</key>
    <string>{exe}</string>
{icon_key}    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>{mic_usage}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.music</string>
</dict>
</plist>
"#,
        name = plugin.name,
        vendor_id = vendor.id,
        bundle_id = plugin.bundle_id,
        exe = bin_stem,
    );
    fs::write(bundle_root.join("Contents/Info.plist"), plist)?;
    Ok(())
}

/// Generate the distribution.xml for the macOS .pkg installer.
#[cfg(target_os = "macos")]
/// A standalone pkg component carrying out-of-bundle payload (VST3
/// presets install to the OS preset folder, not the plugin bundle).
/// Threaded into `distribution.xml` alongside the format components.
#[cfg(target_os = "macos")]
pub(crate) struct ExtraComponent {
    /// pkg-id suffix, e.g. `vst3presets`.
    pub suffix: String,
    /// File-name + choice segment, e.g. `VST3-Presets`.
    pub label: String,
    /// Choice description shown in Installer.app.
    pub description: String,
    /// `true` when the payload's install-location is user-relocatable
    /// (the VST3 preset root is, like the VST3 bundle) - drives the
    /// per-scope `auth` attribute like a non-system-only format.
    pub user_viable: bool,
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_distribution_xml(
    plugin_name: &str,
    vendor_id: &str,
    bundle_id: &str,
    formats: &[PkgFormat],
    extras: &[ExtraComponent],
    version: &str,
    resources: Option<&MacosPackagingConfig>,
    scope: PkgScope,
) -> String {
    let mut choices_outline = String::new();
    let mut choices = String::new();
    let mut pkg_refs = String::new();

    for fmt in formats {
        let id = fmt.pkg_id_suffix();
        let pkg_id = format!("{vendor_id}.{bundle_id}.{id}");
        let label = fmt.label();
        let desc = fmt.choice_description();
        let component_file = format!("{plugin_name}-{label}.pkg");

        // Every format ships checked by default. Pro Tools users
        // expect AAX to be there without hunting through Customize,
        // and a PACE/iLok-signed build won't reach `--formats aax`
        // unless the developer set it up - at which point shipping
        // it pre-checked is the expected behaviour.
        let enabled_attr = "";

        // Per-choice auth override. pkgbuild stamps every component
        // with `auth="root"` because the install-location sits under
        // `/Library/...` or `/Applications/`; left as-is the
        // installer's `shove` step tries to chown the payload to
        // `root:wheel` even when "Install for me only" relocated the
        // destination to the user's home, and fails with EACCES.
        //
        // - `--user` (explicit): user-viable formats (CLAP, VST3,
        //   LV2, AU v2) override to `auth="None"` so the relocated
        //   `~/Library/Audio/Plug-Ins/...` install runs as the
        //   current user with no chown. System-only formats (AAX,
        //   AU v3, standalone) keep `auth="Root"` so they escalate
        //   for `/Library/...` / `/Applications/`.
        // - `--ask` (default): leave user-viable formats at the
        //   component default - the user might pick "System" at
        //   install time, which needs root either way. System-only
        //   formats still get `auth="Root"` so they always escalate.
        // - `--system`: leave defaults; admin is needed regardless.
        let pkg_ref_auth = match (scope, fmt.is_system_only_on_macos()) {
            (PkgScope::User | PkgScope::Ask, true) => " auth=\"Root\"",
            (PkgScope::User, false) => " auth=\"None\"",
            (PkgScope::Ask | PkgScope::System, _) => "",
        };

        let _ = writeln!(choices_outline, "        <line choice=\"{id}\"/>");
        let _ = write!(
            choices,
            r#"
    <choice id="{id}" title="{label}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"{pkg_ref_auth}/>
    </choice>
"#
        );
        let _ = writeln!(
            pkg_refs,
            "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\">{component_file}</pkg-ref>"
        );
    }

    // Out-of-bundle components (VST3 presets). Same choice / pkg-ref
    // shape as a format; `user_viable` mirrors a non-system-only
    // format's auth treatment so a `--user` install relocates the
    // preset payload to `~` cleanly.
    for ec in extras {
        let pkg_id = format!("{vendor_id}.{bundle_id}.{}", ec.suffix);
        let component_file = format!("{plugin_name}-{}.pkg", ec.label);
        let is_system_only = !ec.user_viable;
        let pkg_ref_auth = match (scope, is_system_only) {
            (PkgScope::User | PkgScope::Ask, true) => " auth=\"Root\"",
            (PkgScope::User, false) => " auth=\"None\"",
            (PkgScope::Ask | PkgScope::System, _) => "",
        };
        let _ = writeln!(choices_outline, "        <line choice=\"{}\"/>", ec.suffix);
        let _ = write!(
            choices,
            r#"
    <choice id="{id}" title="{label}" description="{desc}">
        <pkg-ref id="{pkg_id}"{pkg_ref_auth}/>
    </choice>
"#,
            id = ec.suffix,
            label = ec.label,
            desc = ec.description,
        );
        let _ = writeln!(
            pkg_refs,
            "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\">{component_file}</pkg-ref>"
        );
    }

    let welcome = resources
        .and_then(|r| r.welcome_html.as_deref())
        .map_or("", |_| "    <welcome file=\"welcome.html\"/>\n");
    let license = resources
        .and_then(|r| r.license_html.as_deref())
        .map_or("", |_| "    <license file=\"license.html\"/>\n");

    // Per-scope <domains> drives Installer.app's "Destination Select"
    // page. `--ask` enables both - Installer.app shows the radio
    // buttons. `--user` / `--system` hard-lock the prefix, no page.
    let domains = match scope {
        PkgScope::User => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" \
             enable_localSystem=\"false\"/>\n"
        }
        PkgScope::System => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"false\" \
             enable_localSystem=\"true\"/>\n"
        }
        PkgScope::Ask => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" \
             enable_localSystem=\"true\"/>\n"
        }
    };

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{plugin_name}</title>
{welcome}{license}{domains}    <options customize="always" require-scripts="false"/>

    <choices-outline>
{choices_outline}    </choices-outline>
{choices}
{pkg_refs}</installer-gui-script>
"#
    )
}

/// Build per-format pkgbuild scripts under `staging/<fmt>_scripts/`
/// and return the directory path. Every format gets a `preinstall`
/// that removes any existing bundle at the destination before shove
/// runs - without this, a stale leftover (especially one owned by
/// root from a prior admin install) blocks the new payload with
/// `Permission denied` during the relink step. AU v2 additionally
/// gets a `postinstall` that clears the AU cache so Logic / Garage-
/// Band re-scan and pick up the new bundle.
///
/// The preinstall reads `$2` (the resolved install destination -
/// already accounts for `enable_currentUserHome` relocation) and
/// removes `<destination>/<bundle_name>` if present. When running
/// under root auth (`Install for all users` or a per-pkg-ref
/// `auth="Root"`) the rm succeeds regardless of leftover owner;
/// when running as the user the rm only works on user-owned
/// leftovers and fails loudly with an actionable message otherwise
/// (so the developer doing `cargo truce package --user` after a
/// `--system` round sees what to clean up).
#[cfg(target_os = "macos")]
pub(crate) fn write_format_scripts(
    staging: &Path,
    fmt: &PkgFormat,
    bundle_name: &str,
) -> std::result::Result<PathBuf, crate::CargoTruceError> {
    let scripts_dir = staging.join(format!("{}_scripts", fmt.pkg_id_suffix()));
    let _ = fs::remove_dir_all(&scripts_dir);
    fs::create_dir_all(&scripts_dir)?;

    let escaped_bundle = bundle_name.replace('"', "\\\"");
    let preinstall = scripts_dir.join("preinstall");
    fs::write(
        &preinstall,
        format!(
            "#!/bin/bash\n\
             # `cargo truce package` preinstall: remove any prior\n\
             # bundle at the destination before shove writes ours.\n\
             # `$2` is the resolved install destination (with\n\
             # `enable_currentUserHome` redirection applied).\n\
             set -u\n\
             BUNDLE=\"$2/{escaped_bundle}\"\n\
             if [ -e \"$BUNDLE\" ]; then\n    \
                 if rm -rf \"$BUNDLE\" 2>/dev/null; then\n        \
                     echo \"preinstall: removed existing $BUNDLE\"\n    \
                 else\n        \
                     owner=$(stat -f '%Su' \"$BUNDLE\" 2>/dev/null || echo unknown)\n        \
                     echo \"\" >&2\n        \
                     echo \"ERROR: Cannot remove $BUNDLE (owner: $owner).\" >&2\n        \
                     echo \"Either re-run with 'Install for all users of this computer',\" >&2\n        \
                     echo \"or run: sudo rm -rf \\\"$BUNDLE\\\"\" >&2\n        \
                     exit 1\n    \
                 fi\n\
             fi\n\
             exit 0\n",
        ),
    )?;
    Command::new("chmod")
        .args(["+x", preinstall.to_str().unwrap()])
        .status()?;

    if *fmt == PkgFormat::Au2 {
        let postinstall = scripts_dir.join("postinstall");
        fs::write(
            &postinstall,
            "#!/bin/bash\n\
             killall -9 AudioComponentRegistrar 2>/dev/null || true\n\
             rm -rf ~/Library/Caches/AudioUnitCache/ 2>/dev/null || true\n\
             rm -f ~/Library/Preferences/com.apple.audio.InfoHelper.plist 2>/dev/null || true\n\
             exit 0\n",
        )?;
        Command::new("chmod")
            .args(["+x", postinstall.to_str().unwrap()])
            .status()?;
    }

    Ok(scripts_dir)
}
