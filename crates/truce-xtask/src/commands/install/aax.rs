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

// AAX is macOS / Windows only — Avid's SDK ships no Linux libs and
// Pro Tools doesn't run on Linux. The function isn't defined on Linux
// at all; the `pub(crate) use ...build_aax_template;` re-export in
// `lib.rs` is matched by the same gate.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn build_aax_template(_root: &Path, sdk_path: &Path, universal_mac: bool) -> Res {
    // Referenced only by the macOS cmake branch below; touch it on Windows so
    // the parameter doesn't trip the unused-variable lint.
    #[cfg(target_os = "windows")]
    let _ = universal_mac;

    // Per-process memo: the template is identical across plugins in
    // a single cargo-truce invocation, so we only need to run cmake
    // once per (sdk_path, universal_mac) combination. Subsequent
    // calls (e.g. the per-plugin loop in `cmd_install`) are no-ops.
    #[cfg(not(target_os = "windows"))]
    let memo_key = format!("{}|{}", sdk_path.display(), universal_mac);
    #[cfg(target_os = "windows")]
    let memo_key = sdk_path.display().to_string();
    {
        use std::sync::{Mutex, OnceLock};
        static MEMO: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
        let set = MEMO.get_or_init(|| Mutex::new(Default::default()));
        if set.lock().map(|s| s.contains(&memo_key)).unwrap_or(false) {
            return Ok(());
        }
        // Record *before* we finish so a subsequent call on failure
        // doesn't silently skip — but also *before* cmake, because
        // if the build succeeds there's no reason to re-run it in
        // the same process.
        let _ = set.lock().map(|mut s| s.insert(memo_key.clone()));
    }

    // Ensure the AAX SDK's static library is built with the right
    // architecture coverage. Avid's SDK ships as source — the `.a` /
    // `.lib` only exists after the developer has built it themselves,
    // and on Apple Silicon the default build is arm64-only. Our
    // template cmake falls through to building from source whenever
    // the pre-built library is missing or single-arch, but running
    // the SDK's own cmake up-front into a dedicated `build-truce/`
    // tree gives us:
    //   - A fat library in `{sdk}/build-truce/Libs/AAXLibrary/` that
    //     `-DAAX_LIB_PATH` points our template at on subsequent runs.
    //   - A stable cache that survives `target/tmp/` wipes.
    //   - No `add_subdirectory()` pull of AAXLibrary into our
    //     template's build tree on every reconfigure.
    #[cfg(not(target_os = "windows"))]
    let aax_lib_path = ensure_aax_sdk_library(sdk_path, universal_mac)?;
    #[cfg(target_os = "windows")]
    let aax_lib_path = ensure_aax_sdk_library(sdk_path)?;

    // Write embedded template files to a temp directory.
    //
    // Use `write_if_changed` so unchanged files keep their old
    // mtime — cmake then correctly skips recompilation when the
    // embedded template bytes haven't shifted since last run.
    // Previously we wiped `src/` on every invocation, which forced
    // cmake to rebuild every TU on every plugin.
    let template_dir = tmp_dir().join("aax_template");
    let src_dir = template_dir.join("src");
    let cmake_lists = template_dir.join("CMakeLists.txt");
    fs_ctx::create_dir_all(&src_dir)?;

    fs_ctx::write_if_changed(&cmake_lists, templates::aax::CMAKE_LISTS)?;
    fs_ctx::write_if_changed(
        src_dir.join("TruceAAX_Bridge.cpp"),
        templates::aax::BRIDGE_CPP,
    )?;
    fs_ctx::write_if_changed(src_dir.join("TruceAAX_Bridge.h"), templates::aax::BRIDGE_H)?;
    fs_ctx::write_if_changed(
        src_dir.join("TruceAAX_Describe.cpp"),
        templates::aax::DESCRIBE_CPP,
    )?;
    fs_ctx::write_if_changed(src_dir.join("TruceAAX_GUI.cpp"), templates::aax::GUI_CPP)?;
    fs_ctx::write_if_changed(src_dir.join("TruceAAX_GUI.h"), templates::aax::GUI_H)?;
    fs_ctx::write_if_changed(
        src_dir.join("TruceAAX_Parameters.cpp"),
        templates::aax::PARAMETERS_CPP,
    )?;
    fs_ctx::write_if_changed(
        src_dir.join("TruceAAX_Parameters.h"),
        templates::aax::PARAMETERS_H,
    )?;
    fs_ctx::write_if_changed(src_dir.join("Info.plist.in"), templates::aax::INFO_PLIST_IN)?;
    fs_ctx::write_if_changed(
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
            .arg(format!("-DAAX_SDK_PATH={}", sdk_path.display()))
            .arg(format!("-DAAX_LIB_PATH={}", aax_lib_path.display()));
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
             cmake -S \"{src}\" -B \"{build}\" -G Ninja -DCMAKE_BUILD_TYPE=Release \"-DAAX_SDK_PATH={sdk}\" \"-DAAX_LIB_PATH={lib}\" || exit /b 1\r\n\
             cmake --build \"{build}\" || exit /b 1\r\n",
            vcvars = vcvars.display(),
            cmake_dir = cmake_dir,
            ninja_dir = ninja_dir,
            src = to_fwd(&template_dir),
            build = to_fwd(&build_dir),
            sdk = to_fwd(sdk_path),
            lib = to_fwd(&aax_lib_path),
        );
        fs_ctx::write(&bat_path, bat)?;

        let status = Command::new("cmd").arg("/c").arg(&bat_path).status()?;
        if !status.success() {
            return Err("AAX cmake+ninja build failed".into());
        }
    }
    Ok(())
}

/// Build / reuse `libAAXLibrary` under `{sdk}/build-truce/` and return
/// the absolute path to the resulting static library.
///
/// The SDK ships as source and, on Apple Silicon, the developer's
/// default cmake build produces an arm64-only `.a`. Universal
/// packaging needs both arches. We maintain our own `build-truce/`
/// tree next to whatever the developer built in `build-lib/` so we
/// don't clobber their artifacts but can still guarantee the arch
/// coverage we need. Incremental cmake makes subsequent runs a
/// no-op once the library is warm.
#[cfg(target_os = "macos")]
fn ensure_aax_sdk_library(sdk_path: &Path, universal_mac: bool) -> Result<PathBuf, crate::BoxErr> {
    let build_dir = sdk_path.join("build-truce");
    let lib_path = build_dir.join("Libs/AAXLibrary/libAAXLibrary.a");

    let required_archs: &[&str] = if universal_mac {
        &["arm64", "x86_64"]
    } else if cfg!(target_arch = "aarch64") {
        &["arm64"]
    } else {
        &["x86_64"]
    };

    if lib_path.exists() && lipo_has_archs(&lib_path, required_archs) {
        return Ok(lib_path);
    }

    // Stale single-arch cache from a previous non-universal run.
    // cmake caches CMAKE_OSX_ARCHITECTURES in CMakeCache.txt and
    // silently honors the old value unless we clean.
    let _ = fs::remove_dir_all(&build_dir);

    crate::vprintln!(
        "AAX: building SDK library ({}) at {}",
        required_archs.join("+"),
        lib_path.display()
    );

    let osx_arches = required_archs.join(";");
    let status = Command::new("cmake")
        .arg("-S")
        .arg(sdk_path)
        .arg("-B")
        .arg(&build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!("-DCMAKE_OSX_ARCHITECTURES={osx_arches}"))
        .arg("-DAAX_BUILD_EXAMPLES=OFF")
        .arg("-DAAX_BUILD_PTSL_EXAMPLES=OFF")
        .arg("-DAAX_BUILD_JUCE_GUI_EXTENSION=OFF")
        .status()?;
    if !status.success() {
        return Err("cmake configure failed for AAX SDK library".into());
    }

    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--target")
        .arg("AAXLibrary")
        .status()?;
    if !status.success() {
        return Err("cmake build failed for AAX SDK library".into());
    }

    if !lib_path.exists() {
        return Err(format!(
            "cmake succeeded but libAAXLibrary.a is missing at {}",
            lib_path.display()
        )
        .into());
    }
    if !lipo_has_archs(&lib_path, required_archs) {
        return Err(format!(
            "built libAAXLibrary.a at {} does not cover required archs {required_archs:?}",
            lib_path.display()
        )
        .into());
    }
    Ok(lib_path)
}

/// Windows variant: single-arch, no lipo check.
#[cfg(target_os = "windows")]
fn ensure_aax_sdk_library(sdk_path: &Path) -> Result<PathBuf, crate::BoxErr> {
    let build_dir = sdk_path.join("build-truce");
    let lib_path = build_dir.join("Libs/AAXLibrary/AAXLibrary.lib");
    if lib_path.exists() {
        return Ok(lib_path);
    }

    crate::vprintln!("AAX: building SDK library at {}", lib_path.display());

    let vcvars = locate_vcvars64()
        .ok_or("could not locate vcvars64.bat — install VS 2022+ with the C++ workload")?;
    let cmake = locate_cmake().ok_or(
        "could not locate cmake.exe — install cmake or the VS \"C++ CMake tools\" component",
    )?;
    let ninja = locate_ninja().ok_or(
        "could not locate ninja.exe — install ninja or the VS \"C++ CMake tools\" component",
    )?;
    let cmake_dir = cmake.parent().unwrap().display().to_string();
    let ninja_dir = ninja.parent().unwrap().display().to_string();
    let to_fwd = |p: &Path| p.display().to_string().replace('\\', "/");

    let bat_path = tmp_dir().join("truce_aax_sdk_build.bat");
    let bat = format!(
        "@echo off\r\n\
         call \"{vcvars}\" >nul || exit /b 1\r\n\
         set \"PATH={cmake_dir};{ninja_dir};%PATH%\"\r\n\
         cmake -S \"{src}\" -B \"{build}\" -G Ninja -DCMAKE_BUILD_TYPE=Release -DAAX_BUILD_EXAMPLES=OFF -DAAX_BUILD_PTSL_EXAMPLES=OFF -DAAX_BUILD_JUCE_GUI_EXTENSION=OFF || exit /b 1\r\n\
         cmake --build \"{build}\" --target AAXLibrary || exit /b 1\r\n",
        vcvars = vcvars.display(),
        cmake_dir = cmake_dir,
        ninja_dir = ninja_dir,
        src = to_fwd(sdk_path),
        build = to_fwd(&build_dir),
    );
    fs_ctx::write(&bat_path, bat)?;
    let status = Command::new("cmd").arg("/c").arg(&bat_path).status()?;
    if !status.success() {
        return Err("AAX SDK cmake+ninja build failed".into());
    }
    if !lib_path.exists() {
        return Err(format!(
            "cmake succeeded but AAXLibrary.lib is missing at {}",
            lib_path.display()
        )
        .into());
    }
    Ok(lib_path)
}

#[cfg(target_os = "macos")]
fn lipo_has_archs(lib: &Path, required: &[&str]) -> bool {
    let out = match Command::new("lipo").arg("-info").arg(lib).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return false,
    };
    let text = String::from_utf8_lossy(&out);
    required.iter().all(|a| text.contains(a))
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
            crate::vprintln!("AAX: building template with SDK at {}", sdk_path.display());
        }
        build_aax_template(root, &sdk_path, universal_mac)?;
    } else if !template.exists() {
        // Per-plugin skip lines are emitted by `install_aax`; `ensure_template`
        // is called once per plugin during the build phase but the resolution
        // direction is the same for every plugin, so the user-facing surface
        // lives in install_aax (which knows the plugin name).
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
    _p: &PluginDef,
    _config: &Config,
    _universal_mac: bool,
) -> Res {
    crate::log_skip(
        "AAX: not supported on this platform. Use macOS or Windows to build AAX.".to_string(),
    );
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
        None => {
            // SDK missing — log per-plugin so the user sees one Skipped
            // entry per (AAX, plugin) target. Both `cargo truce build`
            // and `cargo truce install` flow through this point so the
            // log fires from either entry.
            let hint = if cfg!(target_os = "windows") {
                "[windows].aax_sdk_path"
            } else {
                "[macos].aax_sdk_path"
            };
            crate::log_skip(format!(
                "AAX: skipped {} — SDK not configured. \
                 Set {hint} in truce.toml or the AAX_SDK_PATH env var.",
                p.name
            ));
            return Ok(());
        }
    };

    let dylib = release_lib(root, &format!("{}_aax", p.dylib_stem()));
    if !dylib.exists() {
        crate::log_skip(format!(
            "AAX: build artifact missing for {} at {}. \
             Re-run `cargo truce build --aax -p {}`.",
            p.name,
            dylib.display(),
            p.crate_name,
        ));
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
    <string>com.truce.{bundle_id}.aax</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>TDMw</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            name = p.name,
            bundle_id = p.bundle_id,
        );
        fs_ctx::write(contents.join("Info.plist"), plist)?;

        // Apple codesign against `target/bundles/` — user-owned, no
        // sudo needed. PACE wraps this signature in a separate
        // packaging step.
        codesign_bundle(
            bundle.to_str().unwrap(),
            config.macos.application_identity(),
            false,
        )?;
        crate::vprintln!("  AAX:  {}", bundle.display());
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
        crate::vprintln!("  AAX:  {}", bundle.display());
    }

    Ok(())
}

// AAX is only supported on macOS and Windows.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn install_aax(_root: &Path, _p: &PluginDef, _config: &Config) -> Res {
    crate::log_skip(
        "AAX: not supported on this platform. Use macOS or Windows to install AAX.".to_string(),
    );
    Ok(())
}

/// Install a pre-built AAX bundle from `target/bundles/` to the
/// system plug-ins directory. Expects [`emit_aax_bundle`] to have
/// been called first.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn install_aax(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let bundle_name = format!("{}.aaxplugin", p.name);
    let built = root.join("target/bundles").join(&bundle_name);
    if !built.exists() {
        // No SDK → emit_aax_bundle (build phase) already logged a per-plugin
        // skip; just no-op here so we don't double-log. SDK present → genuine
        // state mismatch, point the user at the build command.
        if resolve_aax_sdk_path(config).is_none() {
            return Ok(());
        }
        return Err(format!(
            "AAX: bundle missing for {} at {}. Run `cargo truce build --aax -p {}` to produce it.",
            p.name,
            built.display(),
            p.crate_name,
        )
        .into());
    }

    #[cfg(target_os = "macos")]
    {
        let aax_dir = "/Library/Application Support/Avid/Audio/Plug-Ins";
        let dst = format!("{aax_dir}/{bundle_name}");
        run_sudo("rm", &["-rf", &dst])?;
        run_sudo("ditto", &[built.to_str().unwrap(), &dst])?;
        crate::log_output(format!("AAX:  {dst}"));
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
        crate::log_output(format!("AAX:  {}", dst.display()));
    }

    Ok(())
}
