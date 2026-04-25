//! Format-specific staging: copy the built dylib into a bundle layout,
//! write the per-format Info.plist, and codesign.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::PkgFormat;
#[cfg(target_os = "macos")]
use crate::pace_sign_aax_macos;
use crate::{codesign_bundle, release_lib, Config, PluginDef, Res};
#[cfg(target_os = "macos")]
use crate::{copy_dir_recursive, PackagingConfig};
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

/// Slug a plugin's display name into a lowercase, hyphenated, ASCII-safe
/// identifier suitable for LV2 bundle / file / IRI use.
pub(crate) fn lv2_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Stage an LV2 bundle into `staging/{slug}.lv2/`. Copies the built
/// `lib{stem}_lv2.{ext}` shared library into the bundle, then `dlopen`s
/// it and calls `__truce_lv2_emit_bundle` to emit `manifest.ttl` +
/// `plugin.ttl`.
pub(crate) fn stage_lv2(root: &Path, p: &PluginDef, staging: &Path) -> Res {
    use std::ffi::{c_char, CString};
    let built = release_lib(root, &format!("{}_lv2", p.dylib_stem()));
    if !built.exists() {
        return Err(format!("Missing: {}", built.display()).into());
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

    // Load the staged binary so LV2_PATH resolution lines up with what
    // the host sees.
    let bundle_cstr = CString::new(bundle.to_string_lossy().as_bytes())?;
    let bin_cstr = CString::new(bin_name.clone())?;
    unsafe {
        let lib = libloading::Library::new(&bin_path)
            .map_err(|e| format!("load {} failed: {e}", bin_path.display()))?;
        type EmitFn = unsafe extern "C" fn(*const c_char, *const c_char) -> i32;
        let emit: libloading::Symbol<EmitFn> =
            lib.get(b"__truce_lv2_emit_bundle\0").map_err(|e| {
                format!(
                    "{} missing __truce_lv2_emit_bundle: {e}",
                    bin_path.display()
                )
            })?;
        let rc = emit(bundle_cstr.as_ptr(), bin_cstr.as_ptr());
        if rc != 0 {
            return Err(format!("LV2 TTL emission failed (rc={rc})").into());
        }
    }
    Ok(())
}

/// Stage a CLAP bundle into the staging directory.
pub(crate) fn stage_clap(root: &Path, p: &PluginDef, staging: &Path, identity: &str) -> Res {
    let dylib = release_lib(root, &format!("{}_clap", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let dst = staging.join(format!("{}.clap", p.name));
    fs::copy(&dylib, &dst)?;
    codesign_bundle(dst.to_str().unwrap(), identity, false)?;
    Ok(())
}

/// Stage a VST3 bundle into the staging directory.
pub(crate) fn stage_vst3(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = release_lib(root, &format!("{}_vst3", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.vst3", p.name));
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    fs::copy(&dylib, macos_dir.join(&p.name))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        bundle_id = p.bundle_id,
        vendor_id = config.vendor.id,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(
        bundle.to_str().unwrap(),
        config.macos.application_identity(),
        false,
    )?;
    Ok(())
}

/// Stage a VST2 bundle into the staging directory.
pub(crate) fn stage_vst2(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = root.join(format!("target/release/lib{}_vst2.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.vst", p.name));
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    fs::copy(&dylib, macos_dir.join(&p.name))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.truce.{bundle_id}.vst2</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        bundle_id = p.bundle_id,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    fs::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;
    codesign_bundle(
        bundle.to_str().unwrap(),
        config.macos.application_identity(),
        false,
    )?;
    Ok(())
}

/// Stage an AU v2 bundle into the staging directory.
pub(crate) fn stage_au2(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = root.join(format!("target/release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.component", p.name));
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    fs::copy(&dylib, macos_dir.join(&p.name))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.component</string>
    <key>CFBundleName</key>
    <string>{name}</string>
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
            <string>{vendor}: {name}</string>
            <key>description</key>
            <string>{name}</string>
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
        name = p.name,
        bundle_id = p.bundle_id,
        vendor_id = config.vendor.id,
        vendor = config.vendor.name,
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(
        bundle.to_str().unwrap(),
        config.macos.application_identity(),
        false,
    )?;
    Ok(())
}

/// Stage an AAX bundle into the staging directory.
///
/// `universal_mac` controls whether the AAX C++ template (the wrapper binary
/// Pro Tools launches) is built fat — the Rust cdylib in Resources/ is
/// already lipo'd universal when the caller passes `universal_mac = true`.
/// Stage an AAX bundle into the packaging staging tree.
///
/// Reads from `target/bundles/{Plugin}.aaxplugin/` — the fully-
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
    let bundle_name = format!("{}.aaxplugin", p.name);
    let built = root.join("target/bundles").join(&bundle_name);
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
/// Reads from `target/bundles/{Plugin}.app/` — the fully-signed output
/// of [`emit_au_v3_bundle`] — and copies it into the staging tree.
/// The bundle is already signed + has its embedded framework, so this
/// is a pure copy.
#[cfg(target_os = "macos")]
pub(crate) fn stage_au3(root: &Path, p: &PluginDef, _config: &Config, staging: &Path) -> Res {
    let app_name = format!("{}.app", p.au3_app_name());
    let built_app = root.join("target/bundles").join(&app_name);
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

/// Generate the distribution.xml for the macOS .pkg installer.
#[cfg(target_os = "macos")]
pub(crate) fn generate_distribution_xml(
    plugin_name: &str,
    vendor_id: &str,
    bundle_id: &str,
    formats: &[PkgFormat],
    version: &str,
    resources: Option<&PackagingConfig>,
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

        // AAX unchecked by default (may need PACE signing for distribution)
        let enabled_attr = if *fmt == PkgFormat::Aax {
            "\n            selected=\"false\""
        } else {
            ""
        };

        choices_outline.push_str(&format!("        <line choice=\"{id}\"/>\n"));
        choices.push_str(&format!(
            r#"
    <choice id="{id}" title="{label}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"/>
    </choice>
"#
        ));
        pkg_refs.push_str(&format!(
            "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\"\
             >{component_file}</pkg-ref>\n"
        ));
    }

    let welcome = resources
        .and_then(|r| r.welcome_html.as_deref())
        .map(|_| "    <welcome file=\"welcome.html\"/>\n")
        .unwrap_or("");
    let license = resources
        .and_then(|r| r.license_html.as_deref())
        .map(|_| "    <license file=\"license.html\"/>\n")
        .unwrap_or("");

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{plugin_name}</title>
{welcome}{license}
    <options customize="always" require-scripts="false"/>

    <choices-outline>
{choices_outline}    </choices-outline>
{choices}
{pkg_refs}</installer-gui-script>
"#
    )
}

/// Write AU cache clearing post-install script for AU component packages.
#[cfg(target_os = "macos")]
pub(crate) fn write_postinstall_script(dir: &Path) -> Res {
    let scripts_dir = dir.join("scripts");
    fs::create_dir_all(&scripts_dir)?;
    let script = scripts_dir.join("postinstall");
    fs::write(
        &script,
        "#!/bin/bash\n\
         killall -9 AudioComponentRegistrar 2>/dev/null || true\n\
         rm -rf ~/Library/Caches/AudioUnitCache/ 2>/dev/null || true\n\
         rm -f ~/Library/Preferences/com.apple.audio.InfoHelper.plist 2>/dev/null || true\n\
         exit 0\n",
    )?;
    // Make executable
    Command::new("chmod")
        .args(["+x", script.to_str().unwrap()])
        .status()?;
    Ok(())
}
