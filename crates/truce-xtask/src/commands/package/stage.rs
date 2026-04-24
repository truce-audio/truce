//! Format-specific staging: copy the built dylib into a bundle layout,
//! write the per-format Info.plist, and codesign.

use super::PkgFormat;
use crate::{
    codesign_bundle, copy_dir_recursive, release_lib, resolve_aax_sdk_path, tmp_dir,
    Config, PackagingConfig, PluginDef, Res,
};
#[cfg(not(target_os = "windows"))]
use crate::pace_sign_aax_macos;
use std::fs;
use std::path::Path;
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

    let bin_ext = if cfg!(target_os = "windows") { "dll" } else { "so" };
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
        let emit: libloading::Symbol<EmitFn> = lib
            .get(b"__truce_lv2_emit_bundle\0")
            .map_err(|e| format!("{} missing __truce_lv2_emit_bundle: {e}", bin_path.display()))?;
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
    <string>{vendor_id}.{suffix}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
        vendor_id = config.vendor.id,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
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
    <string>com.truce.{suffix}.vst2</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    fs::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
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
    <string>{vendor_id}.{suffix}.component</string>
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
        suffix = p.suffix,
        vendor_id = config.vendor.id,
        vendor = config.vendor.name,
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Stage an AAX bundle into the staging directory.
///
/// `universal_mac` controls whether the AAX C++ template (the wrapper binary
/// Pro Tools launches) is built fat — the Rust cdylib in Resources/ is
/// already lipo'd universal when the caller passes `universal_mac = true`.
#[cfg(not(target_os = "windows"))]
pub(crate) fn stage_aax(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    universal_mac: bool,
    no_pace_sign: bool,
) -> Res {
    let template = tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin/Contents/MacOS/TruceAAXTemplate");
    // Always rebuild the template: it rewrites embedded sources (so
    // template edits in cargo-truce propagate) and cmake incrementally
    // rebuilds only the files whose bytes actually changed.
    if let Some(sdk_path) = resolve_aax_sdk_path(config) {
        if !template.exists() {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
        }
        crate::commands::install::aax::build_aax_template(root, &sdk_path, universal_mac)?;
    } else if !template.exists() {
        return Err("AAX SDK not configured. Set [macos].aax_sdk_path in truce.toml or AAX_SDK_PATH env var.".into());
    }
    if !template.exists() {
        return Err("AAX template build succeeded but binary not found".into());
    }

    let dylib = root.join(format!("target/release/lib{}_aax.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    let bundle = staging.join(format!("{}.aaxplugin", p.name));
    let contents = bundle.join("Contents");
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::create_dir_all(contents.join("Resources"))?;
    fs::copy(&template, contents.join("MacOS").join(&p.name))?;
    fs::copy(&dylib, contents.join("Resources").join(format!("lib{}_aax.dylib", p.dylib_stem())))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.truce.{suffix}.aax</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>TDMw</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
    );
    fs::write(contents.join("Info.plist"), &plist)?;

    // Sign inside-out: inner dylib first, then the outer bundle.
    // notarization rejects bundles where nested binaries lack hardened
    // runtime + timestamp.
    let inner_dylib = contents.join("Resources").join(format!("lib{}_aax.dylib", p.dylib_stem()));
    codesign_bundle(inner_dylib.to_str().unwrap(), config.macos.application_identity(), false)?;
    let inner_exe = contents.join("MacOS").join(&p.name);
    codesign_bundle(inner_exe.to_str().unwrap(), config.macos.application_identity(), false)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;

    // PACE wraps the Apple-signed bundle and re-signs with hardened runtime
    // via --dsigharden. Must be the last touch on the bundle — pkgbuild reads
    // the staging tree directly, so we're safe.
    if !no_pace_sign {
        pace_sign_aax_macos(&bundle)?;
    }
    Ok(())
}

/// Stage an AU v3 .app bundle into the staging directory.
/// Copies from the xcodebuild output in target/tmp/.
pub(crate) fn stage_au3(_root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let app_name = format!("{}.app", p.au3_app_name());
    let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
    let built_app = build_dir.join("build/Release/TruceAUv3.app");
    if !built_app.exists() {
        return Err(format!("AU v3 app not built: {}. Run the build step first.", built_app.display()).into());
    }

    let dst = staging.join(&app_name);
    // May be root-owned from a previous install-based run
    if dst.exists() {
        if fs::remove_dir_all(&dst).is_err() {
            let _ = Command::new("rm").args(["-rf", dst.to_str().unwrap()]).status();
        }
    }
    copy_dir_recursive(&built_app, &dst)?;

    // Copy framework into app
    let fw_name = p.fw_name();
    let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));
    let fw_src = fw_build.join(format!("{fw_name}.framework"));
    if fw_src.exists() {
        let fw_dst = dst.join("Contents/Frameworks");
        fs::create_dir_all(&fw_dst)?;
        copy_dir_recursive(&fw_src, &fw_dst.join(format!("{fw_name}.framework")))?;
    }

    codesign_bundle(dst.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Generate the distribution.xml for the macOS .pkg installer.
pub(crate) fn generate_distribution_xml(
    plugin_name: &str,
    vendor_id: &str,
    suffix: &str,
    formats: &[PkgFormat],
    version: &str,
    resources: Option<&PackagingConfig>,
) -> String {
    let mut choices_outline = String::new();
    let mut choices = String::new();
    let mut pkg_refs = String::new();

    for fmt in formats {
        let id = fmt.pkg_id_suffix();
        let pkg_id = format!("{vendor_id}.{suffix}.{id}");
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
    Command::new("chmod").args(["+x", script.to_str().unwrap()]).status()?;
    Ok(())
}
