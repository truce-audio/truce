//! AAX install: build the C++ template bundle and stage the Rust dylib.

#![allow(unused_imports)]

use crate::templates;
use crate::util::fs_ctx;
use crate::{
    codesign_bundle, release_lib, resolve_aax_sdk_path, run_sudo, tmp_dir,
    Config, PluginDef, Res,
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
    fs_ctx::write(src_dir.join("TruceAAX_Bridge.cpp"), templates::aax::BRIDGE_CPP)?;
    fs_ctx::write(src_dir.join("TruceAAX_Bridge.h"), templates::aax::BRIDGE_H)?;
    fs_ctx::write(src_dir.join("TruceAAX_Describe.cpp"), templates::aax::DESCRIBE_CPP)?;
    fs_ctx::write(src_dir.join("TruceAAX_GUI.cpp"), templates::aax::GUI_CPP)?;
    fs_ctx::write(src_dir.join("TruceAAX_GUI.h"), templates::aax::GUI_H)?;
    fs_ctx::write(src_dir.join("TruceAAX_Parameters.cpp"), templates::aax::PARAMETERS_CPP)?;
    fs_ctx::write(src_dir.join("TruceAAX_Parameters.h"), templates::aax::PARAMETERS_H)?;
    fs_ctx::write(src_dir.join("Info.plist.in"), templates::aax::INFO_PLIST_IN)?;
    fs_ctx::write(src_dir.join("truce_aax_bridge.h"), templates::aax::BRIDGE_HEADER)?;

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
        let cmake = locate_cmake()
            .ok_or("could not locate cmake.exe — install cmake or the VS \"C++ CMake tools\" component")?;
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

        let status = Command::new("cmd")
            .arg("/c")
            .arg(&bat_path)
            .status()?;
        if !status.success() {
            return Err("AAX cmake+ninja build failed".into());
        }
    }
    Ok(())
}


pub(crate) fn install_aax(root: &Path, p: &PluginDef, config: &Config) -> Res {
    // AAX is only supported on macOS and Windows. On Linux (including WSL
    // builds that happen to target Linux), short-circuit before referencing
    // any platform-specific helpers.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (root, p, config);
        eprintln!("AAX: not supported on this platform, skipping {}", p.name);
        return Ok(());
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

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let template = template_binary();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let template: PathBuf = unreachable!();
    // Always invoke `build_aax_template`: it rewrites embedded template
    // sources (so a freshly-built `cargo-truce` with updated templates
    // propagates into the C++ build) and cmake does an incremental
    // rebuild — near-no-op when nothing changed. Previously this was
    // gated on `!template.exists()`, which silently shipped stale C++
    // whenever the Rust↔C++ ABI had shifted since the last install.
    if let Some(sdk_path) = resolve_aax_sdk_path(config) {
        if !template.exists() {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
        }
        // `install` only needs the host arch — universal template builds
        // are reserved for the packaging path (`cargo truce package`).
        build_aax_template(root, &sdk_path, false)?;
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
        return Ok(());
    }
    if !template.exists() {
        return Err(format!(
            "AAX template build succeeded but binary not found at {}",
            template.display()
        )
        .into());
    }

    let dylib = release_lib(root, &format!("{}_aax", p.dylib_stem()));
    if !dylib.exists() {
        eprintln!("AAX: {} not found, skipping {}", dylib.display(), p.name);
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let aax_dir = "/Library/Application Support/Avid/Audio/Plug-Ins";
        let bundle = format!("{aax_dir}/{}.aaxplugin", p.name);
        let contents = format!("{bundle}/Contents");

        run_sudo("rm", &["-rf", &bundle])?;
        run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
        run_sudo("mkdir", &["-p", &format!("{contents}/Resources")])?;

        run_sudo(
            "cp",
            &[
                template.to_str().unwrap(),
                &format!("{contents}/MacOS/{}", p.name),
            ],
        )?;

        run_sudo(
            "cp",
            &[
                dylib.to_str().unwrap(),
                &format!("{contents}/Resources/"),
            ],
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
        let plist_tmp = tmp_dir().join(format!("{}_aax.plist", p.suffix)).to_string_lossy().to_string();
        fs_ctx::write(&plist_tmp, &plist)?;
        run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;

        codesign_bundle(&bundle, config.macos.application_identity(), true)?;
        eprintln!("AAX:  {bundle}");
    }

    #[cfg(target_os = "windows")]
    {
        // Windows AAX bundle layout:
        //   Plugin.aaxplugin/
        //     Contents/
        //       x64/
        //         Plugin.aaxplugin       (template binary, the .dll we built)
        //       Resources/
        //         {name}_aax.dll         (Rust cdylib)
        //
        // Install to %COMMONPROGRAMFILES%\Avid\Audio\Plug-Ins\
        let aax_dir = common_program_files().join("Avid").join("Audio").join("Plug-Ins");
        let bundle = aax_dir.join(format!("{}.aaxplugin", p.name));
        let contents = bundle.join("Contents");
        let x64_dir = contents.join("x64");
        let resources_dir = contents.join("Resources");

        let _ = fs::remove_dir_all(&bundle);
        fs_ctx::create_dir_all(&x64_dir)?;
        fs_ctx::create_dir_all(&resources_dir)?;

        fs_ctx::copy(&template, x64_dir.join(format!("{}.aaxplugin", p.name)))?;
        fs_ctx::copy(&dylib, resources_dir.join(format!("{}_aax.dll", p.dylib_stem())))?;

        eprintln!("AAX:  {}", bundle.display());
    }

    Ok(())
}
