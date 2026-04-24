//! AAX build + install.
//!
//! Split into two phases:
//!
//! - [`build_aax_template`] compiles the embedded C++ template (shared
//!   across plugins) via cmake. Output stays in `tmp_dir()` and is
//!   cached across runs.
//! - [`emit_aax_bundle`] assembles a complete, signed `.aaxplugin`
//!   bundle in `target/bundles/{Plugin}.aaxplugin/` for one plugin.
//!   Requires the Rust `_aax.dylib` to already be built (the outer
//!   `cmd_build` / `cmd_install` loop handles that via the same
//!   `--features aax` cargo invocation as every other per-format
//!   build).
//! - [`install_aax`] copies an existing `target/bundles/*.aaxplugin`
//!   to the system AAX plug-ins directory.
//!
//! See `truce-docs/docs/internal/build-install-split.md`.

#![allow(unused_imports)]

use crate::templates;
use crate::util::fs_ctx;
use crate::{
    codesign_bundle, release_lib, resolve_aax_sdk_path, run_sudo, tmp_dir, Config, PluginDef, Res,
};
#[cfg(target_os = "windows")]
use crate::{common_program_files, locate_cmake, locate_ninja, locate_vcvars64};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn build_aax_template(_root: &Path, sdk_path: &Path, universal_mac: bool) -> Res {
    // Referenced only by the macOS cmake branch below; touch it on Windows so
    // the parameter doesn't trip the unused-variable lint.
    #[cfg(target_os = "windows")]
    let _ = universal_mac;

    // Write embedded template files to a temp directory.
    //
    // Only the source tree is wiped — the sibling `build/` directory is
    // preserved so CMake's incremental build cache survives. The source
    // writes bump each file's mtime, so CMake re-compiles exactly the
    // files whose embedded bytes changed in the cargo-truce binary (and
    // no-ops when nothing has).
    let template_dir = tmp_dir().join("aax_template");
    let src_dir = template_dir.join("src");
    let cmake_lists = template_dir.join("CMakeLists.txt");
    let _ = fs::remove_dir_all(&src_dir);
    let _ = fs::remove_file(&cmake_lists);
    fs_ctx::create_dir_all(&src_dir)?;

    fs_ctx::write(&cmake_lists, templates::aax::CMAKE_LISTS)?;
    fs_ctx::write(
        src_dir.join("TruceAAX_Bridge.cpp"),
        templates::aax::BRIDGE_CPP,
    )?;
    fs_ctx::write(src_dir.join("TruceAAX_Bridge.h"), templates::aax::BRIDGE_H)?;
    fs_ctx::write(
        src_dir.join("TruceAAX_Describe.cpp"),
        templates::aax::DESCRIBE_CPP,
    )?;
    fs_ctx::write(src_dir.join("TruceAAX_GUI.cpp"), templates::aax::GUI_CPP)?;
    fs_ctx::write(src_dir.join("TruceAAX_GUI.h"), templates::aax::GUI_H)?;
    fs_ctx::write(
        src_dir.join("TruceAAX_Parameters.cpp"),
        templates::aax::PARAMETERS_CPP,
    )?;
    fs_ctx::write(
        src_dir.join("TruceAAX_Parameters.h"),
        templates::aax::PARAMETERS_H,
    )?;
    fs_ctx::write(src_dir.join("Info.plist.in"), templates::aax::INFO_PLIST_IN)?;
    fs_ctx::write(
        src_dir.join("truce_aax_bridge.h"),
        templates::aax::BRIDGE_HEADER,
    )?;

    let build_dir = template_dir.join("build");

    #[cfg(not(target_os = "windows"))]
    {
        let mut configure = Command::new("cmake");
        configure
            .arg("-B")
            .arg(&build_dir)
            .arg(format!("-DAAX_SDK_PATH={}", sdk_path.display()));
        if universal_mac {
            configure.arg("-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64");
        }
        let status = configure.current_dir(&template_dir).status()?;
        if !status.success() {
            return Err("cmake configure failed for AAX template".into());
        }
        let status = Command::new("cmake")
            .arg("--build")
            .arg(&build_dir)
            .status()?;
        if !status.success() {
            return Err("cmake build failed for AAX template".into());
        }
    }

    // Windows: cmake's "Visual Studio N YYYY" generators are tied to a specific
    // VS version the cmake binary ships with. If the user's cmake predates the
    // installed VS (common: VS 2026 with an older cmake), the VS generator
    // fails to find MSBuild. Work around this by using the Ninja generator and
    // wrapping the invocation in a vcvars-setup .bat so cl.exe/link.exe are
    // reachable. Ninja also avoids the multi-config output layout.
    #[cfg(target_os = "windows")]
    {
        let vcvars = locate_vcvars64()
            .ok_or("could not locate vcvars64.bat — install VS 2022+ with the C++ workload")?;

        // cmake + ninja aren't necessarily on %PATH% when running outside the
        // truce repo (truce's .cargo/config.toml historically set it). vcvars
        // doesn't add them either. Resolve both explicitly and prepend their
        // directories to the .bat's PATH so the build works from any project.
        let cmake = locate_cmake().ok_or(
            "could not locate cmake.exe — install cmake or the VS \"C++ CMake tools\" component",
        )?;
        let ninja = locate_ninja()
            .ok_or("could not locate ninja.exe — install ninja or the VS \"C++ CMake tools\" component (which bundles it)")?;
        let cmake_dir = cmake.parent().unwrap().display().to_string();
        let ninja_dir = ninja.parent().unwrap().display().to_string();

        // CMake 3.20+ rejects `\U` etc. as invalid escape sequences when a
        // backslash path is interpolated into a generated string literal.
        // Convert all paths we pass to cmake to forward slashes.
        let to_fwd = |p: &Path| p.display().to_string().replace('\\', "/");

        let bat_path = tmp_dir().join("truce_aax_build.bat");
        let bat = format!(
            "@echo off\r\n\
             call \"{vcvars}\" >nul || exit /b 1\r\n\
             set \"PATH={cmake_dir};{ninja_dir};%PATH%\"\r\n\
             cmake -S \"{src}\" -B \"{build}\" -G Ninja -DCMAKE_BUILD_TYPE=Release \"-DAAX_SDK_PATH={sdk}\" || exit /b 1\r\n\
             cmake --build \"{build}\" || exit /b 1\r\n",
            vcvars = vcvars.display(),
            cmake_dir = cmake_dir,
            ninja_dir = ninja_dir,
            src = to_fwd(&template_dir),
            build = to_fwd(&build_dir),
            sdk = to_fwd(sdk_path),
        );
        fs_ctx::write(&bat_path, bat)?;

        let status = Command::new("cmd").arg("/c").arg(&bat_path).status()?;
        if !status.success() {
            return Err("AAX cmake+ninja build failed".into());
        }
    }
    Ok(())
}

/// Template binary path inside the cmake build directory.
#[cfg(target_os = "macos")]
fn template_binary() -> PathBuf {
    tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin/Contents/MacOS/TruceAAXTemplate")
}
#[cfg(target_os = "windows")]
fn template_binary() -> PathBuf {
    // Ninja is single-config — target lands directly in the build dir.
    // CMakeLists.txt sets SUFFIX=.aaxplugin, PREFIX="".
    tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin")
}

/// Ensure the shared AAX cmake template has been built and return its
/// path, or `Ok(None)` if the SDK isn't configured and no prior build
/// exists (the caller treats this as "skip AAX").
///
/// `universal_mac` toggles `-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64`
/// when building for macOS packaging; `false` for host-only dev /
/// install, `true` for `cargo truce package --universal`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn ensure_template(
    root: &Path,
    config: &Config,
    universal_mac: bool,
) -> Result<Option<PathBuf>, crate::BoxErr> {
    let template = template_binary();
    if let Some(sdk_path) = resolve_aax_sdk_path(config) {
        if !template.exists() {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
        }
        build_aax_template(root, &sdk_path, universal_mac)?;
    } else if !template.exists() {
        let hint = if cfg!(target_os = "windows") {
            "[windows].aax_sdk_path"
        } else {
            "[macos].aax_sdk_path"
        };
        eprintln!(
            "AAX: template not built, skipping.\n  \
             Set {hint} in truce.toml or AAX_SDK_PATH env var."
        );
        return Ok(None);
    }
    if !template.exists() {
        return Err(format!(
            "AAX template build succeeded but binary not found at {}",
            template.display()
        )
        .into());
    }
    Ok(Some(template))
}

/// Assemble a signed `.aaxplugin` bundle in `target/bundles/`.
///
/// Requires the Rust `_aax.dylib` to already be built by the outer
/// `cmd_build` / `cmd_install` loop (same pattern as the other
/// format-specific dylibs).
///
/// Steps:
/// 1. Build / reuse the shared cmake C++ template.
/// 2. Assemble `target/bundles/{Plugin Name}.aaxplugin/` with the
///    template binary, Rust dylib, and Info.plist in place.
/// 3. Codesign the bundle (Apple identity) on macOS.
///
/// PACE wraptool signing is separate — `cargo truce package` drives
/// it against the same `target/bundles/` path during the packaging
/// pass.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn emit_aax_bundle(
    _root: &Path,
    p: &PluginDef,
    _config: &Config,
    _universal_mac: bool,
) -> Res {
    eprintln!("AAX: not supported on this platform, skipping {}", p.name);
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn emit_aax_bundle(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    universal_mac: bool,
) -> Res {
    let template = match ensure_template(root, config, universal_mac)? {
        Some(t) => t,
        None => return Ok(()),
    };

    let dylib = release_lib(root, &format!("{}_aax", p.dylib_stem()));
    if !dylib.exists() {
        eprintln!("AAX: {} not found, skipping {}", dylib.display(), p.name);
        return Ok(());
    }

    let bundles_dir = root.join("target/bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;
    let bundle = bundles_dir.join(format!("{}.aaxplugin", p.name));
    let _ = fs::remove_dir_all(&bundle);

    #[cfg(target_os = "macos")]
    {
        let contents = bundle.join("Contents");
        fs_ctx::create_dir_all(contents.join("MacOS"))?;
        fs_ctx::create_dir_all(contents.join("Resources"))?;

        fs_ctx::copy(&template, contents.join("MacOS").join(&p.name))?;
        fs_ctx::copy(
            &dylib,
            contents
                .join("Resources")
                .join(format!("lib{}_aax.dylib", p.dylib_stem())),
        )?;

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
        fs_ctx::write(contents.join("Info.plist"), plist)?;

        // Apple codesign; `true` = recursive (walks the bundle).
        // PACE wraps this signature in a separate packaging step.
        codesign_bundle(
            bundle.to_str().unwrap(),
            config.macos.application_identity(),
            true,
        )?;
        eprintln!("  AAX:  {}", bundle.display());
    }

    #[cfg(target_os = "windows")]
    {
        // Windows AAX bundle layout:
        //   {Plugin}.aaxplugin/
        //     Contents/
        //       x64/
        //         {Plugin}.aaxplugin     (template wrapper .dll)
        //       Resources/
        //         {stem}_aax.dll         (Rust cdylib)
        let contents = bundle.join("Contents");
        let x64_dir = contents.join("x64");
        let resources_dir = contents.join("Resources");
        fs_ctx::create_dir_all(&x64_dir)?;
        fs_ctx::create_dir_all(&resources_dir)?;

        fs_ctx::copy(&template, x64_dir.join(format!("{}.aaxplugin", p.name)))?;
        fs_ctx::copy(
            &dylib,
            resources_dir.join(format!("{}_aax.dll", p.dylib_stem())),
        )?;

        // Authenticode signing is driven by `cargo truce package`'s
        // outer signing loop — the bundle sits unsigned here for
        // `install` to copy verbatim.
        eprintln!("  AAX:  {}", bundle.display());
    }

    Ok(())
}

// AAX is only supported on macOS and Windows.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn install_aax(_root: &Path, p: &PluginDef, _config: &Config) -> Res {
    eprintln!("AAX: not supported on this platform, skipping {}", p.name);
    Ok(())
}

/// Install a pre-built AAX bundle from `target/bundles/` to the
/// system plug-ins directory. Expects [`emit_aax_bundle`] to have
/// been called first.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn install_aax(root: &Path, p: &PluginDef, _config: &Config) -> Res {
    let bundle_name = format!("{}.aaxplugin", p.name);
    let built = root.join("target/bundles").join(&bundle_name);
    if !built.exists() {
        return Err(format!(
            "AAX bundle missing at {}. Run `cargo truce build --aax -p {}` first.",
            built.display(),
            p.suffix,
        )
        .into());
    }

    #[cfg(target_os = "macos")]
    {
        let aax_dir = "/Library/Application Support/Avid/Audio/Plug-Ins";
        let dst = format!("{aax_dir}/{bundle_name}");
        run_sudo("rm", &["-rf", &dst])?;
        run_sudo("ditto", &[built.to_str().unwrap(), &dst])?;
        eprintln!("AAX:  {dst}");
    }

    #[cfg(target_os = "windows")]
    {
        let aax_dir = common_program_files()
            .join("Avid")
            .join("Audio")
            .join("Plug-Ins");
        fs_ctx::create_dir_all(&aax_dir)?;
        let dst = aax_dir.join(&bundle_name);
        let _ = fs::remove_dir_all(&dst);
        crate::util::copy_dir_recursive(&built, &dst)?;
        eprintln!("AAX:  {}", dst.display());
    }

    Ok(())
}
